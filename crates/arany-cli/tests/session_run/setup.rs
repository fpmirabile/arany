#[cfg(any(debug_assertions, target_os = "linux"))]
use super::active_terminal::{ProductGuard, product_child_of_executable};
#[cfg(debug_assertions)]
use super::active_terminal::{product_child_of, tty_settings, wait_stopped};
use super::loopback::{ChildGuard, wait_product};
use super::process::BoundedOutput;
use super::session_picker::{pump, tail};
use arany::{RecordKind, SessionView, StateRoot, Store};
#[cfg(any(debug_assertions, target_os = "linux"))]
use arany::{SessionDefaults, create_session, set_session_defaults};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
#[cfg(any(debug_assertions, target_os = "linux"))]
use rustix::process::{Signal, kill_process};
#[cfg(debug_assertions)]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixListener;
#[cfg(debug_assertions)]
use std::process::{ChildStdin, ChildStdout, Output};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const API_ACCOUNTS_PROMPT: &[u8] =
    b"Credentials belong to this OS user; type a listed choice number or exact name (1-6); empty Enter selects 1\r\nInput:\r\n";

#[cfg(debug_assertions)]
const PRIVATE_FILE_PROMPT: &[u8] =
    b"Choose: NOT encrypted; same-user apps read key; type a choice name: 1. Cancel or 2. Use private file; empty Enter selects 1. Cancel\r\nInput:\r\n";

#[cfg(debug_assertions)]
const PROVIDER_PROMPT: &[u8] =
    b"Choose: Only the selected Provider receives requests; type a choice name: 1. OpenAI or 2. Anthropic; empty Enter selects 1. OpenAI\r\nInput:\r\n";

#[path = "setup/offline_https.rs"]
mod offline_https;

#[cfg(debug_assertions)]
#[path = "setup/checks.rs"]
mod checks;

#[cfg(debug_assertions)]
#[path = "setup/routing.rs"]
mod routing;

#[cfg(debug_assertions)]
#[path = "setup/roster.rs"]
mod roster;

#[cfg(debug_assertions)]
const SETUP_SHELL: &str = "trap ':' INT; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; printf 'SCRIPT_INPUT_GATE\n'; IFS= read -r gate; [ \"$gate\" = go ] || exit 2; \"$ARANY_TEST_EXE\" --screen-reader --setup --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";

#[cfg(debug_assertions)]
const MIXED_START_SHELL: &str = "trap ':' INT; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; printf 'SCRIPT_INPUT_GATE\n'; IFS= read -r gate; [ \"$gate\" = go ] || exit 2; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";

#[cfg(all(target_os = "linux", not(debug_assertions)))]
const RELEASE_ACCOUNT_SHELL: &str = r#"before=$(stty -g); printf 'TTY_BEFORE:%s\n' "$before"; printf 'SCRIPT_INPUT_GATE\n'; IFS= read -r gate; [ "$gate" = go ] || exit 2; if [ "$ARANY_TEST_SETUP" = 1 ]; then set -- --setup; else set --; fi; /usr/bin/timeout --foreground -k 1s 20s /usr/bin/bwrap --unshare-user --unshare-net --unshare-pid --die-with-parent --tmpfs / --ro-bind /usr /usr --symlink usr/lib /lib --ro-bind /lib64 /lib64 --ro-bind /etc/passwd /etc/passwd --bind "$ARANY_TEST_HOME" "$ARANY_PASSWD_HOME" --ro-bind "$ARANY_TEST_EXE" /arany --proc /proc --dev-bind /dev /dev --chdir "$ARANY_PASSWD_HOME/workspace" --clearenv --setenv HOME "$ARANY_TEST_HOME_ENV" --setenv XDG_STATE_HOME "$ARANY_TEST_XDG_STATE" --setenv TERM dumb --setenv PATH /usr/bin -- /arany --screen-reader "$@" --state-dir "$ARANY_TEST_STATE" --workspace "$ARANY_PASSWD_HOME/workspace"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' "$after"; exit "$exit_code""#;

#[cfg(debug_assertions)]
fn prepare_test_file_account(root: &Path) -> Uuid {
    super::prepare_test_file_account_at(&root.join("account-root"))
}

#[cfg(debug_assertions)]
fn prepare_selected_chatgpt_account(root: &Path) -> Uuid {
    prepare_selected_chatgpt_account_at(&root.join("account-root"))
}

fn prepare_selected_chatgpt_account_at(account_root: &Path) -> Uuid {
    let state = StateRoot::admit(account_root).expect("test account root");
    let account_id = Uuid::now_v7();
    let record = serde_json::json!({
        "schema": 2,
        "selected": account_id,
        "accounts": [{
            "id": account_id,
            "host_id": "123e4567-e89b-42d3-a456-426614174000",
            "client_id": "oaiapp_synthetic_saved",
            "subject": "synthetic-subject",
            "storage": "keyring",
            "renewal_pending": false, "signout_pending": false,
            "disconnected": false, "plan_permission_missing": false, "token": null
        }], "model_checks": []
    });
    state
        .record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&record).expect("account index"))
        .expect("test ChatGPT account index");
    let registration = serde_json::json!({
        "schema": 1,
        "host_id": "123e4567-e89b-42d3-a456-426614174000",
        "issued_client_id": null
    });
    state
        .record(RecordKind::ChatgptRegistration)
        .replace(&serde_json::to_vec(&registration).expect("registration"))
        .expect("test ChatGPT registration");
    account_id
}

fn wait_for(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    needle: &[u8],
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::process::pty_text_matches(transcript, needle)
        .next()
        .is_none()
    {
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "setup PTY stage missing: {}",
            tail(transcript)
        );
        thread::yield_now();
    }
}

fn wait_for_after(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    start: usize,
    needle: &[u8],
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::process::pty_text_matches(&transcript[start..], needle)
        .next()
        .is_none()
    {
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "setup PTY stage missing: {}",
            tail(transcript)
        );
        thread::yield_now();
    }
}

#[cfg(debug_assertions)]
fn reviewed_setup_text(transcript: &str) -> String {
    transcript
        .lines()
        .filter_map(|line| {
            line.strip_prefix("Choose: Page ")
                .and_then(|page| page.split_once("\\u{000a}").map(|(_, body)| body))
        })
        .collect()
}

fn wait_linear_page(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    (start, current): (usize, usize),
) -> usize {
    let marker = format!("Page {current}/");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(after) = String::from_utf8_lossy(&transcript[start..])
            .split_once(&marker)
            .and_then(|(_, rest)| rest.split_once("\r\n").map(|(line, _)| line))
            && let Ok(total) = after
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<usize>()
        {
            assert!((current..=128).contains(&total), "bounded setup pages");
            return total;
        }
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "setup review page incomplete: {}",
            tail(transcript)
        );
        thread::yield_now();
    }
}

fn acknowledge_linear_pages(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    (start, next): (usize, &[u8]),
) -> (usize, usize) {
    let total = wait_linear_page(output, input, transcript, answered, (start, 1));
    let mut next_at = transcript.len();
    for current in 1..=total {
        next_at = transcript.len();
        input.write_all(b"\r").expect("acknowledge setup page");
        if current < total {
            assert_eq!(
                wait_linear_page(output, input, transcript, answered, (next_at, current + 1)),
                total
            );
        }
    }
    wait_for_after(output, input, transcript, answered, next_at, next);
    if next != b"Page 1/" {
        wait_for_after(output, input, transcript, answered, next_at, b"Input:\r\n");
    }
    (total, next_at)
}

#[cfg(debug_assertions)]
const FIRST_ROUTE_KEY_CANARY: &str = "first-route-credential-canary";

#[cfg(debug_assertions)]
struct SetupPty {
    child: ChildGuard,
    input: ChildStdin,
    output: ChildStdout,
    transcript: Vec<u8>,
    answered: crate::session_picker::PtyResponses,
    _product: ProductGuard,
}

#[cfg(debug_assertions)]
fn start_setup_pty(temp: &tempfile::TempDir, workspace: &Path, state: &Path) -> SetupPty {
    start_setup_pty_mode(temp, workspace, state, SetupPtyMode::Manager)
}

#[cfg(debug_assertions)]
enum SetupPtyMode {
    Manager,
    Configured,
    InlineManager,
    FullscreenManager,
    Resumed(arany::SessionId),
    ResumedInline(arany::SessionId),
}

#[cfg(debug_assertions)]
fn start_setup_pty_mode(
    temp: &tempfile::TempDir,
    workspace: &Path,
    state: &Path,
    mode: SetupPtyMode,
) -> SetupPty {
    start_setup_pty_with_key(temp, workspace, state, mode, Some("test-key"))
}

#[cfg(debug_assertions)]
fn start_setup_pty_with_key(
    temp: &tempfile::TempDir,
    workspace: &Path,
    state: &Path,
    mode: SetupPtyMode,
    key: Option<&str>,
) -> SetupPty {
    let mut command = setup_pty_command(temp, workspace, state, &mode, key);
    let mut child = ChildGuard::new(command.spawn().expect("setup process"));
    let mut input = child.child().stdin.take().expect("setup input");
    let mut output = child.child().stdout.take().expect("setup output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    if matches!(mode, SetupPtyMode::Configured) {
        enter_conversation_setup(&mut output, &mut input, &mut transcript, &mut answered);
    }
    if matches!(
        mode,
        SetupPtyMode::InlineManager | SetupPtyMode::FullscreenManager
    ) {
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Provider setup",
        );
        let frame = transcript
            .windows(b"Provider setup".len())
            .position(|part| part == b"Provider setup")
            .unwrap();
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            frame,
            b"\x1b[?25l",
        );
    } else {
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"empty Enter selects 1. API accounts\r\nInput:\r\n",
        );
    }
    let shell_pid = String::from_utf8_lossy(&transcript)
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
        .expect("shell PID")
        .parse::<u32>()
        .expect("shell PID number");
    let product = ProductGuard::new(product_child_of(shell_pid), state.to_owned());
    SetupPty {
        child,
        input,
        output,
        transcript,
        answered,
        _product: product,
    }
}

#[cfg(debug_assertions)]
fn setup_pty_command(
    temp: &tempfile::TempDir,
    workspace: &Path,
    state: &Path,
    mode: &SetupPtyMode,
    key: Option<&str>,
) -> Command {
    let arguments = match mode {
        SetupPtyMode::Manager => "--screen-reader --setup".to_owned(),
        SetupPtyMode::InlineManager => "--classic --no-color --setup".to_owned(),
        SetupPtyMode::FullscreenManager => "--no-color --setup".to_owned(),
        SetupPtyMode::Configured => {
            "--screen-reader --provider custom:local --model model-1".to_owned()
        }
        SetupPtyMode::Resumed(id) => format!("--screen-reader --resume {id}"),
        SetupPtyMode::ResumedInline(id) => format!("--classic --no-color --resume {id}"),
    };
    let shell = format!(
        "trap ':' INT; stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" {arguments} --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut command = Command::new("/usr/bin/script");
    command
        .args(["-q", "-e", "-c", &shell, "/dev/null"])
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", temp.path().join("missing-bus").display()),
        )
        .env("ARANY_PROVIDER_FIRST_ROUTE_KEY", FIRST_ROUTE_KEY_CANARY)
        .env("SHELL", "/bin/sh")
        .env(
            "TERM",
            if matches!(
                mode,
                SetupPtyMode::InlineManager
                    | SetupPtyMode::FullscreenManager
                    | SetupPtyMode::ResumedInline(_)
            ) {
                "xterm-256color"
            } else {
                "dumb"
            },
        )
        .env(
            "ARANY_PROVIDER_UNSELECTED_KEY",
            "unselected-credential-canary",
        )
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(key) = key {
        command.env("ARANY_PROVIDER_LOCAL_KEY", key);
    }
    command
}

#[cfg(debug_assertions)]
fn enter_conversation_setup(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
) {
    wait_for(output, input, transcript, answered, b"Input:\r\n");
    let policy_at = transcript.len();
    input
        .write_all(b"/agents single\r")
        .expect("select single-agent policy");
    wait_for_after(
        output,
        input,
        transcript,
        answered,
        policy_at,
        b"Next-Run collaboration: single",
    );
    wait_for_after(
        output,
        input,
        transcript,
        answered,
        policy_at,
        b"Input:\r\n",
    );
    let rename_at = transcript.len();
    input
        .write_all(b"/rename Setup retained title\r")
        .expect("name unsaved conversation");
    wait_for_after(
        output,
        input,
        transcript,
        answered,
        rename_at,
        b"Input:\r\n",
    );
    input
        .write_all(b"/setup\r")
        .expect("open conversation setup");
}

#[cfg(debug_assertions)]
fn finish_setup_pty(pty: SetupPty) -> Output {
    finish_setup_pty_with_transcript(pty).0
}

#[cfg(debug_assertions)]
fn finish_setup_pty_with_transcript(mut pty: SetupPty) -> (Output, Vec<u8>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while pty
        .child
        .child()
        .try_wait()
        .expect("product status")
        .is_none()
    {
        pump(
            &mut pty.output,
            &mut pty.input,
            &mut pty.transcript,
            &mut pty.answered,
        );
        assert!(Instant::now() < deadline, "setup product exit deadline");
        thread::yield_now();
    }
    let result = wait_product(pty.child.take());
    crate::drain_pty(&mut pty.output, &mut pty.transcript);
    (result, pty.transcript)
}

fn choose_native_account_setup(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
) {
    wait_for(
        output,
        input,
        transcript,
        answered,
        b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
    );
    input
        .write_all(b"API accounts\r")
        .expect("open API accounts");
    acknowledge_linear_pages(
        output,
        input,
        transcript,
        answered,
        (0, API_ACCOUNTS_PROMPT),
    );
    input
        .write_all(b"Add or replace native API account\r")
        .expect("add native API account");
}

