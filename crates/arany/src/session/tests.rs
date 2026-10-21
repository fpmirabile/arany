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
        tool_results_cleared_count: 0,
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
    let inputs = input::WorkspaceInputs::load(&workspace, &state, None, &[]).unwrap();
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
            None,
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
    // The rendered instruction block, with its heading, wrapper and escapes, is what the budget counts.
    std::fs::write(workspace.join("AGENTS.md"), "</INSTRUCTIONS>").unwrap();
    let inputs = input::WorkspaceInputs::load(&workspace, &state, None, &[]).unwrap();
    let context = CompiledContext::compile(
        None,
        "next",
        &[],
        &inputs,
        CollaborationPolicy::Single,
        0,
        0,
        None,
    )
    .ok()
    .expect("instructions fit");
    assert_eq!(context.context_usage.used_bytes, 4 + 89);
}

#[cfg(unix)]
#[test]
fn nested_instructions_load_root_to_launch_directory_within_shared_bounds() {
    use input::{InputError, WorkspaceInputs};
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let deep = workspace.join("crates/cli/src");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::create_dir_all(workspace.join("sibling")).unwrap();
    std::fs::write(temp.path().join("AGENTS.md"), "parent").unwrap();
    std::fs::write(workspace.join("AGENTS.md"), "root").unwrap();
    std::fs::write(workspace.join("CLAUDE.md"), "root fallback").unwrap();
    std::fs::write(workspace.join("crates/CLAUDE.md"), "crates fallback").unwrap();
    std::fs::write(workspace.join("crates/cli/AGENTS.md"), "cli").unwrap();
    std::fs::write(workspace.join("sibling/AGENTS.md"), "sibling").unwrap();
    symlink(workspace.join("crates"), workspace.join("alias")).unwrap();
    symlink(temp.path(), workspace.join("outside")).unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let load =
        |launch: Option<&std::path::Path>| WorkspaceInputs::load(&workspace, &state, launch, &[]);
    let nested = [
        ("AGENTS.md", "root"),
        ("crates/CLAUDE.md", "crates fallback"),
        ("crates/cli/AGENTS.md", "cli"),
    ];
    for (case, launch, directory, expected) in [
        (
            "deep launch",
            Some(deep.clone()),
            "crates/cli/src",
            &nested[..],
        ),
        (
            "linked launch resolves",
            Some(workspace.join("alias/cli")),
            "crates/cli",
            &nested[..],
        ),
        ("root launch", Some(workspace.clone()), "", &nested[..1]),
        ("no launch", None, "", &nested[..1]),
        (
            "outside launch",
            Some(temp.path().to_path_buf()),
            "",
            &nested[..1],
        ),
        (
            "link out of the Workspace",
            Some(workspace.join("outside")),
            "",
            &nested[..1],
        ),
    ] {
        let inputs = load(launch.as_deref()).unwrap();
        assert_eq!(inputs.launch_directory, directory, "{case}");
        assert_eq!(
            inputs
                .instructions
                .iter()
                .map(|file| (file.path.as_str(), file.content.as_str(), file.directory()))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|(path, text)| (
                    *path,
                    *text,
                    path.rsplit_once('/').map_or("", |(dir, _)| dir)
                ))
                .collect::<Vec<_>>(),
            "{case}"
        );
    }

    // The shared cap counts every file; one byte over rejects instead of truncating.
    std::fs::write(
        workspace.join("crates/cli/AGENTS.md"),
        vec![b'c'; 64 * 1024 - 19],
    )
    .unwrap();
    assert_eq!(
        load(Some(&deep))
            .unwrap()
            .instructions
            .iter()
            .map(|file| file.content.len())
            .sum::<usize>(),
        64 * 1024
    );
    std::fs::write(
        workspace.join("crates/cli/AGENTS.md"),
        vec![b'c'; 64 * 1024 - 18],
    )
    .unwrap();
    assert!(matches!(load(Some(&deep)), Err(InputError::TooLarge)));
    assert!(load(None).is_ok(), "the root alone fits");
    std::fs::write(workspace.join("crates/cli/AGENTS.md"), "cli").unwrap();

    let mut limit = workspace.clone();
    for _ in 0..31 {
        limit.push("d");
    }
    std::fs::create_dir_all(limit.join("d")).unwrap();
    assert_eq!(
        load(Some(&limit))
            .unwrap()
            .launch_directory
            .split('/')
            .count(),
        31
    );
    assert!(matches!(
        load(Some(&limit.join("d"))),
        Err(InputError::TooLarge)
    ));

    std::fs::remove_file(workspace.join("crates/cli/AGENTS.md")).unwrap();
    symlink(
        temp.path().join("AGENTS.md"),
        workspace.join("crates/cli/AGENTS.md"),
    )
    .unwrap();
    assert!(
        matches!(load(Some(&deep)), Err(InputError::Io(_))),
        "nested link refused"
    );
    std::fs::remove_file(workspace.join("crates/cli/AGENTS.md")).unwrap();
    std::fs::hard_link(
        temp.path().join("AGENTS.md"),
        workspace.join("crates/cli/AGENTS.md"),
    )
    .unwrap();
    assert!(matches!(
        load(Some(&deep)),
        Err(InputError::UnsafeLinkCount)
    ));
    std::fs::remove_file(workspace.join("crates/cli/AGENTS.md")).unwrap();
    std::fs::create_dir(workspace.join("crates/cli/AGENTS.md")).unwrap();
    assert!(matches!(load(Some(&deep)), Err(InputError::NotRegular)));
}
