use crate::{EffectIntent, ToolApprovals};

pub async fn ask(approvals: &ToolApprovals, intent: &EffectIntent) -> bool {
    approvals.ask(intent).await
}

pub async fn delay_withdrawal(
    approvals: ToolApprovals,
    intent: EffectIntent,
    reject: tokio::sync::oneshot::Receiver<()>,
    rejected: tokio::sync::oneshot::Sender<()>,
    committed: tokio::sync::oneshot::Receiver<()>,
) {
    let mut pending = approvals.begin_local(&intent, false, None).await;
    if reject.await.is_err() {
        return;
    }
    let Some(withdrawal) = pending.reject_late_replies() else {
        return;
    };
    let _ = rejected.send(());
    if committed.await.is_ok() {
        withdrawal.commit();
    }
}
