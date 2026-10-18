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
                input: input.input.unwrap_or_default(),
                start: Start::Resume(input.goal_id),
            });
        }
        let input: TurnStartParams = params(value)?;
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
                resume_goal_id: Some(_),
                ..
            })) => return Err(Fault::params()),
            Start::Turn(Some(GoalInput {
                autonomous: true, ..
            })) => GoalRequest::autonomous(loaded.goal_budget, arany::GoalPhase::Implementation),
            Start::Turn(_) => GoalRequest::default(),
        };
        if goal.id.is_none() {
            goal.budget = Some(loaded.goal_budget);
        }
        goal.resource_settings = loaded.resource_settings;
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
                && !turn.state.pending_completion
                && !turn.result.is_finished()
        })
        .ok_or_else(|| Fault::state(Reason::TurnNotActive))
}

pub(super) fn steer(active: Option<&ActiveTurn>, value: Option<Value>) -> Result<Value, Fault> {
    let input: TurnSteerParams = params(value)?;
    let turn = running(active, input.thread_id, Some(input.turn_id))?;
    let note = text(input.input).ok_or_else(Fault::params)?;
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
        SteeringRefused::Unavailable => Reason::Unavailable,
    })
}
