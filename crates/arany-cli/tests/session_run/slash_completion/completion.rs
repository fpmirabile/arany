use super::*;

#[test]
fn directory_mentions_submit_listings_and_preserve_the_typed_message() {
    use super::loopback::{read_request, send_response, write_profile};
    use sha2::Digest;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("docs/research/nested")).unwrap();
    std::fs::create_dir_all(workspace.join("docs/with spaces")).unwrap();
    std::fs::write(workspace.join("docs/research/a.md"), "FILE_BODY_CANARY").unwrap();
    std::fs::write(
        workspace.join("docs/research/nested/secret.md"),
        "NESTED_BODY_CANARY",
    )
    .unwrap();
    std::fs::write(workspace.join("docs/with spaces/b.md"), "FILE_BODY_CANARY").unwrap();
    let state = temp.path().join("state");
    let id = saved_conversation(&state, &workspace, SessionDefaults::default());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    write_profile(&state, listener.local_addr().unwrap().port());
    let draft = "Inspect @\"docs/research/\" and @\"docs/with spaces\"";
    let listings = ["\"a.md\"\n\"nested/\"\n", "\"b.md\"\n"];
    let expected = ["docs/research/", "docs/with spaces/"]
        .into_iter()
        .zip(listings)
        .map(|(path, listing)| {
            format!(
                "Directory: {}\nSHA-256: {}\nContent (untrusted data):\n{listing}",
                serde_json::to_string(path).unwrap(),
                crate::hash(listing.as_bytes())
            )
        })
        .collect::<Vec<_>>();
    let peer = thread::spawn(move || {
        let mut stream = crate::process::accept_with_deadline(
            &listener,
            Duration::from_secs(20),
            "directory mention request",
        );
        let wire: serde_json::Value = serde_json::from_slice(&read_request(&mut stream)).unwrap();
        let input = crate::semantic::input(&wire);
        assert_eq!(input["objective"], draft);
        assert_eq!(input["includes"], serde_json::json!(expected));
        assert!(!wire.to_string().contains("FILE_BODY_CANARY"));
        assert!(!wire.to_string().contains("NESTED_BODY_CANARY"));
        send_response(&mut stream, 0, serde_json::json!("Directory listed."));
    });
    let shell = r#"printf 'SHELL_PID:%s\n' "$$"; "$ARANY_TEST_EXE" --screen-reader --state-dir "$ARANY_TEST_STATE" --workspace "$ARANY_TEST_WORKSPACE" --resume "$ARANY_TEST_SESSION" --provider custom:local --model model-1"#;
    let mut pty = Pty::new(
        crate::product::PtyLaunch::new(
            crate::product::Isolation::Passwd(crate::isolation::account_wrapper(temp.path())),
            shell,
        )
        .session(&state, &workspace)
        .env("ARANY_TEST_SESSION", id.to_string())
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .spawn(),
    );
    pty.wait_for(b"Input:\r\n");
    pty.adopt_product(&state);
    let submitted_at = pty.send(format!("{draft}\r").as_bytes());
    pty.wait_for_after(submitted_at, b"Directory listed.");
    pty.wait_for_after(submitted_at, b"Input:\r\n");
    assert!(
        crate::process::pty_text_matches(&pty.transcript, format!("You:\n  {draft}\n").as_bytes())
            .next()
            .is_some()
    );
    pty.send(b"/exit\r");
    pty.wait_exit("directory mention exit");
    assert!(pty.child.take().wait().unwrap().success());
    peer.join().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let view = store.load_view(id).await.unwrap().unwrap();
            assert_eq!(view.runs.len(), 1);
            assert_eq!(view.runs[0].objective, draft);
            assert_eq!(view.runs[0].status, arany::RunStatus::Finished);
            assert_eq!(
                view.runs[0]
                    .config
                    .as_ref()
                    .unwrap()
                    .include_sources
                    .iter()
                    .map(|source| source.digest)
                    .collect::<Vec<_>>(),
                listings.map(|listing| <[u8; 32]>::from(sha2::Sha256::digest(listing.as_bytes())))
            );
            let events = store.load_session(id).await.unwrap();
            assert!(events.iter().any(|event| matches!(&event.event,
            arany::Event::MessageAccepted { text, .. } if text == draft)));
            store.close().await.unwrap();
        });
}

#[test]
fn local_registry_has_twenty_four_commands_and_refuses_arguments_without_state_access() {
    use arany_cli::terminal::{
        CommandParseError, InteractiveCommand, Submission, command_completions, parse_submission,
    };
    let names = command_completions("/");
    assert_eq!(names.len(), 24);
    assert!(names.contains(&"limits"));
    assert!(names.contains(&"rewind"));
    assert!(names.contains(&"storage"));
    assert!(names.contains(&"usage"));
    assert_eq!(
        InteractiveCommand::Storage.availability(true),
        arany_cli::terminal::CommandAvailability::Locked
    );
    assert!(matches!(
        parse_submission("/storage extra"),
        Err(CommandParseError::UnexpectedArgument { command: "storage" })
    ));
    assert_eq!(
        parse_submission("/rewind"),
        Ok(Submission::Command {
            command: InteractiveCommand::Rewind,
            argument: None
        })
    );
    assert!(matches!(
        parse_submission("/rewind clear"),
        Err(CommandParseError::UnexpectedArgument { command: "rewind" })
    ));
}

fn seed_empty_build_cache(
    state: &std::path::Path,
    workspace: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let workspace = workspace.canonicalize().expect("canonical Workspace");
    let metadata = std::fs::metadata(&workspace).expect("Workspace metadata");
    let path = workspace.to_str().expect("UTF-8 fixture Workspace");
    let mut identity = path.as_bytes().to_vec();
    identity.extend_from_slice(&metadata.dev().to_le_bytes());
    identity.extend_from_slice(&metadata.ino().to_le_bytes());
    let token = crate::hash(&identity);
    let cache = state.join("build-cache");
    if !cache.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&cache)
            .expect("private cache directory");
    }
    let tree = cache.join(format!("directory-{token}.tree"));
    let lock = cache.join(format!("identity-{token}.lock"));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(tree.join("target"))
        .expect("cache directory tree");
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock)
        .expect("cache identity lock");
    (tree, lock)
}

