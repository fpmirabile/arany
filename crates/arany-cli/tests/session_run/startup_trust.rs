use super::*;
#[cfg(target_os = "linux")]
use std::{
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    process::Stdio,
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

#[test]
#[cfg(target_os = "linux")]
fn hostile_repository_startup_does_not_activate_git_or_disclose_omitted_inputs() {
    const OMITTED: [&[u8]; 5] = [
        b"PARENT_INSTRUCTION_CANARY",
        b"FALLBACK_INSTRUCTION_CANARY",
        b"NESTED_INSTRUCTION_CANARY",
        b"OMITTED_FILE_CANARY",
        b"GIT_CONFIG_CANARY",
    ];

    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    std::fs::write(temp.path().join("AGENTS.md"), OMITTED[0]).expect("parent guidance");
    std::fs::write(workspace.join("AGENTS.md"), b"EXPECTED_ROOT_GUIDANCE").expect("root guidance");
    std::fs::write(workspace.join("CLAUDE.md"), OMITTED[1]).expect("fallback guidance");
    let nested = workspace.join("nested");
    std::fs::create_dir(&nested).expect("nested directory");
    std::fs::write(nested.join("AGENTS.md"), OMITTED[2]).expect("nested guidance");
    std::fs::write(workspace.join("private.txt"), OMITTED[3]).expect("omitted file");

    let marker = temp.path().join("git-activated");
    let script = format!("#!/bin/sh\nprintf activated > '{}'\n", marker.display());
    let git = workspace.join(".git");
    let hooks = git.join("hooks");
    std::fs::create_dir_all(&hooks).expect("synthetic Git directory");
    std::fs::create_dir(git.join("objects")).expect("synthetic object directory");
    std::fs::create_dir_all(git.join("refs/heads")).expect("synthetic ref directory");
    std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").expect("synthetic HEAD");
    let fsmonitor = git.join("fsmonitor");
    std::fs::write(&fsmonitor, &script).expect("fsmonitor trap");
    std::fs::set_permissions(&fsmonitor, std::fs::Permissions::from_mode(0o700))
        .expect("executable fsmonitor trap");
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, &script).expect("hook trap");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700))
        .expect("executable hook trap");
    std::fs::write(
        git.join("config"),
        format!(
            "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n[arany]\n\tinjected = {}\n",
            fsmonitor.display(),
            hooks.display(),
            std::str::from_utf8(OMITTED[4]).expect("fixture UTF-8")
        ),
    )
    .expect("hostile Git config");

    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    loopback::write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..4 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "request {index} missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            };
            let request = loopback::read_request(&mut stream);
            for canary in OMITTED {
                assert!(
                    !request.windows(canary.len()).any(|part| part == canary),
                    "omitted input disclosed in request {index}"
                );
            }
            assert!(
                !request
                    .windows(b"test-key".len())
                    .any(|part| part == b"test-key"),
                "credential appeared in request body"
            );
            let body: serde_json::Value =
                serde_json::from_slice(&request).expect("bounded request JSON");
            assert_eq!(body["model"], "model-1");
            let input: serde_json::Value = crate::semantic::input(&body);
            if index == 3 {
                assert_eq!(
                    input["workspace_guidance"],
                    serde_json::json!([{"path": "AGENTS.md", "text": "EXPECTED_ROOT_GUIDANCE"}])
                );
                assert_eq!(body["input"][0]["role"], "user");
                assert_eq!(
                    body["input"][0]["content"],
                    serde_json::json!([{"type": "input_text", "text": "# AGENTS.md instructions for AGENTS.md\n\n<INSTRUCTIONS>\nEXPECTED_ROOT_GUIDANCE\n</INSTRUCTIONS>"}])
                );
                assert_eq!(input["objective"], "answer");
                assert_eq!(input["phase"], "root_plan");
            } else {
                assert!(input["workspace_guidance"].is_null());
            }
            let text = match index {
                0 => serde_json::json!("ok"),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                }
                2 => serde_json::json!(r#"{"summary":"synthetic summary"}"#),
                _ => {
                    serde_json::json!("verified answer")
                }
            };
            loopback::send_response(&mut stream, index, text);
        }
    });

    loopback::check_profile(&workspace, &state);
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "custom:local",
            "--model",
            "model-1",
            "--collaboration",
            "single",
            "answer",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = loopback::wait_product(command.spawn().expect("product exec process"));
    server.join().expect("loopback server completed");
    assert!(output.status.success(), "adversarial startup exit");
    assert_eq!(output.stdout, b"Answer:\n  verified answer\n");
    let receipt = std::str::from_utf8(&output.stderr).expect("receipt UTF-8");
    let lines: Vec<_> = receipt.lines().collect();
    assert_eq!(lines.len(), 4);
    let session_id = SessionId::from_str(lines[0].strip_prefix("Session: ").expect("Session line"))
        .expect("Session ID");
    let run_id =
        RunId::from_str(lines[1].strip_prefix("Run: ").expect("Run line")).expect("Run ID");
    assert_eq!(
        output.stderr,
        format!(
            "Session: {session_id}\nRun: {run_id}\nStatus: finished\nProvider: custom verified\n"
        )
        .as_bytes()
    );
    assert!(!marker.exists(), "repository Git trap activated");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime");
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("existing State"))
        .expect("read-only State");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("canonical Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].id, run_id);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.runs[0].objective, "answer");
    assert_eq!(
        view.runs[0].assistant_message.as_deref(),
        Some("verified answer")
    );
    runtime
        .block_on(store.close())
        .expect("close read-only State");

    let database = std::fs::read(state.join("events.sqlite3")).expect("database bytes");
    for artifact in [&output.stdout[..], &output.stderr, &database] {
        for canary in OMITTED {
            assert!(
                !artifact.windows(canary.len()).any(|part| part == canary),
                "omitted input appeared in a durable or output artifact"
            );
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn subdirectory_launch_follows_nested_instructions_root_first_and_replays_their_pins() {
    use sha2::{Digest, Sha256};
    const OMITTED: [&[u8]; 3] = [
        b"PARENT_INSTRUCTION_CANARY",
        b"SHADOWED_FALLBACK_CANARY",
        b"SIBLING_INSTRUCTION_CANARY",
    ];
    const FILES: [(&str, &str); 3] = [
        ("AGENTS.md", "ROOT_RULE"),
        ("crates/CLAUDE.md", "CRATES_RULE"),
        ("crates/cli/AGENTS.md", "CLI_RULE"),
    ];
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    let launch = workspace.join("crates/cli");
    std::fs::create_dir_all(&launch).expect("launch directory");
    std::fs::create_dir(workspace.join("sibling")).expect("sibling directory");
    for (path, text) in FILES {
        std::fs::write(workspace.join(path), text).expect("instruction file");
    }
    std::fs::write(temp.path().join("AGENTS.md"), OMITTED[0]).expect("parent guidance");
    std::fs::write(launch.join("CLAUDE.md"), OMITTED[1]).expect("shadowed fallback");
    std::fs::write(workspace.join("sibling/AGENTS.md"), OMITTED[2]).expect("sibling guidance");

    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    loopback::write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..4 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "request {index} missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            };
            let request = loopback::read_request(&mut stream);
            for canary in OMITTED {
                assert!(
                    !request.windows(canary.len()).any(|part| part == canary),
                    "omitted input disclosed in request {index}"
                );
            }
            let body: serde_json::Value =
                serde_json::from_slice(&request).expect("bounded request JSON");
            let text = match index {
                0 => serde_json::json!("ok"),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                }
                2 => serde_json::json!(r#"{"summary":"synthetic summary"}"#),
                _ => {
                    assert_eq!(body["input"][0]["role"], "user");
                    assert_eq!(
                        body["input"][0]["content"],
                        serde_json::Value::Array(
                            FILES
                                .iter()
                                .map(|(path, text)| serde_json::json!({"type": "input_text", "text": format!("# AGENTS.md instructions for {path}\n\n<INSTRUCTIONS>\n{text}\n</INSTRUCTIONS>")}))
                                .collect()
                        ),
                        "one block per file, root first"
                    );
                    assert_eq!(body["input"][1]["role"], "user");
                    serde_json::json!("nested answer")
                }
            };
            loopback::send_response(&mut stream, index, text);
        }
    });

    loopback::check_profile(&workspace, &state);
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&launch)
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "custom:local",
            "--model",
            "model-1",
            "--collaboration",
            "single",
            "answer",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = loopback::wait_product(command.spawn().expect("product exec process"));
    server.join().expect("loopback server completed");
    assert!(output.status.success(), "nested instruction exit");
    assert_eq!(output.stdout, b"Answer:\n  nested answer\n");
    let receipt = std::str::from_utf8(&output.stderr).expect("receipt UTF-8");
    let session_id = SessionId::from_str(
        receipt
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("Session: "))
            .expect("Session line"),
    )
    .expect("Session ID");

    // Replay reads only the pins; changed files neither reach nor alter the history.
    for (path, _) in FILES {
        std::fs::write(workspace.join(path), "CHANGED_AFTER_RUN").expect("changed instruction");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime");
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("existing State"))
        .expect("read-only State");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("canonical Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    let config = view.runs[0].config.as_ref().expect("pinned config");
    assert_eq!(config.launch_directory, "crates/cli");
    assert_eq!(
        config.instruction_sources,
        FILES.map(|(path, text)| arany::InstructionSource {
            file: Some(if path.ends_with("CLAUDE.md") {
                arany::InstructionFile::Claude
            } else {
                arany::InstructionFile::Agents
            }),
            directory: path
                .rsplit_once('/')
                .map_or("", |(directory, _)| directory)
                .into(),
            digest: Sha256::digest(text.as_bytes()).into(),
        })
    );
    assert_eq!(
        SessionView::replay(session_id, &events).expect("replay again"),
        Some(view)
    );
    runtime
        .block_on(store.close())
        .expect("close read-only State");
    let database = std::fs::read(state.join("events.sqlite3")).expect("database bytes");
    let followed: [&[u8]; 2] = [b"ROOT_RULE", b"CLI_RULE"];
    for canary in OMITTED.into_iter().chain(followed) {
        assert!(
            !database.windows(canary.len()).any(|part| part == canary),
            "instruction text stays out of the journal"
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn state_instructions_precede_project_rules_and_replay_only_digests() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    const OMITTED: &[u8] = b"PARENT_INSTRUCTION_CANARY";
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("AGENTS.md"), "ROOT_RULE").expect("root guidance");
    std::fs::write(temp.path().join("AGENTS.md"), OMITTED).expect("parent guidance");
    let other_state = temp.path().join("other-state");
    std::fs::create_dir(&other_state).expect("alternate State");
    std::fs::write(other_state.join("AGENTS.md"), "OTHER_STATE_CANARY").expect("alternate file");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    loopback::write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );
    let global = state.join("AGENTS.md");
    let write_global = |text: &str| {
        std::fs::write(&global, text).expect("user guidance");
        std::fs::set_permissions(&global, std::fs::Permissions::from_mode(0o600))
            .expect("user guidance mode");
    };
    write_global("USER_RULE");
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for (index, user_rule) in ["", "", "", "USER_RULE", "NEWER_USER_RULE"]
            .into_iter()
            .enumerate()
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "request {index} missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            };
            let request = loopback::read_request(&mut stream);
            for canary in [OMITTED, b"OTHER_STATE_CANARY"] {
                assert!(
                    !request.windows(canary.len()).any(|part| part == canary),
                    "omitted input disclosed in request {index}"
                );
            }
            if index < 3 {
                let text = match index {
                    0 => serde_json::json!("ok"),
                    1 => {
                        serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                    }
                    _ => serde_json::json!(r#"{"summary":"synthetic summary"}"#),
                };
                loopback::send_response(&mut stream, index, text);
                continue;
            }
            let body: serde_json::Value =
                serde_json::from_slice(&request).expect("bounded request JSON");
            assert_eq!(body["input"][0]["role"], "user");
            assert_eq!(
                body["input"][0]["content"],
                serde_json::json!([
                    {"type": "input_text", "text": format!("# AGENTS.md instructions for user AGENTS.md\n\n<INSTRUCTIONS>\n{user_rule}\n</INSTRUCTIONS>")},
                    {"type": "input_text", "text": "# AGENTS.md instructions for AGENTS.md\n\n<INSTRUCTIONS>\nROOT_RULE\n</INSTRUCTIONS>"},
                ]),
                "user block first, then the project block"
            );
            let rule = body["instructions"].as_str().expect("system prompt");
            assert!(rule.contains("repository files win over the user file"));
            loopback::send_response(&mut stream, index, serde_json::json!("ok"));
        }
    });

    loopback::check_profile(&workspace, &state);
    let run = |select: &dyn Fn(&mut Command)| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
            .current_dir(&workspace);
        command.arg("exec");
        select(&mut command);
        command
            .arg("--workspace")
            .arg(&workspace)
            .args([
                "--provider",
                "custom:local",
                "--model",
                "model-1",
                "--collaboration",
                "single",
                "answer",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = loopback::wait_product(command.spawn().expect("product exec process"));
        assert!(
            output.status.success(),
            "instruction exit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"Answer:\n  ok\n");
        SessionId::from_str(
            std::str::from_utf8(&output.stderr)
                .expect("receipt UTF-8")
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("Session: "))
                .expect("Session line"),
        )
        .expect("Session ID")
    };
    let first = run(&|command| {
        command.arg("--state-dir").arg(&state);
    });
    // Replay reads only the pin; a later Run snapshots the changed file again.
    write_global("NEWER_USER_RULE");
    let second = run(&|command| {
        command.env("ARANY_STATE_DIR", &state);
    });
    server.join().expect("loopback server completed");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime");
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("existing State"))
        .expect("read-only State");
    for (session_id, text) in [(first, "USER_RULE"), (second, "NEWER_USER_RULE")] {
        let events = runtime
            .block_on(store.load_session(session_id))
            .expect("canonical Events");
        let view = SessionView::replay(session_id, &events)
            .expect("strict replay")
            .expect("Session");
        let config = view.runs[0].config.as_ref().expect("pinned config");
        assert_eq!(
            config.user_instruction_digest,
            Some(Sha256::digest(text.as_bytes()).into())
        );
        assert_eq!(config.instruction_sources.len(), 1);
    }
    runtime
        .block_on(store.close())
        .expect("close read-only State");
    let database = std::fs::read(state.join("events.sqlite3")).expect("database bytes");
    for canary in [
        &b"USER_RULE"[..],
        b"ROOT_RULE",
        OMITTED,
        b"OTHER_STATE_CANARY",
    ] {
        assert!(
            !database.windows(canary.len()).any(|part| part == canary),
            "instruction text stays out of the journal"
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "native Linux PTY workspace consent and restoration gate"]
fn workspace_consent_exits_without_an_implicit_choice_and_remembers_explicit_trust() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::io::Write;
    // The full-frame copy spans several pages at 80x24; each page is reviewed before the choices.
    fn review_consent_pages(
        output: &mut std::process::ChildStdout,
        input: &mut std::process::ChildStdin,
        transcript: &mut Vec<u8>,
        answered: &mut crate::session_picker::PtyResponses,
    ) {
        fn wait_for(
            output: &mut std::process::ChildStdout,
            input: &mut std::process::ChildStdin,
            transcript: &mut Vec<u8>,
            answered: &mut crate::session_picker::PtyResponses,
            page: usize,
        ) {
            let deadline = Instant::now() + Duration::from_secs(10);
            let next = format!("Page {page}/");
            while !(super::session_picker::screen_contains(transcript, 80, 24, &next)
                || super::session_picker::screen_contains(transcript, 80, 24, "Continue read only"))
            {
                super::session_picker::pump(output, input, transcript, answered);
                assert!(
                    Instant::now() < deadline,
                    "consent page {page} missing: {}",
                    super::session_picker::tail(transcript)
                );
                thread::yield_now();
            }
        }
        let mut page = 1;
        wait_for(output, input, transcript, answered, page);
        while super::session_picker::screen_contains(transcript, 80, 24, &format!("Page {page}/")) {
            input.write_all(b"\r").unwrap();
            page += 1;
            wait_for(output, input, transcript, answered, page);
        }
    }

    // Each key waits for the marker it must produce, so no key lands on a stale consent state.
    fn toggle_and_confirm(
        output: &mut std::process::ChildStdout,
        input: &mut std::process::ChildStdin,
        transcript: &mut Vec<u8>,
        answered: &mut crate::session_picker::PtyResponses,
    ) {
        fn send_until(
            output: &mut std::process::ChildStdout,
            input: &mut std::process::ChildStdin,
            transcript: &mut Vec<u8>,
            answered: &mut crate::session_picker::PtyResponses,
            key: &[u8],
            done: impl Fn(&[u8]) -> bool,
        ) {
            input.write_all(key).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(transcript) {
                super::session_picker::pump(output, input, transcript, answered);
                assert!(
                    Instant::now() < deadline,
                    "consent did not advance: {}",
                    super::session_picker::tail(transcript)
                );
                thread::yield_now();
            }
        }
        let shows = |text: &'static str| {
            move |transcript: &[u8]| {
                super::session_picker::screen_contains(transcript, 80, 24, text)
            }
        };
        send_until(
            output,
            input,
            transcript,
            answered,
            b"\t",
            shows("Tab back"),
        );
        let before = super::session_picker::screen_rows(transcript, 80, 24);
        send_until(output, input, transcript, answered, b"\r", |transcript| {
            super::session_picker::screen_rows(transcript, 80, 24) != before
        });
        send_until(
            output,
            input,
            transcript,
            answered,
            b"\t",
            shows("Enter confirm"),
        );
        send_until(
            output,
            input,
            transcript,
            answered,
            b"\r",
            shows("Ask Arany"),
        );
    }
    for linear in [false, true] {
        for (trust, startup_escape, unconfigured) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, false, true),
        ] {
            if linear && (startup_escape || unconfigured) {
                continue;
            }
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("project");
            std::fs::create_dir(&workspace).unwrap();
            let runtime_home = temp.path().join("account-home");
            let runtime_path = runtime_home.join(".nvm/versions/node/v22.3.0");
            std::fs::create_dir_all(runtime_path.join("bin")).unwrap();
            let runtime_binary = runtime_path.join("bin/node");
            std::fs::write(&runtime_binary, b"#!/bin/sh\nprintf profile-selected\n").unwrap();
            std::fs::set_permissions(&runtime_binary, std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let runtime_number = "3";
            let skill = workspace.join(".agents/skills/review");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), b"---\nname: review\ndescription: Synthetic review\n---\nReview the current task.\n").unwrap();
            let state = temp.path().join("state");
            let shell = "before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; stty rows 24 cols 80; /arany $ARANY_TEST_MODE --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" $ARANY_TEST_SELECTION; code=$?; after=$(stty -g); printf '\\nTTY_AFTER:%s\\n' \"$after\"; exit \"$code\"";
            let mut command = super::isolation::isolated_script(temp.path());
            if startup_escape {
                let account =
                    StateRoot::admit(&temp.path().join("account-home/.local/state/arany")).unwrap();
                account
                    .record(arany::RecordKind::SavedAccount).replace(
                        &serde_json::to_vec(&serde_json::json!({
                            "schema":1,"storage":"private_file","account":{
                                "schema":2,"id":uuid::Uuid::now_v7(),"provider":"openai",
                                "model":"gpt-5.4","effort":null,"api_key":"synthetic-never-sent-key","anthropic_workspace_id":null
                            }
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            }
            command
                .env_clear()
                .env(
                    "ARANY_TEST_MODE",
                    if linear { "--screen-reader" } else { "" },
                )
                .env(
                    "ARANY_TEST_SELECTION",
                    if startup_escape || unconfigured {
                        ""
                    } else {
                        "--provider openai --model gpt-5.4"
                    },
                )
                .env("ARANY_TEST_STATE", &state)
                .env("ARANY_TEST_WORKSPACE", &workspace)
                .env("OPENAI_API_KEY", "synthetic-never-sent-key")
                .env("SHELL", "/bin/sh")
                .env("TERM", if linear { "dumb" } else { "xterm-256color" })
                .current_dir(&workspace)
                .args(["-q", "-e", "-c", shell, "/dev/null"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = super::loopback::ChildGuard::new(command.spawn().unwrap());
            let mut input = child.child().stdin.take().unwrap();
            let mut output = child.child().stdout.take().unwrap();
            let flags = fcntl_getfl(&output).unwrap();
            fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
            let mut transcript = Vec::new();
            let mut answered = crate::session_picker::PtyResponses::manual_consent();
            let wait = |output: &mut std::process::ChildStdout,
                        input: &mut std::process::ChildStdin,
                        transcript: &mut Vec<u8>,
                        answered: &mut crate::session_picker::PtyResponses,
                        from: usize,
                        needle: &[u8]| {
                let deadline = Instant::now() + Duration::from_secs(10);
                // Styled text can be split by cursor moves in the raw bytes; the rendered screen is authoritative.
                // Linear output is an append-only log whose replayed screen keeps stale banners, so only new bytes count.
                let text = std::str::from_utf8(needle).ok().filter(|_| !linear);
                while crate::process::pty_text_matches(&transcript[from..], needle)
                    .next()
                    .is_none()
                    && !text.is_some_and(|text| {
                        super::session_picker::screen_contains(transcript, 80, 24, text)
                    })
                {
                    super::session_picker::pump(output, input, transcript, answered);
                    assert!(
                        Instant::now() < deadline,
                        "consent stage missing: {}",
                        super::session_picker::tail(transcript)
                    );
                    thread::yield_now();
                }
            };
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                0,
                b"Do you trust this folder?",
            );
            if !linear && unconfigured {
                review_consent_pages(&mut output, &mut input, &mut transcript, &mut answered);
            }
            let from = transcript.len();
            input
                .write_all(if unconfigured {
                    b"\x1b[B\r"
                } else if linear {
                    b"read only\r"
                } else {
                    b"\x1b"
                })
                .unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                if linear { b"Input:" } else { b"Ask Arany" },
            );
            if unconfigured {
                let from = transcript.len();
                input.write_all(b"/provider openai\r").unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Auto edits",
                );
                let from = transcript.len();
                input.write_all(b"\x1b[Z").unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Request approvals",
                );
            }
            if startup_escape {
                let from = transcript.len();
                input.write_all(b"Read-only question\r").unwrap();
                // The isolated network fails the send, so the call shows its first same-route
                // retry; Ctrl+C then ends the Run inside the backoff without a further call.
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Reconnecting",
                );
                assert!(
                    !transcript[from..]
                        .windows(b"Do you trust this folder?".len())
                        .any(|part| part == b"Do you trust this folder?"),
                    "Escape must admit ephemeral read-only chat without another trust prompt"
                );
                let from = transcript.len();
                input.write_all(b"\x03").unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"\x1b[?25h",
                );
            }
            let from = transcript.len();
            input.write_all(b"/permissions\r").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Choose what to manage",
            );
            let from = transcript.len();
            input
                .write_all(if linear { b"Folder trust\r" } else { b"\r" })
                .unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Do you trust this folder?",
            );
            if trust {
                let from = transcript.len();
                if linear {
                    input
                        .write_all(format!("trust {runtime_number}\r").as_bytes())
                        .unwrap();
                } else {
                    review_consent_pages(&mut output, &mut input, &mut transcript, &mut answered);
                    toggle_and_confirm(&mut output, &mut input, &mut transcript, &mut answered);
                }
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    if linear { b"Input:" } else { b"Ask Arany" },
                );
                if !linear {
                    let from = transcript.len();
                    input.write_all(b"/rev\x1b[C").unwrap();
                    wait(
                        &mut output,
                        &mut input,
                        &mut transcript,
                        &mut answered,
                        from,
                        b"Skill /review",
                    );
                    let from = transcript.len();
                    input.write_all(b"\x03").unwrap();
                    // The cleared composer is found by its border wherever the viewport sits.
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !crate::composer_caret(&transcript, from, 80, 24)
                        .is_some_and(|(column, _)| column == 3)
                    {
                        super::session_picker::pump(
                            &mut output,
                            &mut input,
                            &mut transcript,
                            &mut answered,
                        );
                        assert!(
                            Instant::now() < deadline,
                            "cleared Skill draft: {}",
                            super::session_picker::tail(&transcript[from..])
                        );
                        thread::yield_now();
                    }
                }
                let from = transcript.len();
                input.write_all(b"/permissions\r").unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Choose what to manage",
                );
                let from = transcript.len();
                input
                    .write_all(if linear { b"Folder trust\r" } else { b"\r" })
                    .unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Do you trust this folder?",
                );
                let record = temp
                    .path()
                    .join("account-home/.local/state/arany/workspace-permissions.json");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
                assert_eq!(
                    value["workspaces"][0]["runtime_grants"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                let from = transcript.len();
                if linear {
                    input.write_all(b"trust\r").unwrap();
                } else {
                    review_consent_pages(&mut output, &mut input, &mut transcript, &mut answered);
                    input.write_all(b"\t\t\x1b[B\r").unwrap();
                }
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    if linear { b"Input:" } else { b"Ask Arany" },
                );
                let saved = std::fs::read(&record).unwrap();
                value = serde_json::from_slice(&saved).unwrap();
                assert_eq!(
                    value["workspaces"][0]["runtime_grants"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                assert_eq!(value["workspaces"][0]["runtime_grants"][0]["kind"], "node");
                assert_eq!(
                    value["workspaces"][0]["runtime_grants"][0]["version"],
                    "v22.3.0"
                );
                let from = transcript.len();
                input.write_all(b"/permissions\r").unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Choose what to manage",
                );
                let from = transcript.len();
                input
                    .write_all(if linear { b"Folder trust\r" } else { b"\r" })
                    .unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Do you trust this folder?",
                );
                let from = transcript.len();
                if linear {
                    input.write_all(b"trust none\r").unwrap();
                } else {
                    review_consent_pages(&mut output, &mut input, &mut transcript, &mut answered);
                    toggle_and_confirm(&mut output, &mut input, &mut transcript, &mut answered);
                }
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    if linear { b"Input:" } else { b"Ask Arany" },
                );
            }
            let record = temp
                .path()
                .join("account-home/.local/state/arany/workspace-permissions.json");
            let before_cancel = std::fs::read(&record).ok();
            let interrupted = transcript.len();
            input.write_all(b"\x03").unwrap();
            let mut confirmed = false;
            let deadline = Instant::now() + Duration::from_secs(10);
            let status = loop {
                super::session_picker::pump(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                );
                // Chat asks for a second Ctrl+C before exiting; linear output is a log, so match new bytes.
                let again = b"Ctrl+C again exits";
                if !confirmed
                    && (transcript[interrupted..]
                        .windows(again.len())
                        .any(|part| part == again)
                        || super::session_picker::screen_contains(
                            &transcript,
                            80,
                            24,
                            "Ctrl+C again exits",
                        ))
                {
                    input.write_all(b"\x03").unwrap();
                    confirmed = true;
                }
                if let Some(status) = child.child().try_wait().unwrap() {
                    break status;
                }
                assert!(
                    Instant::now() < deadline,
                    "Ctrl+C did not exit: {}",
                    super::session_picker::tail(&transcript)
                );
                thread::yield_now();
            };
            super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                status.success(),
                "consent exit: {}",
                super::session_picker::tail(&transcript)
            );
            let text = String::from_utf8_lossy(&transcript);
            let before = super::active_terminal::transcript_field(&text, "TTY_BEFORE:");
            let after = super::active_terminal::transcript_field(&text, "TTY_AFTER:");
            assert_eq!(before, after, "terminal must be restored");
            let record = temp
                .path()
                .join("account-home/.local/state/arany/workspace-permissions.json");
            assert_eq!(
                std::fs::read(&record).ok(),
                before_cancel,
                "Ctrl+C must never persist a choice"
            );
            if trust {
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
                assert_eq!(value["version"], 2);
                assert_eq!(value["workspaces"].as_array().unwrap().len(), 1);
                assert_eq!(value["workspaces"][0]["path"], workspace.to_str().unwrap());
                assert_eq!(
                    value["workspaces"][0]["runtime_grants"]
                        .as_array()
                        .unwrap()
                        .len(),
                    0
                );
                assert_eq!(
                    std::fs::metadata(record).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            let connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
            assert_eq!(
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM events WHERE kind = 'run_started'",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux PTY trusted startup with bounded runtime candidate overflow"
)]
fn trusted_startup_does_not_enumerate_optional_runtime_candidates() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::{io::Write, os::unix::fs::PermissionsExt};

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let account_home = temp.path().join("account-home");
    std::fs::create_dir(&account_home).unwrap();
    std::fs::set_permissions(&account_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let versions = account_home.join(".pyenv/versions");
    std::fs::create_dir_all(&versions).unwrap();
    for index in 0..129 {
        std::fs::create_dir(versions.join(format!("unrelated-{index:03}"))).unwrap();
    }
    let account = account_home.join(".local/state/arany");
    super::isolation::remember_workspace_trust(&account, &workspace, &workspace);

    let state = temp.path().join("state");
    let shell = "before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; stty rows 24 cols 80; /arany --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4; code=$?; after=$(stty -g); printf '\\nTTY_AFTER:%s\\n' \"$after\"; exit \"$code\"";
    let mut command = super::isolation::isolated_script(temp.path());
    command
        .env_clear()
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("OPENAI_API_KEY", "synthetic-never-sent-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm-256color")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = super::loopback::ChildGuard::new(command.spawn().unwrap());
    let mut input = child.child().stdin.take().unwrap();
    let mut output = child.child().stdout.take().unwrap();
    let flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::manual_consent();
    let wait = |output: &mut std::process::ChildStdout,
                input: &mut std::process::ChildStdin,
                transcript: &mut Vec<u8>,
                answered: &mut crate::session_picker::PtyResponses,
                needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !super::session_picker::screen_contains(transcript, 80, 24, needle) {
            super::session_picker::pump(output, input, transcript, answered);
            assert!(
                Instant::now() < deadline,
                "trusted startup screen stage missing: {}",
                super::session_picker::screen_rows(transcript, 80, 24).join(" | ")
            );
            thread::yield_now();
        }
    };
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "Ask a question, or type / for commands",
    );
    assert!(
        !super::session_picker::screen_contains(&transcript, 80, 24, "Do you trust this folder?"),
        "remembered trust must skip optional candidate discovery"
    );
    input.write_all(b"/permissions").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "/permissions",
    );
    input.write_all(b"\r").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "Choose what to manage",
    );
    input.write_all(b"\r").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "Runtime profile discovery failed",
    );
    input.write_all(b"\x1b").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "Permissions unchanged",
    );
    input.write_all(b"/status").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "/status",
    );
    input.write_all(b"\r").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "New conversation",
    );
    input.write_all(b"\x03").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "Ctrl+C again exits",
    );
    input.write_all(b"\x03").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
        if let Some(status) = child.child().try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "trusted chat did not exit: {}",
            super::session_picker::tail(&transcript)
        );
        thread::yield_now();
    };
    super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(status.success());
    let text = String::from_utf8_lossy(&transcript);
    assert_eq!(
        super::active_terminal::transcript_field(&text, "TTY_BEFORE:"),
        super::active_terminal::transcript_field(&text, "TTY_AFTER:")
    );
}

