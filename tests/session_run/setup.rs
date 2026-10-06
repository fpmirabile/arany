#[cfg(any(debug_assertions, target_os = "linux"))]
use super::active_terminal::{ProductGuard, product_child_of_executable};
#[cfg(debug_assertions)]
use super::active_terminal::{product_child_of, tty_settings, wait_stopped};
use super::loopback::{ChildGuard, wait_product};
use super::process::BoundedOutput;
use super::session_picker::{pump, tail};
#[cfg(any(debug_assertions, target_os = "linux"))]
use arany::{SessionDefaults, create_session, set_session_defaults};
use arany::{SessionView, StateRoot, Store};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
#[cfg(any(debug_assertions, target_os = "linux"))]
use rustix::process::{Signal, kill_process};
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixListener;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[path = "setup/offline_https.rs"]
mod offline_https;

#[cfg(debug_assertions)]
const SETUP_SHELL: &str = "trap ':' INT; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; printf 'SCRIPT_INPUT_GATE\n'; IFS= read -r gate; [ \"$gate\" = go ] || exit 2; \"$ARANY_TEST_EXE\" --screen-reader --setup --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";

#[cfg(debug_assertions)]
const MIXED_START_SHELL: &str = "trap ':' INT; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; printf 'SCRIPT_INPUT_GATE\n'; IFS= read -r gate; [ \"$gate\" = go ] || exit 2; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";

#[cfg(all(target_os = "linux", not(debug_assertions)))]
const RELEASE_ACCOUNT_SHELL: &str = r#"before=$(stty -g); printf 'TTY_BEFORE:%s\n' "$before"; printf 'SCRIPT_INPUT_GATE\n'; IFS= read -r gate; [ "$gate" = go ] || exit 2; if [ "$ARANY_TEST_SETUP" = 1 ]; then set -- --setup; else set --; fi; /usr/bin/timeout --foreground -k 1s 20s /usr/bin/bwrap --unshare-user --unshare-net --unshare-pid --die-with-parent --tmpfs / --ro-bind /usr /usr --symlink usr/lib /lib --ro-bind /lib64 /lib64 --ro-bind /etc/passwd /etc/passwd --bind "$ARANY_TEST_HOME" "$ARANY_PASSWD_HOME" --ro-bind "$ARANY_TEST_EXE" /arany --proc /proc --dev-bind /dev /dev --chdir "$ARANY_PASSWD_HOME/workspace" --clearenv --setenv HOME "$ARANY_TEST_HOME_ENV" --setenv XDG_STATE_HOME "$ARANY_TEST_XDG_STATE" --setenv TERM dumb --setenv PATH /usr/bin -- /arany --screen-reader "$@" --state-dir "$ARANY_TEST_STATE" --workspace "$ARANY_PASSWD_HOME/workspace"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' "$after"; exit "$exit_code""#;

#[cfg(debug_assertions)]
fn prepare_test_file_account(root: &Path) -> Uuid {
    prepare_test_file_account_at(&root.join("account-root"))
}

fn prepare_test_file_account_at(account_root: &Path) -> Uuid {
    let state = StateRoot::admit(account_root).expect("test account root");
    let account_id = Uuid::now_v7();
    let record = serde_json::json!({
        "schema": 1,
        "storage": "private_file",
        "account": {
            "schema": 1,
            "id": account_id,
            "provider": "openai",
            "model": "gpt-5.4",
            "effort": null,
            "api_key": "synthetic-existing-key"
        }
    });
    state
        .replace_saved_account_record(&serde_json::to_vec(&record).expect("test record"))
        .expect("test file backend");
    account_id
}

#[cfg(debug_assertions)]
fn prepare_selected_chatgpt_account(root: &Path) -> Uuid {
    prepare_selected_chatgpt_account_at(&root.join("account-root"))
}

