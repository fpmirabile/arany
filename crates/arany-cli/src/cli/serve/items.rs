use arany::{
    CacheUpdate, EffectIntent, Event, GoalId, GoalView, RunId, RunStatus, RunStop, RunView,
    SessionView, ToolCall, ToolDisposition, ToolObservation,
};
use arany_cli::protocol::{methods::*, rpc::Notification};
use serde::Serialize;

#[derive(Default)]
pub(super) struct TurnState {
    pub goal_id: Option<GoalId>,
    pub run_id: Option<RunId>,
    pub first_sequence: Option<u64>,
    pub last_sequence: u64,
    pub completed_sequence: u64,
    pub pending_completion: bool,
}

pub(super) fn notification(method: &str, params: impl Serialize) -> Notification {
    Notification {
        jsonrpc: Default::default(),
        method: method.into(),
        params: Some(serde_json::to_value(params).expect("typed protocol payload")),
    }
}

pub(super) fn project(
    tapped: &arany::Tapped,
    thread_id: arany::SessionId,
    state: &mut TurnState,
) -> Vec<Notification> {
    let envelope = match tapped {
        arany::Tapped::Committed(envelope) => envelope,
        arany::Tapped::Resync { missed } => {
            return vec![notification(
                "events/resync",
                EventsResyncNotification {
                    thread_id: Some(thread_id),
                    missed: *missed,
                    last_sequence: state.last_sequence,
                },
            )];
        }
    };
    state.last_sequence = envelope.sequence;
    let mut messages = Vec::new();
    let (run_id, item, started) = match &envelope.event {
        Event::GoalRunLinked { goal_id, .. } => {
            state.goal_id = Some(*goal_id);
            return messages;
        }
        Event::MessageAccepted {
            run_id,
            text,
            images,
            ..
        } => {
            state.run_id = Some(*run_id);
            state.first_sequence.get_or_insert(envelope.sequence);
            if let Some(goal_id) = state.goal_id {
                messages.push(notification(
                    "turn/started",
                    TurnNotification {
                        thread_id: envelope.session_id,
                        turn: in_progress(*run_id, goal_id),
                    },
                ));
            }
            (*run_id, user_message(*run_id, text, images), true)
        }
        Event::MessageCommitted { run_id, text } => (*run_id, answer(*run_id, text), false),
        Event::ToolStarted { run_id, intent, .. } => {
            (*run_id, tool_item(intent, None, false), true)
        }
        Event::ToolFinished {
            run_id,
            observation,
            ..
        } => (
            *run_id,
            tool_item(&observation.intent, Some(observation), false),
            false,
        ),
        Event::RunFinished { .. } => {
            state.pending_completion = true;
            return messages;
        }
        _ => return messages,
    };
    if started {
        messages.push(notification(
            "item/started",
            ItemStartedNotification {
                thread_id: envelope.session_id,
                turn_id: run_id,
                item: item.clone(),
                started_at_ms: envelope.created_at_ms,
            },
        ));
    }
    if !started || matches!(item, Item::UserMessage { .. }) {
        messages.push(notification(
            "item/completed",
            ItemCompletedNotification {
                thread_id: envelope.session_id,
                turn_id: run_id,
                item,
                completed_at_ms: envelope.created_at_ms,
            },
        ));
    }
    messages
}

pub(super) fn in_progress(id: RunId, goal_id: GoalId) -> Turn {
    Turn {
        id,
        goal_id,
        status: TurnStatus::InProgress,
        stop_reason: None,
        items: Vec::new(),
        error: None,
        goal: None,
    }
}

pub(super) fn goal_summary(goal: &GoalView) -> GoalSummary {
    GoalSummary {
        status: match goal.status {
            arany::GoalStatus::Running => GoalStatus::Running,
            arany::GoalStatus::Interrupted => GoalStatus::Interrupted,
            arany::GoalStatus::Completed => GoalStatus::Completed,
            arany::GoalStatus::Failed => GoalStatus::Failed,
        },
        stopped: None,
    }
}