#[cfg(debug_assertions)]
#[test]
fn bare_start_reuses_private_file_account_without_setup_or_run() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    prepare_test_file_account(temp.path());
    let shell = "printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false")
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("saved-account PTY process"));
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
    let text = String::from_utf8_lossy(&transcript);
    let shell_pid = text
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
        .expect("shell PID")
        .parse::<u32>()
        .expect("shell PID number");
    let product_pid = product_child_of(shell_pid);
    let _product = ProductGuard::new(product_pid, state.clone());
    input.write_all(b"/exit\r").expect("exit Session");
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "saved-account startup failed");
    assert!(result.stderr.is_empty(), "script wrapper stderr");
    assert_eq!(
        answered.cursor_queries, 0,
        "screen-reader mode uses no cursor query"
    );
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(!text.contains("Provider setup"), "setup was reopened");
    assert!(
        !text.contains("synthetic-existing-key"),
        "key entered output"
    );
    assert!(!text.contains('\x1b'), "screen-reader output has escapes");
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let sessions = arany::list_sessions(
            StateRoot::open_existing(&state).expect("existing State"),
            workspace.clone(),
        )
        .await
        .expect("Workspace Sessions");
        assert!(
            sessions.is_empty(),
            "startup, local rejection and setup navigation save no empty conversation"
        );
    });
}

#[cfg(debug_assertions)]
#[test]
fn bare_start_with_both_accounts_requires_an_explicit_billing_route() {
    for (choice, corrupt_native, corrupt_chatgpt, provider, opens_chat) in [
        (Some("API key"), false, true, Some("openai"), true),
        (Some("ChatGPT plan"), true, false, Some("chatgpt"), true),
        (Some(""), true, true, None, false),
        (Some("Cancel"), true, true, None, false),
        (None, false, false, None, false),
        (Some("API key"), true, false, None, true),
        (Some("ChatGPT plan"), false, true, None, true),
    ] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        prepare_test_file_account(temp.path());
        prepare_selected_chatgpt_account(temp.path());
        if corrupt_native {
            StateRoot::open_existing(&temp.path().join("account-root"))
                .expect("account root")
                .record(RecordKind::SavedAccount)
                .replace(b"invalid unselected account JSON")
                .expect("corrupt unselected native record");
        }
        if corrupt_chatgpt {
            StateRoot::open_existing(&temp.path().join("account-root"))
                .expect("account root")
                .record(RecordKind::ChatgptAccounts)
                .replace(b"invalid unselected ChatGPT JSON")
                .expect("corrupt unselected ChatGPT index");
        }
        let account_root = StateRoot::open_existing(&temp.path().join("account-root"))
            .expect("protected account root");
        let native_before = account_root
            .record(RecordKind::SavedAccount)
            .read()
            .expect("native account snapshot");
        let chatgpt_before = account_root
            .record(RecordKind::ChatgptAccounts)
            .read()
            .expect("ChatGPT account snapshot");
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", temp.path().join("missing-bus").display()),
            )
            .env("SHELL", "/bin/sh")
            .env("TERM", "dumb")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", MIXED_START_SHELL, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("mixed-account PTY"));
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
            b"SCRIPT_INPUT_GATE",
        );
        input.write_all(b"go\r").expect("start product");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Setup: Choose saved access",
        );
        let shell_pid = String::from_utf8_lossy(&transcript)
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse::<u32>()
            .expect("shell PID number");
        let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Input:\r\n",
        );
        assert!(String::from_utf8_lossy(&transcript).contains(
            "type a choice name: 1. Cancel or 2. API key or 3. ChatGPT plan; empty Enter selects 1. Cancel"
        ));
        let choice_at = transcript.len();
        match choice {
            Some(name) => {
                input
                    .write_all(name.as_bytes())
                    .expect("choose billing route");
                input.write_all(b"\r").expect("submit billing route");
                if opens_chat {
                    wait_for_after(
                        &mut output,
                        &mut input,
                        &mut transcript,
                        &mut answered,
                        choice_at,
                        b"Input:\r\n",
                    );
                    input.write_all(b"/exit\r").expect("exit Session");
                }
            }
            None => input.write_all(b"\x03").expect("cancel billing choice"),
        }
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(result.status.success(), "{choice:?}: {}", result.status);
        assert!(result.stderr.is_empty(), "script wrapper stderr");
        assert_eq!(
            answered.cursor_queries, 0,
            "screen-reader mode queried the cursor"
        );
        let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
        assert!(text.contains("Both saved; choose billing route"));
        // Invalid selected-route data reopens unconfigured chat for explicit recovery.
        assert_eq!(
            text.contains("Saved access could not be restored"),
            opens_chat && provider.is_none(),
            "{choice:?}: {text}"
        );
        assert!(!text.contains("use arany --setup"), "{choice:?}: {text}");
        assert!(!text.contains("synthetic-existing-key"));
        assert!(!text.contains("invalid unselected account JSON"));
        assert!(!text.contains("invalid unselected ChatGPT JSON"));
        assert!(!text.contains('\x1b'), "screen-reader output has escapes");
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("terminal marker")
                .to_owned()
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
        assert!(
            account_root
                .record(RecordKind::SavedAccount)
                .read()
                .expect("unchanged native record")
                == native_before
        );
        assert!(
            account_root
                .record(RecordKind::ChatgptAccounts)
                .read()
                .expect("unchanged ChatGPT record")
                == chatgpt_before
        );
        if opens_chat {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("read-only runtime");
            runtime.block_on(async {
                let root = StateRoot::open_existing(&state).expect("existing State");
                let sessions = arany::list_sessions(root, workspace.clone())
                    .await
                    .expect("Workspace Sessions");
                assert!(
                    sessions.is_empty(),
                    "startup, local rejection and setup navigation save no empty conversation"
                );
            });
        } else {
            assert!(
                !state.exists(),
                "declined or invalid choice created a Session"
            );
        }
    }
}

#[cfg(debug_assertions)]
#[test]
fn setup_reuse_and_reconnect_choices_preserve_selected_chatgpt_account_on_failure() {
    for (choice, disconnected, other_saved) in [
        ("Use saved", false, false),
        ("Reconnect", false, false),
        ("Reconnect", true, false),
        ("Use saved", true, true),
    ] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        let selected = prepare_selected_chatgpt_account(temp.path());
        let other = Uuid::now_v7();
        if disconnected || other_saved {
            let account_root =
                StateRoot::open_existing(&temp.path().join("account-root")).expect("account root");
            let bytes = account_root
                .record(RecordKind::ChatgptAccounts)
                .read()
                .expect("account index")
                .expect("selected account metadata");
            let mut index: serde_json::Value =
                serde_json::from_slice(&bytes).expect("account index JSON");
            index["accounts"][0]["disconnected"] = disconnected.into();
            if other_saved {
                index["accounts"]
                    .as_array_mut()
                    .expect("accounts")
                    .push(serde_json::json!({
                        "id": other,
                        "host_id": "123e4567-e89b-42d3-a456-426614174000",
                        "client_id": "oaiapp_synthetic_other",
                        "subject": "synthetic-other-subject",
                        "storage": "keyring",
                        "renewal_pending": false,
                        "signout_pending": false,
                        "disconnected": false,
                        "plan_permission_missing": false,
                        "token": null
                    }));
            }
            account_root
                .record(RecordKind::ChatgptAccounts)
                .replace(&serde_json::to_vec(&index).expect("account index"))
                .expect("disconnected account index");
        }
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", temp.path().join("missing-bus").display()),
            )
            .env("SHELL", "/bin/sh")
            .env("TERM", "dumb")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", SETUP_SHELL, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("saved ChatGPT setup PTY"));
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
            b"SCRIPT_INPUT_GATE",
        );
        input.write_all(b"go\r").expect("start product");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
        );
        let text = String::from_utf8_lossy(&transcript);
        let shell_pid = text
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse::<u32>()
            .expect("shell PID number");
        let product_pid = product_child_of(shell_pid);
        let _product = ProductGuard::new(product_pid, state.clone());
        input.write_all(b"API accounts\r").expect("open accounts");
        acknowledge_linear_pages(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (0, API_ACCOUNTS_PROMPT),
        );
        input
            .write_all(b"Manage ChatGPT account\r")
            .expect("manage ChatGPT account");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            if disconnected && !other_saved {
                b"type a choice name: 1. Reconnect or 2. Connect new or 3. Inspect; empty Enter selects 1. Reconnect\r\nInput:\r\n"
            } else {
                b"type a choice name: 1. Use saved or 2. Reconnect or 3. Connect new or 4. Inspect; empty Enter selects 1. Use saved\r\nInput:\r\n"
            },
        );
        let chosen_at = transcript.len();
        input
            .write_all(format!("{choice}\r").as_bytes())
            .expect("choose selected account action");
        // Failed setup returns to the unconfigured composer; empty Ctrl+D then exits.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !transcript[chosen_at..]
            .split(|byte| *byte == b'\n')
            .skip_while(|line| !line.starts_with(b"Notice: "))
            .any(|line| line.starts_with(b"Input:"))
        {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "failed setup notice missing: {}",
                tail(&transcript)
            );
            thread::yield_now();
        }
        input.write_all(b"\x04").expect("exit unconfigured chat");
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(result.status.success(), "unconfigured chat exit");
        assert_eq!(
            answered.cursor_queries, 0,
            "linear setup queried the cursor"
        );
        let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
        if choice == "Reconnect" {
            assert!(text.contains("Saved ChatGPT keyring is unavailable"));
            assert!(text.contains("Unlock your desktop password store"));
            assert!(text.contains("Connect new, then Use private file (not encrypted)"));
            assert!(!text.contains("Choose Accept to continue"));
            if disconnected {
                assert!(text.contains("ChatGPT account disconnected"));
                assert!(!text.contains("type a choice name: 1. Use saved"));
            }
        } else if other_saved {
            assert!(text.contains("ChatGPT account disconnected"));
            assert!(text.contains("authorization is unavailable"));
            assert!(text.contains("unlock your desktop password store and retry"));
            assert!(!text.contains("no selected ChatGPT account"));
        } else {
            assert!(text.contains("Model catalog failed"));
            assert!(text.contains("saved ChatGPT account remains selected"));
        }
        assert!(!text.contains("new ChatGPT account was saved"));
        assert!(!text.contains("Waiting for ChatGPT"));
        assert!(!text.contains("Choose Accept to continue"));
        if other_saved {
            assert_failed_chatgpt_selection(&state, &workspace, other);
        } else if let Ok(root) = StateRoot::open_existing(&state) {
            let sessions = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("fixture runtime")
                .block_on(arany::list_sessions(root, workspace.clone()))
                .expect("Session list");
            assert!(sessions.is_empty(), "failed reuse created a Session");
        }
        assert!(!text.contains('\x1b'), "screen-reader output has escapes");
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("terminal marker")
                .to_owned()
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
        let index = StateRoot::open_existing(&temp.path().join("account-root"))
            .expect("account index remains")
            .record(RecordKind::ChatgptAccounts)
            .read()
            .expect("account index read")
            .expect("selected account metadata");
        let index: serde_json::Value = serde_json::from_slice(&index).expect("account index JSON");
        assert_eq!(index["selected"], selected.to_string());
        assert_eq!(
            index["accounts"].as_array().map(Vec::len),
            Some(1 + usize::from(other_saved))
        );
        assert_eq!(
            index["accounts"][0]["disconnected"]
                .as_bool()
                .unwrap_or(false),
            disconnected
        );
    }
}

#[cfg(debug_assertions)]
fn assert_failed_chatgpt_selection(state: &Path, workspace: &Path, target_id: Uuid) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime")
        .block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(state).expect("setup StateRoot"),
                workspace.to_owned(),
            )
            .await
            .expect("setup Session list");
            assert_eq!(
                sessions.len(),
                1,
                "confirmed selection materializes a Session"
            );
            let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
            let events = store.load_session(sessions[0].id).await.unwrap();
            assert_eq!(
                events.len(),
                3,
                "only Session start and the failed edit pair"
            );
            assert!(matches!(
                events[0].event,
                arany::Event::SessionStarted { .. }
            ));
            let operation_id = match &events[1].event {
                arany::Event::ProviderSetupEditStarted {
                    operation_id,
                    target,
                    expected_digest,
                } => {
                    assert_eq!(target.kind, arany::ProviderSetupTargetKind::ChatGptAccount);
                    assert_eq!(target.identity, target_id.to_string());
                    assert_eq!(*expected_digest, None);
                    *operation_id
                }
                event => panic!("unexpected selection Event: {}", event.kind()),
            };
            assert!(matches!(
                &events[2].event,
                arany::Event::ProviderSetupEditFinished {
                    operation_id: finished,
                    disposition: arany::ProviderSetupDisposition::Failed,
                    resulting_digest: None,
                } if *finished == operation_id
            ));
            let view = SessionView::replay(sessions[0].id, &events)
                .expect("strict replay")
                .expect("setup Session");
            assert_eq!(view.defaults, SessionDefaults::default());
            assert!(view.runs.is_empty(), "failed selection creates no Run");
            assert_eq!(view.pending_provider_setup_edit, None);
            assert!(!view.provider_setup_uncertain);
            store.close().await.expect("close setup Store");
        });
}

