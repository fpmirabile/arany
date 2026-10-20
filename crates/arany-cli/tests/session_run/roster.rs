use super::*;
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio};

const TERMINAL_REQUIRED: &[u8] =
    b"error: attached mode requires terminal stdin and stderr; use arany exec for pipes\n";

#[cfg(all(target_os = "macos", debug_assertions))]
fn drain_attached_output(
    reader: &mut impl std::io::Read,
    destination: &mut Vec<u8>,
    channel: &str,
) -> bool {
    use std::io::ErrorKind;

    loop {
        let mut bytes = [0; 4096];
        match reader.read(&mut bytes) {
            Ok(0) => return true,
            Ok(count) => {
                assert!(destination.len() + count <= 64 * 1024, "bounded {channel}");
                destination.extend_from_slice(&bytes[..count]);
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
            {
                return false;
            }
            Err(error) => panic!("{channel} read: {error}"),
        }
    }
}

fn tempdir() -> tempfile::TempDir {
    #[cfg(target_os = "macos")]
    {
        tempfile::Builder::new()
            .prefix("arany-roster-")
            .tempdir_in("/private/tmp")
            .unwrap()
    }
    #[cfg(not(target_os = "macos"))]
    {
        tempfile::tempdir().unwrap()
    }
}

fn record() -> serde_json::Value {
    json!({"version":1,"profiles":[
        {"name":"main","routes":[{"provider":"openai","model":"gpt-5.4","effort":"low","account_id":null}]},
        {"name":"unused","routes":[{"provider":"anthropic","model":"claude-sonnet-5","effort":"high","account_id":null}]}
    ],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main","unused"]}]})
}

fn command(workspace: &Path, state: &Path, extra: &[&str]) -> std::process::Output {
    let run = |attached| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command.env_clear().current_dir(workspace);
        if attached {
            command.args(["--screen-reader"]);
        } else {
            command.arg("exec");
        }
        command
            .arg("answer")
            .args(["--profile", "coding", "--state-dir"])
            .arg(state)
            .arg("--workspace")
            .arg(workspace)
            .args(extra)
            .stdin(Stdio::null())
            .bounded_output()
            .unwrap()
    };
    let output = run(false);
    if !extra.contains(&"--output") {
        let attached = run(true);
        assert_eq!(
            attached.status.code(),
            output.status.code(),
            "attached refusal exit"
        );
        assert_eq!(attached.stdout, output.stdout, "attached refusal stdout");
        let mut expected = output.stderr.clone();
        if expected.windows(7).any(|part| part == b"refused") {
            // Credential admission follows folder consent, which needs a terminal.
            let error = expected
                .windows(7)
                .rposition(|part| part == b"error: ")
                .unwrap();
            expected.truncate(error);
            expected.extend_from_slice(TERMINAL_REQUIRED);
        }
        assert_eq!(attached.stderr, expected, "attached refusal stderr");
    }
    output
}

#[test]
fn profile_refusals_precede_workspace_input_and_keep_stdout_empty() {
    let temp = tempdir();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    nix::unistd::mkfifo(
        &workspace.join("AGENTS.md"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let state = temp.path().join("state");
    StateRoot::admit(&state).unwrap();
    let roster = state.join("provider-roster.json");
    let value = record();
    private_file(&roster, &serde_json::to_vec(&value).unwrap());
    let manifest = "Roster preset: coding\nRoles: primary=main, child=main, review=main, compaction=main\nEligible child profiles: main, unused\n  main[0]: openai/gpt-5.4 effort=low (native API)\n  unused[0]: anthropic/claude-sonnet-5 effort=high (native API)\n";
    for (extra, expected) in [
        (
            vec!["--provider", "anthropic"],
            "error: explicit Provider/model/effort differs from --profile primary\n".to_owned(),
        ),
        (
            vec!["--model", "gpt-6-luna"],
            "error: explicit Provider/model/effort differs from --profile primary\n".to_owned(),
        ),
        (
            vec!["--effort", "high"],
            "error: explicit Provider/model/effort differs from --profile primary\n".to_owned(),
        ),
        (
            vec![],
            format!(
                "{manifest}error: roster profile main route 0 refused: Provider role route openai/gpt-5.4 refused: selected native API credentials or workspace configuration unavailable\n"
            ),
        ),
        (
            vec!["--output", "jsonl"],
            format!(
                "{manifest}error: roster profile main route 0 refused: Provider role route openai/gpt-5.4 refused: selected native API credentials or workspace configuration unavailable\n"
            ),
        ),
    ] {
        let output = command(&workspace, &state, &extra);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(output.stdout, b"");
        assert_eq!(output.stderr, expected.as_bytes());
    }
    let mut unused = Command::new(env!("CARGO_BIN_EXE_arany"));
    let output = unused
        .env_clear()
        .env("OPENAI_API_KEY", "synthetic-roster-key")
        .current_dir(&workspace)
        .args(["exec", "--profile", "coding", "--state-dir"])
        .arg(&state)
        .arg("answer")
        .stdin(Stdio::null())
        .bounded_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, format!("{manifest}error: roster profile unused route 0 refused: Provider role route anthropic/claude-sonnet-5 refused: selected native API credentials or workspace configuration unavailable\n").as_bytes());
    private_file(&state.join("provider-routing.json"), b"{}");
    let conflict = command(&workspace, &state, &[]);
    assert_eq!(conflict.status.code(), Some(1));
    assert_eq!(conflict.stdout, b"");
    assert_eq!(
        conflict.stderr,
        b"error: --profile conflicts with provider-routing.json; choose one routing source\n"
    );
    std::fs::remove_file(state.join("provider-routing.json")).unwrap();
    std::fs::set_permissions(&roster, std::fs::Permissions::from_mode(0o644)).unwrap();
    let unsafe_file = command(&workspace, &state, &[]);
    assert_eq!(unsafe_file.status.code(), Some(1));
    assert_eq!(unsafe_file.stdout, b"");
    assert_eq!(
        unsafe_file.stderr,
        b"error: Provider roster unavailable or unsafe\n"
    );
    std::fs::remove_file(&roster).unwrap();
    let missing = command(&workspace, &state, &[]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(missing.stdout, b"");
    assert_eq!(
        missing.stderr,
        b"error: provider-roster.json is unavailable\n"
    );
    assert_eq!(
        std::fs::metadata(state.join("events.sqlite3"))
            .unwrap()
            .len(),
        0,
        "refusal opens no journal"
    );
    let fresh = temp.path().join("fresh");
    let absent = command(&workspace, &fresh, &[]);
    assert_eq!(absent.status.code(), Some(1));
    assert!(!fresh.exists(), "refusal creates no State");
}

#[cfg(debug_assertions)]
#[test]
fn profile_saved_models_share_one_selected_identity_and_refuse_another() {
    let temp = tempdir();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    nix::unistd::mkfifo(
        &workspace.join("AGENTS.md"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let state = temp.path().join("state");
    StateRoot::admit(&state).unwrap();
    let account_root = temp.path().join("account-root");
    let account = super::prepare_test_file_account_at(&account_root);
    let mut roster = record();
    roster["profiles"][0]["routes"][0]["account_id"] = json!(account);
    let same = json!({"name":"same","routes":[{"provider":"openai","model":"gpt-6-luna","effort":"low","account_id":account}]});
    roster["profiles"].as_array_mut().unwrap().push(same);
    roster["presets"][0]["child_profiles"] = json!(["main", "same", "unused"]);
    for other in [false, true] {
        if other {
            roster["profiles"][2]["routes"][0]["account_id"] = json!(uuid::Uuid::now_v7());
        }
        private_file(
            &state.join("provider-roster.json"),
            &serde_json::to_vec(&roster).unwrap(),
        );
        for entry in [vec!["exec", "answer"], vec!["--screen-reader", "answer"]] {
            let exec = entry[0] == "exec";
            let output = Command::new(env!("CARGO_BIN_EXE_arany"))
                .env_clear()
                .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
                .env("HOME", temp.path().join("home"))
                .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
                .env("XDG_DATA_HOME", temp.path().join("xdg-data"))
                .current_dir(&workspace)
                .args(entry)
                .args(["--profile", "coding", "--state-dir"])
                .arg(&state)
                .stdin(Stdio::null())
                .bounded_output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert_eq!(output.stdout, b"");
            let error = if other {
                "error: roster profile same route 0 refused: Provider role route openai/gpt-6-luna refused: invalid saved account; use arany --setup to choose or reset access\n"
            } else {
                "error: roster profile unused route 0 refused: Provider role route anthropic/claude-sonnet-5 refused: selected native API credentials or workspace configuration unavailable\n"
            };
            let expected = format!(
                "Roster preset: coding\nRoles: primary=main, child=main, review=main, compaction=main\nEligible child profiles: main, same, unused\n  main[0]: openai/gpt-5.4 effort=low (native API)\n  same[0]: openai/gpt-6-luna effort=low (native API)\n  unused[0]: anthropic/claude-sonnet-5 effort=high (native API)\n{error}"
            );
            if exec {
                assert_eq!(output.stderr, expected.as_bytes());
            } else {
                let manifest = expected.rfind("error: ").unwrap();
                assert_eq!(
                    output.stderr,
                    [&expected.as_bytes()[..manifest], TERMINAL_REQUIRED].concat()
                );
            }
            assert_eq!(
                std::fs::metadata(state.join("events.sqlite3"))
                    .unwrap()
                    .len(),
                0
            );
        }
        attached_pty_refusal(
            temp.path(),
            &workspace,
            &state,
            Some(&account_root),
            if other {
                b"roster profile same route 0 refused"
            } else {
                b"roster profile unused route 0 refused"
            },
        );
    }
}

#[cfg(all(target_os = "macos", debug_assertions))]
fn attached_leg_macos(
    temp: &Path,
    workspace: &Path,
    state: &Path,
    held_rx: &std::sync::mpsc::Receiver<()>,
    served: &std::sync::atomic::AtomicUsize,
) -> (arany::SessionId, arany::GoalId) {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::{
        io::Write,
        str::FromStr,
        time::{Duration, Instant},
    };

    let geometry = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (input, slave) = crate::process::cloexec_pty(Some(&geometry));
    let master = input.try_clone().unwrap();
    let before = crate::process::terminal_settings(&input);
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.join("account"))
        .env("HOME", temp.join("home"))
        .env("XDG_STATE_HOME", temp.join("xdg-state"))
        .env("XDG_DATA_HOME", temp.join("xdg-data"))
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("TERM", "dumb")
        .current_dir(workspace)
        .args(["--screen-reader", "--no-color", "--profile", "coding"])
        .args(["--provider", "custom:local", "--model", "model-1"])
        .args(["--state-dir"])
        .arg(state)
        .arg("--workspace")
        .arg(workspace)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(slave));
    super::active_terminal::own_terminal(&mut command);
    let child = command.spawn().unwrap();
    drop(command);
    let (mut child, mut master, mut input) =
        (super::loopback::ChildGuard::new(child), master, input);
    let flags = fcntl_getfl(&master).unwrap();
    fcntl_setfl(&master, flags | OFlags::NONBLOCK).unwrap();
    let mut output = child.child().stdout.take().unwrap();
    let output_flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, output_flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut stdout_bytes = Vec::new();
    let mut consent_declined = false;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(120);
    let mut first_byte = None;
    fn normalized_lines(bytes: &[u8]) -> Vec<u8> {
        bytes
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| {
                (*byte != b'\r' || bytes.get(index + 1) != Some(&b'\n')).then_some(*byte)
            })
            .collect()
    }
    let mut pump = |input: &mut std::fs::File, transcript: &mut Vec<u8>| {
        drain_attached_output(&mut master, transcript, "attached PTY transcript");
        if first_byte.is_none() && !transcript.is_empty() {
            first_byte = Some(Instant::now());
        }
        drain_attached_output(&mut output, &mut stdout_bytes, "attached stdout");
        assert!(stdout_bytes.is_empty(), "attached roster writes to stdout");
        crate::process::decline_workspace_consent(input, transcript, &mut consent_declined);
        assert!(
            Instant::now() < deadline,
            "{}",
            crate::process::terminal_wait_report(
                "attached roster journey stalled",
                started,
                first_byte,
                transcript,
                child.child().try_wait().unwrap(),
            )
        );
        crate::process::wait_readable(&master);
    };
    for (needle, send) in [
        (&b"Input:"[..], &b"/agents team 1\r"[..]),
        (&b"Next-Run collaboration: team"[..], &b"answer\r"[..]),
    ] {
        while !transcript.windows(needle.len()).any(|part| part == needle) {
            pump(&mut input, &mut transcript);
        }
        input.write_all(send).unwrap();
    }
    let manifest = "Roster preset: coding\nRoles: primary=main, child=main, review=main, compaction=main\nEligible child profiles: main, inspect\n  inspect[0]: custom:local/model-1 effort=default (exact custom; unknown pricing)\n  main[0]: custom:local/model-1 effort=default (exact custom; unknown pricing)\n";
    while !normalized_lines(&transcript)
        .windows(manifest.len())
        .any(|part| part == manifest.as_bytes())
    {
        pump(&mut input, &mut transcript);
    }
    loop {
        match held_rx.try_recv() {
            Ok(()) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => pump(&mut input, &mut transcript),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("Provider peer ended before the held roster request")
            }
        }
    }
    input.write_all(b"\x03").unwrap();
    for (needle, send) in [
        (&b"Goal paused"[..], &b"\x03"[..]),
        (&b"Ctrl+C again exits"[..], &b"\x03"[..]),
    ] {
        while !transcript.windows(needle.len()).any(|part| part == needle) {
            pump(&mut input, &mut transcript);
        }
        input.write_all(send).unwrap();
    }
    fcntl_setfl(&master, flags).unwrap();
    let captured = crate::process::capture_terminal(
        child.child(),
        master,
        Duration::from_secs(10),
        64 * 1024,
        |_| {},
    );
    assert!(
        captured.status.success(),
        "attached exit: {:?}",
        captured.status
    );
    let stdout_deadline = Instant::now() + Duration::from_secs(5);
    while !drain_attached_output(&mut output, &mut stdout_bytes, "attached stdout") {
        assert!(
            Instant::now() < stdout_deadline,
            "attached stdout did not close"
        );
        crate::process::wait_readable(&output);
    }
    assert!(
        stdout_bytes.is_empty(),
        "cancelled attached run leaves stdout empty"
    );
    assert_eq!(crate::process::terminal_settings(&input), before);
    assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 5);
    let connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
    let session: String = connection
        .query_row("SELECT DISTINCT session_id FROM events", [], |row| {
            row.get(0)
        })
        .unwrap();
    let session_id = SessionId::from_str(&session).unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let events = runtime.block_on(store.load_session(session_id)).unwrap();
    runtime.block_on(store.close()).unwrap();
    let view = SessionView::replay(session_id, &events).unwrap().unwrap();
    (session_id, view.goals[0].id)
}

#[cfg(target_os = "linux")]
fn attached_pty_refusal(
    temp: &Path,
    workspace: &Path,
    state: &Path,
    account_root: Option<&Path>,
    refusal: &[u8],
) {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::io::Write;
    use std::time::{Duration, Instant};

    let shell = "stty rows 24 cols 80; /arany --screen-reader --no-color --profile coding --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" answer";
    let mut command = super::isolation::isolated_script(temp);
    command
        .env_clear()
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env("SHELL", "/bin/sh")
        .envs(account_root.map(|root| ("ARANY_TEST_ACCOUNT_ROOT", root)))
        .env("TERM", "dumb")
        .current_dir(workspace)
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
    let mut answered = super::session_picker::PtyResponses::manual_consent();
    fn wait(
        output: &mut std::process::ChildStdout,
        input: &mut std::process::ChildStdin,
        transcript: &mut Vec<u8>,
        answered: &mut super::session_picker::PtyResponses,
        needle: &[u8],
    ) {
        // Watchdog only: it aborts a hung child and never decides the assertion.
        let watchdog = Instant::now() + Duration::from_secs(120);
        while !transcript.windows(needle.len()).any(|part| part == needle) {
            super::session_picker::pump(output, input, transcript, answered);
            assert!(
                Instant::now() < watchdog,
                "missing {}: {}",
                String::from_utf8_lossy(needle),
                super::session_picker::tail(transcript)
            );
            std::thread::yield_now();
        }
    }
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Do you trust this folder?",
    );
    assert!(
        !transcript
            .windows(b"refused".len())
            .any(|part| part == b"refused"),
        "roster admission must wait for folder consent"
    );
    input.write_all(b"read only\r").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        refusal,
    );
    assert_eq!(
        std::fs::metadata(state.join("events.sqlite3"))
            .unwrap()
            .len(),
        0
    );
}

