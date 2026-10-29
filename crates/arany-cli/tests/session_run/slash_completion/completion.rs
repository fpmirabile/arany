use super::*;

#[test]
fn directory_mentions_submit_listings_and_preserve_the_typed_message() {
    use super::loopback::{check_profile, read_request, send_response, write_profile};
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
        for index in 0..4 {
            let mut stream = crate::process::accept_with_deadline(
                &listener,
                Duration::from_secs(20),
                "directory mention request",
            );
            let wire: serde_json::Value =
                serde_json::from_slice(&read_request(&mut stream)).unwrap();
            if index == 3 {
                let input = crate::semantic::input(&wire);
                assert_eq!(input["objective"], draft);
                assert_eq!(input["includes"], serde_json::json!(expected));
                assert!(!wire.to_string().contains("FILE_BODY_CANARY"));
                assert!(!wire.to_string().contains("NESTED_BODY_CANARY"));
            }
            let answer = match index {
                0 => serde_json::json!("ok"),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"synthetic child","profile":null}]}})
                }
                2 => serde_json::json!("synthetic summary"),
                _ => serde_json::json!("Directory listed."),
            };
            send_response(&mut stream, index, answer);
        }
    });
    check_profile(&workspace, &state);
    let shell = r#"printf 'SHELL_PID:%s\n' "$$"; "$ARANY_TEST_EXE" --screen-reader --state-dir "$ARANY_TEST_STATE" --workspace "$ARANY_TEST_WORKSPACE" --resume "$ARANY_TEST_SESSION" --provider custom:local --model model-1"#;
    let mut command = crate::isolation::account_isolated_script(temp.path());
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", id.to_string())
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("TERM", "dumb")
        .env("SHELL", "/bin/sh")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().unwrap());
    let mut input = child.child().stdin.take().unwrap();
    let mut output = child.child().stdout.take().unwrap();
    let flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Input:\r\n",
    );
    let shell_pid = super::active_terminal::transcript_field(
        &String::from_utf8_lossy(&transcript),
        "SHELL_PID:",
    )
    .parse::<u32>()
    .unwrap();
    let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
    let submitted_at = transcript.len();
    write!(input, "{draft}\r").unwrap();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        submitted_at,
        b"Directory listed.",
    );
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        submitted_at,
        b"Input:\r\n",
    );
    assert!(
        crate::process::pty_text_matches(&transcript, format!("You:\n  {draft}\n").as_bytes())
            .next()
            .is_some()
    );
    input.write_all(b"/exit\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.child().try_wait().unwrap().is_none() {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "directory mention exit: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    assert!(child.take().wait().unwrap().success());
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
fn local_registry_has_twenty_three_commands_and_refuses_arguments_without_state_access() {
    use arany_cli::terminal::{
        CommandParseError, InteractiveCommand, Submission, command_completions, parse_submission,
    };
    let names = command_completions("/");
    assert_eq!(names.len(), 23);
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
    let mut command =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", saved_id.to_string())
        .env("OPENAI_API_KEY", "synthetic-test-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("new-conversation PTY"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Input:\r\n",
    );
    let shell_pid = super::active_terminal::transcript_field(
        &String::from_utf8_lossy(&transcript),
        "SHELL_PID:",
    )
    .parse::<u32>()
    .expect("shell PID");
    let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
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
    let settings_at = transcript.len();
    input
        .write_all(b"/settings\r")
        .expect("linear settings choices");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        settings_at,
        format!("Settings: type NUMBER VALUE to change a row or NUMBER default to reset it; empty Enter saves; q cancels\r\n\
          Choice 1: Slash menu: tabs (default) (tabs|combined)\r\n\
          Choice 2: Chat view: fullscreen (default) (fullscreen|classic)\r\n\
          Choice 3: Provider calls per task: unlimited (default) (number or unlimited)\r\n\
          Choice 4: Tokens per task: unlimited (default) (number or unlimited)\r\n\
          Choice 5: Active time per task: unlimited (default) (duration or unlimited)\r\n\
          Choice 6: Default command timeout: 2 min (default) (duration, e.g. 30s)\r\n\
          Choice 7: Maximum command timeout: 10 min (default) (duration, e.g. 5m)\r\n\
          Choice 8: Build memory: 4.0 GB (default) (size, e.g. 512MB)\r\n\
          Choice 9: Build processes/threads: 256 (default) (number)\r\n\
          Choice 10: Build writable project: 1.0 GB (default) (size, e.g. 1GB)\r\n\
          Choice 11: Build cache: unlimited (default) · 0 B used (size or unlimited, e.g. 8GB)\r\n\
          Choice 12: Copy on select: on (default) (on|off)\r\n\
          Choice 13: Newline shortcut: Shift+Enter (default) (shift+enter|alt+enter|ctrl+enter)\r\n\
          Choice 14: Build jobs: {build_jobs} (default) (automatic or 1..cores)\r\n\
          Choice 15: Build CPU cores: {build_cpu} (default) (automatic or 1..cores; Linux quota)\r\n\
          Choice 16: Context window: 100% (default) (percent or tokens, e.g. 80% or 500k)\r\n\
          Notice: Modified newline shortcuts apply only to visual sessions; use backslash then Enter here\r\n\
          Input:\r\n").as_bytes(),
    );
    for rejected in [
        &b"combined\r"[..],
        b"12 combined\r",
        b"1 classic\r",
        b"6 30000\r",
        b"8 512\r",
        b"5 2h\r",
    ] {
        let invalid_at = transcript.len();
        input
            .write_all(rejected)
            .expect("reject invalid NUMBER VALUE");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            invalid_at,
            b"Notice: Type NUMBER VALUE from the list; q cancels\r\nInput:\r\n",
        );
    }
    for (value, row) in [
        (
            "5 5m",
            "Choice 5: Active time per task: 5 min (duration or unlimited)",
        ),
        (
            "5 unlimited",
            "Choice 5: Active time per task: unlimited (default) (duration or unlimited)",
        ),
        (
            "6 30s",
            "Choice 6: Default command timeout: 30 s (duration, e.g. 30s)",
        ),
        (
            "6 2min",
            "Choice 6: Default command timeout: 2 min (default) (duration, e.g. 30s)",
        ),
        (
            "8 512MB",
            "Choice 8: Build memory: 512.0 MB (size, e.g. 512MB)",
        ),
        (
            "8 4GB",
            "Choice 8: Build memory: 4.0 GB (default) (size, e.g. 512MB)",
        ),
        (
            "11 0B",
            "Choice 11: Build cache: 0 B · 0 B used (size or unlimited, e.g. 8GB)",
        ),
        (
            "11 8GB",
            "Choice 11: Build cache: 8.0 GB · 0 B used (size or unlimited, e.g. 8GB)",
        ),
        (
            "11 unlimited",
            "Choice 11: Build cache: unlimited (default) · 0 B used (size or unlimited, e.g. 8GB)",
        ),
    ] {
        let changed_at = transcript.len();
        input
            .write_all(format!("{value}\r").as_bytes())
            .expect("human-unit setting");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            changed_at,
            format!("{row}\r\n").as_bytes(),
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            changed_at,
            b"Input:\r\n",
        );
    }
    let selected_at = transcript.len();
    input
        .write_all(b"1 combined\r")
        .expect("linear combined selection");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        selected_at,
        b"Choice 1: Slash menu: combined (tabs|combined)\r\n",
    );
    let selected_at = transcript.len();
    input
        .write_all(b"\r")
        .expect("linear combined selection saved by empty Enter");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        selected_at,
        "Notice: Settings saved · calls unlimited · active unlimited · command max 10 min · visual sessions\r\nInput:\r\n".as_bytes(),
    );
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
        let from = transcript.len();
        input
            .write_all(format!("{command}\r").as_bytes())
            .expect("current selection");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
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
        let from = transcript.len();
        input
            .write_all(format!("{command}\r").as_bytes())
            .expect("busy choice");
        wait_for_after(&mut output, &mut input, &mut transcript, &mut answered, from,
            b"Notice: Error: Session has an active operation; selection unchanged. Try again when it finishes\r\nInput:\r\n");
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
        let from = transcript.len();
        input
            .write_all(format!("/model claude-sonnet-5 {effort}\r").as_bytes())
            .expect("explicit post-unlock choice");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
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
        let from = transcript.len();
        input
            .write_all(format!("/resume {current_id}\r").as_bytes())
            .expect("resume original");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Input:\r\n",
        );
        let from = transcript.len();
        input
            .write_all(format!("/rename {title}\r").as_bytes())
            .expect("source title");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
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
        let from = transcript.len();
        input
            .write_all(format!("{command}\r").as_bytes())
            .expect("new conversation command");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Input:\r\n",
        );
        assert!(String::from_utf8_lossy(&transcript[from..]).contains("New conversation"));
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
        let from = transcript.len();
        input
            .write_all(b"/provider\r")
            .expect("inspect preserved Provider");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Notice: Provider: anthropic\r\nInput:\r\n",
        );
    }
    let storage_at = transcript.len();
    input
        .write_all(b"/storage\r")
        .expect("local storage report");
    wait_for_after(&mut output, &mut input, &mut transcript, &mut answered, storage_at,
        b"History, credentials and live/uncertain Guard storage have no automatic-removal hint.\r\nInput:\r\n");
    let storage_text = String::from_utf8_lossy(&transcript[storage_at..]);
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
    let usage_at = transcript.len();
    input
        .write_all(b"/cache clear now\r")
        .expect("malformed cache clear");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        usage_at,
        b"Notice: Error: Usage: /cache clear [all]\r\n",
    );
    let busy_at = transcript.len();
    input
        .write_all(b"/cache clear\r")
        .expect("busy cache clear");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        busy_at,
        b"Notice: Build cache is in use; retry after its command finishes\r\nInput:\r\n",
    );
    assert!(current_cache.exists());
    assert!(other_cache.exists());
    rustix::fs::flock(&held_cache, rustix::fs::FlockOperation::Unlock)
        .expect("release cache lease");
    let clear_workspace_at = transcript.len();
    input
        .write_all(b"/cache clear\r")
        .expect("clear current Workspace cache");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_workspace_at,
        b"Notice: Cleared this Workspace's private build cache\r\nInput:\r\n",
    );
    assert!(!current_cache.exists());
    assert!(other_cache.exists());
    let clear_all_at = transcript.len();
    input
        .write_all(b"/cache clear all\r")
        .expect("clear all private build caches");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_all_at,
        b"Notice: Cleared all private build caches\r\nInput:\r\n",
    );
    assert!(!other_cache.exists());
    let (planted_cache, planted_lock) = seed_empty_build_cache(&state, &workspace);
    std::fs::set_permissions(&planted_lock, std::fs::Permissions::from_mode(0o644))
        .expect("non-private cache identity lock");
    let failed_clear_at = transcript.len();
    input
        .write_all(b"/cache clear\r")
        .expect("cache clear over non-private storage");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        failed_clear_at,
        b"Notice: Error: private build cache could not be cleared\r\nInput:\r\n",
    );
    assert!(planted_cache.exists());
    input.write_all(b"/exit\r").expect("exit");
    let result = super::loopback::wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    assert_eq!(answered.cursor_queries, 0);
    let text = String::from_utf8(transcript).expect("linear transcript UTF-8");
    assert!(text.contains("Run npx --yes skills@1.7.0 experimental_install in this project: .agents/skills is missing and skills-lock.json exists."));
    assert!(!text.contains('\u{1b}'));
    assert_eq!(text.matches("Notice: Error:").count(), 7);
    assert_eq!(
        super::active_terminal::transcript_field(&text, "TTY_BEFORE:"),
        super::active_terminal::transcript_field(&text, "TTY_AFTER:"),
    );
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
    let mut command =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("OPENAI_API_KEY", "synthetic-test-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("attached PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"SHELL_PID:",
    );
    let shell_line = transcript
        .windows(b"SHELL_PID:".len())
        .position(|part| part == b"SHELL_PID:")
        .expect("shell marker")
        + b"SHELL_PID:".len();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        shell_line,
        b"\n",
    );
    let shell_pid = String::from_utf8_lossy(&transcript)
        .lines()
        .find_map(|line| line.strip_prefix("SHELL_PID:"))
        .expect("shell PID")
        .trim_end_matches('\r')
        .parse::<u32>()
        .expect("numeric shell PID");
    let product_pid = product_child_of(shell_pid);
    let _product_guard = ProductGuard::new(product_pid, state.clone());
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Ask Arany",
    );
    let visible = String::from_utf8_lossy(&transcript);
    let before = visible
        .lines()
        .find_map(|line| line.strip_prefix("TTY_BEFORE:"))
        .expect("initial terminal settings")
        .trim_end_matches('\r')
        .to_owned();
    let ready = transcript.len();
    input.write_all(b"/help\r").expect("open help");
    wait_for_frame(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (ready, 16, 8),
        |screen| {
            screen.iter().any(|row| row.contains("Commands"))
                && screen.iter().any(|row| row.contains("Up/Dn Enter Esc"))
        },
    );
    assert_ne!(tty_settings(product_pid), before, "help owns raw mode");
    kill_process(product_pid, Signal::TERM).expect("terminate open help");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if child.child().try_wait().expect("process status").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "help exit: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let status = child.take().wait().expect("process exit");
    assert!(!status.success(), "SIGTERM exits unsuccessfully");
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    let text = String::from_utf8_lossy(&transcript);
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

