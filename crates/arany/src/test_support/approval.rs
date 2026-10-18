use crate::{EffectIntent, ToolApprovals};

pub async fn ask(approvals: &ToolApprovals, intent: &EffectIntent) -> bool {
    approvals.ask(intent).await
}