#[test]
fn new_and_clear_keep_current_selection_without_rewriting_prior_history() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let other_workspace = temp.path().join("other-workspace");
    std::fs::create_dir(&other_workspace).expect("other Workspace");
    std::fs::write(workspace.join("skills-lock.json"), b"unparsed lock data").unwrap();
    let state = temp.path().join("state");
    let saved_id = saved_conversation(&state, &workspace, SessionDefaults::default());
    let shell = "printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4 --resume \"$ARANY_TEST_SESSION\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\"";
    let mut pty = Pty::new(
        crate::product::PtyLaunch::new(
            crate::product::Isolation::Passwd(crate::isolation::account_wrapper(
                state.parent().expect("fixture root"),
            )),
            shell,
        )
        .session(&state, &workspace)
        .env("ARANY_TEST_SESSION", saved_id.to_string())
        .env("OPENAI_API_KEY", "synthetic-test-key")
        .spawn(),
    );
    pty.wait_for(b"Input:\r\n");
    pty.adopt_product(&state);
    let process_limit = arany::ToolResourceSettings::default().build_processes;
    let build_jobs = match std::thread::available_parallelism()
        .ok()
        .and_then(|cores| u32::try_from(cores.get()).ok())
    {
        Some(cores) => format!(
            "automatic; {} effective (cores {cores}, process limit {process_limit})",
            cores.min(process_limit)
        ),
        None => "automatic; Build unavailable".into(),
    };
    let build_cpu = match std::thread::available_parallelism()
        .ok()
        .and_then(|cores| u32::try_from(cores.get()).ok())
    {
        Some(cores) => format!("automatic; {cores} effective (1..{cores})"),
        None => "automatic; Build unavailable".into(),
    };
    let settings = "Settings: type NUMBER VALUE to change a row or NUMBER default to reset it; empty Enter saves; q cancels\r\n\
          Choice 1: Slash menu: tabs (default) (tabs|combined)\r\n\
          Choice 2: Chat view: fullscreen (default) (fullscreen|classic)\r\n\
          Choice 3: Copy on select: on (default) (on|off)\r\n\
          Choice 4: Newline shortcut: Shift+Enter (default) (shift+enter|alt+enter|ctrl+enter)\r\n\
          Choice 5: Guard: auto (default) (auto|required|off)\r\n\
          Choice 6: Guard folders: opens the folders every guarded command may read\r\n\
          Choice 7: Limits: opens /limits\r\n";
    pty.send_and_wait(
        b"/settings\r",
        format!("{settings}Notice: Modified newline shortcuts apply only to visual sessions; use backslash then Enter here\r\nInput:\r\n").as_bytes(),
    );
    pty.send_and_wait(b"7\r", format!("Limits: type NUMBER VALUE to change a row or NUMBER default to reset it; empty Enter saves; q cancels\r\n\
          Choice 1: Provider calls per task: unlimited (default) (number or unlimited)\r\n\
          Choice 2: Tokens per task: unlimited (default) (number or unlimited)\r\n\
          Choice 3: Active time per task: unlimited (default) (duration or unlimited)\r\n\
          Choice 4: Default command timeout: 2 min (default) (duration, e.g. 30s)\r\n\
          Choice 5: Maximum command timeout: 10 min (default) (duration, e.g. 5m)\r\n\
          Choice 6: Build memory: 4.0 GB (default) (size, e.g. 512MB)\r\n\
          Choice 7: Build processes/threads: 256 (default) (number)\r\n\
          Choice 8: Build writable project: 1.0 GB (default) (size, e.g. 1GB)\r\n\
          Choice 9: Build cache: unlimited (default) · 0 B used (size or unlimited, e.g. 8GB)\r\n\
          Choice 10: Build jobs: {build_jobs} (default) (automatic or 1..cores)\r\n\
          Choice 11: Build CPU cores: {build_cpu} (default) (automatic or 1..cores; Linux quota)\r\n\
          Choice 12: Context window: 100% (default) (percent or tokens, e.g. 80% or 500k)\r\n\
          Choice 13: Editor changes: files: 512 (default) (number of files)\r\n\
          Choice 14: Editor changes: size: 64.0 MB (default) (size, e.g. 64MB)\r\n\
          Input:\r\n").as_bytes());
    let over_active_limit = format!("3 {}h\r", arany::MAX_GOAL_ACTIVE_MS / 3_600_000 + 1);
    for rejected in [
        &b"combined\r"[..],
        b"12 combined\r",
        b"1 classic\r",
        b"4 30000\r",
        b"6 512\r",
        b"13 0\r",
        over_active_limit.as_bytes(),
    ] {
        pty.send_and_wait(
            rejected,
            b"Notice: Type NUMBER VALUE from the list; q cancels\r\nInput:\r\n",
        );
    }
    for (value, row) in [
        (
            "3 5m",
            "Choice 3: Active time per task: 5 min (duration or unlimited)",
        ),
        (
            "3 unlimited",
            "Choice 3: Active time per task: unlimited (default) (duration or unlimited)",
        ),
        (
            "4 30s",
            "Choice 4: Default command timeout: 30 s (duration, e.g. 30s)",
        ),
        (
            "4 2min",
            "Choice 4: Default command timeout: 2 min (default) (duration, e.g. 30s)",
        ),
        (
            "6 512MB",
            "Choice 6: Build memory: 512.0 MB (size, e.g. 512MB)",
        ),
        (
            "6 4GB",
            "Choice 6: Build memory: 4.0 GB (default) (size, e.g. 512MB)",
        ),
        (
            "9 0B",
            "Choice 9: Build cache: 0 B · 0 B used (size or unlimited, e.g. 8GB)",
        ),
        (
            "9 8GB",
            "Choice 9: Build cache: 8.0 GB · 0 B used (size or unlimited, e.g. 8GB)",
        ),
        (
            "9 unlimited",
            "Choice 9: Build cache: unlimited (default) · 0 B used (size or unlimited, e.g. 8GB)",
        ),
    ] {
        let changed_at = pty.send_and_wait(
            format!("{value}\r").as_bytes(),
            format!("{row}\r\n").as_bytes(),
        );
        pty.wait_for_after(changed_at, b"Input:\r\n");
    }
    // Closing /limits returns to /settings, which saves both.
    pty.send_and_wait(b"\r", format!("{settings}Input:\r\n").as_bytes());
    pty.send_and_wait(
        b"1 combined\r",
        b"Choice 1: Slash menu: combined (tabs|combined)\r\n",
    );
    pty.send_and_wait(b"\r", "Notice: Settings saved · calls unlimited · active unlimited · command max 10 min · visual sessions\r\nInput:\r\n".as_bytes());
    assert_eq!(
        StateRoot::open_existing(&state)
            .unwrap()
            .record(RecordKind::UiPreferences)
            .read()
            .unwrap()
            .unwrap(),
        saved_ui_preferences("combined", "fullscreen")
    );
    for (command, feedback) in [
        ("/provider anthropic", "Provider: anthropic; set /model"),
        ("/model claude-sonnet-5", "Model: claude-sonnet-5 · low"),
        (
            "/model claude-sonnet-5 high",
            "Model: claude-sonnet-5 · high",
        ),
        (
            "/agents team 2",
            "Next-Run collaboration: team · up to 2 children",
        ),
    ] {
        pty.send_and_wait(
            format!("{command}\r").as_bytes(),
            format!("Notice: {feedback}\r\nInput:\r\n").as_bytes(),
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime");
    let initial = runtime
        .block_on(list_sessions(
            StateRoot::open_existing(&state).expect("State"),
            workspace.clone(),
        ))
        .expect("initial Session");
    assert_eq!(initial.len(), 1);
    let current_id = initial[0].id;
    let held = std::fs::File::open(state.join(format!("session-{current_id}.lock")))
        .expect("existing Session operation lock");
    rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .expect("held Session operation");
    let before = runtime.block_on(async {
        let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
            .expect("before contention Store");
        let prefix = store.load_session(current_id).await.expect("source prefix");
        store.close().await.expect("close source Store");
        prefix
    });
    for command in [
        "/provider openai",
        "/model pending-model",
        "/model claude-sonnet-5 default",
        "/model claude-sonnet-5 low",
        "/agents single",
    ] {
        pty.send_and_wait(format!("{command}\r").as_bytes(), b"Notice: Error: Session has an active operation; selection unchanged. Try again when it finishes\r\nInput:\r\n");
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
                .expect("rejected choice Store");
            assert_eq!(
                store
                    .load_session(current_id)
                    .await
                    .expect("unchanged prefix"),
                before
            );
            store.close().await.expect("close rejection Store");
        });
    }
    rustix::fs::flock(&held, rustix::fs::FlockOperation::Unlock).expect("release operation");
    for effort in ["low", "high"] {
        pty.send_and_wait(
            format!("/model claude-sonnet-5 {effort}\r").as_bytes(),
            format!("Notice: Model: claude-sonnet-5 · {effort}\r\nInput:\r\n").as_bytes(),
        );
    }
    runtime.block_on(async {
        let store = Store::open(StateRoot::open_existing(&state).expect("State"))
            .expect("interrupted source Store");
        let view = store
            .load_view(current_id)
            .await
            .expect("current Session history")
            .expect("current Session");
        let workspace_identity = view.workspace_identity.expect("pinned Workspace identity");
        let goal_id = arany::GoalId::new();
        let run_id = arany::RunId::new();
        store
            .append(
                current_id,
                arany::Event::GoalStarted {
                    include_paths: Some(Vec::new()),
                    goal_id,
                    objective: "Prior accepted objective".into(),
                    workspace_identity,
                    budget: arany::GoalBudget::default(),
                    phase: arany::GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: arany::ToolResourceSettings::default(),
                    continues: None,
                },
            )
            .await
            .expect("source Goal");
        store
            .append(
                current_id,
                arany::Event::GoalRunLinked {
                    goal_id,
                    run_id,
                    phase: arany::GoalPhase::Implementation,
                },
            )
            .await
            .expect("source Goal Run link");
        store
            .append(
                current_id,
                arany::Event::MessageAccepted {
                    skill: None,
                    run_id,
                    text: "Prior accepted objective".into(),
                    images: Vec::new(),
                },
            )
            .await
            .expect("accepted source message");
        store.close().await.expect("close interrupted source Store");
    });
    let mut source_prefix = Vec::new();
    for (command, title) in [("/new", "Before new"), ("/clear", "Before clear")] {
        pty.send_and_wait(format!("/resume {current_id}\r").as_bytes(), b"Input:\r\n");
        pty.send_and_wait(
            format!("/rename {title}\r").as_bytes(),
            format!("Notice: Renamed Session: {title}\r\nInput:\r\n").as_bytes(),
        );
        let prefix = runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
                .expect("source Store");
            let events = store.load_session(current_id).await.expect("source prefix");
            store.close().await.expect("close source Store");
            events
        });
        source_prefix = prefix;
        let from = pty.send_and_wait(format!("{command}\r").as_bytes(), b"Input:\r\n");
        assert!(String::from_utf8_lossy(&pty.transcript[from..]).contains("New conversation"));
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
                .expect("source Store");
            assert_eq!(
                store
                    .load_session(current_id)
                    .await
                    .expect("unchanged history"),
                source_prefix
            );
            store.close().await.expect("close Store");
            assert_eq!(
                list_sessions(
                    StateRoot::open_existing(&state).expect("State"),
                    workspace.clone()
                )
                .await
                .expect("history")
                .len(),
                1
            );
        });
        pty.send_and_wait(b"/provider\r", b"Notice: Provider: anthropic\r\nInput:\r\n");
    }
    let storage_at = pty.send_and_wait(b"/storage\r", b"History, credentials and live/uncertain Guard storage have no automatic-removal hint.\r\nInput:\r\n");
    let storage_text = String::from_utf8_lossy(&pty.transcript[storage_at..]);
    assert!(storage_text.contains("Known owned subtotal:"));
    assert!(storage_text.contains("Guard backing storage/scratch: not yet included"));
    assert!(!storage_text.contains("account-credentials.json"));
    runtime.block_on(async {
        let store =
            Store::open_read_only(StateRoot::open_existing(&state).expect("State")).expect("Store");
        assert_eq!(
            store
                .load_session(current_id)
                .await
                .expect("unchanged storage history"),
            source_prefix
        );
        store.close().await.expect("close Store");
    });
    let (current_cache, current_lock) = seed_empty_build_cache(&state, &workspace);
    let (other_cache, _) = seed_empty_build_cache(&state, &other_workspace);
    let held_cache = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(current_lock)
        .expect("current cache identity lock");
    rustix::fs::flock(
        &held_cache,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .expect("hold current cache lease");
    let clear_result = StateRoot::open_existing(&state)
        .expect("State")
        .clear_workspace_build_cache(&workspace);
    let clear_category = match &clear_result {
        Ok(()) => "cleared".to_owned(),
        Err(arany::StoreError::BuildCacheBusy) => "BuildCacheBusy".to_owned(),
        Err(arany::StoreError::StateNotPrivate) => "StateNotPrivate".to_owned(),
        Err(arany::StoreError::InvalidStateDirectory) => "InvalidStateDirectory".to_owned(),
        Err(arany::StoreError::StorageFull) => "StorageFull".to_owned(),
        Err(arany::StoreError::Io(error)) => format!("Io({:?})", error.kind()),
        Err(_) => "other".to_owned(),
    };
    assert!(
        matches!(clear_result, Err(arany::StoreError::BuildCacheBusy)),
        "Store clear result category: {clear_category}"
    );
    pty.send_and_wait(
        b"/cache clear now\r",
        b"Notice: Error: Usage: /cache clear [all]\r\n",
    );
    pty.send_and_wait(
        b"/cache clear\r",
        b"Notice: Build cache is in use; retry after its command finishes\r\nInput:\r\n",
    );
    assert!(current_cache.exists());
    assert!(other_cache.exists());
    rustix::fs::flock(&held_cache, rustix::fs::FlockOperation::Unlock)
        .expect("release cache lease");
    pty.send_and_wait(
        b"/cache clear\r",
        b"Notice: Cleared this Workspace's private build cache\r\nInput:\r\n",
    );
    assert!(!current_cache.exists());
    assert!(other_cache.exists());
    pty.send_and_wait(
        b"/cache clear all\r",
        b"Notice: Cleared all private build caches\r\nInput:\r\n",
    );
    assert!(!other_cache.exists());
    let (planted_cache, planted_lock) = seed_empty_build_cache(&state, &workspace);
    std::fs::set_permissions(&planted_lock, std::fs::Permissions::from_mode(0o644))
        .expect("non-private cache identity lock");
    pty.send_and_wait(
        b"/cache clear\r",
        b"Notice: Error: private build cache could not be cleared\r\nInput:\r\n",
    );
    assert!(planted_cache.exists());
    pty.send(b"/exit\r");
    let result = pty.finish();
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    assert_eq!(pty.answered.cursor_queries, 0);
    let text = String::from_utf8(pty.transcript).expect("linear transcript UTF-8");
    assert!(text.contains("Run npx --yes skills@1.7.0 experimental_install in this project: .agents/skills is missing and skills-lock.json exists."));
    assert!(!text.contains('\u{1b}'));
    assert_eq!(text.matches("Notice: Error:").count(), 7);
    assert_tty_restored(&text);
    let expected = arany::SessionDefaults {
        provider: Some("anthropic".into()),
        model: Some("claude-sonnet-5".into()),
        effort: Some(arany::Effort::High),
        account_id: None,
        policy: CollaborationPolicy::Team {
            max_active_children: 2,
        },
    };
    runtime.block_on(async {
        let sessions = list_sessions(StateRoot::open_existing(&state).expect("State"), workspace)
            .await
            .expect("closed Sessions");
        assert_eq!(
            sessions.len(),
            1,
            "new/clear without messages saves no Sessions"
        );
        let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
            .expect("closed Store");
        for (id, prefix, title) in [(current_id, source_prefix, "Before clear")] {
            assert_eq!(
                store.load_session(id).await.expect("source history"),
                prefix
            );
            let view = store
                .load_view(id)
                .await
                .expect("source replay")
                .expect("source Session");
            assert_eq!(view.title, title);
            assert_eq!(view.defaults, expected);
            assert!(view.compactions.is_empty());
            assert_eq!(view.runs.len(), 1);
            assert_eq!(view.runs[0].status, arany::RunStatus::Interrupted);
            assert_eq!(view.runs[0].objective, "Prior accepted objective");
            assert!(view.runs[0].agents.is_empty());
        }
        store.close().await.expect("close replay Store");
    });
}

