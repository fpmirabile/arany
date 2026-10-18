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
        facts.asks_approval && (self != Self::AutoEdits || !facts.mutates)
    }
}

#[derive(Clone)]
pub struct ToolApprovals {
    mode: watch::Sender<ApprovalMode>,
    sender: mpsc::Sender<ToolApproval>,
}

pub struct ApprovalInbox(mpsc::Receiver<ToolApproval>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalReply {
    Deny,
    AllowOnce,
    AllowAlwaysInProject,
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
}

pub struct ToolApproval {
    pub intent: EffectIntent,
    pub unguarded: Option<UnguardedPrompt>,
    reply: oneshot::Sender<ApprovalReply>,
}

impl ToolApproval {
    pub fn decide(self, reply: impl Into<ApprovalReply>) {
        let _ = self.reply.send(reply.into());
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
        (Self { mode, sender }, ApprovalInbox(receiver))
    }

    pub fn mode(&self) -> ApprovalMode {
        *self.mode.borrow()
    }

    pub fn set_mode(&self, mode: ApprovalMode) {
        self.mode.send_replace(mode);
    }

    pub(crate) async fn ask(&self, intent: &EffectIntent) -> bool {
        self.request(intent, None).await != ApprovalReply::Deny
    }

    pub(crate) async fn ask_unguarded(
        &self,
        intent: &EffectIntent,
        prompt: UnguardedPrompt,
    ) -> ApprovalReply {
        self.request(intent, Some(prompt)).await
    }

    async fn request(
        &self,
        intent: &EffectIntent,
        unguarded: Option<UnguardedPrompt>,
    ) -> ApprovalReply {
        let (reply, response) = oneshot::channel();
        if self
            .sender
            .send(ToolApproval {
                intent: intent.clone(),
                unguarded,
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
