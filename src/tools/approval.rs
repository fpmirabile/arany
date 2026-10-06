use super::{EffectIntent, ToolCall};
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
        let effect =
            call.mutates() || matches!(call, ToolCall::Command { .. } | ToolCall::McpCall { .. });
        effect && (self != Self::AutoEdits || !call.mutates())
    }
}

#[derive(Clone)]
pub struct ToolApprovals {
    mode: watch::Sender<ApprovalMode>,
    sender: mpsc::Sender<ToolApproval>,
}

pub struct ApprovalInbox(mpsc::Receiver<ToolApproval>);

pub struct ToolApproval {
    pub intent: EffectIntent,
    reply: oneshot::Sender<bool>,
}

impl ToolApproval {
    pub fn decide(self, allow: bool) {
        let _ = self.reply.send(allow);
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
        let (reply, response) = oneshot::channel();
        if self
            .sender
            .send(ToolApproval {
                intent: intent.clone(),
                reply,
            })
            .await
            .is_err()
        {
            return false;
        }
        response.await.unwrap_or(false)
    }
}
