use super::tool_action_ms;
use crate::engine::ledger::{Action, ActiveDeadline, Admission};
use crate::engine::run_loop::{CallResult, RunLoop, TOOL_REVIEW_DEADLINE, call_record};
use crate::engine::{EngineError, STORE_ACTION_OVERHEAD};
use crate::provider::{
    AgentPhase, PromptRole, ProviderOutcome, ProviderRequest, ProviderResponse, RunInput,
};
use crate::session::{CollaborationPolicy, Event};
use crate::tools::EffectIntent;
use tokio::time::Instant;

#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolReviewDecision {
    Approve,
    Deny,
}

impl RunLoop<'_> {
    pub(super) async fn review_tool(
        &self,
        objective: &str,
        intent: &EffectIntent,
        active_deadline: Option<&ActiveDeadline>,
    ) -> Result<Admission<ToolReviewDecision>, EngineError> {
        let mut model = String::new();
        let mut max_output_tokens = 256;
        self.routing.bind(
            crate::provider::ProviderRole::Review,
            intent.agent_run_id,
            &mut model,
            &mut max_output_tokens,
        )?;
        let mut review = ProviderRequest {
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id: intent.agent_run_id,
            phase: AgentPhase::ToolReview,
            collaboration: CollaborationPolicy::Single,
            model,
            max_output_tokens,
            input: RunInput {
                goal_budget: Some(self.ledger().effective_budget()?),
                goal_plan_contract: self.ledger().plan_contract()?,
                ..RunInput::new(
                    PromptRole::ToolReview,
                    serde_json::json!({"user_request":objective,"proposed_action":intent})
                        .to_string(),
                )
            },
        };
        let active_segment = if active_deadline.is_none() {
            self.reserve_active_segment(
                TOOL_REVIEW_DEADLINE + STORE_ACTION_OVERHEAD,
                crate::session::BudgetAction::Tool,
                intent.agent_run_id,
            )?
            .map(Some)
        } else {
            Admission::Reserved(None)
        };
        let Admission::Reserved(active_segment) = active_segment else {
            return Ok(Admission::Exhausted);
        };
        // A researcher's Tool runs inside the delegation's reservation, so its review adds no time.
        let pending_ms = match (active_deadline, self.controls.tools) {
            (None, Some(runtime)) => tool_action_ms(runtime, intent),
            _ => 0,
        };
        let Admission::Reserved(()) = self
            .reserve_provider_call(intent.agent_run_id, Action::Review, pending_ms)
            .await?
        else {
            if let Some(active_segment) = active_segment {
                self.settle_active_segment(active_segment)?;
            }
            return Ok(Admission::Exhausted);
        };
        review.input.goal_context_facts = Some(self.ledger().context_facts()?);
        let review_deadline = active_segment
            .as_ref()
            .map(|segment| {
                segment
                    .deadline()
                    .checked_sub(STORE_ACTION_OVERHEAD)
                    .unwrap_or(segment.started)
            })
            .or_else(|| active_deadline.map(ActiveDeadline::wall_deadline))
            .ok_or(EngineError::CoordinatorFailed)?
            .min(Instant::now() + TOOL_REVIEW_DEADLINE);
        let limits = crate::provider::RateLimitSlot::default();
        let provider_activity = self.ledger().provider_activity();
        let result = {
            let _provider_work = provider_activity.start();
            self.routing
                .invoke(
                    review,
                    review_deadline,
                    self.controls.cancellation.clone(),
                    None,
                    limits.clone(),
                )
                .await?
        };
        self.settle_result(&result)?;
        let valid = matches!(
            &result,
            CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Review(_),
                ..
            })
        );
        let valid = valid && self.ledger().tokens_within_limit();
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id: intent.agent_run_id,
            record: call_record(
                AgentPhase::ToolReview,
                &result,
                valid,
                None,
                None,
                limits.take(),
            ),
        })
        .await?;
        if let Some(active_segment) = active_segment {
            self.settle_active_segment(active_segment)?;
        }
        let exhausted = if active_deadline.is_some() {
            self.ledger().spent_active_time_exhausted()
        } else {
            self.ledger().active_time_exhausted()
        };
        Ok(Admission::Reserved(tool_review_decision(
            &result, valid, exhausted,
        )))
    }
}

fn tool_review_decision(result: &CallResult, valid: bool, exhausted: bool) -> ToolReviewDecision {
    if exhausted {
        return ToolReviewDecision::Deny;
    }
    match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Review(crate::ReviewVerdict::Approve),
            ..
        }) if valid => ToolReviewDecision::Approve,
        CallResult::Response(_) | CallResult::Failed(_) | CallResult::Cancelled => {
            ToolReviewDecision::Deny
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderTurn;

    #[test]
    fn tool_review_requires_valid_approval_without_exhaustion() {
        use ToolReviewDecision::{Approve, Deny};

        let response = |outcome| {
            CallResult::Response(ProviderResponse {
                outcome,
                turn: ProviderTurn::scripted(),
                response_id: None,
                usage: crate::provider::Usage::default(),
                wire_provenance: None,
            })
        };
        let rows = [
            (
                "approve",
                response(ProviderOutcome::Review(crate::ReviewVerdict::Approve)),
                [Approve, Deny, Deny, Deny],
            ),
            (
                "ask",
                response(ProviderOutcome::Review(crate::ReviewVerdict::Ask)),
                [Deny; 4],
            ),
            (
                "non-review outcome",
                response(ProviderOutcome::Answer(crate::provider::Answer {
                    text: "answer".into(),
                })),
                [Deny; 4],
            ),
            (
                "failure",
                CallResult::Failed(crate::engine::run_loop::CallFailure::Timeout),
                [Deny; 4],
            ),
            ("cancelled", CallResult::Cancelled, [Deny; 4]),
        ];
        for (name, result, expected) in rows {
            for ((valid, exhausted), expected) in
                [(true, false), (false, false), (true, true), (false, true)]
                    .into_iter()
                    .zip(expected)
            {
                assert_eq!(
                    tool_review_decision(&result, valid, exhausted),
                    expected,
                    "{name}, valid={valid}, exhausted={exhausted}"
                );
            }
        }
    }
}