pub(super) fn turn_summary(
    view: &SessionView,
    run: &RunView,
    goal: &GoalView,
    history: bool,
) -> Turn {
    let stop = view.run_stop(run);
    Turn {
        id: run.id,
        goal_id: goal.id,
        stop_reason: stop.and_then(stop_reason),
        status: match run.status {
            RunStatus::Pending | RunStatus::Active => TurnStatus::InProgress,
            RunStatus::Finished => TurnStatus::Completed,
            RunStatus::Failed => TurnStatus::Failed,
            RunStatus::Cancelled | RunStatus::Interrupted | RunStatus::StoppedByHook => {
                TurnStatus::Interrupted
            }
        },
        items: if history {
            history_items(run)
        } else {
            Vec::new()
        },
        error: matches!(run.status, RunStatus::Failed | RunStatus::StoppedByHook).then(|| {
            TurnError {
                message: crate::presentation::run_feedback(run)
                    .unwrap_or_else(|| "Run failed".into()),
            }
        }),
        goal: Some(GoalSummary {
            stopped: stop.map(goal_stop),
            ..goal_summary(goal)
        }),
    }
}

fn stop_reason(stop: RunStop) -> Option<TurnStopReason> {
    match stop {
        RunStop::GoalExhausted(arany::GoalLimit::Tokens) => {
            Some(TurnStopReason::TokenBudgetExhausted)
        }
        RunStop::GoalExhausted(
            arany::GoalLimit::Calls | arany::GoalLimit::Runs | arany::GoalLimit::ActiveTime,
        ) => Some(TurnStopReason::ActionBudgetExhausted),
        RunStop::Paused
        | RunStop::Cancelled
        | RunStop::StoppedByHook
        | RunStop::Uncertain
        | RunStop::Interrupted
        | RunStop::Failed(_) => None,
    }
}

fn goal_stop(stop: RunStop) -> GoalStop {
    match stop {
        RunStop::GoalExhausted(limit) => GoalStop::GoalExhausted {
            limit: match limit {
                arany::GoalLimit::Calls => GoalLimit::Calls,
                arany::GoalLimit::Runs => GoalLimit::Runs,
                arany::GoalLimit::ActiveTime => GoalLimit::ActiveTime,
                arany::GoalLimit::Tokens => GoalLimit::Tokens,
            },
        },
        RunStop::Paused => GoalStop::Paused,
        RunStop::Cancelled => GoalStop::Cancelled,
        RunStop::StoppedByHook => GoalStop::StoppedByHook,
        RunStop::Uncertain => GoalStop::Uncertain,
        RunStop::Interrupted => GoalStop::Interrupted,
        RunStop::Failed(failure) => GoalStop::Failed { failure },
    }
}

pub(super) fn history_items(run: &RunView) -> Vec<Item> {
    let mut items = vec![user_message(run.id, &run.objective, &run.images)];
    items.extend(
        run.tools
            .iter()
            .chain(run.agents.iter().flat_map(|agent| &agent.tools))
            .map(|tool| tool_item(&tool.intent, tool.observation.as_ref(), true)),
    );
    if let Some(text) = &run.assistant_message {
        items.push(answer(run.id, text));
    }
    items
}

fn user_message(id: RunId, text: &str, images: &[arany::ImageAttachment]) -> Item {
    Item::UserMessage {
        id: format!("{id}:user"),
        text: text.into(),
        images: images
            .iter()
            .enumerate()
            .map(|(index, image)| ImageMetadata {
                index,
                media_type: ImageMediaType::Png,
                bytes: image.byte_len(),
            })
            .collect(),
    }
}

fn answer(id: RunId, text: &str) -> Item {
    Item::AgentMessage {
        id: format!("{id}:answer"),
        text: text.into(),
        phase: AgentMessagePhase::FinalAnswer,
    }
}