#[cfg(all(target_os = "macos", debug_assertions))]
fn attached_pty_refusal(
    temp: &Path,
    workspace: &Path,
    state: &Path,
    account_root: Option<&Path>,
    refusal: &[u8],
) {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::time::{Duration, Instant};

    let geometry = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (input, slave) = crate::process::cloexec_pty(Some(&geometry));
    let master = input.try_clone().unwrap();
    let before = crate::process::terminal_settings(&input);
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env(
            "ARANY_TEST_ACCOUNT_ROOT",
            account_root.unwrap_or(&temp.join("account")),
        )
        .env("HOME", temp.join("home"))
        .env("XDG_STATE_HOME", temp.join("xdg-state"))
        .env("XDG_DATA_HOME", temp.join("xdg-data"))
        .env("TERM", "dumb")
        .current_dir(workspace)
        .args(["--screen-reader", "--no-color", "--profile", "coding"])
        .args(["--state-dir"])
        .arg(state)
        .arg("--workspace")
        .arg(workspace)
        .arg("answer")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(slave));
    super::active_terminal::own_terminal(&mut command);
    let child = command.spawn().unwrap();
    drop(command);
    let (mut child, mut master, mut input) =
        (super::loopback::ChildGuard::new(child), master, input);
    let flags = fcntl_getfl(&master).unwrap();
    fcntl_setfl(&master, flags | OFlags::NONBLOCK).unwrap();
    let mut stdout = child.child().stdout.take().unwrap();
    let stdout_flags = fcntl_getfl(&stdout).unwrap();
    fcntl_setfl(&stdout, stdout_flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut stdout_bytes = Vec::new();
    let mut consent_declined = false;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(120);
    let mut first_byte = None;
    loop {
        let closed = drain_attached_output(&mut master, &mut transcript, "attached PTY");
        if first_byte.is_none() && !transcript.is_empty() {
            first_byte = Some(Instant::now());
        }
        drain_attached_output(&mut stdout, &mut stdout_bytes, "attached stdout");
        let status = child.child().try_wait().unwrap();
        if (closed || status.is_some()) && !consent_declined {
            crate::process::decline_workspace_consent(
                &mut input,
                &transcript,
                &mut consent_declined,
            );
            assert!(
                consent_declined,
                "{}",
                crate::process::terminal_wait_report(
                    "attached child ended before folder consent",
                    started,
                    first_byte,
                    &transcript,
                    status,
                )
            );
        }
        crate::process::decline_workspace_consent(&mut input, &transcript, &mut consent_declined);
        if transcript
            .windows(b"Do you trust this folder?".len())
            .any(|part| part == b"Do you trust this folder?")
        {
            assert!(
                !transcript
                    .windows(refusal.len())
                    .any(|part| part == refusal),
                "account refusal follows folder consent"
            );
        }
        if consent_declined {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "{}",
                crate::process::terminal_wait_report(
                    "attached folder consent prompt missing",
                    started,
                    first_byte,
                    &transcript,
                    child.child().try_wait().unwrap(),
                )
            );
        }
        crate::process::wait_readable(&master);
    }
    while !transcript
        .windows(refusal.len())
        .any(|part| part == refusal)
    {
        if drain_attached_output(&mut master, &mut transcript, "attached PTY") {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "{}",
                crate::process::terminal_wait_report(
                    "attached roster refusal missing",
                    started,
                    first_byte,
                    &transcript,
                    child.child().try_wait().unwrap(),
                )
            );
        }
        crate::process::wait_readable(&master);
    }
    assert!(
        transcript
            .windows(refusal.len())
            .any(|part| part == refusal)
    );
    fcntl_setfl(&master, flags).unwrap();
    let output = crate::process::capture_terminal(
        child.child(),
        master,
        Duration::from_secs(10),
        64 * 1024,
        |_| {},
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout_deadline = Instant::now() + Duration::from_secs(5);
    while !drain_attached_output(&mut stdout, &mut stdout_bytes, "attached stdout") {
        assert!(
            Instant::now() < stdout_deadline,
            "attached stdout did not close"
        );
        std::thread::yield_now();
    }
    assert!(
        stdout_bytes.is_empty(),
        "attached refusal leaves stdout empty"
    );
    assert_eq!(crate::process::terminal_settings(&input), before);
    assert_eq!(
        std::fs::metadata(state.join("events.sqlite3"))
            .unwrap()
            .len(),
        0
    );
}