#[cfg(debug_assertions)]
#[test]
fn saved_chatgpt_picker_lists_account_ids_and_cancels_cleanly() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let selected = prepare_selected_chatgpt_account(temp.path());
    let account_root =
        StateRoot::open_existing(&temp.path().join("account-root")).expect("account root");
    let bytes = account_root
        .record(RecordKind::ChatgptAccounts)
        .read()
        .expect("account index")
        .expect("selected account metadata");
    let mut index: serde_json::Value = serde_json::from_slice(&bytes).expect("account index JSON");
    let other = Uuid::now_v7();
    index["accounts"]
        .as_array_mut()
        .expect("accounts")
        .push(serde_json::json!({
            "id": other,
            "host_id": "123e4567-e89b-42d3-a456-426614174000",
            "client_id": "oaiapp_synthetic_other",
            "subject": "synthetic-other-subject",
            "storage": "keyring",
            "renewal_pending": false,
            "signout_pending": false,
            "disconnected": false,
            "plan_permission_missing": false,
            "token": null
        }));
    account_root
        .record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&index).expect("two accounts"))
        .expect("save account index");
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", temp.path().join("missing-bus").display()),
        )
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", SETUP_SHELL, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("saved-account picker PTY"));
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
        b"SCRIPT_INPUT_GATE",
    );
    input.write_all(b"go\r").expect("start setup");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
    );
    input.write_all(b"API accounts\r").expect("open accounts");
    acknowledge_linear_pages(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (0, API_ACCOUNTS_PROMPT),
    );
    input
        .write_all(b"Manage ChatGPT account\r")
        .expect("manage ChatGPT account");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"type a choice name: 1. Use saved or 2. Reconnect or 3. Connect new or 4. Inspect; empty Enter selects 1. Use saved\r\nInput:\r\n",
    );
    input
        .write_all(b"Use saved\r")
        .expect("open saved accounts");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"* is current; choose account",
    );
    input.write_all(b"\x03").expect("cancel account choice");
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "cancelled picker failed");
    assert!(result.stderr.is_empty(), "script wrapper stderr");
    assert_eq!(
        answered.cursor_queries, 0,
        "screen-reader picker queried cursor"
    );
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(text.contains(&format!("1* {}", &selected.simple().to_string()[24..])));
    assert!(text.contains(&format!("2  {}", &other.simple().to_string()[24..])));
    assert!(!text.contains("synthetic-subject"));
    assert!(!text.contains("synthetic-other-subject"));
    assert!(!text.contains('\x1b'), "screen-reader output has escapes");
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    assert!(!state.exists(), "cancelled picker created Session State");
    let updated = account_root
        .record(RecordKind::ChatgptAccounts)
        .read()
        .expect("account index")
        .expect("saved accounts");
    let updated: serde_json::Value = serde_json::from_slice(&updated).expect("index JSON");
    assert_eq!(updated["selected"], selected.to_string());
}

#[cfg(debug_assertions)]
#[test]
fn bare_start_without_account_keeps_chat_input_and_offers_named_setup_choices() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let prompt_shell = "before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; if [ -n \"$ARANY_TEST_FORK\" ]; then set -- --fork \"$ARANY_TEST_FORK\"; else set --; fi; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" \"$@\" 'startup objective'; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\"";
    crate::isolation::remember_workspace_trust(
        &temp.path().join("account-root"),
        &workspace,
        &workspace,
    );
    for (name, fork) in [("new", String::new()), ("fork", Uuid::now_v7().to_string())] {
        let prompt_state = temp.path().join(format!("{name}-prompt-state"));
        let result = wait_product(
            Command::new("/usr/bin/script")
                .env_clear()
                .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
                .env("ARANY_TEST_STATE", &prompt_state)
                .env("ARANY_TEST_WORKSPACE", &workspace)
                .env("ARANY_TEST_FORK", &fork)
                .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
                .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", temp.path().join("missing-bus").display()),
                )
                .env("SHELL", "/bin/sh")
                .env("TERM", "dumb")
                .current_dir(&workspace)
                .args(["-q", "-e", "-c", prompt_shell, "/dev/null"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("unconfigured startup-task PTY"),
        );
        assert_eq!(result.status.code(), Some(1), "{name}: ordinary error exit");
        assert!(result.stderr.is_empty(), "{name}: script wrapper stderr");
        let text = std::str::from_utf8(&result.stdout).expect("startup-task UTF-8");
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "{name}: unexpected startup output");
        // A fork validates its source Session before admitting inherited selections.
        let expected = if name == "fork" {
            format!(
                "error: conversation {fork} was not found in this State/Workspace; list conversations with arany --resume or /resume, or check --state-dir"
            )
        } else {
            "error: a prompt requires a selected Provider; run arany and use /setup".to_owned()
        };
        assert_eq!(lines[1], expected, "{name}: actionable admission error");
        let before = lines[0]
            .strip_prefix("TTY_BEFORE:")
            .expect("before settings");
        let after = lines[2].strip_prefix("TTY_AFTER:").expect("after settings");
        assert_eq!(before, after, "{name}: terminal restoration");
        assert!(
            !prompt_state.exists(),
            "{name}: unavailable task created State"
        );
    }
    let state = temp.path().join("state");
    let shell = "trap ':' INT; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", temp.path().join("missing-bus").display()),
        )
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("unconfigured PTY process"));
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
    assert!(
        String::from_utf8_lossy(&transcript).contains("Setup: type /setup to choose an account"),
        "screen-reader startup omitted setup guidance"
    );
    assert!(
        !String::from_utf8_lossy(&transcript).contains("Provider setup"),
        "bare start opened setup"
    );
    let text_at = transcript.len();
    input.write_all(b"visible draft\r").expect("ordinary input");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        text_at,
        b"Use /setup to choose an account",
    );
    let setup_at = transcript.len();
    input.write_all(b"/setup\r").expect("open setup");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        setup_at,
        b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
    );
    input.write_all(b"\x03").expect("cancel setup");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Notice: Setup closed\r\nInput:\r\n",
    );
    input.write_all(b"/exit\r").expect("exit Session");
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "unconfigured chat failed");
    assert!(result.stderr.is_empty(), "script wrapper stderr");
    assert_eq!(
        answered.cursor_queries, 0,
        "screen-reader mode uses no cursor query"
    );
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(
        text.contains("visible draft"),
        "ordinary input was not visible"
    );
    assert!(!text.contains('\x1b'), "screen-reader output has escapes");
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let sessions = arany::list_sessions(
            StateRoot::open_existing(&state).expect("State"),
            workspace.clone(),
        )
        .await
        .expect("Workspace Sessions");
        assert!(
            sessions.is_empty(),
            "startup, local rejection and setup navigation save no empty conversation"
        );
    });
}

