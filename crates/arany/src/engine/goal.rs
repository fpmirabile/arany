use super::{Engine, EngineError};
use crate::session::{BudgetRefusal, Event, GoalId, GoalReport, GoalStatus, SessionId};

pub(super) struct GoalClose {
    pub(super) status: GoalStatus,
    pub(super) active_ms: u64,
    pub(super) report: GoalReport,
    pub(super) exhausted: bool,
    pub(super) budget_refusal: Option<BudgetRefusal>,
}

impl Engine {
    pub(super) async fn finish_goal(
        &self,
        session_id: SessionId,
        goal_id: GoalId,
        close: GoalClose,
    ) -> Result<(), EngineError> {
        let GoalClose {
            status,
            active_ms,
            report,
            exhausted,
            budget_refusal,
        } = close;
        if exhausted && budget_refusal.is_none() {
            return Err(EngineError::InvalidHistory);
        }
        self.append_goal_lifecycle(
            session_id,
            None,
            Event::GoalFinished {
                goal_id,
                status,
                active_ms,
                report,
                exhausted,
                budget_refusal,
            },
        )
        .await
        .map_err(|error| match (error, budget_refusal) {
            (EngineError::GoalAdmissionUncertain, _) => EngineError::GoalAdmissionUncertain,
            (source, Some(refusal)) => EngineError::GoalBudgetUncommitted {
                refusal: Box::new(refusal),
                source: Box::new(source),
            },
            (source, None) => source,
        })?;
        Ok(())
    }
}