fn inspect_storage_pager(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
) {
    let from = transcript.len();
    input.write_all(b"/storage\r").expect("open storage pager");
    wait_for_screen(output, input, transcript, answered, (from, 40, 24), "Esc |");
    assert!(screen_contains(transcript, 40, 24, "Arany storage:"));
    let from = transcript.len();
    input.write_all(b"\x1b[F").expect("last storage page");
    wait_for_screen(
        output,
        input,
        transcript,
        answered,
        (from, 40, 24),
        "History, credentials",
    );
    let from = transcript.len();
    input.write_all(b"\x1b").expect("close storage pager");
    wait_for_caret(output, input, transcript, answered, from, 3);
}

fn inspect_file_suggestions(
    workspace: &Path,
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
) {
    let start = transcript.len();
    input.write_all(b"@").unwrap();
    wait_for_screen(
        output,
        input,
        transcript,
        answered,
        (start, 40, 24),
        "visible-note.txt",
    );
    for hidden in ["ignored.cache", "private-note.txt", ".ENV"] {
        assert!(!screen_contains(transcript, 40, 24, hidden));
        assert!(!String::from_utf8_lossy(&transcript[start..]).contains(hidden));
    }
    std::fs::write(workspace.join(".gitignore"), b"*").unwrap();
    let refining = transcript.len();
    input.write_all(b"visible-no").unwrap();
    let refined_column = 14;
    wait_for_caret(
        output,
        input,
        transcript,
        answered,
        refining,
        refined_column,
    );
    assert!(screen_contains(transcript, 40, 24, "visible-note.txt"));
    let closed = transcript.len();
    input.write_all(b"\x1b").unwrap();
    wait_for_caret(output, input, transcript, answered, closed, refined_column);
    let cleared = transcript.len();
    input.write_all(b"\x03").unwrap();
    wait_for_caret(output, input, transcript, answered, cleared, 3);
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
    let config = serde_json::json!({"version":1,"workspace_paths":["."],"write":false,
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
    let mut command =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", saved_id.to_string())
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("attached PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Ask Arany",
    );
    let shell_pid = String::from_utf8_lossy(&transcript)
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
        .expect("shell PID")
        .parse::<u32>()
        .expect("numeric shell PID");
    let _product_guard = ProductGuard::new(product_child_of(shell_pid), state.clone());
    let tty_before = super::active_terminal::transcript_field(
        &String::from_utf8_lossy(&transcript),
        "TTY_BEFORE:",
    )
    .to_owned();
    inspect_file_suggestions(
        &workspace,
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
    );
    inspect_storage_pager(&mut output, &mut input, &mut transcript, &mut answered);
    let paste_at = transcript.len();
    input
        .write_all(b"\x1b[200~first\r\nsecond\x1b[201~")
        .expect("bracketed multiline paste");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        paste_at,
        b";3Hfirst",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        paste_at,
        9,
    );
    assert!(
        !String::from_utf8_lossy(&transcript[paste_at..]).contains("error:"),
        "paste must edit the draft without submitting either line"
    );
    let clipboard_at = transcript.len();
    input.write_all(b"\x16").expect("explicit clipboard paste");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clipboard_at,
        b"unavailable;",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clipboard_at,
        9,
    );
    let continued_at = transcript.len();
    input
        .write_all(b"Z")
        .expect("continue retained paste draft");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        continued_at,
        10,
    );
    let clear_at = transcript.len();
    input.write_all(b"\x03").expect("clear pasted draft");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_at,
        3,
    );
    fresh_capture(&mut transcript, &mut answered);
    let tabs_at = transcript.len();
    input.write_all(b"/").expect("open default Commands tab");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        tabs_at,
        b"[Commands]",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        tabs_at,
        4,
    );
    let skill_at = transcript.len();
    input.write_all(b"\x1b[C").expect("switch to Skills");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        skill_at,
        b"> (skill) /ask-matt",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        skill_at,
        4,
    );
    let commands_at = transcript.len();
    input.write_all(b"\x1b[D").expect("return to Commands");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        commands_at,
        4,
    );
    let skill_at = transcript.len();
    input
        .write_all(b"ask")
        .expect("filter commands and Skills from the Commands tab");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        skill_at,
        b"> (skill) /ask-matt",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        skill_at,
        7,
    );
    let skill_accept_at = transcript.len();
    input.write_all(b"\t").expect("choose Skill as a draft");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        skill_accept_at,
        13,
    );
    let skill_clear_at = transcript.len();
    input.write_all(b"\x03").expect("clear unsent Skill draft");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        skill_clear_at,
        3,
    );
    let invoke_at = transcript.len();
    input.write_all(b"/ask").expect("filter Skill prefix again");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        invoke_at,
        7,
    );
    let invoke_at = transcript.len();
    input.write_all(b"\r").expect("invoke the focused Skill");
    wait_for_screen(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (invoke_at, 40, 24),
        "Choose a model",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        invoke_at,
        12,
    );
    assert!(
        crate::screen_rows(&transcript, 40, 24)
            .iter()
            .any(|row| row.trim() == "> /ask-matt"),
        "model rejection must retain the complete Skill invocation"
    );
    let clear_at = transcript.len();
    input
        .write_all(b"\x03")
        .expect("clear retained Skill invocation");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_at,
        3,
    );
    let settings_at = transcript.len();
    input
        .write_all(b"/settings\r")
        .expect("open display settings");
    wait_armed(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        settings_at,
    );
    let saved_at = transcript.len();
    input.write_all(b"\x1b[C\r").expect("save combined menu");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        saved_at,
        b"combined",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        saved_at,
        3,
    );
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
    fresh_capture(&mut transcript, &mut answered);
    let menu_at = transcript.len();
    input.write_all(b"/resum").expect("single command prefix");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        menu_at,
        b"> Cmd /resume",
    );
    let picker_at = transcript.len();
    input
        .write_all(b"\r")
        .expect("Enter invokes the focused resume entry");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        picker_at,
        b"Resume",
    );
    wait_armed(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        picker_at,
    );
    let closed_at = transcript.len();
    input.write_all(b"\x1b").expect("dismiss resume picker");
    wait_for_screen(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (closed_at, 40, 24),
        "closed",
    );
    let menu_at = transcript.len();
    input.write_all(b"/s").expect("ambiguous command prefix");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        menu_at,
        5,
    );
    input.write_all(b"\x1b[B").expect("focus status choice");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"> Cmd /status",
    );
    let status_at = transcript.len();
    input.write_all(b"\r").expect("invoke focused status");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        status_at,
        b"Tokens",
    );
    wait_for_frame(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (status_at, 40, 24),
        |screen| {
            screen
                .iter()
                .any(|row| row.contains("Tokens used: no calls yet"))
                && screen.iter().any(|row| row.contains("/usage for details"))
                && screen.iter().any(|row| row.contains("Esc close"))
        },
    );
    let closed = transcript.len();
    input.write_all(b"\x1b").expect("close status report");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        closed,
        b"Ask Arany",
    );
    let newline_at = transcript.len();
    input
        .write_all(b"alpha\\\r")
        .expect("portable backslash-Enter continues the chat draft");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        newline_at,
        3,
    );
    let newline_at = transcript.len();
    input
        .write_all(b"beta\x1b[13;2u")
        .expect("distinct Shift+Enter adds another newline");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        newline_at,
        3,
    );
    input.write_all(b"\x03").expect("clear multiline draft");
    // The viewport shrinks with the draft, so the cleared composer is read from the settled screen.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows = crate::session_picker::screen_rows(&transcript, 40, 24);
        if rows.iter().any(|row| row.starts_with('>'))
            && !rows.iter().any(|row| row.contains("alpha"))
        {
            break;
        }
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "cleared draft: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let model_menu_at = transcript.len();
    input.write_all(b"/s").expect("ambiguous Session prefix");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        model_menu_at,
        5,
    );
    input.write_all(b"\x1b[B").expect("focus status choice");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        model_menu_at,
        b"> Cmd /status",
    );
    let closed_at = transcript.len();
    input.write_all(b"\x1b").expect("dismiss completion menu");
    // The open menu also leaves the caret in column 5; only a decoded composer without the
    // menu proves Escape was handled, wherever the content-sized viewport now sits.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !crate::composer_caret(&transcript, closed_at, 40, 24).is_some_and(|(column, screen)| {
        column == 5
            && !screen.iter().any(|row| row.contains("Cmd /status"))
            && screen.iter().any(|row| row.starts_with("> /s"))
    }) {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "closed completion menu: {}",
            tail(&transcript[closed_at..])
        );
        thread::yield_now();
    }
    input
        .write_all(b"\x7f\x7f/model staged-model\r")
        .expect("correct retained prefix with a literal model selection");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        closed_at,
        b"Model:",
    );
    transcript.clear();
    answered = crate::session_picker::PtyResponses::default();
    let unknown_at = transcript.len();
    input.write_all(b"/provder\r").expect("unknown command");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Unknown",
    );
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"r?",
    );
    assert!(
        String::from_utf8_lossy(&transcript[unknown_at..]).contains("error:"),
        "local command rejection needs a textual error heading: {}",
        tail(&transcript)
    );
    input
        .write_all(b"\x7f\x7f\x7f\x7f\x7f\x7f\x7fprov")
        .expect("correct retained command");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"> Cmd /provider",
    );
    input
        .write_all(b"\topenai\r")
        .expect("Tab completion and Provider argument");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Provider:",
    );
    input
        .write_all(b"/agents bad\r")
        .expect("invalid collaboration argument");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"policy",
    );
    input
        .write_all(b"\x7f\x7f\x7fteam 2\r")
        .expect("correct retained argument");
    let rename_at = transcript.len();
    input
        .write_all(format!("/rename {}Z\x1b[D\r", "A".repeat(128)).as_bytes())
        .expect("oversized rename with a mid-draft caret");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        rename_at,
        b"long;",
    );
    let notice_at = rename_at
        + transcript[rename_at..]
            .windows(b"long;".len())
            .position(|part| part == b"long;")
            .expect("rename rejection")
        + b"long;".len();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        notice_at,
        b"\x1b[?25h",
    );
    let corrected_at = transcript.len();
    input
        .write_all(b"\x7f\r")
        .expect("correct retained rename at its original caret");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        corrected_at,
        b"Renamed",
    );
    transcript.clear();
    answered = crate::session_picker::PtyResponses::default();
    input
        .write_all(b"/exit\r")
        .expect("exit after corrected rename");
    let restart_at = transcript.len();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        restart_at,
        b"SETTINGS_RESTART",
    );
    let restart_at = restart_at
        + transcript[restart_at..]
            .windows(b"SETTINGS_RESTART".len())
            .position(|part| part == b"SETTINGS_RESTART")
            .unwrap()
        + b"SETTINGS_RESTART".len();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        restart_at,
        b"Ask Arany",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        restart_at,
        3,
    );
    let _restarted_product = ProductGuard::new(product_child_of(shell_pid), state.clone());
    let combined_at = transcript.len();
    input
        .write_all(b"/")
        .expect("remembered combined menu in fresh process");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        combined_at,
        4,
    );
    let selected_at = transcript.len();
    input
        .write_all(b"rev\t")
        .expect("remembered combined mode exposes Skills without switching tabs");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        selected_at,
        11,
    );
    let clear_at = transcript.len();
    input.write_all(b"\x03").expect("clear menu without a Run");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_at,
        3,
    );
    let pending = state.join("ui-preferences.pending");
    let outside = temp.path().join("outside-preference-canary");
    std::fs::write(&outside, b"unchanged").unwrap();
    std::os::unix::fs::symlink(&outside, &pending).unwrap();
    let rejected_at = transcript.len();
    input
        .write_all(b"/settings\r")
        .expect("open settings for the unsafe replacement");
    wait_armed(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        rejected_at,
    );
    input
        .write_all(b"\x1b[C\r")
        .expect("unsafe replacement must retain current display");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        rejected_at,
        3,
    );
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
        String::from_utf8_lossy(&transcript[rejected_at..]).contains("error"),
        "unsafe save gives visible failure feedback: {}",
        String::from_utf8_lossy(&transcript[rejected_at..])
            .chars()
            .take(4096)
            .flat_map(char::escape_default)
            .collect::<String>()
    );
    let unchanged_at = transcript.len();
    input
        .write_all(b"/rev\t")
        .expect("failed save leaves combined display");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        unchanged_at,
        11,
    );
    let clear_at = transcript.len();
    input.write_all(b"\x03").expect("clear retained display");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_at,
        3,
    );
    fresh_capture(&mut transcript, &mut answered);
    std::fs::remove_file(pending).unwrap();
    let tabs_at = transcript.len();
    input
        .write_all(b"/settings\r")
        .expect("open settings for tabs");
    wait_armed(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        tabs_at,
    );
    input
        .write_all(format!("\x1b[C{}\x1b[C\x1b[C\r", "\x1b[B".repeat(12)).as_bytes())
        .expect("save tabs and Ctrl+Enter in the settings window");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        tabs_at,
        3,
    );
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
    let shortcut_at = transcript.len();
    input
        .write_all(b"saved\x1b[13;5u")
        .expect("use saved Ctrl+Enter immediately");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        shortcut_at,
        3,
    );
    let clear_at = transcript.len();
    input.write_all(b"\x03").expect("clear newline draft");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_at,
        3,
    );
    for (view, pinned) in [("classic", false), ("fullscreen", true), ("classic", false)] {
        fresh_capture(&mut transcript, &mut answered);
        let chosen_at = transcript.len();
        input
            .write_all(b"/settings\r")
            .expect("open settings for the chat view");
        wait_armed(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            chosen_at,
        );
        input
            .write_all(b"\x1b[B\x1b[C\r")
            .expect("chat view selected with Down and Right");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            chosen_at,
            b"saved",
        );
        wait_for_caret(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            chosen_at,
            3,
        );
        assert_eq!(
            StateRoot::open_existing(&state)
                .unwrap()
                .record(RecordKind::UiPreferences)
                .read()
                .unwrap()
                .unwrap(),
            record(view)
        );
        let frames = &transcript[chosen_at..];
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
    fresh_capture(&mut transcript, &mut answered);
    input
        .write_all(b"/exit\r")
        .expect("exit pinned classic process");
    let view_at = transcript.len();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        view_at,
        b"VIEW_RESTART",
    );
    let view_at = view_at
        + transcript[view_at..]
            .windows(b"VIEW_RESTART".len())
            .position(|part| part == b"VIEW_RESTART")
            .unwrap()
        + b"VIEW_RESTART".len();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        view_at,
        b"Ask Arany",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        view_at,
        3,
    );
    assert!(
        !transcript[view_at..]
            .windows(8)
            .any(|part| part == b"\x1b[?1049h"),
        "saved classic starts on the primary screen"
    );
    let _viewed_product = ProductGuard::new(product_child_of(shell_pid), state.clone());
    let shortcut_at = transcript.len();
    input
        .write_all(b"saved\x1b[13;5u")
        .expect("use saved Ctrl+Enter after a fresh process restores it");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        shortcut_at,
        3,
    );
    let clear_at = transcript.len();
    input.write_all(b"\x03").expect("clear newline draft");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_at,
        3,
    );
    fresh_capture(&mut transcript, &mut answered);
    let switched_at = transcript.len();
    input
        .write_all(b"/settings\r")
        .expect("open settings for the live switch");
    wait_armed(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        switched_at,
    );
    input
        .write_all(b"\x1b[B\x1b[C\r")
        .expect("saved fullscreen switches the live view");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        switched_at,
        b"\x1b[?1049h",
    );
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        switched_at,
        3,
    );
    assert_eq!(
        StateRoot::open_existing(&state)
            .unwrap()
            .record(RecordKind::UiPreferences)
            .read()
            .unwrap()
            .unwrap(),
        record("fullscreen")
    );
    inspect_storage_pager(&mut output, &mut input, &mut transcript, &mut answered);
    let draft_at = transcript.len();
    input
        .write_all(b"abc")
        .expect("draft after the live switch");
    wait_for_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        draft_at,
        6,
    );
    input
        .write_all(b"\x7f\x7f\x7f/exit\r")
        .expect("exit fullscreen process");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if child.child().try_wait().expect("process status").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "PTY exit: {}", tail(&transcript));
        thread::yield_now();
    }
    let status = child.take().wait().expect("process exit");
    assert!(status.success(), "product failed: {}", tail(&transcript));
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    let text = String::from_utf8_lossy(&transcript);
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("TTY marker")
            .to_owned()
    };
    assert_eq!(tty_before, marker("TTY_AFTER:"));
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
    let mut command =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", saved_id.to_string())
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(debug_assertions)]
    command
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false");
    let mut child = ChildGuard::new(command.spawn().expect("screen-reader PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Input:\r\n",
    );
    input.write_all(b"/provder\r").expect("unknown command");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Notice: Error: Unknown command; did you mean /provider?\r\nInput:\r\n",
    );
    let clipboard_at = transcript.len();
    input
        .write_all(b"/paste\r")
        .expect("accessible clipboard command");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clipboard_at,
        b"Notice: Error: local clipboard unavailable; use terminal paste; draft unchanged\r\n",
    );
    input
        .write_all(b"/provider openai\r")
        .expect("fresh Provider command");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Notice: Provider: openai; set /model\r\nInput:\r\n",
    );
    {
        let model_at = transcript.len();
        input
            .write_all(b"/model gpt-5.4\r")
            .expect("stage a native model");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            model_at,
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
            let from = transcript.len();
            input
                .write_all(format!("{objective}\r").as_bytes())
                .unwrap();
            wait_for_after(&mut output, &mut input, &mut transcript, &mut answered, from, format!("Notice: Error: {error}; check the file inputs and enter a new line\r\nInput:\r\n").as_bytes());
            let from = transcript.len();
            input.write_all(b"/provider\r").unwrap();
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Notice: Provider: openai\r\nInput:\r\n",
            );
        }
    }
    #[cfg(debug_assertions)]
    {
        let effort_at = transcript.len();
        input
            .write_all(b"/model gpt-5.4 low\r")
            .expect("select reviewed effort without inference");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            effort_at,
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
            let selection_at = transcript.len();
            input
                .write_all(b"/provider chatgpt\r")
                .expect("reject an unavailable account selection");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                selection_at,
                if malformed {
                    b"Notice: Error: saved ChatGPT setup is incompatible or damaged; use /setup or arany --setup to reset it and sign in again\r\nInput:\r\n"
                } else {
                    b"Notice: Error: no selected ChatGPT account; use /setup to connect\r\nInput:\r\n"
                },
            );
            let provider_at = transcript.len();
            input
                .write_all(b"/provider\r")
                .expect("inspect the unchanged Provider");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                provider_at,
                b"Notice: Provider: openai\r\nInput:\r\n",
            );
        }
    }
    {
        let from = transcript.len();
        input
            .write_all(b"/provider custom:synthetic\r")
            .expect("stage an exact custom selection");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Notice: Provider: custom:synthetic; set /model\r\nInput:\r\n",
        );
    }
    let profile_path = state.join("provider-profiles.json");
    let mut profile = serde_json::json!({
        "version": 2,
        "profiles": [{
            "name": "synthetic", "protocol": "openai-responses",
            "endpoint": "http://127.0.0.1:9/responses", "model": "different-model",
            "credential_env": "ARANY_PROVIDER_SYNTHETIC", "outcome_encoding": "json_schema",
            "privacy": "user_authorized", "max_output_tokens": 4096,
            "capability_evidence_version": 2, "efforts": ["low", "high"]
        }]
    });
    let mismatched = serde_json::to_vec(&profile).expect("mismatched profile JSON");
    profile["profiles"][0]["model"] = "synthetic-custom-model".into();
    let matched = serde_json::to_vec(&profile).expect("matching profile JSON");
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
            let rejected_at = transcript.len();
            input
                .write_all(format!("{command}\r").as_bytes())
                .expect("reject unadmitted custom effort");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                rejected_at,
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
        let accepted_at = transcript.len();
        input
            .write_all(format!("{command}\r").as_bytes())
            .expect("accept an explicit local selection");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            accepted_at,
            format!("Notice: {feedback}\r\nInput:\r\n").as_bytes(),
        );
    }
    #[cfg(debug_assertions)]
    for (command, feedback) in [
        ("/model gpt-5.4", "Model: gpt-5.4 · low"),
        ("/model gpt-5.4 low", "Model: gpt-5.4 · low"),
    ] {
        let accepted_at = transcript.len();
        input
            .write_all(format!("{command}\r").as_bytes())
            .expect("restore native selection explicitly");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            accepted_at,
            format!("Notice: {feedback}\r\nInput:\r\n").as_bytes(),
        );
    }
    let help_at = transcript.len();
    input.write_all(b"/help\r").expect("screen-reader help");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        help_at,
        b"Command: /exit; Exit Session; during Run: view-only\r\nInput:\r\n",
    );
    input
        .write_all(b"/agents bad\r")
        .expect("invalid collaboration command");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Notice: Error: invalid collaboration policy\r\nInput:\r\n",
    );
    input
        .write_all(b"/agents team 2\r")
        .expect("fresh collaboration command");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Notice: Next-Run collaboration: team \xc2\xb7 up to 2 children\r\nInput:\r\n",
    );
    let missing_at = transcript.len();
    input
        .write_all(format!("/resume {}\r", arany::SessionId::new()).as_bytes())
        .expect("resume a syntactically valid missing Session");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        missing_at,
        b"Notice: Error: Session not found\r\nInput:\r\n",
    );
    let fork_at = transcript.len();
    input.write_all(b"/fork\r").expect("reject an empty fork");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        fork_at,
        b"Notice: Error: Session has no committed Run boundary to fork\r\nInput:\r\n",
    );
    let rename_at = transcript.len();
    input
        .write_all(format!("/rename {}\r", "é".repeat(65)).as_bytes())
        .expect("reject a title over the UTF-8 byte bound");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        rename_at,
        b"Notice: Error: Title is empty or too long; use a shorter title\r\nInput:\r\n",
    );
    let state_guard = StateModeGuard {
        path: state.clone(),
        mode: std::fs::metadata(&state).expect("State").permissions(),
    };
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o000))
        .expect("make State unavailable mid-Session");
    let unavailable_at = transcript.len();
    input
        .write_all(format!("/resume {saved_id}\r").as_bytes())
        .expect("resume with unavailable State");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        unavailable_at,
        b"Notice: Error: state directory unavailable or unsafe\r\nInput:\r\n",
    );
    std::fs::set_permissions(&state, state_guard.mode.clone()).expect("restore State");
    let corrected_at = transcript.len();
    input
        .write_all(b"/rename Corrected title\r")
        .expect("fresh rename after rejected physical line");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        corrected_at,
        b"Notice: Renamed Session: Corrected title\r\nInput:\r\n",
    );
    input
        .write_all(b"/exit\r")
        .expect("exit after fresh rename");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if child.child().try_wait().expect("process status").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "PTY exit: {}", tail(&transcript));
        thread::yield_now();
    }
    let status = child.take().wait().expect("process exit");
    assert!(status.success(), "product failed: {}", tail(&transcript));
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    let text = String::from_utf8_lossy(&transcript);
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains("synthetic-malformed-account-canary"));
    assert!(!text.contains("synthetic-malformed-profile-canary"));
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("TTY marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
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
