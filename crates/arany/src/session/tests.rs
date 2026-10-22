use super::*;

#[cfg(unix)]
#[test]
fn directory_includes_snapshot_only_bounded_untrusted_entry_names() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let selected = workspace.join("docs/with spaces");
    std::fs::create_dir_all(selected.join("nested")).unwrap();
    std::fs::write(selected.join("a.txt"), "FILE_BODY_CANARY").unwrap();
    std::fs::write(selected.join("nested/private.txt"), "NESTED_BODY_CANARY").unwrap();
    std::fs::create_dir(selected.join(".git")).unwrap();
    std::os::unix::fs::symlink(temp.path(), selected.join("outside")).unwrap();
    std::fs::hard_link(selected.join("a.txt"), selected.join("alias.txt")).unwrap();
    std::fs::write(selected.join("b.txt"), "FILE_BODY_CANARY").unwrap();
    let state = crate::StateRoot::admit(&temp.path().join("state")).unwrap();
    let load = |path: &str| input::WorkspaceInputs::load(&workspace, &state, None, &[path.into()]);
    let content = "\"b.txt\"\n\"nested/\"\n";
    for path in ["docs/with spaces", "docs/with spaces/"] {
        let loaded = load(path).unwrap();
        assert_eq!(loaded.includes[0].path, "docs/with spaces/");
        assert_eq!(loaded.includes[0].content, content);
        assert_eq!(
            loaded.includes[0].digest,
            <[u8; 32]>::from(Sha256::digest(content))
        );
        assert!(loaded.includes[0].model_include().starts_with("Directory:"));
    }
    assert!(matches!(
        load("docs/with spaces/.git"),
        Err(input::InputError::Include(
            crate::tools::ToolError::ProtectedPath
        ))
    ));
    assert!(matches!(
        load("docs/with spaces/outside"),
        Err(input::InputError::Io(_))
    ));
    assert!(matches!(
        load("docs/with spaces/b.txt/"),
        Err(input::InputError::InvalidPath)
    ));
    for (folder, count, name_len) in [("entry-cap", 70, 10), ("byte-cap", 64, 255)] {
        let directory = workspace.join(folder);
        std::fs::create_dir(&directory).unwrap();
        for index in 0..count {
            let name = format!("{index:03}{}", "a".repeat(name_len - 3));
            std::fs::write(directory.join(name), "OMITTED_BODY_CANARY").unwrap();
        }
        let loaded = load(folder).unwrap();
        let listing = &loaded.includes[0].content;
        assert!(listing.len() <= 16 * 1024);
        assert!(listing.ends_with("[truncated]\n"));
        let entries = listing.lines().filter(|line| line.starts_with('"')).count();
        assert!(entries <= 64);
        if folder == "entry-cap" {
            assert_eq!(entries, 64);
        } else {
            assert!(entries < 64);
        }
        assert!(!listing.contains("OMITTED_BODY_CANARY"));
    }
}

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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let global = temp.path().join("state/AGENTS.md");
        std::fs::write(&global, "<instructions>").unwrap();
        std::fs::set_permissions(&global, std::fs::Permissions::from_mode(0o600)).unwrap();
        let inputs = input::WorkspaceInputs::load(&workspace, &state, None, &[]).unwrap();
        let user = inputs
            .user_instructions
            .as_ref()
            .unwrap()
            .model_instructions();
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
        .expect("both scopes fit");
        assert_eq!(
            context.context_usage.used_bytes as usize,
            4 + 89 + user.len()
        );
    }
}

#[cfg(unix)]
#[test]
fn nested_instructions_load_root_to_launch_directory_within_shared_bounds() {
    use crate::session::InstructionFile;
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

    // An override wins in its directory, as in Codex.
    std::fs::write(
        workspace.join("crates/AGENTS.override.md"),
        "crates override",
    )
    .unwrap();
    std::fs::write(deep.join("CLAUDE.md"), "src fallback").unwrap();
    let inputs = load(Some(&deep)).unwrap();
    assert_eq!(
        inputs
            .instructions
            .iter()
            .map(|file| (file.path.as_str(), file.instruction_file()))
            .collect::<Vec<_>>(),
        [
            ("AGENTS.md", Some(InstructionFile::Agents)),
            (
                "crates/AGENTS.override.md",
                Some(InstructionFile::AgentsOverride)
            ),
            ("crates/cli/AGENTS.md", Some(InstructionFile::Agents)),
            ("crates/cli/src/CLAUDE.md", Some(InstructionFile::Claude)),
        ]
    );
    for name in ["crates/AGENTS.override.md", "crates/cli/src/CLAUDE.md"] {
        std::fs::remove_file(workspace.join(name)).unwrap();
    }

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

#[cfg(unix)]
#[test]
fn user_instructions_have_an_independent_budget_and_fixed_label() {
    use input::{InputError, WorkspaceInputs};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let global = temp.path().join("state/AGENTS.md");
    let write = |bytes: &[u8], mode: u32| {
        std::fs::write(&global, bytes).unwrap();
        std::fs::set_permissions(&global, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let load = || WorkspaceInputs::load(&workspace, &state, None, &[]);

    assert!(load().unwrap().user_instructions.is_none());
    write(b"", 0o600);
    let empty = load().unwrap().user_instructions.unwrap();
    assert_eq!(
        (empty.path.as_str(), empty.content.as_str()),
        ("user AGENTS.md", "")
    );
    assert_eq!(empty.digest, <[u8; 32]>::from(Sha256::digest(b"")));

    std::fs::write(workspace.join("AGENTS.md"), vec![b'p'; 64 * 1024]).unwrap();
    std::fs::write(temp.path().join("AGENTS.md"), "parent canary").unwrap();
    write(&vec![b'g'; 64 * 1024], 0o600);
    let inputs = load().unwrap();
    let user = inputs.user_instructions.as_ref().unwrap();
    assert_eq!(user.content.len(), 64 * 1024);
    assert_eq!(user.digest, <[u8; 32]>::from(Sha256::digest(&user.content)));
    assert_eq!(inputs.instructions.len(), 1);
    assert_eq!(inputs.instructions[0].path, "AGENTS.md");
    assert!(
        user.model_instructions()
            .starts_with("# AGENTS.md instructions for user AGENTS.md\n\n")
    );
    assert!(
        !user
            .model_instructions()
            .contains(temp.path().to_str().unwrap())
    );

    write(&vec![b'g'; 64 * 1024 + 1], 0o600);
    assert!(matches!(load(), Err(InputError::State(_))));
    write(b"rule", 0o644);
    assert!(matches!(load(), Err(InputError::State(_))));
    write(&[0xff, 0xfe], 0o600);
    assert!(matches!(load(), Err(InputError::InvalidUtf8)));
    std::fs::remove_file(&global).unwrap();
    std::os::unix::fs::symlink(temp.path().join("AGENTS.md"), &global).unwrap();
    assert!(matches!(load(), Err(InputError::State(_))));
    std::fs::remove_file(&global).unwrap();
    assert!(
        load().unwrap().user_instructions.is_none(),
        "no fallback to a parent file"
    );
}