#[test]
#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn attached_profile_admission_waits_for_folder_consent() {
    let temp = tempdir();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    StateRoot::admit(&state).unwrap();
    private_file(
        &state.join("provider-roster.json"),
        &serde_json::to_vec(&record()).unwrap(),
    );
    attached_pty_refusal(
        temp.path(),
        &workspace,
        &state,
        None,
        b"roster profile main route 0 refused",
    );
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
mod journey {
    use super::*;
    use arany::{GoalId, GoalStatus, RunStatus, SessionView, StateRoot, Store};
    use std::{
        io::Read,
        net::TcpListener,
        str::FromStr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    fn preset(child_profiles: &[&str]) -> Vec<u8> {
        let route =
            json!({"provider":"custom:local","model":"model-1","effort":null,"account_id":null});
        serde_json::to_vec(&json!({"version":1,"profiles":[
            {"name":"main","routes":[route]},
            {"name":"inspect","routes":[route]}
        ],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":child_profiles}]})).unwrap()
    }

    fn delegate_to_inspect() -> serde_json::Value {
        json!({"outcome":{"type":"delegate","children":[{"objective":"inspect it","profile":"inspect"}]}})
    }

    fn serve(
        listener: TcpListener,
        served: Arc<AtomicUsize>,
        held: mpsc::Sender<()>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            listener.set_nonblocking(true).expect("bounded accept");
            for index in 0..8 {
                let mut stream = crate::process::accept_with_deadline(
                    &listener,
                    Duration::from_secs(60),
                    "roster request",
                );
                let body: serde_json::Value =
                    serde_json::from_slice(&super::super::loopback::read_request(&mut stream))
                        .expect("request JSON");
                served.fetch_add(1, Ordering::SeqCst);
                let input = crate::semantic::input(&body);
                let text = match index {
                    0 => json!("ok"),
                    1 => {
                        json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                    }
                    2 => json!(r#"{"summary":"synthetic summary"}"#),
                    3 | 5 => {
                        assert_eq!(input["phase"], "root_plan");
                        assert!(body.to_string().contains("\"inspect\""));
                        delegate_to_inspect()
                    }
                    4 => {
                        assert_eq!(input["phase"], "child_work");
                        held.send(()).expect("observed child request");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(30)))
                            .expect("hold deadline");
                        // Returns when exec drops the connection after cancellation.
                        let _ = stream.read(&mut [0; 1]);
                        continue;
                    }
                    6 => {
                        assert_eq!(input["phase"], "child_work");
                        json!("child result")
                    }
                    _ => {
                        assert_eq!(input["phase"], "root_synthesis");
                        json!("resumed answer")
                    }
                };
                super::super::loopback::send_response_for(
                    &mut stream,
                    index,
                    text,
                    body["stream"] == true,
                );
            }
        })
    }

    fn exec(workspace: &Path, state: &Path, extra: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
            .current_dir(workspace)
            .args(["exec", "--state-dir"])
            .arg(state)
            .arg("--workspace")
            .arg(workspace)
            .args([
                "--provider",
                "custom:local",
                "--model",
                "model-1",
                "--collaboration",
                "team",
            ])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn exec_leg(
        workspace: &Path,
        state: &Path,
        held_rx: &mpsc::Receiver<()>,
        served: &AtomicUsize,
    ) -> (SessionId, GoalId) {
        let mut child = super::super::loopback::ChildGuard::new(
            exec(workspace, state, &["--profile", "coding", "answer"])
                .spawn()
                .unwrap(),
        );
        held_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("child request before interrupt");
        let pid = child.child().id().to_string();
        assert!(
            Command::new("kill")
                .args(["-INT", &pid])
                .status()
                .unwrap()
                .success()
        );
        let output =
            super::super::process::capture(child.child(), Duration::from_secs(10), 64 * 1024);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(stderr.contains("Status: cancelled\n"), "{stderr}");
        let hint = stderr
            .lines()
            .find(|line| line.starts_with("Goal paused; resume with arany exec --profile coding "))
            .unwrap_or_else(|| panic!("selector-bearing resume hint: {stderr}"));
        let words: Vec<&str> = hint.split_whitespace().collect();
        let session_id = SessionId::from_str(words[9]).unwrap();
        let goal_id = GoalId::from_str(words[11]).unwrap();
        assert_eq!(served.load(Ordering::SeqCst), 5);
        (session_id, goal_id)
    }

    #[cfg(target_os = "linux")]
    fn attached_leg(
        temp: &Path,
        workspace: &Path,
        state: &Path,
        held_rx: &mpsc::Receiver<()>,
        served: &AtomicUsize,
    ) -> (SessionId, GoalId) {
        use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
        use std::{io::Write, time::Instant};

        let shell = "stty rows 24 cols 80; exec \"$ARANY_TEST_EXE\" --screen-reader --no-color --profile coding --provider custom:local --model model-1 --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"";
        let mut command = crate::isolation::account_isolated_script(temp);
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", state)
            .env("ARANY_TEST_WORKSPACE", workspace)
            .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
            .env("SHELL", "/bin/sh")
            .env("TERM", "dumb")
            .current_dir(workspace)
            .args(["-q", "-e", "-c", shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = super::super::loopback::ChildGuard::new(command.spawn().unwrap());
        let mut input = child.child().stdin.take().unwrap();
        let mut output = child.child().stdout.take().unwrap();
        let flags = fcntl_getfl(&output).unwrap();
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
        let mut transcript = Vec::new();
        let mut answered = super::super::session_picker::PtyResponses::default();
        // Watchdog only: it aborts a hung child and never decides the assertion.
        let watchdog = Instant::now() + Duration::from_secs(120);
        let mut pump = |input: &mut std::process::ChildStdin, transcript: &mut Vec<u8>| {
            super::super::session_picker::pump(&mut output, input, transcript, &mut answered);
            assert!(
                Instant::now() < watchdog,
                "attached journey stalled: {}",
                super::super::session_picker::tail(transcript)
            );
            thread::yield_now();
        };
        for (needle, send) in [
            (&b"Input:"[..], &b"/agents team 1\r"[..]),
            (&b"Next-Run collaboration: team"[..], &b"answer\r"[..]),
        ] {
            while !transcript.windows(needle.len()).any(|part| part == needle) {
                pump(&mut input, &mut transcript);
            }
            input.write_all(send).unwrap();
        }
        while held_rx.try_recv().is_err() {
            pump(&mut input, &mut transcript);
        }
        input.write_all(b"\x03").unwrap();
        for (needle, send) in [
            (&b"Goal paused"[..], &b"\x03"[..]),
            (&b"Ctrl+C again exits"[..], &b"\x03"[..]),
        ] {
            while !transcript.windows(needle.len()).any(|part| part == needle) {
                pump(&mut input, &mut transcript);
            }
            input.write_all(send).unwrap();
        }
        let status = loop {
            if let Some(status) = child.child().try_wait().unwrap() {
                break status;
            }
            pump(&mut input, &mut transcript);
        };
        assert!(
            status.success(),
            "attached exit: {status:?}: {}",
            super::super::session_picker::tail(&transcript)
        );
        assert_eq!(served.load(Ordering::SeqCst), 5);
        let connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
        let session: String = connection
            .query_row("SELECT DISTINCT session_id FROM events", [], |row| {
                row.get(0)
            })
            .unwrap();
        let session_id = SessionId::from_str(&session).unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let events = runtime.block_on(store.load_session(session_id)).unwrap();
        runtime.block_on(store.close()).unwrap();
        let view = SessionView::replay(session_id, &events).unwrap().unwrap();
        (session_id, view.goals[0].id)
    }

    fn cancel_then_resume(attached: bool) {
        let temp = tempdir();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        super::super::loopback::write_profile(&state, port);
        let roster = state.join("provider-roster.json");
        private_file(&roster, &preset(&["main", "inspect"]));
        let served = Arc::new(AtomicUsize::new(0));
        let (held_tx, held_rx) = mpsc::channel();
        let server = serve(listener, served.clone(), held_tx);
        super::super::loopback::check_profile(&workspace, &state);

        #[cfg(target_os = "linux")]
        let (session_id, goal_id) = if attached {
            attached_leg(temp.path(), &workspace, &state, &held_rx, &served)
        } else {
            exec_leg(&workspace, &state, &held_rx, &served)
        };
        #[cfg(all(target_os = "macos", debug_assertions))]
        let (session_id, goal_id) = if attached {
            super::attached_leg_macos(temp.path(), &workspace, &state, &held_rx, &served)
        } else {
            exec_leg(&workspace, &state, &held_rx, &served)
        };
        #[cfg(not(any(target_os = "linux", all(target_os = "macos", debug_assertions))))]
        let (session_id, goal_id) = {
            assert!(!attached);
            exec_leg(&workspace, &state, &held_rx, &served)
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let replay = || {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = runtime.block_on(store.load_session(session_id)).unwrap();
            runtime.block_on(store.close()).unwrap();
            SessionView::replay(session_id, &events).unwrap().unwrap()
        };
        let view = replay();
        assert_eq!(view.runs[0].status, RunStatus::Cancelled);
        assert_eq!(view.runs[0].agents[1].profile.as_deref(), Some("inspect"));
        assert_eq!(view.goal(goal_id).unwrap().status, GoalStatus::Interrupted);

        let resume = |profile: bool| {
            let mut extra = vec![];
            if profile {
                extra.extend(["--profile", "coding"]);
            }
            let session_id = session_id.to_string();
            let goal_id = goal_id.to_string();
            let mut command = exec(&workspace, &state, &extra);
            command.args(["--session-id", &session_id, "--resume-goal", &goal_id]);
            super::super::loopback::wait_product(command.spawn().unwrap())
        };
        for (name, profile, edited) in [("omitted", false, false), ("drifted", true, true)] {
            if edited {
                private_file(&roster, &preset(&["main"]));
            }
            let refused = resume(profile);
            let stderr = String::from_utf8(refused.stderr).unwrap();
            assert_eq!(refused.status.code(), Some(1), "{name}: {stderr}");
            assert!(refused.stdout.is_empty(), "{name}");
            assert!(
                stderr.ends_with("error: restore the matching --profile preset to resume this Goal, or start a new Goal\n"),
                "{name}: {stderr}"
            );
            assert_eq!(served.load(Ordering::SeqCst), 5, "{name}: no request");
        }
        private_file(&roster, &preset(&["main", "inspect"]));
        let resumed = resume(true);
        assert!(
            resumed.status.success(),
            "{}",
            String::from_utf8_lossy(&resumed.stderr)
        );
        assert!(String::from_utf8_lossy(&resumed.stdout).contains("resumed answer"));
        server.join().unwrap();
        assert_eq!(served.load(Ordering::SeqCst), 8);

        let view = replay();
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.runs[1].status, RunStatus::Finished);
        assert_eq!(view.runs[1].agents[1].profile.as_deref(), Some("inspect"));
        assert_ne!(view.runs[1].agents[1].id, view.runs[0].agents[1].id);
        assert_eq!(view.goal(goal_id).unwrap().status, GoalStatus::Completed);
        assert_eq!(
            std::fs::read(&roster).unwrap(),
            preset(&["main", "inspect"])
        );
    }

    #[test]
    fn exec_cancellation_resumes_only_with_the_same_preset_and_keeps_assignment_provenance() {
        cancel_then_resume(false);
    }

    #[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
    #[test]
    fn attached_cancellation_resumes_through_exec_only_with_the_same_preset() {
        cancel_then_resume(true);
    }
}