#[test]
fn narrow_help_signal_restores_terminal_without_starting_a_run() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let shell = "stty rows 8 cols 16; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut pty = Pty::new(
        crate::product::PtyLaunch::new(
            crate::product::Isolation::Passwd(crate::isolation::account_wrapper(
                state.parent().expect("fixture root"),
            )),
            shell,
        )
        .session(&state, &workspace)
        .env("OPENAI_API_KEY", "synthetic-test-key")
        .term("xterm")
        .spawn(),
    );
    pty.wait_for(b"SHELL_PID:");
    let shell_line = pty
        .transcript
        .windows(b"SHELL_PID:".len())
        .position(|part| part == b"SHELL_PID:")
        .expect("shell marker")
        + b"SHELL_PID:".len();
    pty.wait_for_after(shell_line, b"\n");
    let product_pid = pty.adopt_product(&state);
    pty.wait_for(b"Ask Arany");
    let visible = String::from_utf8_lossy(&pty.transcript);
    let before = visible
        .lines()
        .find_map(|line| line.strip_prefix("TTY_BEFORE:"))
        .expect("initial terminal settings")
        .trim_end_matches('\r')
        .to_owned();
    let ready = pty.send(b"/help\r");
    wait_for_frame(&mut pty, (ready, 16, 8), |screen| {
        screen.iter().any(|row| row.contains("Commands"))
            && screen.iter().any(|row| row.contains("Up/Dn Enter Esc"))
    });
    assert_ne!(tty_settings(product_pid), before, "help owns raw mode");
    kill_process(product_pid, Signal::TERM).expect("terminate open help");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pty.pump();
        if pty
            .child
            .child()
            .try_wait()
            .expect("process status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "help exit: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
    let status = pty.child.take().wait().expect("process exit");
    assert!(!status.success(), "SIGTERM exits unsuccessfully");
    pty.pump();
    let text = String::from_utf8_lossy(&pty.transcript);
    let after = text
        .rsplit_once("TTY_AFTER:")
        .expect("restored terminal marker")
        .1
        .lines()
        .next()
        .expect("restored terminal settings")
        .trim_end_matches('\r');
    assert_eq!(before, after, "raw mode restored after SIGTERM");
    assert!(text.contains("terminated by SIGTERM"));
    assert!(!text.contains("synthetic-test-key"), "credential not shown");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let root = StateRoot::open_existing(&state).expect("existing State");
        let sessions = list_sessions(root, workspace.clone())
            .await
            .expect("listed Sessions");
        assert!(
            sessions.is_empty(),
            "help and shutdown save no empty conversation"
        );
    });
}