#[cfg(debug_assertions)]
#[test]
fn screen_reader_chatgpt_consent_defaults_to_back_before_browser_or_account_state() {
    for accept_file in [false, true] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        let account_root = temp.path().join("account-root");
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", temp.path().join("missing-bus").display()),
            )
            .env("SHELL", "/bin/sh")
            .env("TERM", "dumb")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", SETUP_SHELL, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("screen-reader setup process"));
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
            b"SCRIPT_INPUT_GATE",
        );
        input.write_all(b"go\r").expect("start product");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
        );
        let shell_pid = String::from_utf8_lossy(&transcript)
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse::<u32>()
            .expect("shell PID number");
        let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
        input.write_all(b"API accounts\r").expect("open accounts");
        acknowledge_linear_pages(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (0, API_ACCOUNTS_PROMPT),
        );
        input
            .write_all(b"Manage ChatGPT account\r")
            .expect("manage ChatGPT account");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"empty Enter selects 1. Cancel\r\nInput:\r\n",
        );
        if accept_file {
            let warning_at = transcript.len();
            input
                .write_all(b"Use private file\r")
                .expect("select explicit fallback");
            acknowledge_linear_pages(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                (
                    warning_at,
                    b"type a choice name: 1. Back or 2. Accept; empty Enter selects 1. Back\r\nInput:\r\n",
                ),
            );
            input
                .write_all(b"\r")
                .expect("default declines plan consent");
        } else {
            input
                .write_all(b"\r")
                .expect("default declines file storage");
        }
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(result.status.success(), "cancelled setup failed");
        assert_eq!(
            answered.cursor_queries, 0,
            "linear setup must not query the cursor"
        );
        assert!(!state.exists(), "cancelled setup created Session State");
        // The harness's Enter at folder consent is an explicit "read only" save, which creates the root.
        if let Ok(root) = StateRoot::open_existing(&account_root) {
            assert!(
                root.record(RecordKind::SavedAccount)
                    .read()
                    .unwrap()
                    .is_none()
                    && root
                        .record(RecordKind::ChatgptRegistration)
                        .read()
                        .unwrap()
                        .is_none()
                    && root
                        .record(RecordKind::ChatgptAccounts)
                        .read()
                        .unwrap()
                        .is_none(),
                "pre-consent setup created account State"
            );
        }
        let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
        let reviewed = reviewed_setup_text(&text);
        assert!(text.contains("Password store unavailable. Unencrypted file"));
        assert_eq!(reviewed.contains("remote output-token limit"), accept_file);
        assert_eq!(
            reviewed.contains("private account file is not encrypted"),
            accept_file
        );
        assert_eq!(
            reviewed.contains("Choose Accept to continue, or Back to cancel."),
            accept_file
        );
        assert!(!text.contains('\x1b'), "screen-reader output has escapes");
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("terminal marker")
                .to_owned()
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    }
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
#[test]
#[ignore = "native Linux Bubblewrap release-account setup gate"]
fn release_setup_uses_private_passwd_home_without_test_override() {
    let temp = tempfile::tempdir().expect("private test root");
    let private_home = temp.path().join("home");
    let workspace = private_home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("private Workspace");
    std::fs::create_dir(private_home.join("environment-home")).expect("environment home");
    std::fs::create_dir(private_home.join("environment-state")).expect("environment State");
    let passwd_home = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .expect("passwd lookup")
        .expect("effective user")
        .dir;
    assert!(passwd_home.is_absolute(), "absolute passwd home");
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_HOME", &private_home)
        .env("ARANY_PASSWD_HOME", &passwd_home)
        .env("ARANY_TEST_SETUP", "1")
        .env("ARANY_TEST_STATE", passwd_home.join("state"))
        .env("ARANY_TEST_HOME_ENV", passwd_home.join("environment-home"))
        .env(
            "ARANY_TEST_XDG_STATE",
            passwd_home.join("environment-state"),
        )
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .env("PATH", "/usr/bin")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", RELEASE_ACCOUNT_SHELL, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("release setup PTY"));
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
        b"SCRIPT_INPUT_GATE",
    );
    input.write_all(b"go\r").expect("start product");
    choose_native_account_setup(&mut output, &mut input, &mut transcript, &mut answered);
    let access_at = transcript.len();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        access_at,
        b"Setup: Use private file?",
    );
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        access_at,
        b"Input:\r\n",
    );
    assert!(String::from_utf8_lossy(&transcript[access_at..]).contains(
        "type a choice name: 1. Cancel or 2. Use private file; empty Enter selects 1. Cancel"
    ));
    let cancel_at = transcript.len();
    input
        .write_all(b"\r")
        .expect("default declines file storage");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        cancel_at,
        b"TTY_AFTER:",
    );
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "cancelled release setup failed");
    assert_eq!(
        answered.cursor_queries, 0,
        "screen-reader setup queried the cursor"
    );
    assert!(
        !private_home.join("state").exists(),
        "created Session State"
    );
    assert!(
        !private_home.join(".local/state/arany").exists(),
        "created OS-user account State"
    );
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(!text.contains('\x1b'), "screen-reader output has escapes");
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
fn release_bare_session(
    private_home: &Path,
    passwd_home: &Path,
    name: &str,
    access_choice: Option<&str>,
    private_values: &[&str],
) -> Option<String> {
    if access_choice.is_some() {
        StateRoot::open_existing(&private_home.join(".local/state/arany"))
            .expect("fixture account root")
            .record(RecordKind::ModelPreferences)
            .replace(br#"{"version":1,"sources":[]}"#)
            .expect("mixed-access fixture without a remembered route");
    }
    let workspace = private_home.join("workspace");
    let state = private_home.join(name);
    let environment_home = format!("{name}-home");
    let environment_state = format!("{name}-xdg");
    std::fs::create_dir(private_home.join(&environment_home)).expect("environment home");
    std::fs::create_dir(private_home.join(&environment_state)).expect("environment State");
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_HOME", private_home)
        .env("ARANY_PASSWD_HOME", passwd_home)
        .env("ARANY_TEST_STATE", passwd_home.join(name))
        .env("ARANY_TEST_HOME_ENV", passwd_home.join(&environment_home))
        .env("ARANY_TEST_XDG_STATE", passwd_home.join(&environment_state))
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .env("PATH", "/usr/bin")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", RELEASE_ACCOUNT_SHELL, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("saved-account release PTY"));
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
        b"SCRIPT_INPUT_GATE",
    );
    input.write_all(b"go\r").expect("start product");
    let cancelled = matches!(access_choice, Some("" | "Cancel"));
    let close_at = transcript.len();
    if let Some(choice) = access_choice {
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"empty Enter selects 1. Cancel\r\nInput:\r\n",
        );
        let choice_at = transcript.len();
        input.write_all(choice.as_bytes()).expect("choose access");
        input.write_all(b"\r").expect("submit access");
        if !cancelled {
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                choice_at,
                b"Input:\r\n",
            );
        }
    } else {
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Input:\r\n",
        );
    }
    if !cancelled {
        input.write_all(b"/exit\r").expect("exit Session");
    }
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        close_at,
        b"TTY_AFTER:",
    );
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "saved-account release exit");
    assert_eq!(
        answered.cursor_queries, 0,
        "screen-reader startup queried the cursor"
    );
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(!text.contains("Provider setup"), "setup reopened");
    if access_choice.is_some() {
        assert!(text.contains("Both saved; choose billing route"));
    }
    for private_value in private_values {
        assert!(
            !text.contains(private_value),
            "private account data entered output"
        );
    }
    assert!(!text.contains('\x1b'), "screen-reader output has escapes");
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    if cancelled {
        assert!(!state.exists(), "cancelled billing choice created State");
        return None;
    }
    let journal = std::fs::read(state.join("events.sqlite3")).expect("Session journal");
    for private_value in private_values {
        assert!(
            !journal
                .windows(private_value.len())
                .any(|part| part == private_value.as_bytes()),
            "private account data entered Session journal"
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let sessions =
            arany::list_sessions(StateRoot::open_existing(&state).expect("State"), workspace)
                .await
                .expect("Workspace Sessions");
        assert!(sessions.is_empty(), "bare startup saves no empty Session");
    });
    assert!(text.contains("Conversation: new · not saved"));
    Some(text)
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
fn release_passwd_home() -> PathBuf {
    let home = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .expect("passwd lookup")
        .expect("effective user")
        .dir;
    assert!(home.is_absolute(), "absolute passwd home");
    home
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
#[test]
#[ignore = "native Linux Bubblewrap release-account cross-StateRoot gate"]
fn release_saved_file_account_is_shared_across_session_roots() {
    let temp = tempfile::tempdir().expect("private test root");
    let private_home = temp.path().join("home");
    std::fs::create_dir_all(private_home.join("workspace")).expect("private Workspace");
    super::prepare_test_file_account_at(&private_home.join(".local/state/arany"));
    let passwd_home = release_passwd_home();
    for name in ["state-one", "state-two"] {
        let view = release_bare_session(
            &private_home,
            &passwd_home,
            name,
            None,
            &["synthetic-existing-key"],
        )
        .expect("native Session");
        assert!(view.contains("Provider: openai\r\nModel: gpt-5.4\r\n"));
    }
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
#[test]
#[ignore = "native Linux Bubblewrap ChatGPT account-only cross-StateRoot gate"]
fn release_selected_chatgpt_account_starts_without_keyring_or_run() {
    let temp = tempfile::tempdir().expect("private test root");
    let private_home = temp.path().join("home");
    std::fs::create_dir_all(private_home.join("workspace")).expect("private Workspace");
    prepare_selected_chatgpt_account_at(&private_home.join(".local/state/arany"));
    let passwd_home = release_passwd_home();
    for name in ["state-one", "state-two"] {
        let view = release_bare_session(
            &private_home,
            &passwd_home,
            name,
            None,
            &["synthetic-subject"],
        )
        .expect("ChatGPT Session");
        assert!(view.contains("Provider: chatgpt\r\nModel: unset\r\n"));
    }
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
#[test]
#[ignore = "native Linux Bubblewrap mixed-access release startup gate"]
fn release_mixed_access_prompts_each_start_without_reading_the_other_record() {
    let temp = tempfile::tempdir().expect("private test root");
    let private_home = temp.path().join("home");
    std::fs::create_dir_all(private_home.join("workspace")).expect("private Workspace");
    let account_path = private_home.join(".local/state/arany");
    super::prepare_test_file_account_at(&account_path);
    prepare_selected_chatgpt_account_at(&account_path);
    let account = StateRoot::open_existing(&account_path).expect("private account root");
    account
        .record(RecordKind::ChatgptAccounts)
        .replace(b"invalid-unselected-chatgpt-canary")
        .expect("unselected index fixture");
    let passwd_home = release_passwd_home();
    let api = release_bare_session(
        &private_home,
        &passwd_home,
        "api-session",
        Some("API key"),
        &[
            "synthetic-existing-key",
            "invalid-unselected-chatgpt-canary",
        ],
    )
    .expect("explicit native Session");
    assert!(api.contains("Provider: openai\r\nModel: gpt-5.4\r\n"));

    prepare_selected_chatgpt_account_at(&account_path);
    account
        .record(RecordKind::SavedAccount)
        .replace(b"invalid-unselected-native-canary")
        .expect("unselected API fixture");
    let chatgpt = release_bare_session(
        &private_home,
        &passwd_home,
        "chatgpt-session",
        Some("ChatGPT plan"),
        &["synthetic-subject", "invalid-unselected-native-canary"],
    )
    .expect("explicit ChatGPT Session");
    assert!(chatgpt.contains("Provider: chatgpt\r\nModel: unset\r\n"));
    account
        .record(RecordKind::ChatgptAccounts)
        .replace(b"invalid-unselected-chatgpt-canary")
        .expect("both invalid fixture");
    let native_before = account
        .record(RecordKind::SavedAccount)
        .read()
        .expect("native snapshot");
    let chatgpt_before = account
        .record(RecordKind::ChatgptAccounts)
        .read()
        .expect("ChatGPT snapshot");
    for (name, choice) in [("blank-session", ""), ("cancel-session", "Cancel")] {
        assert!(
            release_bare_session(
                &private_home,
                &passwd_home,
                name,
                Some(choice),
                &[
                    "invalid-unselected-native-canary",
                    "invalid-unselected-chatgpt-canary"
                ],
            )
            .is_none(),
            "cancelled startup selected a route"
        );
        assert!(
            account
                .record(RecordKind::SavedAccount)
                .read()
                .expect("unchanged native record")
                == native_before
        );
        assert!(
            account
                .record(RecordKind::ChatgptAccounts)
                .read()
                .expect("unchanged ChatGPT record")
                == chatgpt_before
        );
    }
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
#[test]
#[ignore = "native Linux cross-user product gate; requires subordinate IDs, unshare, bwrap, and setpriv"]
fn release_saved_account_is_private_across_os_users() {
    const STAGE: &str = "ARANY_TEST_CROSS_USER_PRODUCT_STAGE";
    const TEST: &str = "setup::release_saved_account_is_private_across_os_users";

    if let Some(stage) = std::env::var_os(STAGE) {
        let (home, expected_account) = match stage.to_str() {
            Some("owner") => {
                assert_eq!(nix::unistd::geteuid().as_raw(), 0);
                assert_eq!(
                    StateRoot::account_path().unwrap(),
                    Path::new("/owner/.local/state/arany")
                );
                (
                    "/owner",
                    Some(
                        Uuid::parse_str(
                            &std::env::var("ARANY_TEST_CROSS_USER_ACCOUNT_ID").unwrap(),
                        )
                        .unwrap(),
                    ),
                )
            }
            Some("other") => {
                assert_eq!(nix::unistd::geteuid().as_raw(), 1);
                assert_eq!(
                    StateRoot::account_path().unwrap(),
                    Path::new("/other/.local/state/arany")
                );
                assert_eq!(
                    std::fs::symlink_metadata("/other/.local/state/arany")
                        .expect_err("second user's account root is absent")
                        .kind(),
                    std::io::ErrorKind::NotFound
                );
                std::fs::create_dir("/other/workspace").expect("second user's Workspace");
                ("/other", None)
            }
            _ => panic!("invalid cross-user product stage"),
        };
        let shell = if expected_account.is_some() {
            "/arany --screen-reader --state-dir /owner/state --workspace /owner/workspace"
        } else {
            "/arany --screen-reader --state-dir /other/state --workspace /other/workspace"
        };
        let fixture_state = if expected_account.is_some() {
            "/owner/spoofed-state"
        } else {
            "/other/fixture-state"
        };
        let mut command = Command::new("/usr/bin/script");
        command
            .args(["-q", "-e", "-c", shell, "/dev/null"])
            .env_clear()
            .env("SHELL", "/usr/bin/sh")
            .env("TERM", "dumb")
            .env("HOME", "/owner")
            .env("XDG_STATE_HOME", fixture_state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("cross-user product PTY"));
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
        input.write_all(b"/exit\r").expect("exit Session");
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(result.status.success(), "cross-user product exit");
        assert_eq!(
            answered.cursor_queries, 0,
            "screen-reader startup queried the cursor"
        );
        assert!(
            !transcript
                .windows(b"synthetic-existing-key".len())
                .any(|part| part == b"synthetic-existing-key")
        );
        assert!(
            !transcript.contains(&b'\x1b'),
            "screen-reader output has escapes"
        );

        let state = Path::new(home).join("state");
        let workspace = Path::new(home).join("workspace");
        let journal = std::fs::read(state.join("events.sqlite3")).expect("Session journal");
        assert!(
            !journal
                .windows(b"synthetic-existing-key".len())
                .any(|part| part == b"synthetic-existing-key"),
            "saved key entered Session journal"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("read-only runtime");
        runtime.block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(&state).expect("existing State"),
                workspace,
            )
            .await
            .expect("Workspace Sessions");
            assert!(
                sessions.is_empty(),
                "bare startup saves no empty conversation"
            );
            assert!(std::str::from_utf8(&transcript).expect("UTF-8").contains(
                if expected_account.is_some() {
                    "Provider: openai\r\nModel: gpt-5.4\r\n"
                } else {
                    "Provider: unset\r\nModel: unset\r\n"
                }
            ));
        });
        if expected_account.is_none() {
            assert!(!Path::new("/other/.local/state/arany").exists());
        } else {
            let status = Command::new("/usr/bin/chown")
                .args(["1:1", "/other"])
                .status()
                .expect("prepare second user's home");
            assert!(status.success(), "prepare second user's home");
            let output = Command::new("/usr/bin/setpriv")
                .args([
                    "--reuid",
                    "1",
                    "--regid",
                    "1",
                    "--clear-groups",
                    "/test",
                    "--exact",
                    TEST,
                    "--ignored",
                    "--nocapture",
                ])
                .env_clear()
                .env(STAGE, "other")
                .bounded_output()
                .expect("second OS user");
            assert!(
                output.status.success(),
                "second OS user's product check failed: {}",
                format!(
                    "stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
                .replace("synthetic-existing-key", "[redacted]")
                .chars()
                .take(2048)
                .collect::<String>()
            );
            crate::process::assert_single_test_completed(&output.stdout, "second OS user");
        }
        return;
    }

    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("private cross-user fixture");
    let owner_home = temp.path().join("owner-home");
    std::fs::create_dir_all(owner_home.join("workspace")).expect("owner Workspace");
    std::fs::set_permissions(&owner_home, std::fs::Permissions::from_mode(0o700))
        .expect("owner-only home");
    let account_id = super::prepare_test_file_account_at(&owner_home.join(".local/state/arany"));
    let passwd = temp.path().join("passwd");
    std::fs::write(
        &passwd,
        b"owner:x:0:0::/owner:/bin/sh\nother:x:1:1::/other:/bin/sh\n",
    )
    .expect("synthetic passwd entries");
    let output = Command::new("/usr/bin/timeout")
        .args([
            "-k",
            "2s",
            "40s",
            "/usr/bin/unshare",
            "--map-auto",
            "--map-user",
            "0",
            "--map-group",
            "0",
            "--user",
            "--mount",
            "--net",
            "--fork",
            "--kill-child",
            "/usr/bin/bwrap",
            "--unshare-pid",
            "--die-with-parent",
            "--tmpfs",
            "/",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--ro-bind",
        ])
        .arg(&passwd)
        .args(["/etc/passwd", "--chmod", "0755", "/etc", "--bind"])
        .arg(&owner_home)
        .args(["/owner", "--tmpfs", "/other", "--ro-bind"])
        .arg(std::env::current_exe().expect("test executable"))
        .args([
            "/test",
            "--ro-bind",
            env!("CARGO_BIN_EXE_arany"),
            "/arany",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--clearenv",
            "--setenv",
            STAGE,
            "owner",
            "--setenv",
            "ARANY_TEST_CROSS_USER_ACCOUNT_ID",
        ])
        .arg(account_id.to_string())
        .args(["--", "/test", "--exact", TEST, "--ignored", "--nocapture"])
        .env_clear()
        .bounded_output()
        .expect("isolated cross-user product test");
    assert!(
        output.status.success(),
        "isolated cross-user product test failed: {}",
        format!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .replace("synthetic-existing-key", "[redacted]")
        .chars()
        .take(4096)
        .collect::<String>()
    );
    crate::process::assert_single_test_completed(&output.stdout, "saved account");
}

#[cfg(debug_assertions)]
#[test]
fn narrow_no_color_chatgpt_warning_pages_before_acceptance_and_restores_terminal() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let account_root = temp.path().join("account-root");
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", temp.path().join("missing-bus").display()),
        )
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args([
            "-q",
            "-e",
            "-c",
            r#"stty rows 8 cols 16; printf 'SHELL_PID:%s\n' "$$"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' "$before"; "$ARANY_TEST_EXE" --no-color --setup --state-dir "$ARANY_TEST_STATE" --workspace "$ARANY_TEST_WORKSPACE"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' "$after"; exit "$exit_code""#,
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("narrow consent PTY"));
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
        b"Provider setup",
    );
    let root_frame = transcript
        .windows(b"Provider setup".len())
        .position(|part| part == b"Provider setup")
        .unwrap();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        root_frame,
        b"\x1b[?25l",
    );
    let text = String::from_utf8_lossy(&transcript);
    let shell_pid = text
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
        .expect("shell PID")
        .parse::<u32>()
        .expect("shell PID number");
    let product_pid = product_child_of(shell_pid);
    let _product = ProductGuard::new(product_pid, state.clone());
    let accounts_at = transcript.len();
    input.write_all(b"\r").expect("open API accounts");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        accounts_at,
        b"API accounts",
    );
    let frame_start = transcript[accounts_at..]
        .windows(b"API accounts".len())
        .position(|part| part == b"API accounts")
        .unwrap()
        + accounts_at;
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        frame_start,
        b"\x1b[?25l",
    );
    input
        .write_all(b"\x1b[B\x1b[B\r")
        .expect("select ChatGPT account manager");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"\x1b[7;13Hfile",
    );
    input
        .write_all(b"\x1b[B\r")
        .expect("choose and confirm file fallback");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "ChatGPT plan co\u{2026}".as_bytes(),
    );
    assert!(!state.exists(), "pre-consent flow created a Session");
    let next_page = transcript.len();
    input.write_all(b"\r").expect("read next page");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        next_page,
        b"\x1b[5;6H2",
    );
    let ignored = transcript.len();
    input
        .write_all(b"Accept")
        .expect("attempt early acceptance");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        ignored,
        b"Read every",
    );
    input.write_all(b"\x1b").expect("cancel before final page");
    let result = wait_product(child.take());
    let mut remaining = [0u8; 4096];
    loop {
        match output.read(&mut remaining) {
            Ok(0) | Err(_) => break,
            Ok(count) => transcript.extend_from_slice(&remaining[..count]),
        }
    }
    assert!(result.status.success(), "cancelled narrow setup failed");
    assert!(!state.exists(), "cancelled setup created a Session");
    assert!(!account_root.exists(), "warning created account State");
    let text = String::from_utf8_lossy(&transcript);
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| {
                line.trim_end_matches('\r')
                    .split_once(name)
                    .map(|(_, value)| value)
            })
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
}

