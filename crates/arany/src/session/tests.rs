use super::*;

pub(crate) fn unanswered_run(status: RunStatus) -> RunView {
    RunView {
        skill: None,
        supervisor: Default::default(),
        workers: Default::default(),
        id: RunId::new(),
        objective: "write test.txt".into(),
        images: Vec::new(),
        config: None,
        agents: Vec::new(),
        assistant_message: None,
        status,
        failure: (status == RunStatus::Failed).then_some(RunFailure::ProposalRefused),
        accepted_sequence: 2,
        finished_sequence: status.is_outcome().then_some(3),
        tools: Vec::new(),
        tool_compaction_summary: None,
        tool_compaction_covered_count: 0,
        definitions: Default::default(),
        display: Vec::new(),
        steering_notes: Vec::new(),
        lifecycle_hooks: Vec::new(),
        lifecycle_hook_sequence: None,
        provider_sequences: Default::default(),
        provider_fallbacks: Vec::new(),
        provider_retries: Vec::new(),
    }
}

#[test]
fn failure_heavy_history_keeps_the_run_and_byte_budgets() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let inputs = input::WorkspaceInputs::load(&workspace, &state, &[]).unwrap();
    let id = SessionId::new();
    let mut view = SessionView::replay(
        id,
        &[EventEnvelope {
            sequence: 1,
            session_id: id,
            run_id: None,
            agent_run_id: None,
            event: Event::SessionStarted {
                title: "History".into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 2,
                },
            },
            created_at_ms: 0,
        }],
    )
    .unwrap()
    .unwrap();
    for objective_bytes in [32, MAX_USER_MESSAGE_BYTES] {
        view.runs = (0..MAX_HISTORY_RUNS + 8)
            .map(|index| {
                let statuses = [
                    RunStatus::Failed,
                    RunStatus::Cancelled,
                    RunStatus::Interrupted,
                    RunStatus::StoppedByHook,
                ];
                let mut run = unanswered_run(statuses[index % statuses.len()]);
                run.objective = "x".repeat(objective_bytes);
                run
            })
            .collect();
        let context = CompiledContext::compile(
            Some(&view),
            "next",
            &[],
            &inputs,
            CollaborationPolicy::Single,
            0,
            0,
        )
        .ok()
        .expect("bounded history");
        let expected_count = if objective_bytes == 32 {
            MAX_HISTORY_RUNS
        } else {
            5
        };
        assert_eq!(context.history.len(), expected_count);
        assert_eq!(
            context.history_run_ids,
            view.runs[view.runs.len() - expected_count..]
                .iter()
                .map(|run| run.id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            context.excluded_history_runs as usize,
            view.runs.len() - expected_count
        );
        let history_bytes: usize = context
            .history
            .iter()
            .map(|turn| turn.user.len() + turn.assistant.len() + 64)
            .sum();
        assert_eq!(
            context.context_usage.used_bytes as usize,
            "next".len() + history_bytes
        );
        assert_eq!(
            context.context_usage.compactable_bytes as usize,
            history_bytes
        );
        assert_eq!(context.context_usage.tool_history_bytes, 0);
        assert_eq!(
            context.context_usage.budget_bytes as usize,
            MAX_CONTEXT_CONTENT_BYTES
        );
        assert!(context.context_usage.used_bytes <= context.context_usage.budget_bytes);
    }
}
