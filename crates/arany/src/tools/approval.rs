use super::{EffectIntent, EnforcementCause, ToolCall};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Request,
    #[default]
    AutoEdits,
    Auto,
}

impl ApprovalMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Request => "Request approvals",
            Self::AutoEdits => "Auto edits",
            Self::Auto => "Auto approval",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Request => Self::Auto,
            Self::Auto => Self::AutoEdits,
            Self::AutoEdits => Self::Request,
        }
    }

    pub(crate) fn asks(self, call: &ToolCall) -> bool {
        let facts = call.facts();
        facts.asks_approval && (self != Self::AutoEdits || !facts.typed_edit())
    }
}

#[derive(Clone)]
pub struct ToolApprovals {
    mode: watch::Sender<ApprovalMode>,
    sender: mpsc::Sender<ToolApproval>,
    human_only: bool,
}

pub struct ApprovalInbox(mpsc::Receiver<ToolApproval>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalReply {
    Deny,
    AllowOnce,
    AllowAlwaysInProject,
    AllowPrefixInFolder,
}

impl From<bool> for ApprovalReply {
    fn from(allow: bool) -> Self {
        if allow { Self::AllowOnce } else { Self::Deny }
    }
}

/// What the user needs, beyond the intent, to judge an unguarded re-run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnguardedPrompt {
    pub executable: String,
    pub attribution: Option<EnforcementCause>,
    pub remember: bool,
}

pub struct ToolApproval {
    pub intent: EffectIntent,
    pub unguarded: Option<UnguardedPrompt>,
    pub allow_auto_edits: bool,
    pub remember_prefix: Option<super::permissions::TypedGitPrefix>,
    reply: oneshot::Sender<ApprovalReply>,
}

impl ToolApproval {
    pub fn decide(self, reply: impl Into<ApprovalReply>) {
        let _ = self.reply.send(reply.into());
    }

    pub async fn abandoned(&mut self) {
        self.reply.closed().await;
    }
}

impl ApprovalInbox {
    pub async fn next(&mut self) -> Option<ToolApproval> {
        self.0.recv().await
    }
}

impl ToolApprovals {
    pub fn new(mode: ApprovalMode) -> (Self, ApprovalInbox) {
        let (sender, receiver) = mpsc::channel(1);
        let (mode, _) = watch::channel(mode);
        (
            Self {
                mode,
                sender,
                human_only: true,
            },
            ApprovalInbox(receiver),
        )
    }

    pub fn without_human_only(mut self) -> Self {
        self.human_only = false;
        self
    }

    pub(crate) fn admits_human_only(&self, call: &ToolCall, hook_ask: bool) -> bool {
        self.human_only || !(call.is_unguarded() || hook_ask)
    }

    pub fn mode(&self) -> ApprovalMode {
        *self.mode.borrow()
    }

    pub fn set_mode(&self, mode: ApprovalMode) {
        self.mode.send_replace(mode);
    }

    pub(crate) async fn ask(&self, intent: &EffectIntent) -> bool {
        self.ask_local(intent, false, None).await != ApprovalReply::Deny
    }

    pub(crate) async fn ask_local(
        &self,
        intent: &EffectIntent,
        allow_auto_edits: bool,
        remember_prefix: Option<super::permissions::TypedGitPrefix>,
    ) -> ApprovalReply {
        self.request(intent, None, allow_auto_edits, remember_prefix)
            .await
    }

    pub(crate) async fn ask_unguarded(
        &self,
        intent: &EffectIntent,
        prompt: UnguardedPrompt,
    ) -> ApprovalReply {
        self.request(intent, Some(prompt), false, None).await
    }

    async fn request(
        &self,
        intent: &EffectIntent,
        unguarded: Option<UnguardedPrompt>,
        allow_auto_edits: bool,
        remember_prefix: Option<super::permissions::TypedGitPrefix>,
    ) -> ApprovalReply {
        let (reply, response) = oneshot::channel();
        if self
            .sender
            .send(ToolApproval {
                intent: intent.clone(),
                unguarded,
                allow_auto_edits,
                remember_prefix,
                reply,
            })
            .await
            .is_err()
        {
            return ApprovalReply::Deny;
        }
        response.await.unwrap_or(ApprovalReply::Deny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_only_preapprove_typed_file_edits_in_auto_edits() {
        let cases = [
            (ToolCall::Mkdir { path: "d".into() }, [true, false, true]),
            (
                ToolCall::Write {
                    path: "a".into(),
                    expected_digest: None,
                    content: "x".into(),
                },
                [true, false, true],
            ),
            (
                ToolCall::Edit {
                    path: "a".into(),
                    expected_digest: "0".repeat(64),
                    old: "x".into(),
                    new: "y".into(),
                },
                [true, false, true],
            ),
            (
                ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::CommandReceipt {
                        source_intent_digest: [1; 32],
                        changeset: super::super::CommandChangeset::default(),
                    },
                },
                [true, false, true],
            ),
            (
                ToolCall::GitCommit {
                    message: "m".into(),
                    paths: vec!["src".into()],
                    staged: false,
                },
                [true; 3],
            ),
            (
                ToolCall::GitCommit {
                    message: "m".into(),
                    paths: Vec::new(),
                    staged: true,
                },
                [true; 3],
            ),
            (
                ToolCall::GitBranch {
                    name: Some("topic".into()),
                    start_point: None,
                    scope: None,
                    base: None,
                    max_count: None,
                },
                [true; 3],
            ),
            (
                ToolCall::GitSwitch {
                    branch: "topic".into(),
                },
                [true; 3],
            ),
            (
                ToolCall::Command {
                    program: "git".into(),
                    args: vec!["status".into()],
                    cwd: ".".into(),
                },
                [true; 3],
            ),
            (
                ToolCall::CommandUnguarded {
                    program: "git".into(),
                    args: vec!["status".into()],
                    cwd: ".".into(),
                    justification: "sandbox failure".into(),
                },
                [true; 3],
            ),
            (
                ToolCall::Read {
                    path: "a".into(),
                    offset: None,
                    limit: Some(1),
                },
                [false; 3],
            ),
            (ToolCall::GitStatus { path: ".".into() }, [false; 3]),
            (
                ToolCall::GitBranch {
                    name: None,
                    start_point: None,
                    scope: None,
                    base: None,
                    max_count: None,
                },
                [false; 3],
            ),
        ];
        for (call, expected) in cases {
            for (mode, asks) in [
                ApprovalMode::Request,
                ApprovalMode::AutoEdits,
                ApprovalMode::Auto,
            ]
            .into_iter()
            .zip(expected)
            {
                assert_eq!(mode.asks(&call), asks, "{mode:?}: {call:?}");
            }
        }
    }

    #[test]
    fn mode_changes_never_restore_human_only_asks() {
        let call = ToolCall::CommandUnguarded {
            program: "x".into(),
            args: Vec::new(),
            cwd: ".".into(),
            justification: "j".into(),
        };
        let (approvals, _inbox) = ToolApprovals::new(ApprovalMode::Request);
        let approvals = approvals.without_human_only();
        for mode in [
            ApprovalMode::Auto,
            ApprovalMode::AutoEdits,
            ApprovalMode::Request,
        ] {
            approvals.set_mode(mode);
            assert_eq!(approvals.mode(), mode);
            assert!(!approvals.admits_human_only(&call, false));
            assert!(!approvals.admits_human_only(&ToolCall::Mkdir { path: "d".into() }, true));
            assert!(approvals.admits_human_only(&ToolCall::Mkdir { path: "d".into() }, false));
        }
    }
}