#[cfg(debug_assertions)]
#[test]
fn fresh_setup_root_saves_its_first_custom_route_without_provider_egress() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let mut pty = start_setup_pty(&temp, &workspace, &state);
    let routes_at = pty.transcript.len();
    pty.input.write_all(b"2\r").expect("open custom routes");
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        routes_at,
        b"Input:\r\n",
    );
    let saved_at = add_custom_route(&mut pty, b"1\r");
    let (pages, _) = acknowledge_linear_pages(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        (saved_at, b"empty Enter selects 1. first-route"),
    );
    assert!(
        pages > 1,
        "save result exercises complete paged acknowledgment"
    );
    pty.input.write_all(b"3\r").expect("leave custom routes");
    let (result, transcript) = finish_setup_pty_with_transcript(pty);
    assert!(result.status.success(), "fresh custom setup failed");
    let profile = std::fs::read_to_string(state.join("provider-profiles.json"))
        .expect("first custom route record");
    assert!(profile.contains("first-route"));
    assert!(profile.contains("model-synthetic"));
    assert!(!profile.contains(FIRST_ROUTE_KEY_CANARY));
    assert!(!String::from_utf8_lossy(&transcript).contains(FIRST_ROUTE_KEY_CANARY));
    assert_setup_journal(&state, &workspace);
}

#[cfg(debug_assertions)]
#[test]
fn configured_unsaved_conversation_survives_custom_save_and_back() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let provider = super::loopback::held_provider(
        &state,
        "retained setup answer",
        super::loopback::HeldResponseKind::Buffered,
    );
    super::loopback::check_profile(&workspace, &state);
    let mut pty = start_setup_pty_mode(&temp, &workspace, &state, SetupPtyMode::Configured);
    assert_retained_setup_conversation(&state, &workspace, None);
    let routes_at = pty.transcript.len();
    pty.input.write_all(b"2\r").expect("open custom routes");
    wait_setup_choices(&mut pty, routes_at);
    let saved_at = add_custom_route(&mut pty, b"2\r");
    wait_setup_choices(&mut pty, saved_at);
    let back_at = pty.transcript.len();
    pty.input
        .write_all(b"Back\r")
        .expect("return to conversation");
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        back_at,
        b"Setup closed\r\nInput:\r\n",
    );
    assert_retained_setup_conversation(&state, &workspace, Some(0));
    submit_retained_setup_turn(&mut pty, &provider);
    pty.input
        .write_all(b"/exit\r")
        .expect("exit configured conversation");
    let result = finish_setup_pty(pty);
    assert!(
        result.status.success(),
        "configured setup conversation failed"
    );
    provider.peer.join().expect("synthetic Provider assertions");
    assert_retained_setup_conversation(&state, &workspace, Some(1));
}

#[cfg(debug_assertions)]
fn submit_retained_setup_turn(pty: &mut SetupPty, provider: &super::loopback::HeldStream) {
    let submitted_at = pty.transcript.len();
    pty.input
        .write_all(b"answer after setup\r")
        .expect("submit configured turn");
    provider
        .streaming
        .recv_timeout(Duration::from_secs(10))
        .expect("configured Provider called");
    provider
        .release
        .send(())
        .expect("complete synthetic answer");
    provider
        .consumed
        .recv_timeout(Duration::from_secs(10))
        .expect("answer read");
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        submitted_at,
        b"Input:\r\n",
    );
    assert!(String::from_utf8_lossy(&pty.transcript).contains("retained setup answer"));
}

#[cfg(debug_assertions)]
fn wait_setup_choices(pty: &mut SetupPty, mut cursor: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(
            &mut pty.output,
            &mut pty.input,
            &mut pty.transcript,
            &mut pty.answered,
        );
        let fresh = &pty.transcript[cursor..];
        if fresh
            .windows(b"Page 1/".len())
            .any(|part| part == b"Page 1/")
        {
            let total = wait_linear_page(
                &mut pty.output,
                &mut pty.input,
                &mut pty.transcript,
                &mut pty.answered,
                (cursor, 1),
            );
            for page in 1..=total {
                cursor = pty.transcript.len();
                pty.input.write_all(b"\r").expect("acknowledge setup page");
                if page < total {
                    assert_eq!(
                        wait_linear_page(
                            &mut pty.output,
                            &mut pty.input,
                            &mut pty.transcript,
                            &mut pty.answered,
                            (cursor, page + 1),
                        ),
                        total
                    );
                }
            }
        } else if fresh
            .windows(b"Input:\r\n".len())
            .any(|part| part == b"Input:\r\n")
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "setup choices deadline: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
}

#[cfg(debug_assertions)]
fn assert_retained_setup_conversation(state: &Path, workspace: &Path, runs: Option<usize>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(state).unwrap(),
                workspace.to_owned(),
            )
            .await
            .unwrap();
            let Some(runs) = runs else {
                assert!(
                    sessions.is_empty(),
                    "configured renamed chat is still unsaved"
                );
                return;
            };
            assert_eq!(sessions.len(), 1);
            let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
            let events = store.load_session(sessions[0].id).await.unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event.event,
                        arany::Event::ProviderSetupEditFinished {
                            disposition: arany::ProviderSetupDisposition::Applied,
                            ..
                        }
                    ))
                    .count(),
                1
            );
            let view = store.load_view(sessions[0].id).await.unwrap().unwrap();
            assert_eq!(view.title, "Setup retained title");
            assert!(view.title_is_explicit);
            assert_eq!(view.defaults.provider.as_deref(), Some("custom:local"));
            assert_eq!(view.defaults.model.as_deref(), Some("model-1"));
            assert_eq!(view.defaults.policy, arany::CollaborationPolicy::Single);
            assert_eq!(view.runs.len(), runs);
            if let Some(run) = view.runs.last() {
                assert_eq!(run.status, arany::RunStatus::Finished);
                assert_eq!(
                    run.config.as_ref().unwrap().provider.profile(),
                    "custom:local"
                );
            }
            assert!(!view.provider_setup_blocks_continuation());
            store.close().await.unwrap();
        });
}

#[cfg(debug_assertions)]
fn assert_setup_journal(state: &Path, workspace: &Path) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("journal runtime")
        .block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(state).expect("setup StateRoot"),
                workspace.to_owned(),
            )
            .await
            .expect("reopen setup Session list");
            assert_eq!(sessions.len(), 1);
            let store = Store::open(StateRoot::open_existing(state).expect("setup StateRoot"))
                .expect("reopen setup journal");
            let events = store
                .load_session(sessions[0].id)
                .await
                .expect("setup Events");
            assert!(events.iter().all(|envelope| {
                !envelope
                    .event
                    .payload()
                    .expect("setup Event payload")
                    .contains(FIRST_ROUTE_KEY_CANARY)
            }));
            assert!(matches!(
                events[0].event,
                arany::Event::SessionStarted { .. }
            ));
            let operation_id = match &events[1].event {
                arany::Event::ProviderSetupEditStarted {
                    operation_id,
                    target,
                    ..
                } => {
                    assert_eq!(target.identity, "first-route");
                    *operation_id
                }
                event => panic!("unexpected setup Event: {}", event.kind()),
            };
            assert!(matches!(
                &events[2].event,
                arany::Event::ProviderSetupEditFinished {
                    operation_id: finished,
                    disposition: arany::ProviderSetupDisposition::Applied,
                    resulting_digest: Some(_),
                } if *finished == operation_id
            ));
            let view = store
                .load_view(sessions[0].id)
                .await
                .expect("reopened setup Session view")
                .expect("setup Session");
            assert_eq!(view.pending_provider_setup_edit, None);
            assert!(!view.provider_setup_uncertain);
            store.close().await.expect("close setup store");
        });
}

#[cfg(debug_assertions)]
fn add_custom_route(pty: &mut SetupPty, choice: &[u8]) -> usize {
    let mut prompt_at = pty.transcript.len();
    pty.input.write_all(choice).expect("add custom route");
    wait_for(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        b"Custom route protocol",
    );
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        prompt_at,
        b"Input:\r\n",
    );
    prompt_at = pty.transcript.len();
    pty.input
        .write_all(b"1\r")
        .expect("select Responses protocol");
    let fields = [
        (b"Custom route name".as_slice(), b"first-route\r".as_slice()),
        (
            b"Custom route destination".as_slice(),
            b"http://127.0.0.1:9000/v1/responses\r".as_slice(),
        ),
        (
            b"Custom route model".as_slice(),
            b"model-synthetic\r".as_slice(),
        ),
        (
            b"Credential environment name".as_slice(),
            b"ARANY_PROVIDER_FIRST_ROUTE_KEY\r".as_slice(),
        ),
    ];
    for (prompt, value) in fields {
        wait_for(
            &mut pty.output,
            &mut pty.input,
            &mut pty.transcript,
            &mut pty.answered,
            prompt,
        );
        wait_for_after(
            &mut pty.output,
            &mut pty.input,
            &mut pty.transcript,
            &mut pty.answered,
            prompt_at,
            b"Input:\r\n",
        );
        prompt_at = pty.transcript.len();
        pty.input
            .write_all(value)
            .expect("enter custom route field");
    }
    wait_for(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        b"Declared efforts",
    );
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        prompt_at,
        b"Input:\r\n",
    );
    let warning_at = pty.transcript.len();
    pty.input.write_all(b"\r").expect("keep default effort set");
    acknowledge_linear_pages(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        (
            warning_at,
            b"type a choice name: 1. Back or 2. Accept; empty Enter selects 1. Back\r\nInput:\r\n",
        ),
    );
    let saved_at = pty.transcript.len();
    pty.input
        .write_all(b"2\r")
        .expect("confirm route declaration");
    saved_at
}

#[cfg(debug_assertions)]
fn seed_long_custom_profiles(state: &Path) -> String {
    StateRoot::admit(state).expect("custom route state");
    let long_name = "z".repeat(arany::MAX_CUSTOM_PROFILE_NAME_BYTES);
    let profile = |name: &str, model: &str, credential_env: &str| {
        serde_json::json!({
            "name": name,
            "protocol": "openai-responses",
            "endpoint": "https://api.example.com/v1/responses",
            "model": model,
            "credential_env": credential_env,
            "outcome_encoding": "json_schema",
            "privacy": "user_authorized",
            "max_output_tokens": 4096,
            "capability_evidence_version": 2,
            "efforts": []
        })
    };
    let document = serde_json::json!({
        "version": 2,
        "profiles": [
            profile("first-route", "first-model", "ARANY_PROVIDER_FIRST_ROUTE_KEY"),
            profile(&long_name, "second-model", "ARANY_PROVIDER_SECOND_ROUTE_KEY")
        ]
    });
    let path = state.join("provider-profiles.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&document).expect("profile document"),
    )
    .expect("write profile document");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("private profiles");
    long_name
}

#[cfg(debug_assertions)]
#[test]
fn linear_setup_selects_nonfirst_maximum_length_custom_route() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let long_name = seed_long_custom_profiles(&state);
    let mut pty = start_setup_pty(&temp, &workspace, &state);
    let routes_at = pty.transcript.len();
    pty.input.write_all(b"2\r").expect("open custom routes");
    let route_prompt =
        b"Declarations are saved privately; Select and Check are separate actions; type a listed choice number or exact name (1-4); empty Enter selects 1\r\nInput:\r\n";
    acknowledge_linear_pages(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        (routes_at, route_prompt),
    );
    pty.input
        .write_all(b"2\r")
        .expect("select second route by token");
    wait_for(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        b"ARANY_PROVIDER_SECOND_ROUTE_KEY",
    );
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        routes_at,
        b"empty Enter selects 1. Inspect\r\nInput:\r\n",
    );
    let inspect_at = pty.transcript.len();
    pty.input.write_all(b"1\r").expect("inspect second route");
    let (_, list_at) = acknowledge_linear_pages(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        (inspect_at, b"Page 1/"),
    );
    acknowledge_linear_pages(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        (list_at, route_prompt),
    );
    wait_for(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        b"Model: second-model",
    );
    assert!(
        String::from_utf8_lossy(&pty.transcript).contains(&long_name),
        "selected route title did not preserve its maximum-length name"
    );
    pty.input.write_all(b"Back\r").expect("leave custom routes");
    let result = finish_setup_pty(pty);
    assert!(
        result.status.success(),
        "linear custom route selection failed"
    );
}

