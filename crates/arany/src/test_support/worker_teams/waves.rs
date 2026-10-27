use super::*;

impl LiveProvider {
    pub(super) fn wave_outcome(&self, request: &ProviderRequest, step: usize) -> ProviderOutcome {
        match (request.phase, step) {
            (AgentPhase::RootPlan, 0) => super::super::scenario::plan_outcome(self.plan()),
            (AgentPhase::RootPlan, 1) => self.wave_delegation(request, 1),
            (AgentPhase::RootSynthesis, 0) => read("notes/0"),
            (AgentPhase::RootSynthesis, 1) => self.wave_delegation(request, 2),
            (AgentPhase::RootSynthesis, 2) => read("notes/1"),
            (AgentPhase::RootSynthesis, 3) if self.case == "waves_fresh" => {
                self.wave_write("notes/0", "edited 0")
            }
            (AgentPhase::RootSynthesis, 4) if self.case == "waves_fresh" => read("notes/1"),
            (AgentPhase::RootSynthesis, _) => finish_with("Both waves verified"),
            (AgentPhase::ChildWork, _) => {
                let calls = self.calls.lock().unwrap();
                let first = calls
                    .iter()
                    .find(|(phase, _)| *phase == AgentPhase::ChildWork)
                    .unwrap()
                    .1;
                let first_wave = request.agent_run_id == first;
                drop(calls);
                match (first_wave, step) {
                    (true, 0) => self.wave_write("notes/0", "edited 0"),
                    (false, 0) => self.wave_write("notes/0", "changed 0"),
                    (false, 1) => self.wave_write("notes/1", "edited 1"),
                    (true, _) | (false, _) => finish_with("Wave edits completed"),
                }
            }
            (AgentPhase::ToolReview, _) => ProviderOutcome::Review(ReviewVerdict::Approve),
            (AgentPhase::RootPlan, _) => panic!("unexpected wave planning call"),
        }
    }

    fn wave_delegation(&self, request: &ProviderRequest, wave: u8) -> ProviderOutcome {
        let ProviderOutcome::Delegate(mut delegate) = self.delegate(request) else {
            unreachable!()
        };
        let assignment = delegate.children[0].editor.as_mut().unwrap();
        assignment.wave = wave;
        assignment.lease = vec![if wave == 1 { "notes/0" } else { "notes" }.into()];
        assignment.criterion = GoalSuccessCriterion::WorkspaceFileContent {
            path: format!("notes/{}", wave - 1),
            content: format!("edited {}", wave - 1),
        };
        ProviderOutcome::Delegate(delegate)
    }

    pub(super) fn wave_write(&self, path: &str, content: &str) -> ProviderOutcome {
        tool_outcome(ToolCall::Write {
            path: path.into(),
            content: content.into(),
            expected_digest: Some(crate::validate::hex_digest(
                &std::fs::read(self.workspace.join(path)).unwrap(),
            )),
        })
    }
}

pub(super) fn read(path: &str) -> ProviderOutcome {
    tool_outcome(ToolCall::Read {
        path: path.into(),
        offset: None,
        limit: Some(128),
    })
}

pub(super) fn assert_waves(case: &str, events: &[EventEnvelope], view: &SessionView) {
    if !matches!(case, "waves_stale" | "waves_fresh") {
        return;
    }
    let run = &view.runs[0];
    assert_eq!(run.agents.len(), 3);
    let first = run.agents[1].assignment.as_ref().unwrap();
    let second = run.agents[2].assignment.as_ref().unwrap();
    assert!(first.released.is_some() && second.released.is_some());
    let changed = run.agents[2].tools[0].finished_sequence.unwrap();
    let records = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::WorkerVerificationRecorded { record, .. }
                if record.assignment_id == run.agents[1].id =>
            {
                Some((event.sequence, record))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        records
            .iter()
            .any(|(sequence, record)| *sequence < changed && record.passed)
    );
    assert!(
        records
            .iter()
            .any(|(sequence, record)| *sequence > changed && !record.passed)
    );
    let first_verification = &first.verification.as_ref().unwrap().1;
    assert!(second.verification.as_ref().unwrap().1.passed);
    assert!(view.goals[0].success_criterion_satisfied);
    if case == "waves_stale" {
        assert_eq!(run.failure, Some(RunFailure::WorkersUnverified));
        assert_eq!(view.goals[0].status, GoalStatus::Failed);
        assert!(!first_verification.passed);
    } else {
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(view.goals[0].status, GoalStatus::Completed);
        let fresh = run
            .tools
            .iter()
            .find(|tool| {
                matches!(&tool.intent.call,
            ToolCall::Write { path, .. } if path == "notes/0")
            })
            .unwrap()
            .finished_sequence
            .unwrap();
        assert!(fresh > second.released.unwrap());
        assert!(first_verification.passed);
        assert!(first_verification.evidence.contains(&fresh));
    }
}
