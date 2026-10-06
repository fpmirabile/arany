use super::{
    active_terminal::{ProductGuard, product_child_of, tty_settings},
    loopback::ChildGuard,
    session_picker::{pump, tail},
};
use arany::{CollaborationPolicy, SessionDefaults, SessionId, StateRoot, Store, list_sessions};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Signal, kill_process};
use std::{
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn wait_for(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut usize,
    needle: &[u8],
) {
    wait_for_after(output, input, transcript, answered, 0, needle);
}

fn wait_for_after(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut usize,
    from: usize,
    needle: &[u8],
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[from..]
        .windows(needle.len())
        .any(|part| part == needle)
    {
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "PTY stage missing: {}",
            tail(transcript)
        );
        thread::yield_now();
    }
}

pub(super) fn saved_conversation(
    state: &std::path::Path,
    workspace: &std::path::Path,
    defaults: SessionDefaults,
) -> SessionId {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("fixture runtime")
        .block_on(async {
            let id = arany::create_session(
                StateRoot::admit(state).expect("State"),
                workspace.to_owned(),
                None,
            )
            .await
            .expect("explicit existing Session");
            if defaults != SessionDefaults::default() {
                arany::set_session_defaults(
                    StateRoot::open_existing(state).expect("State"),
                    workspace.to_owned(),
                    id,
                    defaults,
                )
                .await
                .expect("existing defaults");
            }
            id
        })
}