#[cfg(debug_assertions)]
#[test]
fn unconfigured_inline_composer_shows_draft_and_setup_selection_at_narrow_widths() {
    let executable = std::env::var_os("ARANY_RELEASE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_arany")));
    assert!(executable.is_absolute() && executable.is_file());
    for columns in [16_u16, 40] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        let shell = format!(
            "stty rows 8 cols {columns}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
        );
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("ARANY_TEST_EXE", &executable)
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", temp.path().join("missing-bus").display()),
            )
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", &shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("inline PTY process"));
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
        // Narrow frames can flush the setup hint after the composer border.
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"/setup",
        );
        let text = String::from_utf8_lossy(&transcript);
        let shell_pid = text
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse::<u32>()
            .expect("shell PID number");
        let _product = ProductGuard::for_executable(
            product_child_of_executable(shell_pid, &executable),
            state.clone(),
            executable.clone(),
        );
        assert!(
            text.contains("Type") && text.contains("/setup"),
            "{columns}: missing setup action"
        );
        assert!(
            !text.contains("Describe a task"),
            "{columns}: unconfigured welcome invites an unavailable Run"
        );
        input.write_all(b"visible").expect("type ordinary draft");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"\x1b[6;9He",
        );
        let rejected_at = transcript.len();
        input.write_all(b"\r").expect("submit without setup");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            rejected_at,
            b"/setup",
        );
        let editing_at = transcript.len();
        input.write_all(b"!").expect("edit rejected draft");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            editing_at,
            b"\x1b[6;10H!",
        );
        let quick_at = transcript.len();
        input
            .write_all(b"\x0b")
            .expect("open local actions with a draft");
        // Ctrl+K takes keys only once its armed frame focuses Setup.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !crate::armed_selector(&transcript, quick_at, columns.into(), 8)
            .is_some_and(|screen| screen.iter().any(|row| row.starts_with("› 4. Setup")))
        {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "{columns}: armed quick actions missing: {}",
                tail(&transcript[quick_at..])
            );
            thread::yield_now();
        }
        let quick_setup_at = transcript.len();
        input.write_all(b"\r").expect("open focused setup action");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            quick_setup_at,
            b"> API accounts",
        );
        let header_end = quick_setup_at
            + transcript[quick_setup_at..]
                .windows(b"> API accounts".len())
                .position(|part| part == b"> API accounts")
                .expect("selected setup row")
            + b"> API accounts".len();
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            header_end,
            b"\x1b[?25l",
        );
        let closed_at = transcript.len();
        input
            .write_all(b"\x1b")
            .expect("cancel setup without consuming draft");
        crate::session_picker::wait_for_screen(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (closed_at, columns.into(), 8),
            "Setup closed",
        );
        let returned_at = transcript.len();
        input.write_all(b"?").expect("edit restored draft");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            returned_at,
            b"\x1b[6;11H?",
        );
        input.write_all(b"\x03").expect("clear rejected draft");
        let setup_at = transcript.len();
        input.write_all(b"/setup\r").expect("open setup selector");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            setup_at,
            b"Provider setup",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            setup_at,
            b"\x1b[?25l",
        );
        let selected_at = transcript.len();
        input
            .write_all(b"\x1b[B")
            .expect("select Custom routes row");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            selected_at,
            b"> Custom routes",
        );
        let restored_at = transcript.len();
        input
            .write_all(b"\x1b[A")
            .expect("return to API accounts row");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            restored_at,
            b"> API accounts",
        );
        let closed_at = transcript.len();
        input.write_all(b"\x1b").expect("close setup selector");
        crate::session_picker::wait_for_screen(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (closed_at, columns.into(), 8),
            "Setup closed",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            closed_at,
            b"\x1b[?25h",
        );
        input.write_all(b"/exit\r").expect("exit Session");
        // Viewport height changes query the cursor, so keep answering until the product exits.
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.child().try_wait().expect("product status").is_none() {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "{columns}: exit: {}",
                tail(&transcript)
            );
            thread::yield_now();
        }
        let result = wait_product(child.take());
        output
            .read_to_end(&mut transcript)
            .expect("final PTY output");
        assert!(
            result.status.success(),
            "inline setup journey failed at {columns}: {}",
            tail(&transcript)
        );
        assert!(
            (1..=128).contains(&answered.cursor_queries),
            "Classic observes the cursor at acquisition and height changes at {columns}"
        );
        let text = String::from_utf8(transcript).expect("PTY UTF-8");
        assert!(
            !text.contains("\x1b[?1049h"),
            "alternate screen at {columns}"
        );
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("terminal marker")
                .to_owned()
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("read-only runtime");
        runtime.block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(&state).expect("State"),
                workspace.clone(),
            )
            .await
            .expect("Workspace Sessions");
            assert!(
                sessions.is_empty(),
                "startup, local rejection and setup navigation save no empty conversation"
            );
        });
    }
}

#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
#[ignore = "native Linux network-isolated saved-account replacement gate"]
fn replaced_file_account_cannot_run_an_older_session() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let original_account = prepare_test_file_account(temp.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("Session runtime");
    let session_id = runtime
        .block_on(create_session(
            StateRoot::admit(&state).expect("State"),
            workspace.clone(),
            None,
        ))
        .expect("Session");
    runtime
        .block_on(set_session_defaults(
            StateRoot::open_existing(&state).expect("State reopen"),
            workspace.clone(),
            session_id,
            SessionDefaults {
                provider: Some("openai".into()),
                model: Some("gpt-5.4".into()),
                effort: None,
                account_id: Some(original_account),
                policy: Default::default(),
            },
        ))
        .expect("saved-account defaults");
    let replacement = prepare_test_file_account(temp.path());
    assert_ne!(replacement, original_account);

    crate::isolation::remember_workspace_trust(
        &temp.path().join("account-root"),
        &workspace,
        std::path::Path::new("/data/workspace"),
    );
    let shell = "/usr/bin/timeout -k 1s 5s /usr/bin/bwrap --unshare-user --unshare-net --unshare-pid --die-with-parent --tmpfs / --ro-bind /usr /usr --symlink usr/lib /lib --ro-bind /lib64 /lib64 --proc /proc --dev-bind /dev /dev --bind \"$ARANY_TEST_ROOT\" /data --ro-bind \"$ARANY_TEST_EXE\" /arany --clearenv --setenv HOME /data --setenv XDG_STATE_HOME /data/xdg-state --setenv ARANY_TEST_ACCOUNT_ROOT /data/account-root --setenv DBUS_SESSION_BUS_ADDRESS unixexec:path=/usr/bin/false --setenv TERM dumb --chdir /data/workspace -- /arany --screen-reader --state-dir /data/state --workspace /data/workspace --resume \"$ARANY_TEST_SESSION\" 'synthetic stale-account objective'";
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_ROOT", temp.path())
        .env("ARANY_TEST_SESSION", session_id.to_string())
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = wait_product(command.spawn().expect("network-isolated product"));
    assert_eq!(output.status.code(), Some(1), "stale account must fail");
    assert!(output.stderr.is_empty(), "script wrapper stderr");
    let transcript = String::from_utf8(output.stdout).expect("screen-reader UTF-8");
    let diagnostic = tail(
        transcript
            .replace("synthetic-existing-key", "[redacted]")
            .as_bytes(),
    );
    assert!(!transcript.contains("synthetic-existing-key"));
    assert!(
        !transcript.contains('\x1b'),
        "screen-reader output has escapes"
    );
    assert!(
        transcript
            == "error: invalid saved account; use arany --setup to choose or reset access\r\n",
        "stale account was not rejected exactly: {diagnostic}"
    );

    runtime.block_on(async {
        let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
            .expect("read-only Store");
        let events = store.load_session(session_id).await.expect("Events");
        let view = SessionView::replay(session_id, &events)
            .expect("strict replay")
            .expect("Session");
        assert_eq!(events.len(), 2, "rejected objective changed the journal");
        assert!(view.runs.is_empty(), "rejected objective began a Run");
        assert_eq!(view.defaults.account_id, Some(original_account));
        store.close().await.expect("close Store");
    });
}

#[cfg(debug_assertions)]
#[test]
fn narrow_no_color_setup_hides_key_and_restores_terminal_on_cancel() {
    for explicit_workspace in [false, true] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        prepare_test_file_account(temp.path());
        let account_root = StateRoot::open_existing(&temp.path().join("account-root"))
            .expect("synthetic account root");
        let account_before = account_root
            .record(RecordKind::SavedAccount)
            .read()
            .expect("initial account");
        let mut command = Command::new("/usr/bin/script");
        command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unixexec:path=/usr/bin/false",
        )
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args([
            "-q",
            "-e",
            "-c",
            "stty rows 8 cols 16; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --no-color --setup --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"",
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("narrow setup PTY"));
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
            b"TTY_BEFORE:",
        );
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Esc",
        );
        input.write_all(b"x").expect("non-navigation setup key");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Use Up/Down",
        );
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Enter",
        );
        assert!(!state.exists(), "invalid choice created State");
        let accounts_at = transcript.len();
        input.write_all(b"\r").expect("open API accounts");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            accounts_at,
            b"API accounts",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            accounts_at,
            b"\x1b[?25l",
        );
        let access_at = transcript.len();
        input.write_all(b"\r").expect("select API key access");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            access_at,
            b"NOT encrypted",
        );
        let consent_at = transcript.len();
        input
            .write_all(b"\x1b[B\r")
            .expect("choose and confirm file storage");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            consent_at,
            b"Choose Provid",
        );
        let provider_at = consent_at
            + transcript[consent_at..]
                .windows(b"Choose Provid".len())
                .position(|part| part == b"Choose Provid")
                .expect("provider frame title")
            + b"Choose Provid".len();
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            provider_at,
            b"\x1b[?25l",
        );
        input
            .write_all(if explicit_workspace {
                b"\x1b[B\r"
            } else {
                b"\r"
            })
            .expect("native Provider choice");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"512",
        );
        let text = String::from_utf8_lossy(&transcript);
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("setup PTY marker")
                .to_owned()
        };
        let shell_pid = marker("SHELL_PID:").parse::<u32>().expect("shell PID");
        let before = marker("TTY_BEFORE:");
        let product_pid = product_child_of(shell_pid);
        let _product = ProductGuard::new(product_pid, state.clone());
        assert_ne!(tty_settings(product_pid), before, "key input hides echo");
        input
            .write_all(b"\x1b[200~CANARYKEY0\x1b[201~")
            .expect("unsubmitted secret paste");
        let key_at = transcript.len();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !transcript[key_at..]
            .windows(b"10 chars".len())
            .any(|part| part == b"10 chars")
        {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "hidden input was not consumed: {}",
                tail(&transcript)
            );
            thread::yield_now();
        }
        let rejected_at = transcript.len();
        input
            .write_all(b"\x1b[200~ \r/setup\x1b[201~")
            .expect("reject whole invalid secret paste without submission");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            rejected_at,
            b"Invalid",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            rejected_at,
            b"\x1b[?25l",
        );
        let blocked_at = transcript.len();
        input
            .write_all(b"X")
            .expect("valid byte remains blocked after rejected byte");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            blocked_at,
            b"\x1b[?25l",
        );
        assert!(
            !transcript[blocked_at..]
                .windows(b"\x1b[6;2H1".len())
                .any(|part| part == b"\x1b[6;2H1"),
            "a byte entered the key while rejected input remained"
        );
        let corrected_at = transcript.len();
        input
            .write_all(b"\x7f\x7fX")
            .expect("correct rejected key bytes and append valid byte");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            corrected_at,
            b"\x1b[6;2H1",
        );
        if explicit_workspace {
            let scope_at = transcript.len();
            input.write_all(b"\r").expect("submit hidden synthetic key");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                scope_at,
                b"API workspace",
            );
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                scope_at,
                b"\x1b[?25l",
            );
            let field_at = transcript.len();
            input
                .write_all(b"\x1b[B\r")
                .expect("choose explicit workspace");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                field_at,
                b"wrkspc_...",
            );
            let typed_at = transcript.len();
            input
                .write_all(b"\x1b[200~wrkspc_Test123\x1b[201~")
                .expect("visible workspace ID paste without submission");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                typed_at,
                b"\x1b[5;3H",
            );
            let invalid_at = transcript.len();
            input
                .write_all(b"\x1b[D\x7f-\r")
                .expect("reject locally malformed workspace");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                invalid_at,
                b"Error:",
            );
        }
        input.write_all(b"\x1b").expect("cancel setup with Escape");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            if child.child().try_wait().expect("product status").is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "setup did not exit after cancellation"
            );
            thread::yield_now();
        }
        let status = child.take().wait().expect("product exit");
        assert!(status.success(), "setup failed after cancellation");
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        let text = String::from_utf8_lossy(&transcript);
        assert!(!text.contains("CANARYKEY0"));
        assert!(
            !text.contains("\x1b[?1049h"),
            "setup entered alternate screen"
        );
        assert!(!state.exists(), "cancelled setup created State");
        let after = text
            .split("TTY_AFTER:")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .expect("TTY after marker")
            .trim_end_matches('\r');
        assert_eq!(before, after, "terminal settings restored");
        assert!(
            account_before
                == account_root
                    .record(RecordKind::SavedAccount)
                    .read()
                    .expect("account after cancel"),
            "cancelled workspace setup replaced the account"
        );
    }
}