pub(in crate::cli) fn tool_item(
    intent: &EffectIntent,
    observation: Option<&ToolObservation>,
    history: bool,
) -> Item {
    let id = intent.id.to_string();
    let disposition = observation.map(|o| o.disposition);
    let output = observation.map(|o| o.output.clone());
    let status = match observation {
        Some(o) if o.cache_update == CacheUpdate::Uncertain => ItemStatus::Uncertain,
        Some(o) => match o.disposition {
            ToolDisposition::Succeeded => ItemStatus::Completed,
            ToolDisposition::Denied => ItemStatus::Declined,
            ToolDisposition::Uncertain => ItemStatus::Uncertain,
            ToolDisposition::Failed
            | ToolDisposition::Conflict
            | ToolDisposition::Limit
            | ToolDisposition::Cancelled => ItemStatus::Failed,
        },
        None if history => ItemStatus::Uncertain,
        None => ItemStatus::InProgress,
    };
    match &intent.call {
        ToolCall::Command { program, args, cwd } => Item::CommandExecution {
            id,
            command: program.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            status,
            disposition,
            output,
            attestation: observation
                .and_then(|o| o.attestation.as_ref())
                .and_then(|a| match a {
                    arany::Attestation::Guard(_) => Some(Attestation::Guard),
                    arany::Attestation::InProcess { .. } => Some(Attestation::InProcess),
                    arany::Attestation::Unguarded(_)
                    | arany::Attestation::WorkerCommand { .. }
                    | arany::Attestation::WorkerRead { .. }
                    | arany::Attestation::WorkerMutation { .. }
                    | arany::Attestation::WorkerData { .. } => None,
                }),
        },
        ToolCall::Write {
            path,
            expected_digest,
            ..
        } => Item::FileChange {
            id,
            changes: vec![FileChange {
                path: path.clone(),
                kind: if expected_digest.is_some() {
                    FileChangeKind::Update
                } else {
                    FileChangeKind::Add
                },
            }],
            status,
            disposition,
            output,
        },
        ToolCall::Edit { path, .. } | ToolCall::Mkdir { path } => Item::FileChange {
            id,
            changes: vec![FileChange {
                path: path.clone(),
                kind: if matches!(intent.call, ToolCall::Mkdir { .. }) {
                    FileChangeKind::Mkdir
                } else {
                    FileChangeKind::Update
                },
            }],
            status,
            disposition,
            output,
        },
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } => Item::FileChange {
            id,
            changes: changeset
                .files
                .iter()
                .map(|change| FileChange {
                    path: change.path.clone(),
                    kind: match (&change.before, &change.after) {
                        (None, _) => FileChangeKind::Add,
                        (_, None) => FileChangeKind::Delete,
                        _ => FileChangeKind::Update,
                    },
                })
                .collect(),
            status,
            disposition,
            output,
        },
        ToolCall::McpList { server } => Item::McpToolCall {
            id,
            server: server.clone(),
            tool: None,
            arguments: None,
            status,
            disposition,
            output,
        },
        ToolCall::McpCall {
            server,
            tool,
            arguments,
            ..
        } => Item::McpToolCall {
            id,
            server: server.clone(),
            tool: Some(tool.clone()),
            arguments: Some(serde_json::from_str(arguments).expect("journaled MCP arguments")),
            status,
            disposition,
            output,
        },
        call => Item::ToolCall {
            id,
            call: serde_json::to_value(call).expect("journaled Tool call"),
            status,
            disposition,
            output,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::{GoalLimit as Limit, RunFailure};
    use serde_json::json;

    #[test]
    fn every_projected_stop_has_one_closed_wire_value() {
        let rows = [
            (
                RunStop::GoalExhausted(Limit::Calls),
                json!({"reason":"goalExhausted","limit":"calls"}),
                Some("actionBudgetExhausted"),
            ),
            (
                RunStop::GoalExhausted(Limit::Runs),
                json!({"reason":"goalExhausted","limit":"runs"}),
                Some("actionBudgetExhausted"),
            ),
            (
                RunStop::GoalExhausted(Limit::ActiveTime),
                json!({"reason":"goalExhausted","limit":"activeTime"}),
                Some("actionBudgetExhausted"),
            ),
            (
                RunStop::GoalExhausted(Limit::Tokens),
                json!({"reason":"goalExhausted","limit":"tokens"}),
                Some("tokenBudgetExhausted"),
            ),
            (RunStop::Paused, json!({"reason":"paused"}), None),
            (RunStop::Cancelled, json!({"reason":"cancelled"}), None),
            (
                RunStop::StoppedByHook,
                json!({"reason":"stoppedByHook"}),
                None,
            ),
            (RunStop::Uncertain, json!({"reason":"uncertain"}), None),
            (RunStop::Interrupted, json!({"reason":"interrupted"}), None),
            (
                RunStop::Failed(RunFailure::CriterionUnmet),
                json!({"reason":"failed","failure":"criterion_unmet"}),
                None,
            ),
        ];
        let schema = arany_cli::protocol::protocol_schema();
        let mut stop_schema = schema.clone();
        stop_schema["$ref"] = json!("#/$defs/GoalStop");
        let validator = jsonschema::validator_for(&stop_schema).unwrap();
        for (stop, wire, reason) in rows {
            let value = serde_json::to_value(goal_stop(stop)).unwrap();
            assert_eq!(value, wire, "{stop:?}");
            assert!(validator.is_valid(&value), "{stop:?}");
            assert_eq!(
                serde_json::from_value::<GoalStop>(value).unwrap(),
                goal_stop(stop)
            );
            assert_eq!(
                stop_reason(stop).map(|reason| serde_json::to_value(reason).unwrap()),
                reason.map(|reason| json!(reason)),
                "{stop:?}"
            );
        }
    }
}