#[test]
fn new_and_clear_keep_current_selection_without_rewriting_prior_history() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let saved_id = saved_conversation(&state, &workspace, SessionDefaults::default());
    let shell = "printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4 --resume \"$ARANY_TEST_SESSION\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\"";
    let mut command = Command::new("/usr/bin/script");
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
    let mut answered = 0;
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
    for (command, feedback) in [
        ("/provider anthropic", "Provider: anthropic; set /model"),
        ("/model claude-sonnet-5", "Model: claude-sonnet-5 · low"),
        (
            "/model claude-sonnet-5 high",
            "Model: claude-sonnet-5 · high",
        ),
        (
            "/agents team 2",
            "Next-Run collaboration: Team { max_active_children: 2 }",
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
        store
            .append(
                current_id,
                arany::Event::MessageAccepted {
                    run_id: arany::RunId::new(),
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
    input.write_all(b"/quit\r").expect("quit");
    let result = super::loopback::wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    assert_eq!(answered, 0);
    let text = String::from_utf8(transcript).expect("linear transcript UTF-8");
    assert!(!text.contains('\u{1b}'));
    assert_eq!(text.matches("Notice: Error:").count(), 5);
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
    let shell = "stty rows 8 cols 16; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut command = Command::new("/usr/bin/script");
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
    let mut answered = 0;
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
    for cue in [b"Commands".as_slice(), b"Up/Dn Enter Esc", b"\x1b[?25l"] {
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            ready,
            cue,
        );
    }
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

#[test]
fn inline_tab_completion_keeps_placeholders_out_of_commands_and_restores_terminal() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let saved_id = saved_conversation(&state, &workspace, SessionDefaults::default());
    use sha2::Digest;
    let skill = temp.path().join("skill");
    std::fs::create_dir(&skill).unwrap();
    let guidance =
        b"---\nname: review\ndescription: Synthetic review\n---\nReview only the selected task.\n";
    std::fs::write(skill.join("SKILL.md"), guidance).unwrap();
    let config = serde_json::json!({"version":1,"workspace_paths":["."],"write":false,
        "commands":[],"mcp":[],"skills":[{"name":"review","description":"Synthetic review",
        "directory":skill,"files":{"SKILL.md":sha2::Sha256::digest(guidance).iter().map(|byte| format!("{byte:02x}")).collect::<String>()}}]});
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
    let shell = "stty rows 24 cols 40; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider anthropic --tools --no-color; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut command = Command::new("/usr/bin/script");
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
    let mut answered = 0;
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
        b"\x1b[21;3Hfirst",
    );
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        paste_at,
        b"\x1b[22;9H",
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
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clipboard_at,
        b"\x1b[22;9H",
    );
    let continued_at = transcript.len();
    input
        .write_all(b"Z")
        .expect("continue retained paste draft");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        continued_at,
        b"\x1b[22;10H",
    );
    let clear_at = transcript.len();
    input.write_all(b"\x03").expect("clear pasted draft");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_at,
        b"\x1b[22;3H",
    );
    let skill_at = transcript.len();
    input.write_all(b"/rev").expect("configured Skill prefix");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        skill_at,
        b"> Skill /review",
    );
    let skill_accept_at = transcript.len();
    input.write_all(b"\t").expect("choose Skill as a draft");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        skill_accept_at,
        b"\x1b[22;11H",
    );
    let skill_clear_at = transcript.len();
    input.write_all(b"\x03").expect("clear unsent Skill draft");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        skill_clear_at,
        b"\x1b[22;3H",
    );
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
    let completed_at = transcript.len();
    input
        .write_all(b"\r")
        .expect("complete resume without executing");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        completed_at,
        b"[session-id]",
    );
    assert!(!String::from_utf8_lossy(&transcript[completed_at..]).contains("Resume · 1"));
    let picker_at = transcript.len();
    input
        .write_all(b"\r")
        .expect("execute resume without an argument");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        picker_at,
        b"Resume",
    );
    let closed_at = transcript.len();
    input.write_all(b"\x1b").expect("dismiss resume picker");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        closed_at,
        b"closed",
    );
    let menu_at = transcript.len();
    input.write_all(b"/s").expect("ambiguous command prefix");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        menu_at,
        b"\x1b[22;5H",
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
    input
        .write_all(b"\r")
        .expect("complete focused status without submitting prefix");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        status_at,
        b"\x1b[22;10H",
    );
    assert!(
        !String::from_utf8_lossy(&transcript[status_at..]).contains("error:"),
        "Enter must complete the focused command, not reject its prefix: {}",
        tail(&transcript)
    );
    input
        .write_all(b"\r")
        .expect("submit completed local status");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        status_at,
        b"Runs",
    );
    let newline_at = transcript.len();
    input
        .write_all(b"alpha\x1b[13;2u")
        .expect("encoded Shift+Enter continues the chat draft");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        newline_at,
        b"\x1b[22;3H",
    );
    let clear_at = transcript.len();
    input.write_all(b"\x03").expect("clear multiline draft");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        clear_at,
        b"\x1b[22;3H",
    );
    let model_menu_at = transcript.len();
    input.write_all(b"/s").expect("ambiguous Session prefix");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        model_menu_at,
        b"\x1b[22;5H",
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
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        closed_at,
        b"newline",
    );
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
        b"named Session:",
    );
    input
        .write_all(b"/quit\r")
        .expect("quit after corrected rename");
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

