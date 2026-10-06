use super::*;
use std::{
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    process::Stdio,
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

#[test]
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
            let input: serde_json::Value =
                serde_json::from_str(body["input"].as_str().expect("semantic input"))
                    .expect("semantic JSON");
            if index == 3 {
                assert_eq!(input["workspace_guidance"], "EXPECTED_ROOT_GUIDANCE");
                assert_eq!(input["objective"], "answer");
                assert_eq!(input["phase"], "root_plan");
            } else {
                assert!(input["workspace_guidance"].is_null());
            }
            let text = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["one read-only question"]}})
                }
                2 => serde_json::json!({"summary":"synthetic summary"}),
                _ => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"verified summary","result":"verified answer"}})
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
#[ignore = "native Linux PTY workspace consent and restoration gate"]
fn workspace_consent_exits_without_an_implicit_choice_and_remembers_explicit_trust() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::io::Write;
    for linear in [false, true] {
        for (trust, startup_escape) in [(false, false), (true, false), (false, true)] {
            if linear && startup_escape {
                continue;
            }
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("project");
            std::fs::create_dir(&workspace).unwrap();
            let state = temp.path().join("state");
            let shell = "before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; stty rows 24 cols 80; /arany $ARANY_TEST_MODE --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" $ARANY_TEST_SELECTION; code=$?; after=$(stty -g); printf '\\nTTY_AFTER:%s\\n' \"$after\"; exit \"$code\"";
            let mut command = super::process::isolated_script(temp.path());
            if startup_escape {
                let account =
                    StateRoot::admit(&temp.path().join("account-home/.local/state/arany")).unwrap();
                account
                    .replace_saved_account_record(
                        &serde_json::to_vec(&serde_json::json!({
                            "schema":1,"storage":"private_file","account":{
                                "schema":1,"id":uuid::Uuid::now_v7(),"provider":"openai",
                                "model":"gpt-5.4","effort":null,"api_key":"synthetic-never-sent-key"
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
                    if startup_escape {
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
            let mut answered = 0;
            let wait = |output: &mut std::process::ChildStdout,
                        input: &mut std::process::ChildStdin,
                        transcript: &mut Vec<u8>,
                        answered: &mut usize,
                        from: usize,
                        needle: &[u8]| {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !transcript[from..]
                    .windows(needle.len())
                    .any(|part| part == needle)
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
                if startup_escape {
                    b"Do you trust this folder?"
                } else if linear {
                    b"Input:"
                } else {
                    b"Ask Arany"
                },
            );
            let from = transcript.len();
            if startup_escape {
                input.write_all(b"\x1b").unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Ask Arany",
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
                b"Do you trust this folder?",
            );
            if trust {
                let from = transcript.len();
                input
                    .write_all(if linear { b"trust\r" } else { b"\x1b[B\r" })
                    .unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    if linear { b"Input:" } else { b"Ask Arany" },
                );
                let from = transcript.len();
                input.write_all(b"/permissions\r").unwrap();
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Do you trust this folder?",
                );
            }
            input.write_all(b"\x03").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let status = loop {
                super::session_picker::pump(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                );
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
            assert_eq!(record.exists(), trust, "Ctrl+C must never persist a choice");
            if trust {
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
                assert_eq!(value["version"], 1);
                assert_eq!(value["workspaces"].as_array().unwrap().len(), 1);
                assert_eq!(value["workspaces"][0]["path"], workspace.to_str().unwrap());
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