#[test]
#[cfg(target_os = "linux")]
#[cfg_attr(not(arany_ci), ignore = "native Linux PTY folder trust recovery")]
fn unsupported_trust_record_is_named_and_reset_only_with_consent() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::io::Write;

    for symlink in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        let account = temp.path().join("account-home/.local/state/arany");
        let root = arany::StateRoot::admit(&account).unwrap();
        let record = root.path().join("workspace-permissions.json");
        if symlink {
            let outside = temp.path().join("outside-permissions.json");
            std::fs::write(&outside, br#"{"version":1,"workspaces":[]}"#).unwrap();
            std::os::unix::fs::symlink(&outside, &record).unwrap();
        } else {
            root.record(arany::RecordKind::WorkspacePermissions)
                .replace(br#"{"version":1,"workspaces":[]}"#)
                .unwrap();
        }
        drop(root);

        let state = temp.path().join("state");
        let shell = "before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; stty rows 24 cols 80; /arany --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4; code=$?; after=$(stty -g); printf '\\nTTY_AFTER:%s\\n' \"$after\"; exit \"$code\"";
        let mut command = super::isolation::isolated_script(temp.path());
        command
            .env_clear()
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("OPENAI_API_KEY", "synthetic-never-sent-key")
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = super::loopback::ChildGuard::new(command.spawn().unwrap());
        let mut input = child.child().stdin.take().unwrap();
        let mut output = child.child().stdout.take().unwrap();
        let flags = fcntl_getfl(&output).unwrap();
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
        let mut transcript = Vec::new();
        let mut answered = crate::session_picker::PtyResponses::manual_consent();
        let wait = |output: &mut std::process::ChildStdout,
                    input: &mut std::process::ChildStdin,
                    transcript: &mut Vec<u8>,
                    answered: &mut crate::session_picker::PtyResponses,
                    needle: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !super::session_picker::screen_contains(transcript, 80, 24, needle) {
                super::session_picker::pump(output, input, transcript, answered);
                assert!(
                    Instant::now() < deadline,
                    "folder trust recovery stage missing: {}",
                    super::session_picker::screen_rows(transcript, 80, 24).join(" | ")
                );
                thread::yield_now();
            }
        };
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            if symlink {
                "owner-only regular"
            } else {
                "unsupported version 1"
            },
        );
        if symlink {
            assert!(
                !super::session_picker::screen_contains(&transcript, 80, 24, "Reset folder trust"),
                "a non-private record must not offer a reset"
            );
            input.write_all(b"\x1b[B\r").unwrap();
        } else {
            input.write_all(b"\x1b[B\r").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                "Reset folder trust?",
            );
            input.write_all(b"\t\r").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                "Do you trust this folder?",
            );
            assert!(!record.exists(), "consented reset must delete the record");
            input.write_all(b"\x03").unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
            if let Some(status) = child.child().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "folder trust recovery did not exit: {}",
                super::session_picker::tail(&transcript)
            );
            thread::yield_now();
        };
        super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(status.success());
        if symlink {
            assert!(
                std::fs::symlink_metadata(&record)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "a non-private record is never modified"
            );
        }
        let text = String::from_utf8_lossy(&transcript);
        assert_eq!(
            super::active_terminal::transcript_field(&text, "TTY_BEFORE:"),
            super::active_terminal::transcript_field(&text, "TTY_AFTER:")
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux narrow PTY consent paging regression"
)]
fn tab_cannot_skip_narrow_folder_consent_pages() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::io::Write;

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let runtime_home = temp.path().join("account-home");
    let runtime_path = runtime_home.join(".nvm/versions/node/v22.3.0");
    std::fs::create_dir_all(runtime_path.join("bin")).unwrap();
    let runtime_binary = runtime_path.join("bin/node");
    std::fs::write(&runtime_binary, b"synthetic runtime marker").unwrap();
    std::fs::set_permissions(&runtime_binary, std::fs::Permissions::from_mode(0o700)).unwrap();

    let account = temp.path().join("account-home/.local/state/arany");
    let state = temp.path().join("state");
    let shell = "before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; stty rows 8 cols 16; /arany --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4; code=$?; after=$(stty -g); printf '\\nTTY_AFTER:%s\\n' \"$after\"; exit \"$code\"";
    let mut command = super::isolation::isolated_script(temp.path());
    command
        .env_clear()
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("OPENAI_API_KEY", "synthetic-never-sent-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm-256color")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = super::loopback::ChildGuard::new(command.spawn().unwrap());
    let mut input = child.child().stdin.take().unwrap();
    let mut output = child.child().stdout.take().unwrap();
    let flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::manual_consent();
    let wait = |output: &mut std::process::ChildStdout,
                input: &mut std::process::ChildStdin,
                transcript: &mut Vec<u8>,
                answered: &mut crate::session_picker::PtyResponses,
                needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !super::session_picker::screen_contains(transcript, 16, 8, needle) {
            super::session_picker::pump(output, input, transcript, answered);
            assert!(
                Instant::now() < deadline,
                "narrow consent screen stage missing: {}",
                super::session_picker::screen_rows(transcript, 16, 8).join(" | ")
            );
            thread::yield_now();
        }
    };
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "Page 1/",
    );
    for &key in b"\t\t\r" {
        input.write_all(&[key]).unwrap();
        super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
    }
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "Page 2/",
    );
    assert!(
        !account.join("workspace-permissions.json").exists(),
        "Tab/Tab/Enter on the first narrow consent page must not save trust"
    );
    input.write_all(b"\x03").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
        if let Some(status) = child.child().try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "consent interruption did not exit: {}",
            super::session_picker::tail(&transcript)
        );
        thread::yield_now();
    };
    super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(status.success());
    let text = String::from_utf8_lossy(&transcript);
    assert_eq!(
        super::active_terminal::transcript_field(&text, "TTY_BEFORE:"),
        super::active_terminal::transcript_field(&text, "TTY_AFTER:")
    );
}

