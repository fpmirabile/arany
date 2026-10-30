use super::*;

pub(super) struct Gates {
    primary_read: tokio::sync::Semaphore,
    primary_written: tokio::sync::Semaphore,
    verification: std::sync::atomic::AtomicUsize,
}

impl Default for Gates {
    fn default() -> Self {
        Self {
            primary_read: tokio::sync::Semaphore::new(0),
            primary_written: tokio::sync::Semaphore::new(0),
            verification: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl LiveProvider {
    pub(super) async fn background_outcome(
        &self,
        request: &ProviderRequest,
        step: usize,
    ) -> ProviderOutcome {
        match (request.phase, step) {
            (AgentPhase::RootPlan, 0) => super::super::scenario::plan_outcome(self.plan()),
            (AgentPhase::RootPlan, 1) => {
                let ProviderOutcome::Delegate(mut delegate) = self.delegate(request) else {
                    unreachable!()
                };
                let mut second = delegate.children[0].clone();
                for (index, child) in delegate
                    .children
                    .iter_mut()
                    .chain(std::iter::once(&mut second))
                    .enumerate()
                {
                    let assignment = child.editor.as_mut().unwrap();
                    assignment.lease = vec![format!("notes/{index}")];
                    assignment.mission = format!("Edit note {index}");
                    assignment.criterion = GoalSuccessCriterion::WorkspaceFileContent {
                        path: format!("notes/{index}"),
                        content: format!("edited {index}"),
                    };
                    child.objective = assignment.mission.clone();
                }
                delegate.children.push(second);
                delegate.children.push(ChildAssignment {
                    objective: "Inspect live notes".into(),
                    profile: None,
                    editor: None,
                });
                ProviderOutcome::Delegate(delegate)
            }
            (AgentPhase::RootSynthesis, 0) => super::waves::read("notes/0"),
            (AgentPhase::RootSynthesis, 1) => {
                self.background.primary_read.add_permits(2);
                tool_outcome(ToolCall::Write {
                    path: "notes/primary".into(),
                    expected_digest: None,
                    content: "primary edit".into(),
                })
            }
            (AgentPhase::RootSynthesis, 2) => {
                self.background.primary_written.add_permits(2);
                finish_with("Wait for the background team")
            }
            (AgentPhase::RootSynthesis, _) if request.input.child_results.len() < 3 => {
                finish_with("Wait for every editor report")
            }
            (AgentPhase::RootSynthesis, _) => {
                match self
                    .background
                    .verification
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                {
                    0 => return super::waves::read("notes/0"),
                    1 => return super::waves::read("notes/1"),
                    2 => {}
                    _ => panic!("unexpected background verification call"),
                }
                assert_eq!(request.input.child_results.len(), 3);
                for report in &request.input.child_results[..2] {
                    assert!(report.result.contains("Changed paths (untrusted"));
                    assert!(report.result.contains("after_sha256"));
                }
                finish_with("Verified both background editors")
            }
            (AgentPhase::ChildWork, 0) if request.input.objective == "Inspect live notes" => {
                assert_eq!(
                    std::fs::read(self.workspace.join("notes/1")).unwrap(),
                    b"original 1"
                );
                super::waves::read("notes/1")
            }
            (AgentPhase::ChildWork, _) if request.input.objective == "Inspect live notes" => {
                finish_with("Researcher observed the live edit")
            }
            (AgentPhase::ChildWork, 0) => {
                self.background
                    .primary_read
                    .acquire()
                    .await
                    .unwrap()
                    .forget();
                let index = if request.input.objective == "Edit note 0" {
                    0
                } else {
                    1
                };
                self.wave_write(&format!("notes/{index}"), &format!("edited {index}"))
            }
            (AgentPhase::ChildWork, _) => {
                self.background
                    .primary_written
                    .acquire()
                    .await
                    .unwrap()
                    .forget();
                assert_eq!(
                    std::fs::read(self.workspace.join("notes/primary")).unwrap(),
                    b"primary edit"
                );
                finish_with("Background edit completed")
            }
            (AgentPhase::ToolReview, _) => ProviderOutcome::Review(ReviewVerdict::Approve),
            (AgentPhase::RootPlan, _) => panic!("unexpected background planning call"),
        }
    }
}

pub(super) fn assert_background(case: &str, events: &[EventEnvelope], view: &SessionView) {
    if case != "background" {
        return;
    }
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.agents.len(), 4);
    let editors = &run.agents[1..3];
    let primary_write = run
        .tools
        .iter()
        .find(|t| matches!(&t.intent.call, ToolCall::Write { path, .. } if path == "notes/primary"))
        .unwrap();
    for editor in editors {
        let worker = editor.assignment.as_ref().unwrap();
        assert_eq!(worker.pins.request.mode, WorkerMode::Background);
        assert!(worker.verification.as_ref().unwrap().1.passed);
        assert!(primary_write.finished_sequence.unwrap() < worker.released.unwrap());
        let integration = editor.tools.last().unwrap();
        assert!(crate::session::editor_integration(&integration.intent.call));
        assert!(integration.checkpoint.is_some());
        let history = run
            .worker_history()
            .into_iter()
            .find(|w| w.assignment_id == editor.id)
            .unwrap();
        assert!(history.changed_paths.contains("after_sha256"));
    }
    assert!(events.iter().any(|e| matches!(&e.event, Event::ProviderCallRecorded { record, .. } if record.failure_reason == Some(ProviderFailureReason::WorkerAnswerDeferred))));
    assert!(
        run.agents[3].tools[0].finished_sequence.unwrap()
            < editors
                .iter()
                .map(|a| a.assignment.as_ref().unwrap().released.unwrap())
                .min()
                .unwrap()
    );
    assert_eq!(view.goals[0].status, GoalStatus::Completed);
}