#[test]
fn inline_quick_selector_returns_to_the_same_draft_and_caret() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let shell = "stty rows 24 cols 40; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --no-color; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
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
    let mut answered = 0;
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Ask Arany",
    );
    input
        .write_all("a🙂b\x1b[D\x0b".as_bytes())
        .expect("draft and selector");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Quick actions",
    );
    let after_menu = transcript.len();
    input
        .write_all(b"\x1b[200~INERT_MODAL_PASTE\r/quit\x03\x1b[201~\x1b")
        .expect("ignore paste in selector and close without changing draft");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        after_menu,
        b"Ask Arany",
    );
    input.write_all(b"X").expect("edit returned draft");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Xb",
    );
    input
        .write_all(b"\x0b\x1b[B\r")
        .expect("open Agent inspector");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Agents: 0 in 0 recent Runs",
    );
    let after_inspector = transcript.len();
    input.write_all(b"\x1b").expect("close inspector");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        after_inspector,
        b"Ask Arany",
    );
    input.write_all(b"Y").expect("edit returned draft");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Yb",
    );
    let after_inspection = transcript.len();
    input.write_all(b"\x0b").expect("reopen quick selector");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        after_inspection,
        b"return",
    );
    input
        .write_all(b"\x1b[B\x1b[B\r")
        .expect("choose guarded Session action");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        after_inspection,
        b"Submit",
    );
    input.write_all(b"Z").expect("edit after guarded action");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Zb",
    );
    input
        .write_all(b"\x03/quit\r")
        .expect("clear draft and quit");
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
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    assert!(
        !text.contains("INERT_MODAL_PASTE"),
        "ignored modal paste entered the draft"
    );
    let enabled = text.matches("\x1b[?2004h").count();
    let disabled = text.matches("\x1b[?2004l").count();
    assert!(
        enabled > 0 && enabled == disabled,
        "paste mode was not restored"
    );
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
            "selector navigation and retained drafts save no conversation"
        );
    });
}

fn session_picker_close_case(quick: bool) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let saved_id = saved_conversation(&state, &workspace, SessionDefaults::default());
    let shell = "stty rows 24 cols 40; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --no-color; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
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
    let mut answered = 0;
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Ask Arany",
    );
    let mut before_picker = transcript.len();
    if quick {
        input.write_all(b"\x0b").expect("quick actions");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            before_picker,
            b"return",
        );
        before_picker = transcript.len();
        input.write_all(b"\x1b[B\x1b[B\r").expect("Session action");
    } else {
        input.write_all(b"/resume\r").expect("Session command");
    }
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        before_picker,
        b"1 in this Workspace",
    );
    let picker_header = before_picker
        + transcript[before_picker..]
            .windows(b"1 in this Workspace".len())
            .position(|part| part == b"1 in this Workspace")
            .expect("Session picker header");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        picker_header,
        b"\x1b[?25l",
    );
    let before_close = transcript.len();
    input.write_all(b"\x1b").expect("close picker");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        let recent = &transcript[before_close..];
        assert!(
            !recent
                .windows(b"failed".len())
                .any(|part| part == b"failed"),
            "picker close failed: {}",
            tail(&transcript)
        );
        if recent
            .windows(b"closed".len())
            .any(|part| part == b"closed")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "picker close: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    input.write_all(b"/quit\r").expect("quit");
    let deadline = Instant::now() + Duration::from_secs(5);
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
    let text = String::from_utf8_lossy(&transcript);
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("TTY marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime")
        .block_on(async {
            let sessions =
                list_sessions(StateRoot::open_existing(&state).expect("State"), workspace)
                    .await
                    .expect("closed history");
            assert_eq!(
                sessions.len(),
                1,
                "opening and closing resume must not save the empty composer"
            );
            assert_eq!(sessions[0].id, saved_id);
        });
}

#[test]
fn inline_session_picker_closes_after_slash_command() {
    session_picker_close_case(false);
}

#[test]
fn inline_session_picker_closes_after_quick_action() {
    session_picker_close_case(true);
}

#[test]
fn screen_reader_rejected_command_does_not_join_the_next_line() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let saved_id = saved_conversation(&state, &workspace, SessionDefaults::default());
    let shell = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider anthropic; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut command = Command::new("/usr/bin/script");
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
    let mut answered = 0;
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
    #[cfg(debug_assertions)]
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
                    .replace_chatgpt_accounts_record(b"synthetic-malformed-account-canary")
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
                    b"Notice: Error: invalid authorization identity\r\nInput:\r\n"
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
        "version": 1,
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
        b"Notice: Next-Run collaboration: Team { max_active_children: 2 }\r\nInput:\r\n",
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
        .write_all(b"/quit\r")
        .expect("quit after fresh rename");
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
            if cfg!(debug_assertions) { 12 } else { 8 },
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
            arany::render_session(&view, &events, arany::Output::Jsonl).into_bytes(),
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