#[cfg(debug_assertions)]
#[test]
fn cancelled_screen_reader_setup_hides_key_and_creates_no_session() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    prepare_test_file_account(temp.path());
    let account_root = StateRoot::open_existing(&temp.path().join("account-root"))
        .expect("synthetic account root");
    let account_before = account_root
        .record(RecordKind::SavedAccount)
        .read()
        .expect("initial account");
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false")
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", SETUP_SHELL, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("setup PTY process"));
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
        b"SCRIPT_INPUT_GATE",
    );
    input.write_all(b"go\n").expect("script input gate");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
    );
    let (shell_pid, before) = {
        let text = String::from_utf8_lossy(&transcript);
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("setup PTY marker")
                .to_owned()
        };
        (
            field("SHELL_PID:").parse::<u32>().expect("shell PID"),
            field("TTY_BEFORE:"),
        )
    };
    let product_pid = product_child_of(shell_pid);
    let _product = ProductGuard::new(product_pid, state.clone());
    assert_eq!(
        tty_settings(product_pid),
        before,
        "choices keep terminal echo"
    );
    let access_at = transcript.len();
    assert!(
        String::from_utf8_lossy(&transcript)
            .contains("1. API accounts or 2. Custom routes or 3. Role routing")
    );
    input
        .write_all(b"API accounts\r")
        .expect("open API accounts");
    acknowledge_linear_pages(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (access_at, API_ACCOUNTS_PROMPT),
    );
    input
        .write_all(b"Add or replace native API account\r")
        .expect("select native API account");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        access_at,
        PRIVATE_FILE_PROMPT,
    );
    let consent_at = transcript.len();
    input
        .write_all(b"Use private file\r")
        .expect("private file consent");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        consent_at,
        PROVIDER_PROMPT,
    );
    input
        .write_all(b"Anthropic\r")
        .expect("named Anthropic choice");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Enter API key",
    );
    let hidden = tty_settings(product_pid);
    assert_ne!(hidden, before, "setup input echo is disabled");
    kill_process(product_pid, Signal::TSTP).expect("suspend setup");
    wait_stopped(product_pid);
    assert_eq!(
        tty_settings(product_pid),
        before,
        "echo restored while stopped"
    );
    let resumed_at = transcript.len();
    kill_process(product_pid, Signal::CONT).expect("resume setup");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[resumed_at..]
        .windows(b"Enter API key".len())
        .any(|part| part == b"Enter API key")
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(Instant::now() < deadline, "setup did not resume");
        thread::yield_now();
    }
    assert_eq!(
        tty_settings(product_pid),
        hidden,
        "echo hidden after resume"
    );
    input.write_all(&[b'K'; 513]).expect("overlong key prefix");
    input.write_all(b"\n").expect("reject overlong key");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Invalid or overlong API key; retry",
    );
    input
        .write_all(b"SYNTHETIC_SECRET_CANARY\r")
        .expect("submit hidden synthetic key");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"type a choice name: 1. Key scoped to workspace or 2. Choose API workspace; empty Enter selects 1. Key scoped to workspace\r\nInput:\r\n",
    );
    assert_eq!(
        tty_settings(product_pid),
        before,
        "workspace choices restore echo"
    );
    let field_at = transcript.len();
    input
        .write_all(b"Choose API workspace\r")
        .expect("explicit workspace choice");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        field_at,
        b"Input:\r\n",
    );
    for invalid in [b"\r".as_slice(), b"wrong\r".as_slice()] {
        let invalid_at = transcript.len();
        input.write_all(invalid).expect("invalid visible workspace");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            invalid_at,
            b"Notice: Error: enter wrkspc_ followed by letters or digits",
        );
    }
    input
        .write_all(b"wrkspc_Unsubmitted")
        .expect("visible workspace draft");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"wrkspc_Unsubmitted",
    );
    input.write_all(b"\x03").expect("cancel setup");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if child.child().try_wait().expect("product status").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "setup did not exit after cancellation"
        );
        thread::yield_now();
    }
    let status = child.take().wait().expect("product exit");
    assert!(status.success(), "setup failed after cancellation");
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    let text = String::from_utf8_lossy(&transcript);
    assert!(!text.contains("SYNTHETIC_SECRET_CANARY"));
    assert!(!text.contains(&"K".repeat(513)));
    assert!(!state.exists(), "cancelled setup created State");
    assert!(
        account_before
            == account_root
                .record(RecordKind::SavedAccount)
                .read()
                .expect("account after cancel"),
        "cancelled workspace setup replaced the account"
    );
    assert!(
        !transcript.contains(&0x1b),
        "linear workspace setup emitted terminal controls"
    );
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("TTY marker")
            .to_owned()
    };
    assert_eq!(before, marker("TTY_AFTER:"));
}

#[cfg(debug_assertions)]
#[test]
fn setup_signals_after_hidden_key_segment_restore_echo_and_preserve_defaults() {
    for (signal, signal_name, attached) in [
        (Signal::TERM, "SIGTERM", false),
        (Signal::HUP, "SIGHUP", false),
        (Signal::TERM, "SIGTERM", true),
        (Signal::HUP, "SIGHUP", true),
    ] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        prepare_test_file_account(temp.path());
        let account_path = temp.path().join("account-root/account-credentials.json");
        let account_before = std::fs::read(&account_path).expect("original account record");
        let shell = if attached {
            MIXED_START_SHELL
        } else {
            SETUP_SHELL
        };
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
            .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false")
            .env("SHELL", "/bin/sh")
            .env("TERM", "dumb")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("setup PTY process"));
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
            b"SCRIPT_INPUT_GATE",
        );
        input.write_all(b"go\n").expect("script input gate");
        if attached {
            wait_for(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                b"Input:\r\n",
            );
        }
        let setup_at = transcript.len();
        if attached {
            input
                .write_all(b"/setup\r")
                .expect("setup in existing Session");
        }
        let access_at = transcript.len();
        if attached {
            choose_native_account_setup(&mut output, &mut input, &mut transcript, &mut answered);
        } else {
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                setup_at,
                b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
            );
            input
                .write_all(b"API accounts\r")
                .expect("open API accounts");
            acknowledge_linear_pages(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                (access_at, API_ACCOUNTS_PROMPT),
            );
            input
                .write_all(b"Add or replace native API account\r")
                .expect("select native account");
        }
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            access_at,
            PRIVATE_FILE_PROMPT,
        );
        let consent_at = transcript.len();
        input
            .write_all(b"Use private file\r")
            .expect("private file consent");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            consent_at,
            PROVIDER_PROMPT,
        );
        input.write_all(b"OpenAI\r").expect("OpenAI choice");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Enter API key",
        );
        let text = String::from_utf8_lossy(&transcript);
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("setup PTY marker")
                .to_owned()
        };
        let shell_pid = marker("SHELL_PID:").parse::<u32>().expect("shell PID");
        let before = marker("TTY_BEFORE:");
        let product_pid = product_child_of(shell_pid);
        let _product = ProductGuard::new(product_pid, state.clone());
        assert_ne!(tty_settings(product_pid), before, "hidden input owns echo");
        let segment_at = transcript.len();
        input
            .write_all(b"SYNTHETIC_SECRET_CANARY")
            .expect("hidden key segment");
        input.write_all(b"\x04").expect("continue hidden key line");
        let retry_notice = b"Invalid or overlong API key line; retry";
        let deadline = Instant::now() + Duration::from_secs(10);
        while !transcript[segment_at..]
            .windows(retry_notice.len())
            .any(|part| part == retry_notice)
        {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "{signal_name} did not consume hidden key segment"
            );
            thread::yield_now();
        }
        kill_process(product_pid, signal).expect("signal setup");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            if child.child().try_wait().expect("product status").is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "setup did not exit after signal");
            thread::yield_now();
        }
        let status = child.take().wait().expect("product exit");
        assert_eq!(status.code(), Some(1), "{signal_name}: ordinary error exit");
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        let text = String::from_utf8_lossy(&transcript);
        assert!(text.contains(&format!("terminated by {signal_name}")));
        let canary = text.find("SYNTHETIC_SECRET_CANARY");
        let shutdown = text.find(&format!("terminated by {signal_name}"));
        let shell_return = text.find("TTY_AFTER:");
        assert!(
            canary.is_none(),
            "{signal_name} exposed synthetic input: canary={canary:?}, shutdown={shutdown:?}, shell_return={shell_return:?}"
        );
        let account_after = std::fs::read(&account_path).expect("account after shutdown");
        assert!(
            account_before == account_after,
            "shutdown changed account record"
        );
        if attached {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("read-only runtime");
            runtime.block_on(async {
                let sessions = arany::list_sessions(
                    StateRoot::open_existing(&state).expect("existing State"),
                    workspace.clone(),
                )
                .await
                .expect("Workspace Sessions");
                assert!(
                    sessions.is_empty(),
                    "startup, local rejection and setup navigation save no empty conversation"
                );
            });
        } else {
            assert!(!state.exists(), "signaled pre-Session setup created State");
        }
        let after = text
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("TTY_AFTER:"))
            .expect("TTY after marker");
        assert_eq!(before, after, "terminal echo restored after {signal_name}");
    }
}

