use super::{ActiveTurn, Fault, input_parts, params, threads::LoadedThread};
use arany::{EngineError, GoalId, GoalRequest, RunId, SessionId, SteeringRefused};
use arany_cli::protocol::{methods::*, rpc::Reason};
use serde_json::{Value, json};

pub(super) enum Start {
    Turn(Option<GoalInput>),
    Resume(GoalId),
}

pub(super) struct Submission {
    pub thread_id: SessionId,
    input: Vec<UserInput>,
    start: Start,
}

impl Submission {
    pub fn parse(method: &str, value: Option<Value>) -> Result<Self, Fault> {
        if method == "goal/resume" {
            let input: GoalResumeParams = params(value)?;
            return Ok(Self {
                thread_id: input.thread_id,
                input: input
                    .input
                    .unwrap_or_default()
                    .into_iter()
                    .map(UserInput::from)
                    .collect(),
                start: Start::Resume(input.goal_id),
            });
        }
        let input: TurnStartParams = params(value)?;
        if input
            .goal
            .as_ref()
            .is_some_and(|goal| goal.resume_goal_id.is_some())
        {
            return Err(Fault::params());
        }
        Ok(Self {
            thread_id: input.thread_id,
            input: input.input,
            start: Start::Turn(input.goal),
        })
    }

    pub fn parts(&mut self) -> Result<(String, Vec<arany::ImageAttachment>), Fault> {
        let input = std::mem::take(&mut self.input);
        if matches!(self.start, Start::Resume(_)) {
            return Ok((text(input).unwrap_or_default(), Vec::new()));
        }
        input_parts(input)
    }

    pub fn goal(
        &self,
        loaded: &LoadedThread,
        view: &arany::SessionView,
    ) -> Result<GoalRequest, Fault> {
        let mut goal = match &self.start {
            Start::Resume(id) => {
                if view.interrupted_goal_id() != Some(*id) {
                    return Err(Fault::engine(EngineError::GoalUnavailable));
                }
                GoalRequest::resume(*id, true)
            }
            Start::Turn(Some(GoalInput {
                autonomous: true, ..
            })) => GoalRequest::autonomous(loaded.goal_budget, arany::GoalPhase::Implementation),
            Start::Turn(_) => GoalRequest::default(),
        };
        if goal.id.is_none() {
            goal.budget = Some(loaded.goal_budget);
            goal.resource_settings = loaded.resource_settings;
        }
        Ok(goal)
    }
}

fn text(input: Vec<UserInput>) -> Option<String> {
    input.into_iter().find_map(|item| match item {
        UserInput::Text { text } => Some(text),
        UserInput::Image { .. } => None,
    })
}

fn running(
    active: Option<&ActiveTurn>,
    thread_id: SessionId,
    turn_id: Option<RunId>,
) -> Result<&ActiveTurn, Fault> {
    active
        .filter(|turn| {
            turn.thread_id == thread_id
                && turn.state.run_id.is_some()
                && turn_id.is_none_or(|id| turn.state.run_id == Some(id))
                && !turn.result.is_finished()
        })
        .ok_or_else(|| Fault::state(Reason::TurnNotActive))
}

pub(super) fn steer(active: Option<&ActiveTurn>, value: Option<Value>) -> Result<Value, Fault> {
    let input: TurnSteerParams = params(value)?;
    let turn = running(active, input.thread_id, Some(input.turn_id))?;
    let note =
        text(input.input.into_iter().map(UserInput::from).collect()).ok_or_else(Fault::params)?;
    turn.steering.offer_note(note).map_err(refusal)?;
    Ok(json!(TurnSteerResult {
        turn_id: input.turn_id
    }))
}

pub(super) fn pause(active: Option<&ActiveTurn>, value: Option<Value>) -> Result<Value, Fault> {
    let input: GoalPauseParams = params(value)?;
    let turn = running(active, input.thread_id, None)?;
    if turn.state.goal_id != Some(input.goal_id) {
        return Err(Fault::engine(EngineError::GoalUnavailable));
    }
    turn.steering.pause();
    Ok(json!({}))
}

fn refusal(refused: SteeringRefused) -> Fault {
    Fault::refusal(match refused {
        SteeringRefused::RunFull => Reason::RunFull,
        SteeringRefused::Empty => Reason::Empty,
        SteeringRefused::TooLarge => Reason::TooLarge,
        SteeringRefused::Unavailable | SteeringRefused::ChildLimit => Reason::Unavailable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::serve::items::TurnState;

    #[tokio::test]
    async fn pause_and_steer_are_accepted_between_autonomous_runs() {
        let temp = tempfile::tempdir().unwrap();
        let engine = arany::Engine::open(
            arany::StateRoot::admit(&temp.path().join("state")).unwrap(),
            arany::EngineConfig::default(),
        )
        .unwrap();
        let thread_id = SessionId::new();
        let (run_id, goal_id) = (RunId::new(), GoalId::new());
        let worker = tokio::spawn(std::future::pending::<()>());
        let turn = ActiveTurn {
            thread_id,
            result: worker.abort_handle(),
            events: engine.subscribe(),
            cancel: arany::RunCancellation::new(),
            approvals: std::sync::Arc::new(
                crate::cli::serve::approvals::Approvals::empty_for_test(),
            ),
            approval_mode: arany::ToolApprovals::new(arany::ApprovalMode::Request).0,
            steering: arany::RunSteering::new(),
            pending_start: None,
            result_done: false,
            pending_close: None,
            state: TurnState {
                run_id: Some(run_id),
                goal_id: Some(goal_id),
                pending_completion: true,
                ..Default::default()
            },
        };
        let ids = json!({"threadId":thread_id,"goalId":goal_id});
        assert_eq!(pause(Some(&turn), Some(ids)).ok().unwrap(), json!({}));
        assert!(!turn.steering.pause(), "the pause is already requested");
        let note = json!({"threadId":thread_id,"turnId":run_id,"input":[{"type":"text","text":"next Run"}]});
        assert_eq!(
            steer(Some(&turn), Some(note)).ok().unwrap(),
            json!({"turnId":run_id})
        );
        assert_eq!(turn.steering.withdraw_note().as_deref(), Some("next Run"));
        let mut control = crate::cli::serve::server::OutputControl::from(&turn);
        control.pending_completion = false;
        for (method, params, expected) in [
            (
                "turn/interrupt",
                json!({"threadId":thread_id,"turnId":run_id}),
                true,
            ),
            (
                "turn/interrupt",
                json!({"threadId":SessionId::new(),"turnId":run_id}),
                false,
            ),
            (
                "turn/interrupt",
                json!({"threadId":thread_id,"turnId":RunId::new()}),
                false,
            ),
            ("turn/interrupt", json!({"threadId":thread_id}), false),
            ("thread/unsubscribe", json!({"threadId":thread_id}), true),
            (
                "thread/unsubscribe",
                json!({"threadId":SessionId::new()}),
                false,
            ),
        ] {
            let request = arany_cli::protocol::rpc::Request {
                jsonrpc: Default::default(),
                id: arany_cli::protocol::rpc::RequestId::Integer(1),
                method: method.into(),
                params: Some(params),
            };
            assert_eq!(control.interrupt(&request), expected, "{method}");
        }
        drop(turn);
        engine.close().await.unwrap();
    }
}