#[test]
#[cfg(target_os = "linux")]
#[cfg_attr(not(arany_ci), ignore = "native Linux PTY Classic consent viewport")]
fn classic_folder_consent_fits_80x24_and_pages_back_when_short() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::io::Write;

    for (rows, paged) in [(24, false), (14, true)] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let shell = format!(
            "stty rows {rows} cols 80; /arany --classic --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4"
        );
        let mut command = super::isolation::isolated_script(temp.path());
        command
            .env_clear()
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("OPENAI_API_KEY", "synthetic-never-sent-key")
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", &shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = super::loopback::ChildGuard::new(command.spawn().unwrap());
        let mut input = child.child().stdin.take().unwrap();
        let mut output = child.child().stdout.take().unwrap();
        let flags = fcntl_getfl(&output).unwrap();
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
        let mut transcript = Vec::new();
        let mut answered = crate::session_picker::PtyResponses::manual_consent();
        let mut wait = |input: &mut std::process::ChildStdin, needle: &str, absent: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                super::session_picker::pump(&mut output, input, &mut transcript, &mut answered);
                let screen = super::session_picker::screen_rows(&transcript, 80, rows).join(" | ");
                if screen.contains(needle) && (absent.is_empty() || !screen.contains(absent)) {
                    return screen;
                }
                assert!(
                    Instant::now() < deadline,
                    "consent stage {needle} missing: {screen}"
                );
                thread::yield_now();
            }
        };
        if !paged {
            let screen = wait(&mut input, "Trust this folder", "Page 1/");
            assert!(screen.contains("Continue read only"));
            assert!(screen.contains("Do you trust this folder?"));
        } else {
            let first = wait(&mut input, "Page 1/", "");
            assert!(first.contains("Do you trust this folder?"));
            input.write_all(b"\x1b[6~").unwrap();
            wait(&mut input, "Trust this folder", "Do you trust this folder?");
            input.write_all(b"\x1b[A").unwrap();
            wait(&mut input, "Do you trust this folder?", "Trust this folder");
        }
        input.write_all(b"\x03").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.child().try_wait().unwrap().is_none() {
            super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "consent interruption did not exit"
            );
            thread::yield_now();
        }
    }
}