#[cfg(debug_assertions)]
#[test]
fn unsafe_dbus_address_cannot_launch_a_credential_command() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let marker = temp.path().join("credential-command-marker");
    let address = format!("unixexec:path=/usr/bin/touch,argv1={}", marker.display());
    crate::isolation::remember_workspace_trust(
        &temp.path().join("account-root"),
        &workspace,
        &workspace,
    );
    prepare_selected_chatgpt_account(temp.path());
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env("DBUS_SESSION_BUS_ADDRESS", address)
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args([
            "-q",
            "-e",
            "-c",
            "exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"",
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGuard::new(command.spawn().expect("setup PTY process"));
    let _product = ProductGuard::new(product_child_of(child.child().id()), state.clone());
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
    assert!(
        String::from_utf8_lossy(&transcript)
            .contains("unsafe or unsupported D-Bus session address"),
        "unsafe transport rejection missing: {}",
        tail(&transcript)
    );
    input.write_all(b"/exit\r").expect("exit unconfigured chat");
    child.child().stdout = Some(output);
    let result = wait_product(child.take());
    transcript.extend_from_slice(&result.stdout);
    assert!(
        result.status.success(),
        "unsafe transport startup exit: {:?}; stdout: {}; stderr: {}",
        result.status,
        tail(&transcript),
        tail(&result.stderr)
    );
    let text = String::from_utf8_lossy(&transcript);
    assert!(!marker.exists(), "credential command was not executed");
    // Rejected startup authorization keeps unconfigured chat; it saves no conversation.
    assert!(text.contains("No conversation saved."));
    if let Ok(root) = StateRoot::open_existing(&state) {
        let sessions = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("fixture runtime")
            .block_on(arany::list_sessions(root, workspace.clone()))
            .expect("Session list");
        assert!(sessions.is_empty(), "unsafe transport created a Session");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn credential_helper_rejects_invalid_record_before_os_store_access() {
    let temp = tempfile::tempdir().expect("private test root");
    let socket = temp.path().join("bus");
    let listener = UnixListener::bind(&socket).expect("private synthetic D-Bus socket");
    listener
        .set_nonblocking(true)
        .expect("nonblocking synthetic bus");
    let mut child = Command::new("/usr/bin/timeout")
        .args([
            "2s",
            env!("CARGO_BIN_EXE_arany"),
            "--internal-credential-helper",
            "write",
            "default-native-api-account",
        ])
        .env_clear()
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("credential helper");
    child
        .stdin
        .take()
        .expect("bounded input pipe")
        .write_all(b"invalid record")
        .expect("synthetic invalid record");
    let status = child.wait().expect("helper status");
    assert_ne!(
        status.code(),
        Some(124),
        "invalid record caused OS-store wait"
    );
    assert!(!status.success(), "invalid record accepted");
    assert!(
        listener
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "invalid record reached OS store"
    );
    let status = Command::new("/usr/bin/timeout")
        .args([
            "2s",
            env!("CARGO_BIN_EXE_arany"),
            "--internal-credential-helper",
            "delete",
            "unsupported-account-slot",
        ])
        .env_clear()
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("invalid deletion helper");
    assert!(!status.success() && status.code() != Some(124));
    assert!(
        listener
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "native account deletion reached OS store"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn saved_account_fifo_is_rejected_without_blocking_or_keyring_access() {
    let temp = tempfile::tempdir().expect("private test root");
    #[cfg(debug_assertions)]
    let account_root = temp.path().join("account-root");
    #[cfg(not(debug_assertions))]
    let private_home = temp.path().join("home");
    #[cfg(not(debug_assertions))]
    let account_root = private_home.join(".local/state/arany");
    drop(StateRoot::admit(&account_root).expect("private account root"));
    let run = || {
        let mut command = Command::new("/usr/bin/timeout");
        command.args(["--signal=TERM", "--kill-after=1s", "2s"]);
        #[cfg(debug_assertions)]
        command
            .arg(env!("CARGO_BIN_EXE_arany"))
            .args(["provider", "models", "openai", "--saved-account"])
            .env_clear()
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false")
            .current_dir(temp.path());
        #[cfg(not(debug_assertions))]
        {
            let passwd_home = release_passwd_home();
            command
                .args([
                    "/usr/bin/bwrap",
                    "--unshare-user",
                    "--unshare-net",
                    "--unshare-pid",
                    "--die-with-parent",
                    "--tmpfs",
                    "/",
                    "--ro-bind",
                    "/usr",
                    "/usr",
                    "--symlink",
                    "usr/lib",
                    "/lib",
                    "--ro-bind-try",
                    "/lib64",
                    "/lib64",
                    "--ro-bind",
                    "/etc/passwd",
                    "/etc/passwd",
                ])
                .arg("--bind")
                .arg(&private_home)
                .arg(&passwd_home)
                .arg("--ro-bind")
                .arg(env!("CARGO_BIN_EXE_arany"))
                .arg("/arany")
                .args(["--proc", "/proc", "--dev-bind", "/dev", "/dev"])
                .arg("--chdir")
                .arg(&passwd_home)
                .arg("--clearenv")
                .arg("--setenv")
                .arg("HOME")
                .arg(&passwd_home)
                .arg("--setenv")
                .arg("XDG_STATE_HOME")
                .arg(passwd_home.join(".local/state"))
                .args([
                    "--setenv",
                    "DBUS_SESSION_BUS_ADDRESS",
                    "unixexec:path=/usr/bin/false",
                    "--setenv",
                    "PATH",
                    "/usr/bin",
                    "--",
                    "/arany",
                    "provider",
                    "models",
                    "openai",
                    "--saved-account",
                ])
                .env_clear()
                .current_dir(temp.path());
        }
        command.bounded_output().expect("saved-account read")
    };
    let without_leaf = run();
    assert!(
        String::from_utf8_lossy(&without_leaf.stderr)
            .contains("unsafe or unsupported D-Bus session address"),
        "missing account leaf must reach credential transport admission; exit={:?}; stderr={:?}",
        without_leaf.status,
        tail(&without_leaf.stderr)
    );
    let fifo = account_root.join("account-credentials.json");
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .expect("FIFO fixture")
            .success()
    );
    let output = run();
    assert!(
        !matches!(output.status.code(), Some(124 | 137)),
        "FIFO blocked account admission"
    );
    assert!(!output.status.success(), "FIFO account accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("private account state unavailable"),
        "FIFO did not fail at account State admission"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires a native unlocked OS credential store"]
fn native_credential_helper_round_trips_a_synthetic_isolated_slot() {
    const SERVICE: &str = "io.github.fpmirabile.arany";
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(entry) = keyring::Entry::new(SERVICE, &self.0) {
                let _ = entry.delete_credential();
            }
        }
    }

    let temp = tempfile::tempdir().expect("private test root");
    let slot = format!("native-test-{}", Uuid::now_v7());
    let _cleanup = Cleanup(slot.clone());
    let account = serde_json::to_vec(&serde_json::json!({
        "schema": 3,
        "id": Uuid::now_v7(),
        "provider": {"kind": "openai", "custom_name": null},
        "model": "gpt-5.4",
        "effort": null,
        "api_key": "synthetic-native-helper-key", "anthropic_workspace_id": null
    }))
    .expect("synthetic account");
    let mut command = credential_helper_command("write", &slot, temp.path());
    let mut write = ChildGuard::new(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("native helper write"),
    );
    write
        .child()
        .stdin
        .take()
        .expect("private input pipe")
        .write_all(&account)
        .expect("synthetic account write");
    assert!(wait_product(write.take()).status.success());

    let output = wait_product(
        credential_helper_command("read", &slot, temp.path())
            .stdout(Stdio::piped())
            .spawn()
            .expect("native helper read"),
    );
    assert!(output.status.success(), "native helper read failed");
    assert!(output.stdout.len() <= RecordKind::SavedAccount.limit());
    let loaded: serde_json::Value = serde_json::from_slice(&output.stdout).expect("saved record");
    let expected: serde_json::Value = serde_json::from_slice(&account).expect("synthetic record");
    assert_eq!(loaded, expected);
    assert!(output.stderr.is_empty());
    keyring::Entry::new(SERVICE, &slot)
        .expect("synthetic account entry")
        .delete_credential()
        .expect("remove synthetic account");
}

#[cfg(unix)]
fn credential_helper_command(operation: &str, slot: &str, directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .args(["--internal-credential-helper", operation, slot])
        .env_clear()
        .current_dir(directory)
        .stderr(Stdio::null());
    for name in [
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "HOME",
        "USER",
        "LOGNAME",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

#[cfg(target_os = "linux")]
// Waits for the Classic composer caret in `column`, located by the composer border on the
// decoded 40x12 screen instead of a fixed row, since the content-sized viewport moves.
fn wait_composer_column(
    output: &mut std::process::ChildStdout,
    input: &mut std::process::ChildStdin,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    from: usize,
    column: usize,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !crate::composer_caret(transcript, from, 40, 12).is_some_and(|(caret, _)| caret == column)
    {
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "composer caret at column {column}: {:?}",
            crate::screen_rows(transcript, 40, 12)
        );
        thread::yield_now();
    }
}

#[test]
fn stalled_credential_bus_does_not_strand_unconfigured_start() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    crate::isolation::remember_workspace_trust(
        &temp.path().join("account-home/.local/state/arany"),
        &workspace,
        &workspace,
    );
    let state = temp.path().join("state");
    let socket = temp.path().join("bus");
    let listener = UnixListener::bind(&socket).expect("private synthetic D-Bus socket");
    listener
        .set_nonblocking(true)
        .expect("nonblocking synthetic bus");
    let mut command = super::isolation::isolated_script(temp.path());
    command
        .env_clear()
        .env("ARANY_TEST_EXE", "/arany")
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", socket.display()))
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args([
            "-q",
            "-e",
            "-c",
            "exec /usr/bin/timeout --signal=TERM --kill-after=1s 7s \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"",
            "/dev/null",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("first-run setup PTY"));
    let output = wait_product(child.take());
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "unconfigured startup must not probe the OS store"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Input:"),
        "unconfigured chat must open before EOF"
    );
    assert_ne!(
        output.status.code(),
        Some(124),
        "credential lookup exceeded 7s"
    );
    assert_ne!(
        output.status.code(),
        Some(137),
        "credential lookup was killed"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let root = StateRoot::open_existing(&state).expect("unconfigured Session State");
        let sessions = arany::list_sessions(root, workspace.clone())
            .await
            .expect("Workspace Sessions");
        assert!(
            sessions.is_empty(),
            "startup, local rejection and setup navigation save no empty conversation"
        );
    });

    let account_path = temp.path().join("account-home/.local/state/arany");
    let account_root = StateRoot::admit(&account_path).expect("isolated account marker");
    account_root
        .record(RecordKind::SavedAccount)
        .replace(br#"{"schema":1,"storage":"keyring","account":null}"#)
        .expect("keyring marker without credential");
    drop(account_root);
    let state = temp.path().join("resumed-state");
    let session_id = runtime
        .block_on(create_session(
            StateRoot::admit(&state).expect("resumed State"),
            workspace.clone(),
            None,
        ))
        .expect("configured Session");
    let defaults = SessionDefaults {
        provider: Some("openai".into()),
        model: Some("gpt-5.4".into()),
        effort: None,
        account_id: Some(Uuid::now_v7()),
        policy: Default::default(),
    };
    runtime
        .block_on(set_session_defaults(
            StateRoot::open_existing(&state).expect("State reopen"),
            workspace.clone(),
            session_id,
            defaults.clone(),
        ))
        .expect("configured defaults");
    for (linear, replaced) in [(true, false), (false, false), (true, true)] {
        let executable = if replaced {
            let executable = temp.path().join("mapped-arany");
            std::fs::copy(env!("CARGO_BIN_EXE_arany"), &executable).expect("private product image");
            executable
        } else {
            PathBuf::from("/arany")
        };
        let defaults = if replaced {
            let account_id = prepare_selected_chatgpt_account_at(&account_path);
            SessionDefaults {
                provider: Some("chatgpt".into()),
                model: Some("gpt-6.1-sol".into()),
                effort: Some(arany::Effort::Low),
                account_id: Some(account_id),
                policy: Default::default(),
            }
        } else {
            defaults.clone()
        };
        runtime
            .block_on(set_session_defaults(
                StateRoot::open_existing(&state).expect("resume State"),
                workspace.clone(),
                session_id,
                defaults.clone(),
            ))
            .expect("resume selection");
        let prefix = runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
                .expect("read-only prefix");
            let events = store.load_session(session_id).await.expect("prefix Events");
            store.close().await.expect("close prefix");
            events
        });
        let shell = "printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; stty cols 40 rows 12; if [ \"$ARANY_TEST_LINEAR\" = 1 ]; then set -- --screen-reader; else set -- --no-color; fi; \"$ARANY_TEST_EXE\" --classic \"$@\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"; exit_code=$?; stty \"$before\"; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
        let mut command = super::isolation::isolated_script(temp.path());
        command
            .env_clear()
            .env("ARANY_TEST_EXE", &executable)
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("ARANY_TEST_SESSION", session_id.to_string())
            .env("ARANY_TEST_LINEAR", if linear { "1" } else { "0" })
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", socket.display()),
            )
            .env("SHELL", "/bin/sh")
            .env("TERM", if linear { "dumb" } else { "xterm-256color" })
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("resumed credential-wait PTY"));
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
            b"Authorize saved access",
        );
        let text = String::from_utf8_lossy(&transcript);
        let shell_pid = text
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse::<u32>()
            .expect("shell PID number");
        let product_pid = product_child_of_executable(shell_pid, &executable);
        let _product = ProductGuard::for_executable(product_pid, state.clone(), executable.clone());
        let startup_deadline = Instant::now() + Duration::from_secs(2);
        let startup_connection = loop {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    pump(&mut output, &mut input, &mut transcript, &mut answered);
                    assert!(
                        Instant::now() < startup_deadline,
                        "startup authorization never reached the private bus"
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("synthetic startup bus accept failed: {error}"),
            }
        };
        let startup_end = transcript.len();
        drop(startup_connection);
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            startup_end,
            if linear {
                b"Input:\r\n"
            } else {
                b"Arany \xc2\xb7 error:"
            },
        );
        if !linear {
            wait_composer_column(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                startup_end,
                3,
            );
        }
        if replaced {
            let replacement = temp.path().join("replacement-arany");
            std::fs::write(&replacement, "#!/bin/sh\nexit 42\n").expect("replacement image");
            std::fs::set_permissions(
                &replacement,
                std::fs::metadata(&executable)
                    .expect("image metadata")
                    .permissions(),
            )
            .expect("replacement permissions");
            std::fs::rename(&replacement, &executable).expect("replace mapped product image");
            assert!(
                std::fs::read_link(format!("/proc/{}/exe", product_pid.as_raw_pid()))
                    .expect("mapped executable")
                    .as_os_str()
                    .as_encoded_bytes()
                    .ends_with(b" (deleted)"),
                "replacement must leave the original process image mapped"
            );
        }
        let submitted_start = transcript.len();
        input
            .write_all(b"synthetic credential-wait objective\r")
            .expect("submit objective");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut connection = loop {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    pump(&mut output, &mut input, &mut transcript, &mut answered);
                    assert!(
                        Instant::now() < deadline,
                        "submission never reached credential wait (replaced image: {replaced}): {}",
                        tail(&transcript)
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("synthetic admission bus accept failed: {error}"),
            }
        };
        // While admission waits, the decoded composer shows the draft and the status row names
        // the preparing state. The composer row follows the content-sized viewport, so the
        // frame is found by its border, not by a fixed caret row.
        let admission_frame = |transcript: &[u8], from: usize, draft: &str| {
            let end = transcript[from..]
                .windows(6)
                .rposition(|part| part == b"\x1b[?25h")?;
            let screen = crate::screen_rows(&transcript[..from + end + 6], 40, 12);
            let top = screen
                .iter()
                .position(|row| row.starts_with("─ Ask Arany"))?;
            (screen.get(top + 1)?.starts_with(&format!("> {draft}"))
                && screen.get(top + 3)?.trim_start().starts_with("preparing"))
            .then_some(screen)
        };
        let wait_admission =
            |output: &mut std::process::ChildStdout,
             input: &mut std::process::ChildStdin,
             transcript: &mut Vec<u8>,
             answered: &mut crate::session_picker::PtyResponses,
             (from, draft, label): (usize, &str, &str)| {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let Some(screen) = admission_frame(transcript, from, draft) {
                        return screen;
                    }
                    pump(output, input, transcript, answered);
                    assert!(
                        Instant::now() < deadline,
                        "{label}: {:?}",
                        crate::screen_rows(transcript, 40, 12)
                    );
                    thread::yield_now();
                }
            };
        if linear {
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                submitted_start,
                b"Notice: Preparing request... Ctrl+C cancels\r\n",
            );
        } else {
            wait_admission(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                (submitted_start, "", "admission shows preparing"),
            );
        }
        let edited_start = transcript.len();
        input
            .write_all(if linear { b"retained\x04" } else { b"retained" })
            .expect("continue a new draft during admission");
        if linear {
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                edited_start,
                b"Notice: Draft: 8 characters; Enter offers it as a steering note\r\n",
            );
        } else {
            wait_admission(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                (
                    edited_start,
                    "retained",
                    "editing must not restore ready during admission",
                ),
            );
        }
        let interrupted_start = transcript.len();
        kill_process(product_pid, Signal::INT).expect("interrupt pending admission");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            interrupted_start,
            if linear {
                b"Notice: Submission interrupted before Run admission; no task was accepted. Account work already started may still complete.\r\n"
            } else {
                b"Submission"
            },
        );
        if linear {
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                interrupted_start,
                b"Notice: Draft retained: 44 characters; Enter submits\r\n",
            );
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                interrupted_start,
                b"Input:\r\n",
            );
        } else {
            wait_composer_column(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                interrupted_start,
                11,
            );
        }
        let continuation_start = transcript.len();
        input
            .write_all(if linear { b"-suffix\x04" } else { b"-suffix" })
            .expect("continue recovered draft");
        if linear {
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                continuation_start,
                b"Notice: Draft: 51 characters; Enter submits\r\n",
            );
        } else {
            wait_composer_column(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                continuation_start,
                18,
            );
        }
        if linear {
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                continuation_start,
                b"Input:\r\n",
            );
        }
        let cleared_start = transcript.len();
        kill_process(product_pid, Signal::INT).expect("clear retained draft while idle");
        if linear {
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                cleared_start,
                b"Input:\r\n",
            );
        } else {
            wait_composer_column(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                cleared_start,
                3,
            );
        }
        input
            .write_all(b"/exit\r")
            .expect("explicit quit after cancellation");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            cleared_start,
            b"TTY_AFTER:",
        );
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            result.status.success(),
            "interrupted admission must return to chat"
        );
        assert!(result.stderr.is_empty());
        assert_eq!(
            answered.cursor_queries > 0,
            !linear,
            "only Classic observes the cursor at acquisition and height changes"
        );
        let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
        if linear {
            assert!(!text.contains('\x1b'));
        }
        assert!(!text.contains("Run outcome unavailable"));
        assert!(!text.contains("Status: Cancelled"));
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| {
                    line.trim_end_matches('\r')
                        .split_once(name)
                        .map(|(_, value)| value)
                })
                .expect("terminal marker")
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
        runtime.block_on(async {
            let store =
                Store::open_read_only(StateRoot::open_existing(&state).expect("State reopen"))
                    .expect("closed read-only Store");
            let events = store.load_session(session_id).await.expect("closed Events");
            assert_eq!(
                events, prefix,
                "no accepted objective or Run on interrupted admission"
            );
            let view = SessionView::replay(session_id, &events)
                .expect("strict replay")
                .expect("Session");
            assert_eq!(view.defaults, defaults);
            assert!(view.runs.is_empty());
            store.close().await.expect("close replay");
        });
        connection
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer read deadline");
        let mut observed = 0;
        loop {
            let mut bytes = [0; 1024];
            let count = connection
                .read(&mut bytes)
                .expect("supervised helper closed peer");
            if count == 0 {
                break;
            }
            observed += count;
            assert!(observed <= 4096, "bounded admission handshake");
        }
    }
}