fn inspect_storage_pager(pty: &mut Pty) {
    let from = pty.send(b"/storage\r");
    pty.wait_for_screen((from, 40, 24), "Esc |");
    assert!(screen_contains(&pty.transcript, 40, 24, "Arany storage:"));
    let from = pty.send(b"\x1b[F");
    pty.wait_for_screen((from, 40, 24), "History, credentials");
    let from = pty.send(b"\x1b");
    wait_for_caret(pty, from, 3);
}

fn inspect_file_suggestions(workspace: &Path, pty: &mut Pty) {
    let start = pty.send(b"@");
    pty.wait_for_screen((start, 40, 24), "visible-note.txt");
    for hidden in ["ignored.cache", "private-note.txt", ".ENV"] {
        assert!(!screen_contains(&pty.transcript, 40, 24, hidden));
        assert!(!String::from_utf8_lossy(&pty.transcript[start..]).contains(hidden));
    }
    std::fs::write(workspace.join(".gitignore"), b"*").unwrap();
    let refining = pty.send(b"visible-no");
    let refined_column = 14;
    wait_for_caret(pty, refining, refined_column);
    assert!(screen_contains(&pty.transcript, 40, 24, "visible-note.txt"));
    let closed = pty.send(b"\x1b");
    wait_for_caret(pty, closed, refined_column);
    let cleared = pty.send(b"\x03");
    wait_for_caret(pty, cleared, 3);
    std::fs::write(workspace.join(".gitignore"), b"*.cache\n").unwrap();
}