#[cfg(target_os = "linux")]
#[test]
#[ignore = "isolated clipboard product gate; requires bubblewrap user/PID/network namespaces"]
fn isolated_clipboard_clients_preserve_drafts_and_cancel_owned_processes() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::os::unix::net::UnixListener;

    const CLIENT: &str = r#"#!/usr/bin/sh
set -eu
[ "$(pwd)" = / ]
[ -z "${OPENAI_API_KEY+x}${ANTHROPIC_API_KEY+x}${HTTP_PROXY+x}${ARANY_TEST_EXE+x}${DBUS_SESSION_BUS_ADDRESS+x}" ]
arguments="$*"
if [ -f /data/client-pid ]; then
    previous=$(/usr/bin/cat /data/client-pid)
    [ ! -e "/proc/$previous" ]
fi
if [ -f /data/descendant-pid ]; then
    previous=$(/usr/bin/cat /data/descendant-pid)
    if [ -e "/proc/$previous/stat" ]; then
        set -- $(/usr/bin/cat "/proc/$previous/stat")
        [ "$3" = Z ]
    fi
fi
case "$0" in
    /usr/bin/wl-paste)
        [ "$XDG_RUNTIME_DIR" = /data/runtime ]
        [ "$WAYLAND_DISPLAY" = wayland-0 ]
        [ -z "${DISPLAY+x}${XAUTHORITY+x}" ]
        case "$arguments" in
            --list-types) action=types ;;
            '--no-newline --type image/png'|'--no-newline --type text/plain') action=payload ;;
            *) exit 31 ;;
        esac ;;
    /usr/bin/xclip)
        [ "$DISPLAY" = :99.0 ]
        [ "$XAUTHORITY" = /data/authority ]
        [ "$HOME" = / ]
        [ -z "${XDG_RUNTIME_DIR+x}${WAYLAND_DISPLAY+x}" ]
        case "$arguments" in
            '-selection clipboard -out -target TARGETS') action=types ;;
            '-selection clipboard -out -target image/png'|'-selection clipboard -out -target text/plain') action=payload ;;
            *) exit 32 ;;
        esac ;;
    *) exit 33 ;;
esac
printf '%s\n' "$action" >> /data/client-log
if [ "$action" = types ]; then
    exec /usr/bin/cat /data/types
fi
printf '%s\n' "$$" > /data/client-pid
mode=$(/usr/bin/cat /data/mode)
case "$mode" in
    success) exec /usr/bin/cat /data/payload ;;
    stderr) exec /usr/bin/cat /data/payload >&2 ;;
    failure) exit 1 ;;
    held)
        /usr/bin/cat /data/hold &
        printf '%s\n' "$!" > /data/descendant-pid
        printf 'ready\n' > /data/ready
        wait ;;
    *) exit 34 ;;