#[cfg(all(target_os = "macos", debug_assertions))]
#[test]
fn folder_consent_precedes_setup_and_remembers_only_the_exact_trusted_directory() {
    use rustix::process::{Pid, Signal, kill_process};
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, PermissionsExt},
        process::Stdio,
        time::Duration,
    };

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let account = temp.path().join("account");
    let state = temp.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    let probe = |args: &[&str], expect_prompt: bool, choice: Option<&[u8]>| {
        let (master, slave) = crate::process::cloexec_pty(None);
        let mut input = master.try_clone().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .env("ARANY_TEST_ACCOUNT_ROOT", &account)
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("TERM", "dumb")
            .current_dir(&workspace)
            .args(["--screen-reader", "--no-color", "--workspace"])
            .arg(&workspace)
            .arg("--state-dir")
            .arg(&state)
            .args(args)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::from(slave));
        let mut child = loopback::ChildGuard::new(command.spawn().unwrap());
        drop(command);
        let pid = Pid::from_raw(child.child().id().try_into().unwrap()).unwrap();
        let mut consent_seen = false;
        let mut stopped = false;
        let output = process::capture_terminal(
            child.child(),
            master,
            Duration::from_secs(15),
            64 * 1024,
            |bytes| {
                let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
                if !consent_seen
                    && (text.contains(crate::process::READ_ONLY_CONSENT_PROMPT)
                        || text.contains(crate::process::CONFIG_CONSENT_PROMPT))
                {
                    assert!(expect_prompt, "remembered trust must skip consent");
                    assert!(
                        !text.contains("Setup:"),
                        "folder consent must precede setup"
                    );
                    assert!(!text.contains("Presentation:"), "consent must precede chat");
                    consent_seen = true;
                    if let Some(choice) = choice {
                        input.write_all(choice).unwrap();
                    } else {
                        kill_process(pid, Signal::INT).unwrap();
                        stopped = true;
                    }
                }
                if !stopped
                    && text.contains("Setup: Choose access method\n")
                    && text.ends_with("Input:\n")
                {
                    assert_eq!(
                        consent_seen, expect_prompt,
                        "folder consent must precede setup"
                    );
                    kill_process(pid, Signal::INT).unwrap();
                    stopped = true;
                }
            },
        );
        assert!(stopped, "expected startup boundary was not reached");
        assert_eq!(consent_seen, expect_prompt);
        assert!(
            output.status.success(),
            "startup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "cancelled startup has no answer");
        assert!(
            !state.exists(),
            "startup cancellation must precede Session admission"
        );
        // Reading folder trust creates no account root; only an explicit remember may.
        if account.exists() {
            for entry in std::fs::read_dir(&account).unwrap() {
                let name = entry.unwrap().file_name();
                assert!(
                    name == "workspace-permissions.json"
                        || name == "account-credentials.lock"
                        || name == "history.lock"
                        || name == "events.sqlite3",
                    "startup must not create account metadata: {name:?}"
                );
                if name == "events.sqlite3" || name == "history.lock" {
                    assert_eq!(std::fs::metadata(account.join(name)).unwrap().len(), 0);
                }
            }
        }
        String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n")
    };
    for args in [
        &["--setup"][..],
        &[][..],
        &["--provider", "openai", "--model", "gpt-5.4"][..],
        &["--tools"][..],
        &["--resume", "01900000-0000-7000-8000-000000000001"][..],
    ] {
        let transcript = probe(args, true, None);
        if args.contains(&"--tools") {
            assert!(transcript.contains("1. Use explicit Tool config"));
            assert!(transcript.contains("the explicit configuration keeps precedence"));
            assert!(!transcript.contains("1. Continue read only"));
        }
        assert!(
            !account.join("workspace-permissions.json").exists(),
            "Ctrl+C must save no choice"
        );
    }
    probe(&["--setup"], true, Some(b"read only\n"));
    let untrusted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(account.join("workspace-permissions.json")).unwrap())
            .unwrap();
    assert_eq!(untrusted["workspaces"], serde_json::json!([]));
    probe(&["--setup"], true, Some(b"trust\n"));
    let record = account.join("workspace-permissions.json");
    let remembered = std::fs::read(&record).unwrap();
    let trusted: serde_json::Value = serde_json::from_slice(&remembered).unwrap();
    assert_eq!(trusted["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(
        trusted["workspaces"][0]["path"],
        workspace.to_str().unwrap()
    );
    let identity = std::fs::metadata(&workspace).unwrap();
    assert_eq!(trusted["workspaces"][0]["device"], identity.dev());
    assert_eq!(trusted["workspaces"][0]["inode"], identity.ino());
    assert_eq!(
        std::fs::metadata(&record).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let transcript = probe(&["--setup"], false, None);
    assert!(!transcript.contains("Do you trust this folder?"));
    assert_eq!(std::fs::read(&record).unwrap(), remembered);
    std::fs::rename(&workspace, temp.path().join("old-project")).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    assert_ne!(std::fs::metadata(&workspace).unwrap().ino(), identity.ino());
    probe(&["--setup"], true, None);
    assert_eq!(std::fs::read(&record).unwrap(), remembered);
}

#[test]
#[cfg(target_os = "linux")]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux PTY typed Git rule editing through /permissions"
)]
fn typed_prefix_permissions_edit_remove_and_restart() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::{io::Write, os::unix::fs::PermissionsExt};

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let account_home = temp.path().join("account-home");
    std::fs::create_dir(&account_home).unwrap();
    std::fs::set_permissions(&account_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let account = account_home.join(".local/state/arany");
    super::isolation::remember_workspace_trust(&account, &workspace, &workspace);
    let state = temp.path().join("state");
    let shell = "stty rows 24 cols 80; /arany --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4";
    let mut command = super::isolation::isolated_script(temp.path());
    command
        .env_clear()
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("OPENAI_API_KEY", "synthetic-never-sent-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm-256color")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = super::loopback::ChildGuard::new(command.spawn().unwrap());
    let mut input = child.child().stdin.take().unwrap();
    let mut output = child.child().stdout.take().unwrap();
    let flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::manual_consent();
    let file = state.join("permissions.json");
    fn until(
        output: &mut std::process::ChildStdout,
        input: &mut std::process::ChildStdin,
        transcript: &mut Vec<u8>,
        answered: &mut crate::session_picker::PtyResponses,
        what: &str,
        ready: &dyn Fn(&[u8]) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready(transcript) {
            super::session_picker::pump(output, input, transcript, answered);
            assert!(
                Instant::now() < deadline,
                "{what}: {}",
                super::session_picker::screen_rows(transcript, 80, 24).join(" | ")
            );
            thread::yield_now();
        }
    }
    let shows = |needle: &'static str| {
        move |transcript: &[u8]| super::session_picker::screen_contains(transcript, 80, 24, needle)
    };
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "composer",
        &shows("Ask a question, or type / for commands"),
    );
    input.write_all(b"/permissions\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "area menu",
        &shows("Choose what to manage"),
    );
    input.write_all(b"\x1b[B\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "rules list with the settings creation note",
        &shows("creates permissions.json"),
    );
    input.write_all(b"\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "verb menu",
        &shows("gh pr create (host; human approval required)"),
    );
    assert!(super::session_picker::screen_contains(
        &transcript,
        80,
        24,
        "host; human approval required"
    ));
    input.write_all(b"\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "decision menu",
        &shows("Rule decision"),
    );
    input.write_all(b"\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "scope menu",
        &shows("Rule scope"),
    );
    input.write_all(b"\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "saved rule",
        &|_| file.exists(),
    );
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(value["permissions"]["defaultMode"], "default");
    assert_eq!(
        value["permissions"]["typedPrefixes"][0]["prefix"],
        serde_json::json!(["git", "commit"])
    );
    assert_eq!(
        value["permissions"]["typedPrefixes"][0]["decision"],
        "allow"
    );
    assert_eq!(
        value["permissions"]["typedPrefixes"][0]["workspace"]["path"],
        workspace.to_str().unwrap()
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "listed rule",
        &shows("allow git commit"),
    );
    input.write_all(b"\x1b[B\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "rule menu",
        &shows("Remove rule"),
    );
    // Change the decision to ask before removing the rule.
    input.write_all(b"\x1b[B\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "changed rule",
        &|_| {
            std::fs::read_to_string(&file)
                .unwrap()
                .contains(r#""decision":"ask""#)
        },
    );
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "listed changed rule",
        &shows("ask git commit"),
    );
    input.write_all(b"\x1b[B\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "second rule menu",
        &shows("Remove rule"),
    );
    input.write_all(b"\x1b[B\x1b[B\x1b[B\r").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "removed rule",
        &|_| {
            !std::fs::read_to_string(&file)
                .unwrap()
                .contains("typedPrefixes")
        },
    );
    input.write_all(b"\x1b").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "saved notice",
        &shows("Typed Git rules saved"),
    );
    input.write_all(b"\x03").unwrap();
    until(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "exit hint",
        &shows("Ctrl+C again exits"),
    );
    input.write_all(b"\x03").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        super::session_picker::pump(&mut output, &mut input, &mut transcript, &mut answered);
        if let Some(status) = child.child().try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "chat did not exit");
        thread::yield_now();
    };
    assert!(status.success());
    let reopened = arany::StateRoot::open_existing(&state).unwrap();
    assert!(
        arany::permissions::PermissionSettings::load(&reopened, &workspace)
            .unwrap()
            .typed_prefixes()
            .is_empty()
    );
}