fn prepare_selected_chatgpt_account_at(account_root: &Path) -> Uuid {
    let state = StateRoot::admit(account_root).expect("test account root");
    let account_id = Uuid::now_v7();
    let record = serde_json::json!({
        "schema": 1,
        "selected": account_id,
        "accounts": [{
            "id": account_id,
            "host_id": "123e4567-e89b-42d3-a456-426614174000",
            "client_id": "oaiapp_synthetic_saved",
            "subject": "synthetic-subject",
            "storage": "keyring",
            "renewal_pending": false,
            "token": null
        }]
    });
    state
        .replace_chatgpt_accounts_record(&serde_json::to_vec(&record).expect("account index"))
        .expect("test ChatGPT account index");
    account_id
}

fn wait_for(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut usize,
    needle: &[u8],
) {
    let mut declined = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript.windows(needle.len()).any(|part| part == needle) {
        pump(output, input, transcript, answered);
        super::process::decline_workspace_consent(input, transcript, &mut declined);
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
    answered: &mut usize,
    start: usize,
    needle: &[u8],
) {
    let mut declined = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[start..]
        .windows(needle.len())
        .any(|part| part == needle)
    {
        pump(output, input, transcript, answered);
        super::process::decline_workspace_consent(input, transcript, &mut declined);
        assert!(
            Instant::now() < deadline,
            "setup PTY stage missing: {}",
            tail(transcript)
        );
        thread::yield_now();
    }
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
    let mut answered = 0;
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
    input.write_all(b"/quit\r").expect("quit Session");
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "saved-account startup failed");
    assert!(result.stderr.is_empty(), "script wrapper stderr");
    assert_eq!(answered, 0, "screen-reader mode uses no cursor query");
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(!text.contains("Choose access method"), "setup was reopened");
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
    for (choice, corrupt_native, corrupt_chatgpt, provider, success) in [
        (Some("API key"), false, true, Some("openai"), true),
        (Some("ChatGPT plan"), true, false, Some("chatgpt"), true),
        (Some(""), true, true, None, true),
        (Some("Cancel"), true, true, None, true),
        (None, false, false, None, true),
        (Some("API key"), true, false, None, false),
        (Some("ChatGPT plan"), false, true, None, false),
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
                .replace_saved_account_record(b"invalid unselected account JSON")
                .expect("corrupt unselected native record");
        }
        if corrupt_chatgpt {
            StateRoot::open_existing(&temp.path().join("account-root"))
                .expect("account root")
                .replace_chatgpt_accounts_record(b"invalid unselected ChatGPT JSON")
                .expect("corrupt unselected ChatGPT index");
        }
        let account_root = StateRoot::open_existing(&temp.path().join("account-root"))
            .expect("protected account root");
        let native_before = account_root
            .read_saved_account_record()
            .expect("native account snapshot");
        let chatgpt_before = account_root
            .read_chatgpt_accounts_record()
            .expect("ChatGPT account snapshot");
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
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
        let mut answered = 0;
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
            "type a choice name: Cancel or API key or ChatGPT plan; empty Enter selects Cancel"
        ));
        let choice_at = transcript.len();
        match choice {
            Some(name) => {
                input
                    .write_all(name.as_bytes())
                    .expect("choose billing route");
                input.write_all(b"\r").expect("submit billing route");
                if provider.is_some() {
                    wait_for_after(
                        &mut output,
                        &mut input,
                        &mut transcript,
                        &mut answered,
                        choice_at,
                        b"Input:\r\n",
                    );
                    input.write_all(b"/quit\r").expect("quit Session");
                }
            }
            None => input.write_all(b"\x03").expect("cancel billing choice"),
        }
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert_eq!(
            result.status.success(),
            success,
            "{choice:?}: {}",
            result.status
        );
        assert!(result.stderr.is_empty(), "script wrapper stderr");
        assert_eq!(answered, 0, "screen-reader mode queried the cursor");
        let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
        assert!(text.contains("Both saved; choose billing route"));
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
                .read_saved_account_record()
                .expect("unchanged native record")
                == native_before
        );
        assert!(
            account_root
                .read_chatgpt_accounts_record()
                .expect("unchanged ChatGPT record")
                == chatgpt_before
        );
        if provider.is_some() {
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
        if disconnected || other_saved {
            let account_root =
                StateRoot::open_existing(&temp.path().join("account-root")).expect("account root");
            let bytes = account_root
                .read_chatgpt_accounts_record()
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
                        "id": Uuid::now_v7(),
                        "host_id": "123e4567-e89b-42d3-a456-426614174000",
                        "client_id": "oaiapp_synthetic_other",
                        "subject": "synthetic-other-subject",
                        "storage": "keyring",
                        "renewal_pending": false,
                        "token": null
                    }));
            }
            account_root
                .replace_chatgpt_accounts_record(
                    &serde_json::to_vec(&index).expect("account index"),
                )
                .expect("disconnected account index");
        }
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
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
        let mut answered = 0;
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
            b"type a choice name: API key or ChatGPT plan",
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
        input.write_all(b"ChatGPT plan\r").expect("choose plan");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            if disconnected && !other_saved {
                b"type a choice name: Reconnect or Connect new"
            } else {
                b"type a choice name: Use saved or Reconnect or Connect new"
            },
        );
        input
            .write_all(format!("{choice}\r").as_bytes())
            .expect("choose selected account action");
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            !result.status.success(),
            "missing synthetic token was usable"
        );
        assert_eq!(answered, 0, "linear setup queried the cursor");
        let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
        if choice == "Reconnect" {
            assert!(text.contains("Saved ChatGPT keyring is unavailable"));
            assert!(text.contains("Unlock your desktop password store"));
            assert!(text.contains("Connect new, then Use private file (not encrypted)"));
            assert!(!text.contains("Choose Accept to continue"));
            if disconnected {
                assert!(text.contains("ChatGPT account disconnected"));
                assert!(!text.contains("type a choice name: Use saved"));
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
        assert!(!state.exists(), "failed reuse created Session State");
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
            .read_chatgpt_accounts_record()
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
        .read_chatgpt_accounts_record()
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
            "token": null
        }));
    account_root
        .replace_chatgpt_accounts_record(&serde_json::to_vec(&index).expect("two accounts"))
        .expect("save account index");
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
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
    let mut answered = 0;
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
        b"type a choice name: API key or ChatGPT plan",
    );
    input.write_all(b"ChatGPT plan\r").expect("select plan");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"type a choice name: Use saved or Reconnect or Connect new",
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
    assert_eq!(answered, 0, "screen-reader picker queried cursor");
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
        .read_chatgpt_accounts_record()
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
    for (name, fork) in [("new", String::new()), ("fork", Uuid::now_v7().to_string())] {
        let prompt_state = temp.path().join(format!("{name}-prompt-state"));
        let result = wait_product(
            Command::new("/usr/bin/script")
                .env_clear()
                .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
                .env("ARANY_TEST_STATE", &prompt_state)
                .env("ARANY_TEST_WORKSPACE", &workspace)
                .env("ARANY_TEST_FORK", fork)
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
        assert_eq!(
            lines[1], "error: a prompt requires a selected Provider; run arany and use /setup",
            "{name}: actionable admission error"
        );
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
    let mut answered = 0;
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
        !String::from_utf8_lossy(&transcript).contains("Choose access method"),
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
        b"type a choice name: API key or ChatGPT plan",
    );
    input.write_all(b"\x03").expect("cancel setup");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Notice: Setup closed\r\nInput:\r\n",
    );
    input.write_all(b"/quit\r").expect("quit Session");
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "unconfigured chat failed");
    assert!(result.stderr.is_empty(), "script wrapper stderr");
    assert_eq!(answered, 0, "screen-reader mode uses no cursor query");
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
        let mut answered = 0;
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
            b"empty Enter selects API key\r\nInput:\r\n",
        );
        let shell_pid = String::from_utf8_lossy(&transcript)
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse::<u32>()
            .expect("shell PID number");
        let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
        input.write_all(b"ChatGPT plan\r").expect("select plan");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"empty Enter selects Cancel\r\nInput:\r\n",
        );
        if accept_file {
            input
                .write_all(b"Use private file\r")
                .expect("select explicit fallback");
            wait_for(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                b"empty Enter selects Back\r\nInput:\r\n",
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
        assert_eq!(answered, 0, "linear setup must not query the cursor");
        assert!(!state.exists(), "cancelled setup created Session State");
        assert!(
            !account_root.exists(),
            "pre-consent setup created account State"
        );
        let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
        assert!(text.contains("Password store unavailable. Unencrypted file"));
        assert_eq!(text.contains("remote output-token limit"), accept_file);
        assert_eq!(
            text.contains("private account file is not encrypted"),
            accept_file
        );
        assert_eq!(
            text.contains("Choose Accept to continue, or Back to cancel."),
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
    let mut answered = 0;
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
        b"empty Enter selects API key\r\nInput:\r\n",
    );
    let access_at = transcript.len();
    input.write_all(b"API key\r").expect("select API key");
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
    assert!(
        String::from_utf8_lossy(&transcript[access_at..])
            .contains("type a choice name: Cancel or Use private file; empty Enter selects Cancel")
    );
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
    assert_eq!(answered, 0, "screen-reader setup queried the cursor");
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
            .replace_model_preferences_record(br#"{"version":1,"sources":[]}"#)
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
    let mut answered = 0;
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
            b"empty Enter selects Cancel\r\nInput:\r\n",
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
        input.write_all(b"/quit\r").expect("quit Session");
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
    assert_eq!(answered, 0, "screen-reader startup queried the cursor");
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(!text.contains("Choose access method"), "setup reopened");
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
    prepare_test_file_account_at(&private_home.join(".local/state/arany"));
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
    prepare_test_file_account_at(&account_path);
    prepare_selected_chatgpt_account_at(&account_path);
    let account = StateRoot::open_existing(&account_path).expect("private account root");
    account
        .replace_chatgpt_accounts_record(b"invalid-unselected-chatgpt-canary")
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
        .replace_saved_account_record(b"invalid-unselected-native-canary")
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
        .replace_chatgpt_accounts_record(b"invalid-unselected-chatgpt-canary")
        .expect("both invalid fixture");
    let native_before = account
        .read_saved_account_record()
        .expect("native snapshot");
    let chatgpt_before = account
        .read_chatgpt_accounts_record()
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
                .read_saved_account_record()
                .expect("unchanged native record")
                == native_before
        );
        assert!(
            account
                .read_chatgpt_accounts_record()
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
        let legacy_state = if expected_account.is_some() {
            "/owner/spoofed-state"
        } else {
            "/other/legacy-state"
        };
        let mut command = Command::new("/usr/bin/script");
        command
            .args(["-q", "-e", "-c", shell, "/dev/null"])
            .env_clear()
            .env("SHELL", "/usr/bin/sh")
            .env("TERM", "dumb")
            .env("HOME", "/owner")
            .env("XDG_STATE_HOME", legacy_state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("cross-user product PTY"));
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
        input.write_all(b"/quit\r").expect("quit Session");
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(result.status.success(), "cross-user product exit");
        assert_eq!(answered, 0, "screen-reader startup queried the cursor");
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
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        }
        return;
    }

    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("private cross-user fixture");
    let owner_home = temp.path().join("owner-home");
    std::fs::create_dir_all(owner_home.join("workspace")).expect("owner Workspace");
    std::fs::set_permissions(&owner_home, std::fs::Permissions::from_mode(0o700))
        .expect("owner-only home");
    let account_id = prepare_test_file_account_at(&owner_home.join(".local/state/arany"));
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
            "--ro-bind",
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
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
    let mut answered = 0;
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
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
    input.write_all(b"\x1b[B\r").expect("select ChatGPT plan");
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
        b"ChatGPT plan con",
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
fn unconfigured_inline_composer_shows_draft_and_setup_selection_at_narrow_widths() {
    let executable = std::env::var_os("ARANY_RELEASE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_arany")));
    assert!(executable.is_absolute() && executable.is_file());
    for columns in [16, 40] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        let shell = format!(
            "stty rows 8 cols {columns}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
        );
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("ARANY_TEST_EXE", &executable)
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
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
        let mut answered = 0;
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Ask Arany",
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
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            quick_at,
            b"> Setup",
        );
        let quick_setup_at = transcript.len();
        input.write_all(b"\r").expect("open focused setup action");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            quick_setup_at,
            b"> API",
        );
        let header_end = quick_setup_at
            + transcript[quick_setup_at..]
                .windows(5)
                .position(|part| part == b"> API")
                .expect("selected setup row")
            + 5;
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
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            closed_at,
            b"Setup closed",
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
            b"API key",
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
        input.write_all(b"\x1b[B").expect("select ChatGPT plan row");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            selected_at,
            b"> ChatGPT plan",
        );
        let restored_at = transcript.len();
        input.write_all(b"\x1b[A").expect("return to API key row");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            restored_at,
            b"> API key",
        );
        let closed_at = transcript.len();
        input.write_all(b"\x1b").expect("close setup selector");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            closed_at,
            b"Setup closed",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            closed_at,
            b"\x1b[?25h",
        );
        input.write_all(b"/quit\r").expect("quit Session");
        let result = wait_product(child.take());
        output
            .read_to_end(&mut transcript)
            .expect("final PTY output");
        assert!(
            result.status.success(),
            "inline setup journey failed at {columns}"
        );
        assert_eq!(answered, 1, "inline cursor position query at {columns}");
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
        transcript == "error: invalid saved account\r\n",
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
            .read_saved_account_record()
            .expect("initial account");
        let mut command = Command::new("/usr/bin/script");
        command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
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
            "stty rows 8 cols 16; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --no-color --setup --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"",
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
        let mut answered = 0;
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
                    .read_saved_account_record()
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
        .read_saved_account_record()
        .expect("initial account");
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
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
    let mut answered = 0;
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
        b"Choose access method",
    );
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Input:\r\n",
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
    assert!(String::from_utf8_lossy(&transcript).contains(
        "Choose: ChatGPT plan uses your subscription; type a choice name: API key or ChatGPT plan; empty Enter selects API key\r\nInput:\r\n"
    ));
    input.write_all(b"API key\r").expect("API key choice");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        access_at,
        b"NOT encrypted",
    );
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        access_at,
        b"Input:\r\n",
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
        b"Choose Provider",
    );
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        consent_at,
        b"Input:\r\n",
    );
    assert!(String::from_utf8_lossy(&transcript[consent_at..]).contains(
        "Choose: Only the selected Provider receives requests; type a choice name: OpenAI or Anthropic; empty Enter selects OpenAI\r\nInput:\r\n"
    ));
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
        b"type a choice name: Key scoped to workspace or Choose API workspace",
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
                .read_saved_account_record()
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
        let mut answered = 0;
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
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            setup_at,
            b"Choose access method",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            setup_at,
            b"Input:\r\n",
        );
        let access_at = transcript.len();
        input.write_all(b"API key\r").expect("API key choice");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            access_at,
            b"NOT encrypted",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            access_at,
            b"Input:\r\n",
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
            b"Choose Provider",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            consent_at,
            b"Input:\r\n",
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
    let output = Command::new("/usr/bin/script")
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
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
        .bounded_output()
        .expect("setup PTY process");
    assert!(!output.status.success(), "unsafe transport rejected");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("unsafe or unsupported D-Bus session address"));
    assert!(!marker.exists(), "credential command was not executed");
    assert!(!state.exists(), "unsafe transport created State");
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
            "default-native-api-account",
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
                    "--ro-bind",
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
        "schema": 1,
        "id": Uuid::now_v7(),
        "provider": "openai",
        "model": "gpt-5.4",
        "effort": null,
        "api_key": "synthetic-native-helper-key"
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
    assert!(output.stdout.len() <= StateRoot::MAX_ACCOUNT_RECORD_BYTES);
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
#[test]
fn stalled_credential_bus_does_not_strand_unconfigured_start() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let socket = temp.path().join("bus");
    let listener = UnixListener::bind(&socket).expect("private synthetic D-Bus socket");
    listener
        .set_nonblocking(true)
        .expect("nonblocking synthetic bus");
    let mut command = super::process::isolated_script(temp.path());
    command
        .env_clear()
        .env("ARANY_TEST_EXE", "/arany")
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
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
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("first-run setup PTY"));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut connection = loop {
        match listener.accept() {
            Ok((connection, _)) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "setup never reached the OS store"
                );
                thread::yield_now();
            }
            Err(error) => panic!("synthetic bus accept failed: {error}"),
        }
    };
    let output = wait_product(child.take());
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
    connection
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("synthetic bus read deadline");
    let mut observed = 0;
    loop {
        let mut bytes = [0; 1024];
        let count = connection
            .read(&mut bytes)
            .expect("credential helper closed stalled connection");
        if count == 0 {
            break;
        }
        observed += count;
        assert!(observed <= 4096, "bounded synthetic D-Bus handshake");
    }

    let account_path = temp.path().join("account-home/.local/state/arany");
    let account_root = StateRoot::admit(&account_path).expect("isolated account marker");
    account_root
        .replace_saved_account_record(br#"{"schema":1,"storage":"keyring","account":null}"#)
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
        let shell = "printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; stty cols 40 rows 12; if [ \"$ARANY_TEST_LINEAR\" = 1 ]; then set -- --screen-reader; else set -- --no-color; fi; \"$ARANY_TEST_EXE\" \"$@\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"; exit_code=$?; stty \"$before\"; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
        let mut command = super::process::isolated_script(temp.path());
        command
            .env_clear()
            .env("ARANY_TEST_EXE", &executable)
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("ARANY_TEST_SESSION", session_id.to_string())
            .env("ARANY_TEST_LINEAR", if linear { "1" } else { "0" })
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
        let mut answered = 0;
        if !linear {
            wait_for(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                b"Do you trust this folder?",
            );
            let page_start = transcript.len();
            input.write_all(b"\r").expect("review second consent page");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                page_start,
                b"\x1b[?25l",
            );
            input
                .write_all(b"\r")
                .expect("explicit read-only fixture choice");
        }
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            if linear { b"Input:\r\n" } else { b"\x1b[10;3H" },
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
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            submitted_start,
            if linear {
                b"Notice: Preparing request... Ctrl+C cancels\r\n"
            } else {
                b"\x1b[10;3H"
            },
        );
        let edited_start = transcript.len();
        input
            .write_all(if linear { b"retained\x04" } else { b"retained" })
            .expect("continue a new draft during admission");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            edited_start,
            if linear {
                b"Notice: Draft: 8 characters; Enter retains until Run ends\r\n"
            } else {
                b"\x1b[10;11H"
            },
        );
        if !linear {
            assert!(
                !String::from_utf8_lossy(&transcript[edited_start..]).contains("ready"),
                "editing must not restore ready during admission"
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
                b"Notice: Draft retained: 8 characters; Enter submits\r\n",
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
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                interrupted_start,
                b"\x1b[10;11H",
            );
        }
        let continuation_start = transcript.len();
        input
            .write_all(if linear { b"-suffix\x04" } else { b"-suffix" })
            .expect("continue recovered draft");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            continuation_start,
            if linear {
                b"Notice: Draft: 15 characters; Enter submits\r\n"
            } else {
                b"\x1b[10;18H"
            },
        );
        let cleared_start = transcript.len();
        kill_process(product_pid, Signal::INT).expect("clear retained draft while idle");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            cleared_start,
            if linear { b"Input:\r\n" } else { b"\x1b[10;3H" },
        );
        input
            .write_all(b"/quit\r")
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
            answered == 0,
            linear,
            "only inline ownership queries the cursor"
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
