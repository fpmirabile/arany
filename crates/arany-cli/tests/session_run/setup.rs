#[cfg(any(debug_assertions, target_os = "linux"))]
use super::active_terminal::{ProductGuard, product_child_of_executable};
#[cfg(debug_assertions)]
use super::active_terminal::{product_child_of, tty_settings, wait_stopped};
use super::loopback::{ChildGuard, wait_product};
use super::process::BoundedOutput;
use super::session_picker::{pump, tail, wait_for, wait_for_after};
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

#[path = "setup/native.rs"]
mod native;

#[path = "setup/chatgpt.rs"]
mod chatgpt;

#[cfg(debug_assertions)]
#[path = "setup/custom.rs"]
mod custom;

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

pub(crate) fn legacy_chatgpt_slot(client_id: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    format!(
        "chatgpt-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(client_id))
    )
}

fn prepare_selected_chatgpt_account_at(account_root: &Path) -> Uuid {
    let state = StateRoot::admit(account_root).expect("test account root");
    let account_id = Uuid::now_v7();
    let record = serde_json::json!({
        "schema": 4,
        "selected": account_id,
        "accounts": [{
            "id": account_id,
            "host_id": "123e4567-e89b-42d3-a456-426614174000",
            "client_id": "oaiapp_synthetic_saved",
            "subject": "synthetic-subject",
            "chatgpt_account_id": null,
            "keyring_slot": legacy_chatgpt_slot("oaiapp_synthetic_saved"),
            "storage": "keyring",
            "renewal_pending": false, "signout_pending": false,
            "disconnected": false, "slot_client_id": "oaiapp_synthetic_saved", "token": null
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
    let saved_at = add_custom_route(&mut pty, b"2\r", b"1\r");
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
fn add_custom_route(pty: &mut SetupPty, choice: &[u8], protocol: &[u8]) -> usize {
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
        .write_all(protocol)
        .expect("select custom protocol");
    let destination: &[u8] = if protocol == b"3\r" {
        b"http://127.0.0.1:9000/v1\r"
    } else {
        b"http://127.0.0.1:9000/v1/responses\r"
    };
    let fields = [
        (b"Custom route name".as_slice(), b"first-route\r".as_slice()),
        (b"Custom route destination".as_slice(), destination),
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