#[test]
fn inline_tab_completion_keeps_placeholders_out_of_commands_and_restores_terminal() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let saved_id = saved_conversation(&state, &workspace, SessionDefaults::default());
    let skill = workspace.join(".agents/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    let guidance =
        b"---\nname: review\ndescription: Synthetic review\n---\nReview only the selected task.\n";
    std::fs::write(skill.join("SKILL.md"), guidance).unwrap();
    let ask_skill = workspace.join(".agents/skills/ask-matt");
    std::fs::create_dir(&ask_skill).unwrap();
    std::fs::write(
        ask_skill.join("SKILL.md"),
        b"---\nname: ask-matt\ndescription: Synthetic guidance\n---\nGuide the selected task.\n",
    )
    .unwrap();
    let config = serde_json::json!({"workspace_paths":["."],"write":false,
        "commands":[],"mcp":[],"skills":[]});
    std::fs::write(
        state.join("tools.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(
        state.join("tools.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(workspace.join(".gitignore"), b"*.cache\n!.env\n").unwrap();
    for name in [
        "visible-note.txt",
        "ignored.cache",
        "private-note.txt",
        ".ENV",
    ] {
        std::fs::write(workspace.join(name), b"synthetic suggestion payload").unwrap();
    }
    std::fs::write(
        state.join("permissions.json"),
        br#"{"permissions":{"deny":["Read(./private-note.txt)"]}}"#,
    )
    .unwrap();
    std::fs::set_permissions(
        state.join("permissions.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let shell = "stty rows 24 cols 40; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider anthropic --tools --no-color; first_code=$?; printf 'SETTINGS_RESTART\n'; \"$ARANY_TEST_EXE\" --classic --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider openai --tools --no-color; second_code=$?; printf 'VIEW_RESTART\n'; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider openai --tools --no-color; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; test \"$first_code\" -eq 0 || exit \"$first_code\"; test \"$second_code\" -eq 0 || exit \"$second_code\"; exit \"$exit_code\"";
    let mut pty = Pty::new(
        crate::product::PtyLaunch::new(
            crate::product::Isolation::Passwd(crate::isolation::account_wrapper(
                state.parent().expect("fixture root"),
            )),
            shell,
        )
        .session(&state, &workspace)
        .env("ARANY_TEST_SESSION", saved_id.to_string())
        .term("xterm")
        .spawn(),
    );
    pty.wait_for(b"Ask Arany");
    let shell_pid = pty.shell_pid();
    pty.adopt_product(&state);
    let tty_before = super::active_terminal::transcript_field(
        &String::from_utf8_lossy(&pty.transcript),
        "TTY_BEFORE:",
    )
    .to_owned();
    inspect_file_suggestions(&workspace, &mut pty);
    inspect_storage_pager(&mut pty);
    let paste_at = pty.send_and_wait(b"\x1b[200~first\r\nsecond\x1b[201~", b";3Hfirst");
    wait_for_caret(&mut pty, paste_at, 9);
    assert!(
        !String::from_utf8_lossy(&pty.transcript[paste_at..]).contains("error:"),
        "paste must edit the draft without submitting either line"
    );
    let clipboard_at = pty.send_and_wait(b"\x16", b"unavailable;");
    wait_for_caret(&mut pty, clipboard_at, 9);
    let continued_at = pty.send(b"Z");
    wait_for_caret(&mut pty, continued_at, 10);
    let clear_at = pty.send(b"\x03");
    wait_for_caret(&mut pty, clear_at, 3);
    pty.fresh_capture();
    let tabs_at = pty.send_and_wait(b"/", b"[Commands]");
    wait_for_caret(&mut pty, tabs_at, 4);
    let skill_at = pty.send_and_wait(b"\x1b[C", b"> (skill) /ask-matt");
    wait_for_caret(&mut pty, skill_at, 4);
    let commands_at = pty.send(b"\x1b[D");
    wait_for_caret(&mut pty, commands_at, 4);
    let skill_at = pty.send_and_wait(b"ask", b"> (skill) /ask-matt");
    wait_for_caret(&mut pty, skill_at, 7);
    let skill_accept_at = pty.send(b"\t");
    wait_for_caret(&mut pty, skill_accept_at, 13);
    let skill_clear_at = pty.send(b"\x03");
    wait_for_caret(&mut pty, skill_clear_at, 3);
    let invoke_at = pty.send(b"/ask");
    wait_for_caret(&mut pty, invoke_at, 7);
    let invoke_at = pty.send(b"\r");
    pty.wait_for_screen((invoke_at, 40, 24), "Choose a model");
    wait_for_caret(&mut pty, invoke_at, 12);
    assert!(
        crate::screen_rows(&pty.transcript, 40, 24)
            .iter()
            .any(|row| row.trim() == "> /ask-matt"),
        "model rejection must retain the complete Skill invocation"
    );
    let clear_at = pty.send(b"\x03");
    wait_for_caret(&mut pty, clear_at, 3);
    let settings_at = pty.send(b"/settings\r");
    wait_armed(&mut pty, settings_at);
    let saved_at = pty.send_and_wait(b"\x1b[C\r", b"combined");
    wait_for_caret(&mut pty, saved_at, 3);
    assert_eq!(
        StateRoot::open_existing(&state)
            .unwrap()
            .record(RecordKind::UiPreferences)
            .read()
            .unwrap()
            .unwrap(),
        saved_ui_preferences("combined", "fullscreen"),
        "--classic must not leak into the saved chat view"
    );
    pty.fresh_capture();
    pty.send_and_wait(b"/resum", b"> Cmd /resume");
    let picker_at = pty.send_and_wait(b"\r", b"Resume");
    wait_armed(&mut pty, picker_at);
    let closed_at = pty.send(b"\x1b");
    pty.wait_for_screen((closed_at, 40, 24), "closed");
    let menu_at = pty.send(b"/s");
    wait_for_caret(&mut pty, menu_at, 5);
    pty.send(b"\x1b[B");
    pty.wait_for(b"> Cmd /status");
    let status_at = pty.send_and_wait(b"\r", b"Tokens");
    wait_for_frame(&mut pty, (status_at, 40, 24), |screen| {
        screen
            .iter()
            .any(|row| row.contains("Tokens used: no calls yet"))
            && screen.iter().any(|row| row.contains("/usage for details"))
            && screen.iter().any(|row| row.contains("Esc close"))
    });
    pty.send_and_wait(b"\x1b", b"Ask Arany");
    let newline_at = pty.send(b"alpha\\\r");
    wait_for_caret(&mut pty, newline_at, 3);
    let newline_at = pty.send(b"beta\x1b[13;2u");
    wait_for_caret(&mut pty, newline_at, 3);
    pty.send(b"\x03");
    // The viewport shrinks with the draft, so the cleared composer is read from the settled screen.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows = crate::session_picker::screen_rows(&pty.transcript, 40, 24);
        if rows.iter().any(|row| row.starts_with('>'))
            && !rows.iter().any(|row| row.contains("alpha"))
        {
            break;
        }
        pty.pump();
        assert!(
            Instant::now() < deadline,
            "cleared draft: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
    let model_menu_at = pty.send(b"/s");
    wait_for_caret(&mut pty, model_menu_at, 5);
    pty.send(b"\x1b[B");
    pty.wait_for_after(model_menu_at, b"> Cmd /status");
    let closed_at = pty.send(b"\x1b");
    // The open menu also leaves the caret in column 5; only a decoded composer without the
    // menu proves Escape was handled, wherever the content-sized viewport now sits.
    pty.pump_until("closed completion menu", |transcript| {
        crate::composer_caret(transcript, closed_at, 40, 24).is_some_and(|(column, screen)| {
            column == 5
                && !screen.iter().any(|row| row.contains("Cmd /status"))
                && screen.iter().any(|row| row.starts_with("> /s"))
        })
    });
    pty.send(b"\x7f\x7f/model staged-model\r");
    pty.wait_for_after(closed_at, b"Model:");
    pty.fresh_capture();
    let unknown_at = pty.send(b"/provder\r");
    pty.wait_for(b"Unknown");
    pty.wait_for(b"r?");
    assert!(
        String::from_utf8_lossy(&pty.transcript[unknown_at..]).contains("error:"),
        "local command rejection needs a textual error heading: {}",
        tail(&pty.transcript)
    );
    pty.send(b"\x7f\x7f\x7f\x7f\x7f\x7f\x7fprov");
    pty.wait_for(b"> Cmd /provider");
    pty.send(b"\topenai\r");
    pty.wait_for(b"Provider:");
    pty.send(b"/agents bad\r");
    pty.wait_for(b"policy");
    pty.send(b"\x7f\x7f\x7fteam 2\r");
    let rename_at = pty.send_and_wait(
        format!("/rename {}Z\x1b[D\r", "A".repeat(128)).as_bytes(),
        b"long;",
    );
    let notice_at = rename_at
        + pty.transcript[rename_at..]
            .windows(b"long;".len())
            .position(|part| part == b"long;")
            .expect("rename rejection")
        + b"long;".len();
    pty.wait_for_after(notice_at, b"\x1b[?25h");
    pty.send_and_wait(b"\x7f\r", b"Renamed");
    pty.fresh_capture();
    pty.send(b"/exit\r");
    let restart_at = pty.transcript.len();
    pty.wait_for_after(restart_at, b"SETTINGS_RESTART");
    let restart_at = restart_at
        + pty.transcript[restart_at..]
            .windows(b"SETTINGS_RESTART".len())
            .position(|part| part == b"SETTINGS_RESTART")
            .unwrap()
        + b"SETTINGS_RESTART".len();
    pty.wait_for_after(restart_at, b"Ask Arany");
    wait_for_caret(&mut pty, restart_at, 3);
    pty.guard_product(ProductGuard::new(
        product_child_of(shell_pid),
        state.clone(),
    ));
    let combined_at = pty.send(b"/");
    wait_for_caret(&mut pty, combined_at, 4);
    let selected_at = pty.send(b"rev\t");
    wait_for_caret(&mut pty, selected_at, 11);
    let clear_at = pty.send(b"\x03");
    wait_for_caret(&mut pty, clear_at, 3);
    let pending = state.join("ui-preferences.pending");
    let outside = temp.path().join("outside-preference-canary");
    std::fs::write(&outside, b"unchanged").unwrap();
    std::os::unix::fs::symlink(&outside, &pending).unwrap();
    let rejected_at = pty.send(b"/settings\r");
    wait_armed(&mut pty, rejected_at);
    pty.send(b"\x1b[C\r");
    wait_for_caret(&mut pty, rejected_at, 3);
    assert_eq!(
        StateRoot::open_existing(&state)
            .unwrap()
            .record(RecordKind::UiPreferences)
            .read()
            .unwrap()
            .unwrap(),
        saved_ui_preferences("combined", "fullscreen")
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
    assert!(
        String::from_utf8_lossy(&pty.transcript[rejected_at..]).contains("error"),
        "unsafe save gives visible failure feedback: {}",
        String::from_utf8_lossy(&pty.transcript[rejected_at..])
            .chars()
            .take(4096)
            .flat_map(char::escape_default)
            .collect::<String>()
    );
    let unchanged_at = pty.send(b"/rev\t");
    wait_for_caret(&mut pty, unchanged_at, 11);
    let clear_at = pty.send(b"\x03");
    wait_for_caret(&mut pty, clear_at, 3);
    pty.fresh_capture();
    std::fs::remove_file(pending).unwrap();
    let tabs_at = pty.send(b"/settings\r");
    wait_armed(&mut pty, tabs_at);
    pty.send(format!("\x1b[C{}\x1b[C\x1b[C\r", "\x1b[B".repeat(3)).as_bytes());
    wait_for_caret(&mut pty, tabs_at, 3);
    let record = |view: &str| {
        ui_preferences(&[
            ("completion", "tabs", "tabs"),
            ("newline", "ctrl_enter", "shift_enter"),
            ("view", view, "fullscreen"),
        ])
    };
    assert_eq!(
        StateRoot::open_existing(&state)
            .unwrap()
            .record(RecordKind::UiPreferences)
            .read()
            .unwrap()
            .unwrap(),
        record("fullscreen")
    );
    let shortcut_at = pty.send(b"saved\x1b[13;5u");
    wait_for_caret(&mut pty, shortcut_at, 3);
    let clear_at = pty.send(b"\x03");
    wait_for_caret(&mut pty, clear_at, 3);
    for (view, pinned) in [("classic", false), ("fullscreen", true), ("classic", false)] {
        pty.fresh_capture();
        let chosen_at = pty.send(b"/settings\r");
        wait_armed(&mut pty, chosen_at);
        pty.send(b"\x1b[B\x1b[C\r");
        pty.wait_for_after(chosen_at, b"saved");
        wait_for_caret(&mut pty, chosen_at, 3);
        assert_eq!(
            StateRoot::open_existing(&state)
                .unwrap()
                .record(RecordKind::UiPreferences)
                .read()
                .unwrap()
                .unwrap(),
            record(view)
        );
        let frames = &pty.transcript[chosen_at..];
        assert!(
            !frames.windows(8).any(|part| part == b"\x1b[?1049h"),
            "--classic keeps this process classic: {}",
            tail(frames)
        );
        assert_eq!(
            frames.windows(7).any(|part| part == b"without"),
            pinned,
            "pinned notice: {}",
            tail(frames)
        );
    }
    pty.fresh_capture();
    pty.send(b"/exit\r");
    let view_at = pty.transcript.len();
    pty.wait_for_after(view_at, b"VIEW_RESTART");
    let view_at = view_at
        + pty.transcript[view_at..]
            .windows(b"VIEW_RESTART".len())
            .position(|part| part == b"VIEW_RESTART")
            .unwrap()
        + b"VIEW_RESTART".len();
    pty.wait_for_after(view_at, b"Ask Arany");
    wait_for_caret(&mut pty, view_at, 3);
    assert!(
        !pty.transcript[view_at..]
            .windows(8)
            .any(|part| part == b"\x1b[?1049h"),
        "saved classic starts on the primary screen"
    );
    pty.guard_product(ProductGuard::new(
        product_child_of(shell_pid),
        state.clone(),
    ));
    let shortcut_at = pty.send(b"saved\x1b[13;5u");
    wait_for_caret(&mut pty, shortcut_at, 3);
    let clear_at = pty.send(b"\x03");
    wait_for_caret(&mut pty, clear_at, 3);
    pty.fresh_capture();
    let switched_at = pty.send(b"/settings\r");
    wait_armed(&mut pty, switched_at);
    pty.send(b"\x1b[B\x1b[C\r");
    pty.wait_for_after(switched_at, b"\x1b[?1049h");
    wait_for_caret(&mut pty, switched_at, 3);
    assert_eq!(
        StateRoot::open_existing(&state)
            .unwrap()
            .record(RecordKind::UiPreferences)
            .read()
            .unwrap()
            .unwrap(),
        record("fullscreen")
    );
    inspect_storage_pager(&mut pty);
    let draft_at = pty.send(b"abc");
    wait_for_caret(&mut pty, draft_at, 6);
    pty.send(b"\x7f\x7f\x7f/exit\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pty.pump();
        if pty
            .child
            .child()
            .try_wait()
            .expect("process status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "PTY exit: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
    let status = pty.child.take().wait().expect("process exit");
    assert!(
        status.success(),
        "product failed: {}",
        tail(&pty.transcript)
    );
    pty.pump();
    let text = String::from_utf8_lossy(&pty.transcript);
    assert_eq!(
        tty_before,
        active_terminal::transcript_field(&text, "TTY_AFTER:")
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let root = StateRoot::open_existing(&state).expect("existing State");
        let sessions = list_sessions(root, workspace.clone())
            .await
            .expect("listed Sessions");
        assert_eq!(sessions.len(), 1);
        let root = StateRoot::open_existing(&state).expect("existing State");
        let store = Store::open_read_only(root).expect("read-only Store");
        let view = store
            .load_view(sessions[0].id)
            .await
            .expect("valid history")
            .expect("Session");
        assert_eq!(view.defaults.provider.as_deref(), Some("openai"));
        assert_eq!(view.defaults.model, None);
        assert_eq!(
            view.defaults.policy,
            CollaborationPolicy::Team {
                max_active_children: 2
            }
        );
        assert!(view.runs.is_empty());
        let events = store
            .load_session(sessions[0].id)
            .await
            .expect("closed command Events");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    &event.event,
                    arany::Event::SessionDefaultChanged { defaults }
                        if defaults.provider.as_deref() == Some("anthropic")
                        && defaults.model.as_deref() == Some("staged-model")
                        && defaults.effort == Some(arany::Effort::Low)
                        && defaults.account_id.is_none()
                ))
                .count(),
            1,
            "corrected prefix commits the literal model exactly once before Provider change"
        );
        let expected_title = format!("{}Z", "A".repeat(127));
        assert_eq!(
            view.title, expected_title,
            "rename keeps its original caret"
        );
        let renames = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                arany::Event::SessionRenamed { title } => Some(title.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            renames,
            [expected_title.as_str()],
            "only the corrected title commits"
        );
        store.close().await.expect("close Store");
    });
}

struct StateModeGuard {
    path: std::path::PathBuf,
    mode: std::fs::Permissions,
}

impl Drop for StateModeGuard {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.path, self.mode.clone());
    }
}

#[test]
fn screen_reader_rejected_command_does_not_join_the_next_line() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let saved_id = saved_conversation(&state, &workspace, SessionDefaults::default());
    let shell = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider anthropic; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let command = crate::product::PtyLaunch::new(
        crate::product::Isolation::Passwd(crate::isolation::account_wrapper(
            state.parent().expect("fixture root"),
        )),
        shell,
    )
    .session(&state, &workspace)
    .env("ARANY_TEST_SESSION", saved_id.to_string())
    .term("xterm");
    #[cfg(debug_assertions)]
    let command = command
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false");
    let mut pty = Pty::new(command.spawn());
    pty.wait_for(b"Input:\r\n");
    pty.send(b"/provder\r");
    pty.wait_for(b"Notice: Error: Unknown command; did you mean /provider?\r\nInput:\r\n");
    pty.send_and_wait(
        b"/paste\r",
        b"Notice: Error: local clipboard unavailable; use terminal paste; draft unchanged\r\n",
    );
    pty.send(b"/provider openai\r");
    pty.wait_for(b"Notice: Provider: openai; set /model\r\nInput:\r\n");
    {
        pty.send_and_wait(
            b"/model gpt-5.4\r",
            "Notice: Model: gpt-5.4 · low\r\nInput:\r\n".as_bytes(),
        );
        for (objective, error) in [
            (
                "Check @../outside",
                "\"../outside\": path is outside workspace",
            ),
            (
                "Check @missing.txt",
                "\"missing.txt\": explicit include does not exist",
            ),
            (
                "Check @\"unfinished",
                "\"\\\"unfinished\": unterminated quoted path",
            ),
            ("Check @\".ENV/\"", "\".ENV/\": protected path"),
        ] {
            pty.send_and_wait(format!("{objective}\r").as_bytes(), format!("Notice: Error: {error}; check the file inputs and enter a new line\r\nInput:\r\n").as_bytes());
            pty.send_and_wait(b"/provider\r", b"Notice: Provider: openai\r\nInput:\r\n");
        }
    }
    #[cfg(debug_assertions)]
    {
        pty.send_and_wait(
            b"/model gpt-5.4 low\r",
            "Notice: Model: gpt-5.4 · low\r\nInput:\r\n".as_bytes(),
        );
        for malformed in [false, true] {
            if malformed {
                StateRoot::admit(&temp.path().join("account-root"))
                    .expect("isolated account root")
                    .record(RecordKind::ChatgptAccounts)
                    .replace(b"synthetic-malformed-account-canary")
                    .expect("synthetic malformed metadata");
            }
            pty.send_and_wait(b"/provider chatgpt\r", if malformed {
                    b"Notice: Error: saved ChatGPT setup is incompatible or damaged; use /setup or arany --setup to reset it and sign in again\r\nInput:\r\n"
                } else {
                    b"Notice: Error: no selected ChatGPT account; use /setup to connect\r\nInput:\r\n"
                });
            pty.send_and_wait(b"/provider\r", b"Notice: Provider: openai\r\nInput:\r\n");
        }
    }
    {
        pty.send_and_wait(
            b"/provider custom:synthetic\r",
            b"Notice: Provider: custom:synthetic; set /model\r\nInput:\r\n",
        );
    }
    let profile_path = state.join("provider-profiles.json");
    let document = |model: &str| {
        let draft = arany::CustomProfileDraft {
            name: "synthetic".into(),
            url: "http://127.0.0.1:9/responses".into(),
            model: model.into(),
            credential_env: "ARANY_PROVIDER_SYNTHETIC".into(),
            efforts: vec![arany::Effort::Low, arany::Effort::High],
            ..arany::test_support::local_custom_profile(9)
        };
        serde_json::to_vec(&serde_json::json!({ "profiles": [draft] })).expect("profile JSON")
    };
    let mismatched = document("different-model");
    let matched = document("synthetic-custom-model");
    let forms = [
        "/model synthetic-custom-model max",
        "/model synthetic-custom-model low",
    ];
    for (record, mode, commands, feedback) in [
        (
            None,
            0o600,
            forms.as_slice(),
            "custom Provider profile file is unavailable",
        ),
        (
            Some(b"synthetic-malformed-profile-canary".as_slice()),
            0o600,
            forms.as_slice(),
            "invalid custom Provider profile configuration",
        ),
        (
            Some(b"synthetic-malformed-profile-canary".as_slice()),
            0o644,
            forms.as_slice(),
            "custom Provider profile file is unsafe",
        ),
        (
            Some(mismatched.as_slice()),
            0o600,
            forms.as_slice(),
            "Selected model does not match its custom profile",
        ),
        (
            Some(matched.as_slice()),
            0o600,
            &forms[..1],
            "Effort unavailable for the selected Provider/model",
        ),
    ] {
        if let Some(record) = record {
            std::fs::write(&profile_path, record).expect("synthetic profile");
            std::fs::set_permissions(&profile_path, std::fs::Permissions::from_mode(mode))
                .expect("test profile permissions");
        }
        for command in commands {
            pty.send_and_wait(
                format!("{command}\r").as_bytes(),
                format!("Notice: Error: {feedback}\r\nInput:\r\n").as_bytes(),
            );
        }
    }
    for (command, feedback) in [
        (
            "/model synthetic-custom-model low",
            "Model: synthetic-custom-model · low",
        ),
        ("/provider openai", "Provider: openai; set /model"),
    ] {
        pty.send_and_wait(
            format!("{command}\r").as_bytes(),
            format!("Notice: {feedback}\r\nInput:\r\n").as_bytes(),
        );
    }
    #[cfg(debug_assertions)]
    for (command, feedback) in [
        ("/model gpt-5.4", "Model: gpt-5.4 · low"),
        ("/model gpt-5.4 low", "Model: gpt-5.4 · low"),
    ] {
        pty.send_and_wait(
            format!("{command}\r").as_bytes(),
            format!("Notice: {feedback}\r\nInput:\r\n").as_bytes(),
        );
    }
    pty.send_and_wait(
        b"/help\r",
        b"Command: /exit; Exit Session; during Run: view-only\r\nInput:\r\n",
    );
    pty.send(b"/agents bad\r");
    pty.wait_for(b"Notice: Error: invalid collaboration policy\r\nInput:\r\n");
    pty.send(b"/agents team 2\r");
    pty.wait_for(b"Notice: Next-Run collaboration: team \xc2\xb7 up to 2 children\r\nInput:\r\n");
    pty.send_and_wait(
        format!("/resume {}\r", arany::SessionId::new()).as_bytes(),
        b"Notice: Error: Session not found\r\nInput:\r\n",
    );
    pty.send_and_wait(
        b"/fork\r",
        b"Notice: Error: Session has no committed Run boundary to fork\r\nInput:\r\n",
    );
    pty.send_and_wait(
        format!("/rename {}\r", "é".repeat(65)).as_bytes(),
        b"Notice: Error: Title is empty or too long; use a shorter title\r\nInput:\r\n",
    );
    let state_guard = StateModeGuard {
        path: state.clone(),
        mode: std::fs::metadata(&state).expect("State").permissions(),
    };
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o000))
        .expect("make State unavailable mid-Session");
    pty.send_and_wait(
        format!("/resume {saved_id}\r").as_bytes(),
        b"Notice: Error: state directory unavailable or unsafe\r\nInput:\r\n",
    );
    std::fs::set_permissions(&state, state_guard.mode.clone()).expect("restore State");
    pty.send_and_wait(
        b"/rename Corrected title\r",
        b"Notice: Renamed Session: Corrected title\r\nInput:\r\n",
    );
    pty.send(b"/exit\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pty.pump();
        if pty
            .child
            .child()
            .try_wait()
            .expect("process status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "PTY exit: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
    let status = pty.child.take().wait().expect("process exit");
    assert!(
        status.success(),
        "product failed: {}",
        tail(&pty.transcript)
    );
    pty.pump();
    let text = String::from_utf8_lossy(&pty.transcript);
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains("synthetic-malformed-account-canary"));
    assert!(!text.contains("synthetic-malformed-profile-canary"));
    assert_tty_restored(&text);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let root = StateRoot::open_existing(&state).expect("existing State");
        let sessions = list_sessions(root, workspace.clone())
            .await
            .expect("listed Sessions");
        assert_eq!(sessions.len(), 1);
        let root = StateRoot::open_existing(&state).expect("existing State");
        let store = Store::open_read_only(root).expect("read-only Store");
        let view = store
            .load_view(sessions[0].id)
            .await
            .expect("valid history")
            .expect("Session");
        assert_eq!(view.defaults.provider.as_deref(), Some("openai"));
        assert_eq!(view.defaults.account_id, None);
        #[cfg(debug_assertions)]
        {
            assert_eq!(view.defaults.model.as_deref(), Some("gpt-5.4"));
            assert_eq!(view.defaults.effort, Some(arany::Effort::Low));
        }
        #[cfg(not(debug_assertions))]
        {
            assert_eq!(view.defaults.model, None);
            assert_eq!(view.defaults.effort, None);
        }
        assert_eq!(view.title, "Corrected title");
        assert_eq!(
            view.defaults.policy,
            CollaborationPolicy::Team {
                max_active_children: 2
            }
        );
        assert!(view.runs.is_empty());
        let events = store
            .load_session(sessions[0].id)
            .await
            .expect("closed command Events");
        assert_eq!(
            events.len(),
            if cfg!(debug_assertions) { 12 } else { 9 },
            "only Session creation, accepted defaults and one rename are durable"
        );
        let renames = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                arany::Event::SessionRenamed { title } => Some(title.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            renames,
            ["Corrected title"],
            "rejected title creates no Event"
        );
        let custom_defaults = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                arany::Event::SessionDefaultChanged { defaults }
                    if defaults.provider.as_deref() == Some("custom:synthetic") =>
                {
                    Some(defaults)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            custom_defaults.len(),
            2,
            "only explicit custom staging is committed"
        );
        assert_eq!(custom_defaults[0].model, None);
        assert_eq!(
            custom_defaults[1].model.as_deref(),
            Some("synthetic-custom-model")
        );
        assert_eq!(custom_defaults[1].effort, Some(arany::Effort::Low));
        assert!(
            custom_defaults
                .iter()
                .all(|defaults| defaults.account_id.is_none())
        );
        store.close().await.expect("close Store");
        for bytes in [
            arany_cli::presentation::render_session(
                &view,
                &events,
                arany_cli::presentation::Output::Jsonl,
            )
            .into_bytes(),
            std::fs::read(state.join("events.sqlite3")).expect("closed journal"),
        ] {
            assert!(
                !bytes
                    .windows(b"synthetic-malformed-account-canary".len())
                    .any(|part| part == b"synthetic-malformed-account-canary"),
                "account metadata entered canonical state"
            );
            assert!(
                !bytes
                    .windows(b"synthetic-malformed-profile-canary".len())
                    .any(|part| part == b"synthetic-malformed-profile-canary"),
                "profile metadata entered canonical state"
            );
        }
    });
}

