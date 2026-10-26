use super::{Output, append_jsonl, render_answer};
use arany::{Event, EventEnvelope, RunView};

pub fn render_exec(
    run: &RunView,
    events: &[EventEnvelope],
    output: Output,
    new_session: bool,
) -> String {
    match output {
        Output::Text => render_answer(run, "Answer:"),
        Output::Jsonl => {
            let mut rendered = String::new();
            let goal_id = events.iter().find_map(|envelope| match &envelope.event {
                Event::GoalRunLinked {
                    goal_id,
                    run_id: linked_run,
                    ..
                } if *linked_run == run.id => Some(*goal_id),
                _ => None,
            });
            for envelope in events {
                let task_goal_event = goal_id.is_some_and(|goal_id| match &envelope.event {
                    Event::GoalStarted {
                        goal_id: event_goal,
                        ..
                    }
                    | Event::GoalResumed {
                        goal_id: event_goal,
                    }
                    | Event::GoalFinished {
                        goal_id: event_goal,
                        ..
                    } => *event_goal == goal_id,
                    _ => false,
                });
                if envelope.run_id == Some(run.id)
                    || task_goal_event
                    || new_session && matches!(envelope.event, Event::SessionStarted { .. })
                {
                    append_jsonl(&mut rendered, envelope);
                }
            }
            rendered
        }
    }
}

#[cfg(test)]
mod exec_jsonl_tests {
    use super::*;
    use arany::{
        GoalBudget, GoalId, GoalPhase, GoalReport, GoalStatus, RunStatus, SessionId,
        WorkspaceIdentity,
    };

    fn envelope(
        sequence: u64,
        session_id: SessionId,
        run_id: Option<arany::RunId>,
        event: Event,
    ) -> EventEnvelope {
        EventEnvelope {
            sequence,
            session_id,
            run_id,
            agent_run_id: None,
            event,
            created_at_ms: 0,
        }
    }

    fn goal_started(goal_id: GoalId, objective: &str) -> Event {
        Event::GoalStarted {
            include_paths: Some(Vec::new()),
            goal_id,
            objective: objective.into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: GoalBudget::default(),
            phase: GoalPhase::Implementation,
            autonomous: false,
            resource_settings: arany::ToolResourceSettings::default(),
            continues: None,
        }
    }

    fn goal_finished(goal_id: GoalId, summary: &str) -> Event {
        Event::GoalFinished {
            goal_id,
            status: GoalStatus::Completed,
            active_ms: 0,
            report: GoalReport {
                summary: summary.into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
            exhausted: false,
            budget_refusal: None,
        }
    }

    fn message(run_id: arany::RunId, text: &str) -> Event {
        Event::MessageAccepted {
            skill: None,
            run_id,
            text: text.into(),
            images: Vec::new(),
        }
    }

    #[test]
    fn exec_jsonl_selects_latest_run_and_matching_goal_lifecycle() {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let earlier_goal = GoalId::new();
        let earlier_run = arany::RunId::new();
        let current_run = arany::RunId::new();
        let events = vec![
            envelope(
                1,
                session_id,
                None,
                goal_started(earlier_goal, "Earlier task"),
            ),
            envelope(
                2,
                session_id,
                None,
                goal_finished(earlier_goal, "Earlier report"),
            ),
            envelope(3, session_id, None, goal_started(goal_id, "Current task")),
            envelope(
                4,
                session_id,
                Some(earlier_run),
                Event::GoalRunLinked {
                    goal_id,
                    run_id: earlier_run,
                    phase: GoalPhase::Research,
                },
            ),
            envelope(
                5,
                session_id,
                Some(earlier_run),
                message(earlier_run, "Earlier Run"),
            ),
            envelope(6, session_id, None, Event::GoalResumed { goal_id }),
            envelope(
                7,
                session_id,
                Some(current_run),
                Event::GoalRunLinked {
                    goal_id,
                    run_id: current_run,
                    phase: GoalPhase::Implementation,
                },
            ),
            envelope(
                8,
                session_id,
                Some(current_run),
                message(current_run, "Current Run"),
            ),
            envelope(
                9,
                session_id,
                None,
                goal_finished(goal_id, "Current report"),
            ),
        ];
        let run = RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: current_run,
            objective: "Current Run".into(),
            images: Vec::new(),
            config: None,
            agents: Vec::new(),
            assistant_message: None,
            status: RunStatus::Finished,
            accepted_sequence: 8,
            finished_sequence: Some(8),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_recent_files: Vec::new(),
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
        };

        let output = render_exec(&run, &events, Output::Jsonl, false);
        let lines: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSONL Event"))
            .collect();
        assert_eq!(
            lines
                .iter()
                .map(|line| line["kind"].as_str().expect("Event kind"))
                .collect::<Vec<_>>(),
            vec![
                "GoalStarted",
                "GoalResumed",
                "GoalRunLinked",
                "MessageAccepted",
                "GoalFinished",
            ]
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| line["sequence"].as_u64().expect("Event sequence"))
                .collect::<Vec<_>>(),
            vec![3, 6, 7, 8, 9]
        );
        assert_eq!(lines[0]["payload"]["goal_id"], goal_id.to_string());
        assert_eq!(lines[4]["payload"]["goal_id"], goal_id.to_string());
        assert!(lines[0]["run_id"].is_null());
        assert!(lines[1]["run_id"].is_null());
        assert!(lines[4]["run_id"].is_null());
        assert!(
            lines[2..4]
                .iter()
                .all(|line| line["run_id"] == current_run.to_string())
        );
    }
}