esac
"#;
    for wayland in [true, false] {
        let temp = tempfile::tempdir().expect("private clipboard fixture");
        let dir = temp.path();
        let workspace = dir.join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        std::fs::write(
            dir.join("passwd"),
            b"root:x:0:0:Synthetic owner:/data:/usr/bin/sh\n",
        )
        .expect("isolated OS account");
        let listener = UnixListener::bind(dir.join("bus")).expect("synthetic credential bus");
        listener.set_nonblocking(true).expect("nonblocking bus");
        StateRoot::admit(&dir.join(".local/state/arany"))
            .expect("isolated account root")
            .replace_saved_account_record(br#"{"schema":1,"storage":"keyring","account":null}"#)
            .expect("credential-free backend marker");
        std::fs::create_dir(dir.join("runtime")).expect("private Wayland runtime");
        std::fs::set_permissions(dir.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .expect("runtime mode");
        let _socket =
            UnixListener::bind(dir.join("runtime/wayland-0")).expect("synthetic Wayland socket");
        std::fs::write(dir.join("authority"), b"synthetic X authority")
            .expect("private X authority");
        std::fs::set_permissions(
            dir.join("authority"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("authority mode");
        std::fs::write(dir.join("client"), CLIENT).expect("synthetic clipboard client");
        std::fs::set_permissions(dir.join("client"), std::fs::Permissions::from_mode(0o755))
            .expect("client mode");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            dir.join("hold"),
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .expect("held clipboard FIFO");
        let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC")
            .expect("synthetic PNG");
        let saved_id =
            saved_conversation(&dir.join("state"), &workspace, SessionDefaults::default());
        let shell = format!(
            "trap ':' INT; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; /arany --screen-reader --provider openai --model gpt-5.4 --resume {saved_id} --state-dir /data/state --workspace /data/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$code\""
        );
        let mut command = Command::new("/usr/bin/bwrap");
        command
            .env_clear()
            .current_dir(dir)
            .args([
                "--unshare-user",
                "--uid",
                "0",
                "--gid",
                "0",
                "--unshare-pid",
                "--unshare-net",
                "--die-with-parent",
                "--tmpfs",
                "/",
                "--dir",
                "/usr",
                "--dir",
                "/usr/bin",
                "--dir",
                "/etc",
                "--ro-bind",
                "/usr/lib",
                "/usr/lib",
                "--symlink",
                "usr/lib",
                "/lib",
                "--symlink",
                "usr/lib",
                "/lib64",
                "--proc",
                "/proc",
                "--dev-bind",
                "/dev",
                "/dev",
                "--bind",
            ])
            .arg(dir)
            .arg("/data")
            .arg("--ro-bind")
            .arg(dir.join("passwd"))
            .arg("/etc/passwd");
        for program in ["sh", "script", "stty", "cat"] {
            command
                .arg("--ro-bind")
                .arg(format!("/usr/bin/{program}"))
                .arg(format!("/usr/bin/{program}"));
        }
        command
            .arg("--ro-bind")
            .arg(env!("CARGO_BIN_EXE_arany"))
            .arg("/arany");
        for client in ["wl-paste", "xclip"] {
            command
                .arg("--ro-bind")
                .arg(dir.join("client"))
                .arg(format!("/usr/bin/{client}"));
        }
        command.args([
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/bin",
            "--setenv",
            "SHELL",
            "/usr/bin/sh",
            "--setenv",
            "TERM",
            "dumb",
            "--setenv",
            "OPENAI_API_KEY",
            "synthetic-clipboard-key",
            "--setenv",
            "ANTHROPIC_API_KEY",
            "omitted-clipboard-key",
            "--setenv",
            "HTTP_PROXY",
            "http://127.0.0.1:9",
            "--setenv",
            "ARANY_TEST_EXE",
            "must-not-forward",
            "--setenv",
            "HOME",
            "/data",
            "--setenv",
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/data/bus",
        ]);
        if wayland {
            command.args([
                "--setenv",
                "XDG_RUNTIME_DIR",
                "/data/runtime",
                "--setenv",
                "WAYLAND_DISPLAY",
                "wayland-0",
            ]);
        } else {
            command.args([
                "--setenv",
                "DISPLAY",
                ":99.0",
                "--setenv",
                "XAUTHORITY",
                "/data/authority",
            ]);
        }
        command
            .args([
                "--chdir",
                "/data/workspace",
                "--",
                "/usr/bin/script",
                "-q",
                "-e",
                "-c",
                &shell,
                "/dev/null",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("isolated clipboard PTY"));
        let mut input = child.child().stdin.take().expect("PTY input");
        let mut output = child.child().stdout.take().expect("PTY output");
        fcntl_setfl(
            &output,
            fcntl_getfl(&output).expect("output flags") | OFlags::NONBLOCK,
        )
        .expect("nonblocking output");
        let mut transcript = Vec::new();
        let mut answered = 0;
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Input:\r\n",
        );
        assert!(!dir.join("client-log").exists(), "startup read clipboard");
        for (types, mode, payload, expected, calls) in [
            (
                "text/plain\n",
                "success",
                b"first\nsecond".as_slice(),
                "Draft (not sent):\r\n  first\r\n  second\r\nDraft: 12 characters; Enter submits when idle; Ctrl+C clears when idle\r\nInput:\r\n",
                "types\npayload\n",
            ),
            (
                "image/png\ntext/plain\n",
                "success",
                png.as_slice(),
                "Draft (not sent):\r\n  \r\n  Image 1: PNG 1x1, 0.1 KB\r\nDraft: 0 characters; Enter submits when idle; Ctrl+C clears when idle\r\nInput:\r\n",
                "types\npayload\n",
            ),
        ] {
            std::fs::write(dir.join("types"), types).expect("offered clipboard types");
            std::fs::write(dir.join("mode"), mode).expect("client mode");
            std::fs::write(dir.join("payload"), payload).expect("clipboard payload");
            std::fs::write(dir.join("client-log"), b"").expect("fresh client log");
            let from = transcript.len();
            input.write_all(b"/paste\r").expect("explicit paste");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                expected.as_bytes(),
            );
            assert_eq!(
                std::fs::read(dir.join("client-log")).expect("client calls"),
                calls.as_bytes()
            );
            if types == "text/plain\n" {
                let from = transcript.len();
                input.write_all(b"\x03").expect("clear pasted text");
                wait_for_after(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    from,
                    b"Input:\r\n",
                );
            }
        }
        let from = transcript.len();
        input
            .write_all(b"/new\r")
            .expect("reject switching with an attachment");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Notice: Error: Submit or clear image attachments before switching Sessions\r\nInput:\r\n",
        );
        for (types, mode, payload, diagnostic, calls) in [
            (
                b"image/jpeg\ntext/plain\n".to_vec(),
                "success",
                b"filename.png".to_vec(),
                "clipboard image format is unsupported; use PNG",
                "types\n",
            ),
            (
                b"image/png\n".to_vec(),
                "success",
                b"invalid PNG".to_vec(),
                "invalid or unsupported PNG image",
                "types\npayload\n",
            ),
            (
                b"image/png\n".to_vec(),
                "success",
                vec![0; arany::MAX_IMAGE_BYTES + 1],
                "clipboard output exceeds its limit",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "success",
                vec![b'x'; 8193],
                "clipboard output exceeds its limit",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "success",
                vec![0xff],
                "clipboard text is not valid UTF-8",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "success",
                b"\x1b[31m/quit".to_vec(),
                "paste contains unsupported control characters",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "stderr",
                b"PRIVATE_CLIPBOARD_ERROR".repeat(256),
                "clipboard output exceeds its limit",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "failure",
                Vec::new(),
                "clipboard read unavailable",
                "types\npayload\n",
            ),
            (
                vec![b'x'; 16 * 1024 + 1],
                "success",
                Vec::new(),
                "clipboard output exceeds its limit",
                "types\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "held",
                Vec::new(),
                "clipboard read cancelled or timed out",
                "types\npayload\n",
            ),
        ] {
            std::fs::write(dir.join("types"), types).expect("offered clipboard types");
            std::fs::write(dir.join("mode"), mode).expect("client mode");
            std::fs::write(dir.join("payload"), payload).expect("hostile clipboard payload");
            std::fs::write(dir.join("client-log"), b"").expect("fresh client log");
            let from = transcript.len();
            input.write_all(b"/paste\r").expect("reject hostile paste");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                format!("Notice: Error: {diagnostic}; draft unchanged\r\nInput:\r\n").as_bytes(),
            );
            assert_eq!(
                std::fs::read(dir.join("client-log")).expect("client calls"),
                calls.as_bytes()
            );
        }
        std::fs::write(dir.join("types"), b"image/png\n").expect("offered PNG");
        std::fs::write(dir.join("mode"), b"success").expect("successful client");
        std::fs::write(dir.join("payload"), &png).expect("valid PNG");
        let from = transcript.len();
        input
            .write_all(b"/paste\r")
            .expect("retain previous image after rejections");
        wait_for_after(&mut output, &mut input, &mut transcript, &mut answered, from,
            b"Image 2: PNG 1x1, 0.1 KB\r\nDraft: 0 characters; Enter submits when idle; Ctrl+C clears when idle\r\nInput:\r\n");
        let from = transcript.len();
        input.write_all(b"\x03").expect("clear images explicitly");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Input:\r\n",
        );
        std::fs::write(dir.join("types"), b"text/plain\n").expect("held text offer");
        std::fs::write(dir.join("mode"), b"held").expect("held client");
        std::fs::write(dir.join("ready"), b"").expect("new cancellation readiness");
        input
            .write_all(b"/paste\r")
            .expect("start cancellable clipboard read");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read(dir.join("ready")).ok().as_deref() != Some(b"ready\n") {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "held clipboard client did not start"
            );
            thread::yield_now();
        }
        let from = transcript.len();
        input
            .write_all(b"kept\x04")
            .expect("edit during clipboard read");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Notice: Draft: 4 characters; Enter submits\r\nInput:\r\n",
        );
        let from = transcript.len();
        input
            .write_all(b"\x03")
            .expect("cancel owned clipboard process");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Notice: Clipboard read cancelled; draft retained\r\nInput:\r\n",
        );
        let from = transcript.len();
        input
            .write_all(b"\x04")
            .expect("check retained draft without submitting");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Notice: Draft retained; press Enter to submit or Ctrl+C to clear\r\nInput:\r\n",
        );
        let from = transcript.len();
        input.write_all(b"\x03").expect("clear retained text");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Input:\r\n",
        );
        std::fs::write(dir.join("mode"), b"success").expect("recover after cancellation");
        std::fs::write(dir.join("payload"), b"recovered").expect("post-cancel text");
        let from = transcript.len();
        input
            .write_all(b"/paste\r")
            .expect("verify process cleanup through next read");
        wait_for_after(&mut output, &mut input, &mut transcript, &mut answered, from,
            b"Draft (not sent):\r\n  recovered\r\nDraft: 9 characters; Enter submits when idle; Ctrl+C clears when idle\r\nInput:\r\n");
        let from = transcript.len();
        input.write_all(b"\x03").expect("clear recovered text");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Input:\r\n",
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("public lifecycle runtime");
        let state = dir.join("state");
        let session_id = runtime.block_on(async {
            let sessions = list_sessions(
                StateRoot::open_existing(&state).expect("State"),
                workspace.clone(),
            )
            .await
            .expect("current Session");
            assert_eq!(sessions.len(), 1);
            sessions[0].id
        });
        let defaults = arany::SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: None,
            account_id: Some(uuid::Uuid::now_v7()),
            policy: Default::default(),
        };
        runtime
            .block_on(arany::set_session_defaults(
                StateRoot::open_existing(&state).expect("State"),
                workspace.clone(),
                session_id,
                defaults.clone(),
            ))
            .expect("synthetic saved-account selection");
        let prefix = runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
                .expect("prefix Store");
            let events = store.load_session(session_id).await.expect("prefix Events");
            store.close().await.expect("close prefix");
            events
        });
        assert_eq!(prefix.len(), 3, "clipboard persisted transient data");
        let from = transcript.len();
        input
            .write_all(format!("/resume {session_id}\r").as_bytes())
            .expect("reload saved-account selection");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Notice: Resumed: Empty conversation\r\nInput:\r\n",
        );
        let from = transcript.len();
        input
            .write_all(b"synthetic credential-wait objective\r")
            .expect("enter bounded preparation");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut connection = loop {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    pump(&mut output, &mut input, &mut transcript, &mut answered);
                    assert!(
                        Instant::now() < deadline,
                        "credential helper did not connect"
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("synthetic bus accept: {error}"),
            }
        };
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Notice: Preparing request... Ctrl+C cancels\r\n",
        );
        std::fs::write(dir.join("types"), b"image/png\n").expect("preparing PNG offer");
        std::fs::write(dir.join("payload"), &png).expect("preparing PNG");
        let from = transcript.len();
        input.write_all(b"/paste\r").expect("paste while preparing");
        wait_for_after(&mut output, &mut input, &mut transcript, &mut answered, from,
            b"Draft (not sent):\r\n  \r\n  Image 1: PNG 1x1, 0.1 KB\r\nDraft: 0 characters; Enter submits when idle; Ctrl+C clears when idle\r\n");
        let pasted = String::from_utf8_lossy(&transcript[from..]);
        assert!(
            !pasted.contains("Error:"),
            "paste waited for admission failure"
        );
        assert!(
            !pasted.contains("Input:\r\n"),
            "busy paste printed an idle prompt"
        );
        connection
            .set_nonblocking(true)
            .expect("nonblocking handshake");
        let mut observed = 0;
        loop {
            let mut bytes = [0; 1024];
            match connection.read(&mut bytes) {
                Ok(0) => panic!("paste preview waited for credential helper exit"),
                Ok(count) => {
                    observed += count;
                    assert!(observed <= 4096, "bounded credential handshake");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("synthetic handshake: {error}"),
            }
        }
        let from = transcript.len();
        input.write_all(b"\x03").expect("interrupt preparation");
        wait_for_after(&mut output, &mut input, &mut transcript, &mut answered, from,
            b"Notice: Submission interrupted before Run admission; no task was accepted. Account work already started may still complete.\r\nInput:\r\n");
        let from = transcript.len();
        input.write_all(b"\x04").expect("image draft retained");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Notice: Draft retained; press Enter to submit or Ctrl+C to clear\r\nInput:\r\n",
        );
        let from = transcript.len();
        input.write_all(b"\x03").expect("clear retained image");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Input:\r\n",
        );
        input
            .write_all(b"/quit\r")
            .expect("quit without submitting");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            if child
                .child()
                .try_wait()
                .expect("namespace status")
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "clipboard namespace did not exit"
            );
            thread::yield_now();
        }
        assert!(child.take().wait().expect("namespace reap").success());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        for omitted in [
            b"PRIVATE_CLIPBOARD_ERROR".as_slice(),
            b"synthetic-clipboard-key",
            b"omitted-clipboard-key",
            b"iVBORw0KGgo",
        ] {
            assert!(
                !transcript
                    .windows(omitted.len())
                    .any(|part| part == omitted),
                "private clipboard bytes escaped"
            );
        }
        let text = String::from_utf8_lossy(&transcript);
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("terminal marker")
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
        connection
            .set_nonblocking(false)
            .expect("blocking final handshake");
        connection
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("closed-helper deadline");
        loop {
            let mut bytes = [0; 1024];
            let count = connection
                .read(&mut bytes)
                .expect("credential helper closed");
            if count == 0 {
                break;
            }
            observed += count;
            assert!(observed <= 4096, "bounded final credential handshake");
        }
        runtime.block_on(async {
            let state = dir.join("state");
            let root = StateRoot::open_existing(&state).expect("closed State");
            let sessions = list_sessions(root, workspace).await.expect("Session list");
            assert_eq!(sessions.len(), 1, "image switching created a Session");
            let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
                .expect("read-only Store");
            let events = store
                .load_session(sessions[0].id)
                .await
                .expect("closed Events");
            let view = arany::SessionView::replay(sessions[0].id, &events)
                .expect("strict replay")
                .expect("Session");
            assert!(view.runs.is_empty(), "clipboard started a Run");
            assert_eq!(
                events, prefix,
                "clipboard or preparation changed the prefix"
            );
            assert_eq!(view.defaults, defaults);
            store.close().await.expect("close Store");
        });
    }
}