#[test]
fn slash_skill_enter_submits_the_admitted_name_and_task() {
    use arany_cli::terminal::{
        CompletionLayout, Composer, ComposerEdit, Submission, TerminalInput,
    };
    let mut composer = Composer::default();
    composer.set_runtime_skills(&["ask-matt".into(), "help".into()]);
    composer.set_completion_layout(CompletionLayout::Combined);
    for (draft, task) in [
        ("/ask-matt", ""),
        ("/ask-matt do X", "do X"),
        ("/ask-matt /ask-matt", "/ask-matt"),
        ("/ask-m", ""),
    ] {
        composer.clear();
        composer.insert_paste(draft).unwrap();
        assert_eq!(
            composer.apply(TerminalInput::Submit),
            ComposerEdit::Unchanged
        );
        assert_eq!(
            composer.submission(),
            Ok(Submission::Skill {
                name: "ask-matt",
                task
            })
        );
    }
    for draft in [
        "/ask-matt",
        "/ask-matt   do X  ",
        "/ask-matt\tline one\nline two",
    ] {
        composer.clear();
        composer.insert_paste(draft).unwrap();
        assert!(matches!(
            composer.submission(),
            Ok(Submission::Skill {
                name: "ask-matt",
                ..
            })
        ));
        assert_eq!(composer.take(), draft);
    }
    let mut tabs = Composer::default();
    tabs.set_runtime_skills(&["ask-matt".into()]);
    tabs.set_completion_layout(CompletionLayout::Tabs);
    tabs.insert_paste("/ask-m").unwrap();
    assert_eq!(tabs.apply(TerminalInput::Submit), ComposerEdit::Unchanged);
    assert_eq!(
        tabs.submission(),
        Ok(Submission::Skill {
            name: "ask-matt",
            task: ""
        })
    );
    tabs.clear();
    tabs.set_runtime_skills(&["ask-matt".into(), "ask-mary".into()]);
    tabs.insert_paste("/ask-m").unwrap();
    assert_eq!(tabs.apply(TerminalInput::Submit), ComposerEdit::Unchanged);
    assert_eq!(tabs.text(), "/ask-mary");
    assert_eq!(
        tabs.submission(),
        Ok(Submission::Skill {
            name: "ask-mary",
            task: ""
        })
    );
    composer.clear();
    composer.insert_paste("/ask-m").unwrap();
    assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
    assert_eq!(composer.text(), "/ask-matt ");
    for literal in ["//ask-matt", "$ask-matt"] {
        composer.clear();
        composer.insert_paste(literal).unwrap();
        assert!(matches!(
            composer.submission(),
            Ok(Submission::Objective(_))
        ));
    }
    composer.clear();
    composer.insert_paste("/help").unwrap();
    assert!(matches!(
        composer.submission(),
        Ok(Submission::Command { .. })
    ));
    composer.clear();
    composer.insert_paste("/unadmitted").unwrap();
    assert!(composer.submission().is_err());
}
