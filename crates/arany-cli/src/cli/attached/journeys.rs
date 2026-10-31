#[cfg(debug_assertions)]
fn reviewed_setup_text(transcript: &str, title: &str) -> String {
    let mut current = "";
    let mut text = String::new();
    for line in transcript.lines() {
        if let Some(header) = line
            .strip_prefix("Settings: ")
            .or_else(|| line.strip_prefix("Setup: "))
        {
            current = header;
        } else if current == title
            && let Some(page) = line.strip_prefix("Choose: Page ")
            && let Some((_, body)) = page.split_once("\\u{000a}")
        {
            text.push_str(body);
        }
    }
    text
}

#[cfg(target_os = "macos")]
use crate::terminal::{AttachedTerminal, Composer};

#[cfg(target_os = "macos")]
use crate::cli::serve::embedded::{self, RunAttemptError, RunFailure};
#[cfg(debug_assertions)]
use arany::RecordKind;
#[cfg(target_os = "macos")]
use arany::{
    AdmittedProviders, AgentPhase, Answer, ApprovalMode, ChatGptAdmission, ChatGptProvenance,
    CollaborationPolicy, OutputTokenBound, Provenance, Provider, ProviderError, ProviderIdentity,
    ProviderOutcome, ProviderRequest, ProviderResponse, RunRequest, RunStatus,
};
use arany::{Effort, SessionView, StateRoot, Store};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};
use std::{
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    time::Duration,
};
use uuid::Uuid;

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn saved_access_is_authorized_before_chat_without_exposing_secrets() {
    enum StartupInput {
        Reply(&'static str),
        Interrupt,
    }
    use StartupInput::{Interrupt, Reply};
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Case {
        ChatGpt,
        Locked,
        Missing,
        Secret,
        Native,
        NativeCached,
        Replaced,
        PrivateFile,
        Unconfigured,
        InvalidChatgpt,
        InvalidChatgptReset,
        InvalidRegistration,
        InitialSetupFailure,
        InvalidHistory,
        InvalidHistorySameRoot,
        InvalidHistorySetup,
        InvalidHistorySetupQuit,
        CorruptHistory,
        CorruptHistoryCancel,
        StaleEventHistory,
        UnsafeHistory,
        ResumeLocked,
        ContinueMissing,
        Cancel,
    }
    impl Case {
        const ALL: [Self; 24] = [
            Self::ChatGpt,
            Self::Locked,
            Self::Missing,
            Self::Secret,
            Self::Native,
            Self::NativeCached,
            Self::Replaced,
            Self::PrivateFile,
            Self::Unconfigured,
            Self::InvalidChatgpt,
            Self::InvalidChatgptReset,
            Self::InvalidRegistration,
            Self::InitialSetupFailure,
            Self::InvalidHistory,
            Self::InvalidHistorySameRoot,
            Self::InvalidHistorySetup,
            Self::InvalidHistorySetupQuit,
            Self::CorruptHistory,
            Self::CorruptHistoryCancel,
            Self::StaleEventHistory,
            Self::UnsafeHistory,
            Self::ResumeLocked,
            Self::ContinueMissing,
            Self::Cancel,
        ];

        fn name(self) -> &'static str {
            match self {
                Self::ChatGpt => "chatgpt",
                Self::Locked => "locked",
                Self::Missing => "missing",
                Self::Secret => "secret",
                Self::Native => "native",
                Self::NativeCached => "native_cached",
                Self::Replaced => "replaced",
                Self::PrivateFile => "private_file",
                Self::Unconfigured => "unconfigured",
                Self::InvalidChatgpt => "invalid_chatgpt",
                Self::InvalidChatgptReset => "invalid_chatgpt_reset",
                Self::InvalidRegistration => "invalid_registration",
                Self::InitialSetupFailure => "initial_setup_failure",
                Self::InvalidHistory => "invalid_history",
                Self::InvalidHistorySameRoot => "invalid_history_same_root",
                Self::CorruptHistory => "corrupt_history",
                Self::CorruptHistoryCancel => "corrupt_history_cancel",
                Self::StaleEventHistory => "stale_event_history",
                Self::InvalidHistorySetup => "invalid_history_setup",
                Self::InvalidHistorySetupQuit => "invalid_history_setup_quit",
                Self::UnsafeHistory => "unsafe_history",
                Self::ResumeLocked => "resume_locked",
                Self::ContinueMissing => "continue_missing",
                Self::Cancel => "cancel",
            }
        }

        fn archives_history(self) -> bool {
            matches!(
                self,
                Self::InvalidHistory
                    | Self::InvalidHistorySameRoot
                    | Self::InvalidHistorySetup
                    | Self::InvalidHistorySetupQuit
                    | Self::StaleEventHistory
            )
        }

        fn parse(name: &str) -> Self {
            Self::ALL
                .into_iter()
                .find(|case| case.name() == name)
                .expect("synthetic startup case")
        }
    }
    impl std::fmt::Display for Case {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.name())
        }
    }

    const ENGINE_TEST_STACK_BYTES: usize = 8 * 1024 * 1024;
    const ROOT: &str = "ARANY_TEST_SAVED_ACCESS_ROOT";
    const CASE: &str = "ARANY_TEST_SAVED_ACCESS_CASE";
    const TEST: &str =
        "cli::attached::journeys::saved_access_is_authorized_before_chat_without_exposing_secrets";
    let Some(root) = std::env::var_os(ROOT) else {
        for case in Case::ALL {
            let dialogue: &[StartupInput] = match case {
                Case::InvalidChatgpt | Case::InvalidRegistration => &[
                    Reply("/setup\n"),
                    Reply("API accounts\n"),
                    Reply("Manage ChatGPT account\n"),
                    Reply("\n"),
                    Reply("/exit\n"),
                ],
                Case::InvalidChatgptReset => &[
                    Reply("/setup\n"),
                    Reply("API accounts\n"),
                    Reply("Manage ChatGPT account\n"),
                    Reply("Reset local setup\n"),
                    Reply("\n"),
                    Reply("/exit\n"),
                ],
                Case::InitialSetupFailure | Case::InvalidHistorySetup => &[
                    Reply("API accounts\n"),
                    Reply("Manage ChatGPT account\n"),
                    Reply("/exit\n"),
                ],
                Case::InvalidHistorySetupQuit => &[Interrupt],
                Case::CorruptHistory => &[
                    Reply("Reset history\n"),
                    Reply("Accept\n"),
                    Reply("/exit\n"),
                ],
                Case::CorruptHistoryCancel => {
                    &[Reply("Reset history\n"), Reply("\n"), Reply("Exit\n")]
                }
                Case::UnsafeHistory => &[Reply("Exit\n")],
                Case::ResumeLocked | Case::ContinueMissing | Case::Cancel => &[],
                _ => &[Reply("/exit\n")],
            };
            let mut inputs = dialogue.iter();
            let temp = tempfile::tempdir().unwrap();
            let (master, slave) = crate::test_process::cloexec_pty(None);
            let settings = nix::sys::termios::tcgetattr(&master).unwrap();
            let mut input = master.try_clone().unwrap();
            let mut answered_bytes = 0;
            let listener = if case == Case::Cancel {
                let listener =
                    std::os::unix::net::UnixListener::bind(temp.path().join("authorization"))
                        .unwrap();
                listener.set_nonblocking(true).unwrap();
                Some(listener)
            } else {
                None
            };
            let mut connection = None;
            let mut helper_pid = None;
            let mut interrupted = false;
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .env_clear()
                // The fixture polls the Engine on a libtest thread instead of the product main thread.
                .env("RUST_MIN_STACK", ENGINE_TEST_STACK_BYTES.to_string())
                .env(ROOT, temp.path())
                .env(CASE, case.name())
                .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
                .env("ARANY_TEST_EXE", temp.path().join("credential-helper"))
                .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("TERM", "dumb")
                .args(["--exact", TEST, "--nocapture"])
                .current_dir(temp.path())
                .process_group(0)
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::piped())
                .stderr(Stdio::from(slave));
            let mut child = OwnedChild(command.spawn().unwrap());
            let product_pid =
                rustix::process::Pid::from_raw(child.0.id().try_into().unwrap()).unwrap();
            drop(command);
            let output = crate::test_process::capture_terminal(
                &mut child.0,
                master,
                Duration::from_secs(20),
                64 * 1024,
                |bytes| {
                    if !interrupted && let Some(listener) = &listener {
                        match listener.accept() {
                            Ok((mut ready, _)) => {
                                use std::io::Read;
                                ready.set_nonblocking(false).unwrap();
                                ready
                                    .set_read_timeout(Some(Duration::from_secs(4)))
                                    .unwrap();
                                let mut pid = [0; 4];
                                ready.read_exact(&mut pid).unwrap();
                                helper_pid = rustix::process::Pid::from_raw(
                                    u32::from_be_bytes(pid).try_into().unwrap(),
                                );
                                connection = Some(ready);
                                rustix::process::kill_process(
                                    product_pid,
                                    rustix::process::Signal::INT,
                                )
                                .unwrap();
                                interrupted = true;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(error) => panic!("authorization readiness: {error}"),
                        }
                    }
                    let fresh =
                        String::from_utf8_lossy(&bytes[answered_bytes..]).replace("\r\n", "\n");
                    if fresh
                        .split_once("Choose: Page ")
                        .is_some_and(|(_, page)| page.contains('\n'))
                    {
                        input.write_all(b"\n").unwrap();
                        answered_bytes = bytes.len();
                        return;
                    }
                    if fresh.contains("Input:\n")
                        && let Some(action) = inputs.next()
                    {
                        match action {
                            Reply(line) => input.write_all(line.as_bytes()).unwrap(),
                            Interrupt => {
                                rustix::process::kill_process(
                                    product_pid,
                                    rustix::process::Signal::INT,
                                )
                                .unwrap();
                            }
                        }
                        answered_bytes = bytes.len();
                    }
                },
            );
            let transcript = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
            assert!(
                output.status.success(),
                "saved access case {case}: {}; terminal {}",
                String::from_utf8_lossy(&output.stdout),
                crate::presentation::escape_terminal(
                    &transcript.replace("synthetic-startup-secret", "[redacted]")
                )
            );
            crate::test_process::assert_single_test_completed(
                &output.stdout,
                &format!("saved access case {case}"),
            );
            let expects_prompt = !matches!(
                case,
                Case::ResumeLocked | Case::ContinueMissing | Case::Cancel
            );
            assert_eq!(
                transcript.contains("Input:\n"),
                expects_prompt,
                "startup input ownership for {case}"
            );
            assert!(
                inputs.as_slice().is_empty(),
                "startup dialogue must complete for {case}"
            );
            assert_eq!(
                nix::sys::termios::tcgetattr(&input).unwrap(),
                settings,
                "startup must restore exact terminal settings"
            );
            if case == Case::Cancel {
                use std::io::Read;
                assert!(interrupted);
                let mut byte = [0];
                assert_eq!(
                    connection.as_mut().unwrap().read(&mut byte).unwrap(),
                    0,
                    "cancelled credential helper EOF"
                );
                assert_eq!(
                    rustix::process::test_kill_process(helper_pid.unwrap()),
                    Err(rustix::io::Errno::SRCH),
                    "credential helper must be reaped before exit"
                );
            }
            assert!(
                !transcript.contains("synthetic-startup-secret"),
                "credential escaped to presentation"
            );
            let marker = temp.path().join("helper-access");
            if case.archives_history() {
                assert!(
                    !transcript.contains("Setup: History recovery"),
                    "incompatible history must not need a manual reset: {case}"
                );
                // Setup replaces earlier startup notices; the archive notice must survive it.
                assert_eq!(
                    transcript
                        .lines()
                        .filter(|line| line.starts_with("Notice: ")
                            && line.contains(
                                "History from an incompatible version archived intact to "
                            ))
                        .count(),
                    usize::from(case != Case::InvalidHistorySetupQuit),
                    "archive notice for {case}: {}",
                    crate::presentation::escape_terminal(&transcript)
                );
                if case == Case::InvalidHistorySameRoot {
                    assert!(transcript.contains("Initial task was not submitted"));
                }
                if case == Case::InvalidHistorySetup {
                    assert!(transcript.contains("Setup could not be completed"));
                }
                let calls = std::fs::read_to_string(marker).unwrap_or_default();
                assert_eq!(
                    calls.lines().count(),
                    usize::from(!case.name().contains("_setup")),
                    "archived history keeps access"
                );
            } else if case.name().contains("history") {
                assert!(
                    transcript.contains("Setup: History recovery"),
                    "history case {case}: {}",
                    crate::presentation::escape_terminal(&transcript)
                );
                assert!(transcript.contains("empty Enter selects 1. Retry"));
                if case == Case::UnsafeHistory {
                    assert!(!transcript.contains("Reset history"));
                    assert!(transcript.contains("unsafe or inaccessible"));
                } else {
                    assert!(transcript.contains("Reset deletes all conversations"));
                    assert!(transcript.contains("Reset history"));
                    assert!(
                        reviewed_setup_text(&transcript, "Reset history?")
                            .contains("This cannot be undone.")
                    );
                    assert!(transcript.contains("empty Enter selects 1. Back"));
                }
                let calls = std::fs::read_to_string(marker).unwrap_or_default();
                assert_eq!(
                    calls.lines().count(),
                    usize::from(!case.name().ends_with("_cancel") && case != Case::UnsafeHistory)
                );
            } else if case.name().starts_with("invalid_") || case == Case::InitialSetupFailure {
                assert!(transcript.contains("Setup: "));
                if case != Case::InitialSetupFailure {
                    assert!(
                        transcript.contains("(chatgpt-registration.json) archived intact"),
                        "refused records are archived with a notice"
                    );
                }
                if !matches!(case, Case::InvalidRegistration | Case::InitialSetupFailure) {
                    assert!(transcript.contains("use /setup to review saved access"));
                }
                if case == Case::InitialSetupFailure {
                    assert!(transcript.contains("Setup could not be completed"));
                } else {
                    assert!(
                        transcript.contains("1. Back")
                            && transcript.contains("empty Enter selects 1"),
                        "reset must initially decline"
                    );
                    assert!(
                        transcript.contains("Reset local setup"),
                        "reset choice must not be clipped"
                    );
                    {
                        let reviewed = transcript
                            .lines()
                            .filter_map(|line| {
                                line.strip_prefix("Choose: Page ")?
                                    .split_once("\\u{000a}")
                                    .map(|(_, content)| content)
                            })
                            .collect::<String>();
                        assert!(
                            transcript.contains("no remote sign-out")
                                || reviewed.contains("no remote sign-out")
                        );
                    }
                }
                // The intact accounts pass startup authorization once; a reset authorizes again.
                let expected = match case {
                    Case::InitialSetupFailure => 0,
                    Case::InvalidChatgptReset => 2,
                    _ => 1,
                };
                let calls = std::fs::read_to_string(marker).unwrap_or_default();
                assert_eq!(
                    calls.lines().count(),
                    expected,
                    "only explicit setup probes or valid startup authorization may use the helper: {case}"
                );
            } else if matches!(case, Case::PrivateFile | Case::Unconfigured) {
                assert!(
                    !marker.exists(),
                    "unconfigured/private file must not access the OS store"
                );
                assert!(!transcript.contains("Authorize saved access"));
            } else {
                let phase = transcript.find("Approve this Arany credential in your OS password store; Esc or Ctrl+C exits\n").expect("authorization phase");
                if expects_prompt {
                    assert!(phase < transcript.find("Input:\n").expect("composer"));
                }
                let calls = std::fs::read_to_string(marker).unwrap();
                assert_eq!(
                    calls.lines().count(),
                    1,
                    "startup must not retry or inspect another item"
                );
                match case {
                    Case::Locked | Case::ResumeLocked => {
                        assert!(transcript.contains("OS credential store is locked"))
                    }
                    Case::Missing | Case::ContinueMissing => assert!(
                        transcript.contains("Saved sign-in is missing from the OS password store")
                    ),
                    Case::Secret => {
                        assert!(transcript.contains("OS credential store unavailable"))
                    }
                    Case::Replaced => assert!(transcript.contains("invalid saved account")),
                    Case::Cancel => {
                        assert!(transcript.contains("Startup authorization cancelled"))
                    }
                    _ => assert!(
                        !transcript.contains("Error:"),
                        "authorization failed in {case}"
                    ),
                }
            }
        }
        return;
    };
    let root = std::path::PathBuf::from(root);
    let case = Case::parse(&std::env::var(CASE).unwrap());
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let session_path = root.join(if case == Case::InvalidHistorySameRoot {
        "account"
    } else {
        "session"
    });
    let state = StateRoot::admit(&root.join("account")).unwrap();
    let metadata = std::fs::metadata(&workspace).unwrap();
    let trust = serde_json::to_vec(&serde_json::json!({
        "workspaces": [{ "path": workspace,
            "device": metadata.dev(), "inode": metadata.ino(), "mode": "auto_edits",
            "runtime_grants": [], "git_hooks": null, "repository": null }]
    }))
    .unwrap();
    let trust_path = state.path().join("workspace-permissions.json");
    std::fs::write(&trust_path, trust).unwrap();
    std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let native = matches!(
        case,
        Case::Native | Case::NativeCached | Case::Replaced | Case::PrivateFile
    );
    let account = crate::accounts::credentials::SavedAccount::new(
        "openai".into(),
        "gpt-5.4".into(),
        Some(Effort::Low),
        "synthetic-startup-secret".into(),
    )
    .unwrap();
    let id = if native { account.id } else { Uuid::now_v7() };
    let profile = if native { "openai" } else { "chatgpt" };
    let (operation, slot, response) = if case == Case::Unconfigured {
        ("forbidden", "forbidden".into(), "exit 1".into())
    } else if native {
        if case == Case::PrivateFile {
            crate::accounts::credentials::save(
                &workspace,
                account,
                crate::accounts::credentials::AccountStorage::PrivateFile,
                None,
            )
            .await
            .unwrap();
            ("forbidden", "forbidden".into(), "exit 1".into())
        } else {
            state
                .record(RecordKind::SavedAccount)
                .replace(br#"{"storage":"keyring","account":null}"#)
                .unwrap();
            let record = serde_json::to_string(&account)
                .unwrap()
                .replace('\'', "'\\''");
            if case == Case::Native {
                (
                    "read",
                    "default-native-api-account".into(),
                    format!("printf '%s' '{record}'"),
                )
            } else {
                (
                    "authorize",
                    "default-native-api-account".into(),
                    if case == Case::Replaced {
                        "exit 4"
                    } else {
                        "exit 0"
                    }
                    .into(),
                )
            }
        }
    } else {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        let client_slot = format!(
            "chatgpt-{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                Sha256::new()
                    .chain_update("synthetic-client")
                    .chain_update([0])
                    .chain_update(id.as_bytes())
                    .finalize()
            )
        );
        state
            .record(RecordKind::ChatgptAccounts)
            .replace(
                &serde_json::to_vec(&serde_json::json!({
                    "selected": id, "accounts": [{
                        "id": id, "host_id": "11111111-1111-4111-8111-111111111111",
                        "client_id": "synthetic-client", "subject": "synthetic-subject",
                        "chatgpt_account_id": "synthetic-account", "keyring_slot": client_slot,
                        "storage": "keyring", "renewal_pending": false, "signout_pending": false,
                        "disconnected": false, "token": null
                    }]
                }))
                .unwrap(),
            )
            .unwrap();
        let slot = crate::accounts::chatgpt::selected_keyring_slot(id)
            .unwrap()
            .unwrap();
        let response = match case {
            Case::ChatGpt => "exit 0",
            Case::Locked | Case::ResumeLocked => "exit 3",
            Case::Missing | Case::ContinueMissing => "exit 2",
            Case::Secret => "printf synthetic-startup-secret",
            Case::Cancel => "held-helper",
            Case::InvalidRegistration
            | Case::InvalidHistorySameRoot
            | Case::InvalidHistory
            | Case::CorruptHistory
            | Case::CorruptHistoryCancel
            | Case::StaleEventHistory
            | Case::UnsafeHistory => "exit 0",
            Case::InitialSetupFailure
            | Case::InvalidHistorySetup
            | Case::InvalidHistorySetupQuit
            | Case::InvalidChatgpt
            | Case::InvalidChatgptReset => "exit 1",
            _ => panic!("invalid synthetic startup case"),
        }
        .into();
        ("authorize", slot, response)
    };
    if !matches!(case, Case::Native | Case::Unconfigured) {
        state
            .record(RecordKind::ModelPreferences)
            .replace(
                &serde_json::to_vec(&serde_json::json!({
                    "selected": { "profile": profile, "account_id": id },
                    "sources": [{ "profile": profile, "account_id": id, "model": "gpt-5.4",
                        "effort": "low", "catalog": [{ "id": "gpt-5.4" }] }]
                }))
                .unwrap(),
            )
            .unwrap();
    }
    // Startup archives the refused registration; the accounts it bound then need a setup reset.
    if case.name().starts_with("invalid_chatgpt") || case == Case::InvalidRegistration {
        state
            .record(RecordKind::ChatgptRegistration)
            .replace(b"unsupported-registration")
            .unwrap();
    }
    if matches!(case, Case::InitialSetupFailure | Case::InvalidHistorySetup) {
        nix::unistd::mkfifo(
            &state.path().join("chatgpt-registration.json"),
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
    }
    let recovery = case.name().starts_with("invalid_")
        || case.name().contains("_history")
        || case == Case::InitialSetupFailure;
    let before_accounts = if recovery {
        Some((
            state.record(RecordKind::SavedAccount).read().unwrap(),
            state.record(RecordKind::ChatgptAccounts).read().unwrap(),
        ))
    } else {
        None
    };
    let marker = root
        .join("helper-access")
        .to_str()
        .unwrap()
        .replace('\'', "'\\''");
    let identity = if native && operation == "authorize" {
        format!("[ \"$#\" -eq 5 ] && [ \"$4\" = '{id}' ] && [ \"$5\" = 'openai' ] || exit 1")
    } else {
        "[ \"$#\" -eq 3 ] || exit 1".into()
    };
    let response = if case == Case::Cancel {
        let socket = root
            .join("authorization")
            .to_str()
            .unwrap()
            .replace('\'', "'\\''");
        let executable = std::env::current_exe()
            .unwrap()
            .to_str()
            .unwrap()
            .replace('\'', "'\\''");
        format!(
            "export ARANY_TEST_AUTHORIZATION_SOCKET='{socket}'\nexec '{executable}' --exact accounts::credentials::keyring_helper::tests::held_authorization_helper --ignored --nocapture"
        )
    } else {
        response
    };
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = --internal-credential-helper ] || exit 1\nprintf '%s:%s\\n' \"$2\" \"$3\" >> '{marker}'\nif [ \"$2\" = probe ]; then exit 1; fi\n{identity}\n[ \"$2\" = '{operation}' ] && [ \"$3\" = '{slot}' ] || exit 1\n{response}\n"
    );
    let helper = root.join("credential-helper");
    std::fs::write(&helper, script).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut args = super::AttachedArgs {
        workspace: Some(workspace.clone()),
        state_dir: Some(session_path.clone()),
        screen_reader: true,
        ..Default::default()
    };
    args.setup = case == Case::InitialSetupFailure || case.name().contains("_setup");
    if case == Case::InvalidHistorySameRoot {
        args.prompt = Some("synthetic task must remain unsubmitted".into());
        args.continue_session = true;
    }
    let before_history = if recovery {
        let session = arany::create_session(
            StateRoot::admit(&session_path).unwrap(),
            workspace.clone(),
            None,
        )
        .await
        .unwrap();
        let store =
            Store::open_read_only(StateRoot::open_existing(&session_path).unwrap()).unwrap();
        let events = store.load_session(session).await.unwrap();
        store.close().await.unwrap();
        Some((session, events))
    } else {
        None
    };
    if case.name().starts_with("invalid_history") {
        let connection = rusqlite::Connection::open(session_path.join("events.sqlite3")).unwrap();
        connection.execute_batch("PRAGMA user_version=3").unwrap();
    }
    if case == Case::StaleEventHistory {
        let connection = rusqlite::Connection::open(session_path.join("events.sqlite3")).unwrap();
        connection
            .execute_batch("UPDATE events SET kind = 'ToolPermissionDecided' WHERE sequence = 1")
            .unwrap();
    }
    if case.name().starts_with("corrupt_history") {
        std::fs::write(
            session_path.join("events.sqlite3"),
            b"synthetic-broken-history",
        )
        .unwrap();
    }
    let outside = root.join("outside-history");
    if case == Case::UnsafeHistory {
        std::fs::write(&outside, b"outside-stays").unwrap();
        std::os::unix::fs::symlink(&outside, session_path.join("events.sqlite3-journal")).unwrap();
    }
    let before_database = if case.name().contains("history") {
        Some(std::fs::read(session_path.join("events.sqlite3")).unwrap())
    } else {
        None
    };
    if matches!(case, Case::ResumeLocked | Case::ContinueMissing) {
        let session = arany::create_session(
            StateRoot::admit(&session_path).unwrap(),
            workspace.clone(),
            None,
        )
        .await
        .unwrap();
        arany::set_session_defaults(
            StateRoot::open_existing(&session_path).unwrap(),
            workspace.clone(),
            session,
            arany::SessionDefaults {
                provider: Some("chatgpt".into()),
                model: Some("gpt-5.4".into()),
                effort: Some(Effort::Low),
                account_id: Some(id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let store =
            Store::open_read_only(StateRoot::open_existing(&session_path).unwrap()).unwrap();
        let before = store.load_session(session).await.unwrap();
        store.close().await.unwrap();
        args.prompt = Some("synthetic task must not be submitted".into());
        if case == Case::ResumeLocked {
            args.resume = Some(Some(session.to_string()));
        } else {
            args.continue_session = true;
        }
        let error = super::run(args, arany::Telemetry::disabled())
            .await
            .unwrap_err();
        assert!(error.contains(if case == Case::ResumeLocked {
            "OS credential store is locked"
        } else {
            "Saved sign-in is missing"
        }));
        eprintln!("Startup rejected: {error}");
        let store =
            Store::open_read_only(StateRoot::open_existing(&session_path).unwrap()).unwrap();
        assert_eq!(
            store.load_session(session).await.unwrap(),
            before,
            "denied startup must preserve the closed Event prefix"
        );
        store.close().await.unwrap();
        return;
    }
    super::run(args, arany::Telemetry::disabled())
        .await
        .unwrap();
    if case == Case::Cancel {
        assert!(
            !session_path.exists(),
            "cancelled startup must not create Session state"
        );
        eprintln!("Startup authorization cancelled");
        return;
    }
    if let Some((session, events)) = before_history {
        if let Some(database) = before_database {
            if case.name().ends_with("_cancel") || case == Case::UnsafeHistory {
                assert!(std::fs::read(session_path.join("events.sqlite3")).unwrap() == database);
                if case == Case::UnsafeHistory {
                    assert_eq!(std::fs::read(outside).unwrap(), b"outside-stays");
                }
            } else {
                let store = Store::open_read_only(StateRoot::open_existing(&session_path).unwrap())
                    .unwrap();
                assert!(store.load_session(session).await.unwrap().is_empty());
                store.close().await.unwrap();
                assert!(
                    arany::list_sessions(
                        StateRoot::open_existing(&session_path).unwrap(),
                        workspace.clone(),
                    )
                    .await
                    .unwrap()
                    .is_empty()
                );
                let archive = session_path.join("archive");
                if case.archives_history() {
                    let archived = std::fs::read_dir(&archive)
                        .unwrap()
                        .map(|entry| entry.unwrap().path())
                        .filter(|path| {
                            path.file_name()
                                .unwrap()
                                .to_str()
                                .unwrap()
                                .starts_with("history-")
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(archived.len(), 1, "one archive for {case}");
                    assert!(
                        std::fs::read(archived[0].join("events.sqlite3")).unwrap() == database,
                        "archived history stays intact"
                    );
                    // Only a drawn archive notice is acknowledged into the manifest.
                    let drawn = case != Case::InvalidHistorySetupQuit;
                    assert_eq!(archived[0].join("manifest.json").exists(), drawn, "{case}");
                    assert_eq!(archive.join("pending.json").exists(), !drawn, "{case}");
                } else {
                    assert!(!archive.exists(), "manual reset never archives");
                }
            }
            assert!(
                state.record(RecordKind::ChatgptAccounts).read().unwrap()
                    == before_accounts.unwrap().1
            );
            // Startup re-remembers the selection, so only presence is stable here.
            assert!(
                state
                    .record(RecordKind::ModelPreferences)
                    .present()
                    .unwrap()
            );
            assert!(trust_path.exists());
            return;
        }
        let store =
            Store::open_read_only(StateRoot::open_existing(&session_path).unwrap()).unwrap();
        let current = store.load_session(session).await.unwrap();
        assert_eq!(current, events, "recovery preserves closed history");
        assert!(
            SessionView::replay(session, &current)
                .unwrap()
                .unwrap()
                .runs
                .is_empty()
        );
        store.close().await.unwrap();
        let (native_before, chatgpt_before) = before_accounts.unwrap();
        let reset = case.name().ends_with("_reset");
        assert!(
            state.record(RecordKind::SavedAccount).read().unwrap()
                == if reset && native { None } else { native_before }
        );
        assert!(
            state.record(RecordKind::ChatgptAccounts).read().unwrap()
                == if reset && case.name().starts_with("invalid_chatgpt") {
                    None
                } else {
                    chatgpt_before
                }
        );
        // Startup re-remembers the selection, so only presence is stable here.
        assert_eq!(
            state
                .record(RecordKind::ModelPreferences)
                .present()
                .unwrap(),
            !reset
        );
        if case != Case::InitialSetupFailure {
            assert!(
                state
                    .record(RecordKind::ChatgptRegistration)
                    .read()
                    .unwrap()
                    .is_none()
            );
            let archived = std::fs::read_dir(state.path().join("archive"))
                .unwrap()
                .map(|entry| entry.unwrap().path().join("chatgpt-registration.json"))
                .find(|path| path.exists())
                .expect("archived registration");
            assert_eq!(
                std::fs::read(archived).unwrap(),
                b"unsupported-registration"
            );
        }
        return;
    }
    assert!(
        arany::list_sessions(StateRoot::open_existing(&session_path).unwrap(), workspace,)
            .await
            .unwrap()
            .is_empty(),
        "startup and quit must create no canonical Session or Run"
    );
}

#[cfg(target_os = "macos")]
struct Greeting {
    identity: ProviderIdentity,
    observed: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[cfg(target_os = "macos")]
impl Greeting {
    fn new(
        profile: &'static str,
        account_id: Uuid,
        observed: Arc<Mutex<Vec<ProviderRequest>>>,
    ) -> Self {
        let mut identity =
            ProviderIdentity::new(profile, "synthetic-model", arany::Provenance::native());
        identity.effort = Some(Effort::Low);
        if profile == "chatgpt" {
            identity.provenance = Provenance::ChatGpt(ChatGptProvenance {
                account_id,
                admission_fingerprint: [1; 32],
                admission: ChatGptAdmission::AccountConsent,
            });
            identity.limits.output_token_bound = OutputTokenBound::LocalAcceptanceOnly;
        }
        Self { identity, observed }
    }
}

#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl Provider for Greeting {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }
    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.observed.lock().unwrap().push(request);
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome: ProviderOutcome::Answer(Answer {
                text: "Hola".into(),
            }),
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn native_chat_preserves_turns_with_guarded_tools_and_rejects_invalid_admission() {
    const FIXTURE: &str = "ARANY_TEST_NATIVE_CHAT_ROOT";
    const TEST: &str = "cli::attached::journeys::native_chat_preserves_turns_with_guarded_tools_and_rejects_invalid_admission";
    let Some(root) = std::env::var_os(FIXTURE) else {
        let temp = tempfile::tempdir().unwrap();
        let (master, slave) = crate::test_process::cloexec_pty(None);
        let mut input = master.try_clone().unwrap();
        let mut cancelled = false;
        let mut recovery_prompts = 0;
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .env(FIXTURE, temp.path())
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("TERM", "dumb")
            .current_dir(temp.path())
            .args(["--exact", TEST, "--nocapture"])
            .process_group(0)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::from(slave));
        command.env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"));
        let mut child = OwnedChild(command.spawn().unwrap());
        drop(command);
        let output = crate::test_process::capture_terminal(
            &mut child.0,
            master,
            Duration::from_secs(30),
            64 * 1024,
            |bytes| {
                let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
                let prompts = text.matches("Submission recovery").count();
                if prompts > recovery_prompts {
                    recovery_prompts = prompts;
                    match prompts {
                        1 => input.write_all(b"\n").unwrap(),
                        2 => {
                            std::fs::rename(
                                temp.path().join("recovery-state"),
                                temp.path().join("openai"),
                            )
                            .unwrap();
                            input.write_all(b"\n").unwrap();
                        }
                        3 => input.write_all(b"Exit (discard local draft)\n").unwrap(),
                        _ => panic!("unexpected submission recovery prompt"),
                    }
                    return;
                }
                if cancelled {
                    return;
                }
                // Release builds reject the account override, so startup offers recovery instead of consent.
                if text.contains(crate::test_process::READ_ONLY_CONSENT_PROMPT) {
                    input.write_all(b"\x04").unwrap();
                    cancelled = true;
                } else if text.contains("Folder trust recovery") {
                    // Linear mode reads whole lines, so a lone Escape never arrives.
                    input.write_all(b"\x04").unwrap();
                    cancelled = true;
                }
            },
        );
        assert!(
            output.status.success(),
            "native chat regression: stdout {}; terminal {}",
            crate::presentation::escape_terminal(&String::from_utf8_lossy(&output.stdout)),
            crate::presentation::escape_terminal(&String::from_utf8_lossy(&output.stderr))
        );
        crate::test_process::assert_single_test_completed(&output.stdout, "native chat regression");
        assert!(cancelled, "startup must reach consent or recovery and exit");
        assert_eq!(recovery_prompts, 3, "history retry, recovery, and exit");
        return;
    };
    let root = std::path::PathBuf::from(root);
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let account = StateRoot::admit(&root.join("account")).unwrap();
    let metadata = std::fs::metadata(&workspace).unwrap();
    let remembered = serde_json::to_vec(&serde_json::json!({
        "workspaces": [{ "path": workspace,
            "device": metadata.dev(), "inode": metadata.ino(), "mode": "auto_edits",
            "runtime_grants": [], "git_hooks": null, "repository": null }]
    }))
    .unwrap();
    let trust_record = account.path().join("workspace-permissions.json");
    std::fs::write(&trust_record, &remembered).unwrap();
    std::fs::set_permissions(&trust_record, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut terminal = AttachedTerminal::acquire_with_mode(
        true,
        crate::terminal::TerminalMode::default(),
        Default::default(),
    )
    .unwrap();
    let mut composer = Composer::default();
    composer.apply(crate::terminal::TerminalInput::Character('x'));
    composer.set_approval_mode(Some(ApprovalMode::AutoEdits));
    let choice = super::permissions::admit(
        &mut terminal,
        &workspace,
        account.path(),
        &mut composer,
        false,
    )
    .await;
    #[cfg(debug_assertions)]
    assert!(
        matches!(&choice, Ok(super::permissions::PermissionChoice::Selected(access)) if access.is_trusted())
    );
    #[cfg(not(debug_assertions))]
    assert!(
        matches!(&choice, Ok(super::permissions::PermissionChoice::Exit)),
        "optimized account override must end at recovery before host access"
    );
    let permissions = match &choice {
        Ok(super::permissions::PermissionChoice::Selected(access)) => Some(access),
        _ => None,
    };

    for profile in ["chatgpt", "openai", "anthropic"] {
        let state = root.join(profile);
        let account_id = Uuid::now_v7();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let engine = arany::test_support::engine::open(
            StateRoot::admit(&state).unwrap(),
            arany::EngineConfig {
                guard_executable: std::env::current_exe().ok(),
                ..Default::default()
            },
        )
        .unwrap();
        let engine = Arc::new(engine);
        let mut session = None;
        for _ in 0..2 {
            let outcome = embedded::run_with_providers(
                &engine,
                AdmittedProviders::single(Arc::new(Greeting::new(
                    profile,
                    account_id,
                    observed.clone(),
                ))),
                RunRequest {
                    skill: None,
                    session_id: session,
                    title: None,
                    objective: "hola hola".into(),
                    images: Vec::new(),
                    workspace: workspace.clone(),
                    launch_directory: None,
                    include_paths: Vec::new(),
                    policy: CollaborationPolicy::Single,
                    goal: arany::GoalRequest::default(),
                },
                &state,
                false,
                permissions,
                arany::ApprovalMode::AutoEdits,
            )
            .await
            .map(|result| result.run)
            .expect("ordinary greeting Run finishes");
            session = Some(outcome.session_id);
            assert_eq!(outcome.run.status, RunStatus::Finished);
        }
        {
            let requests = observed.lock().unwrap();
            assert_eq!(requests.len(), 2, "exact greeting call count");
            assert!(
                requests
                    .iter()
                    .all(|request| request.phase == AgentPhase::RootPlan
                        && request.input.objective == "hola hola"
                        && request.input.tools.is_some() == permissions.is_some()
                        && request.collaboration == CollaborationPolicy::Single
                        && request.model == "synthetic-model"
                        && request.input.workspace_guidance.is_empty()
                        && request.input.includes.is_empty()
                        && request.input.images.is_empty()
                        && request.input.child_results.is_empty()
                        && request.input.context_summary.is_none())
            );
            assert!(requests[0].input.history.is_empty());
            assert_eq!(
                requests[1].input.history.len(),
                1,
                "resumed greeting retains its accepted turn"
            );
            assert_eq!(requests[1].input.history[0].user, "hola hola");
            assert_eq!(requests[1].input.history[0].assistant, "Hola");
        }
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let events = store.load_session(session.unwrap()).await.unwrap();
        let view = SessionView::replay(session.unwrap(), &events)
            .unwrap()
            .unwrap();
        assert_eq!(view.runs.len(), 2);
        assert!(view.runs.iter().all(|run| run.status == RunStatus::Finished
            && run.objective == "hola hola"
            && run.assistant_message.as_deref() == Some("Hola")
            && run.tools.is_empty()));
        store.close().await.unwrap();
        let private = state.join("tools.json");
        std::fs::write(&private, serde_json::to_vec(&serde_json::json!({"workspace_paths":["."],"write":true,
            "commands":[{"name":"sh","executable":"/bin/sh","sha256":"0".repeat(64),"interpreter":true,"inputs":[]}],"skills":[],"mcp":[]})).unwrap()).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = embedded::run_with_providers(
            &engine,
            AdmittedProviders::single(Arc::new(Greeting::new(
                profile,
                account_id,
                observed.clone(),
            ))),
            RunRequest {
                skill: None,
                session_id: session,
                title: None,
                objective: "explicit Tools".into(),
                images: Vec::new(),
                workspace: workspace.clone(),
                launch_directory: None,
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Single,
                goal: arany::GoalRequest::default(),
            },
            &state,
            true,
            None,
            arany::ApprovalMode::AutoEdits,
        )
        .await
        .expect_err("explicit Tool admission must reject before inference");
        #[cfg(debug_assertions)]
        assert!(
            matches!(
                error,
                RunAttemptError::Failed(RunFailure::Engine(arany::EngineError::Tool(
                    arany::ToolError::ChangedInput
                )))
            ),
            "the admitted private account fixture must reach stale executable-pin rejection"
        );
        #[cfg(not(debug_assertions))]
        assert!(
            matches!(
                error,
                RunAttemptError::Failed(RunFailure::Engine(arany::EngineError::Store(
                    arany::StoreError::InvalidStateDirectory
                )))
            ),
            "the optimized account override must reject from the Workspace lease before Tool discovery or Provider use"
        );
        assert_eq!(
            observed.lock().unwrap().len(),
            2,
            "rejection must make no Provider call"
        );
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        assert_eq!(
            store.load_session(session.unwrap()).await.unwrap(),
            events,
            "explicit rejection preserves the committed prefix"
        );
        store.close().await.unwrap();
        Arc::try_unwrap(engine)
            .ok()
            .expect("Engine released")
            .close()
            .await
            .unwrap();
        if profile == "openai" {
            let mut recovery_view = view.clone();
            let mut recovery_composer = Composer::default();
            recovery_composer
                .insert_paste(&"payload ".repeat(200))
                .unwrap();
            recovery_composer.insert_paste(" @visible.txt").unwrap();
            recovery_composer
                .attach_image(arany::test_support::test_image())
                .unwrap();
            let submitted = recovery_composer.take_draft();
            let backup = root.join("recovery-state");
            std::fs::rename(&state, &backup).unwrap();
            super::reconcile_failed_submission(
                super::run::RunUi {
                    terminal: &mut terminal,
                    view: &mut recovery_view,
                    composer: &mut recovery_composer,
                },
                &state,
                submitted.clone(),
                view.last_sequence,
            )
            .await
            .unwrap();
            assert_eq!(recovery_composer.take_draft(), submitted);
            super::reconcile_failed_submission(
                super::run::RunUi {
                    terminal: &mut terminal,
                    view: &mut recovery_view,
                    composer: &mut recovery_composer,
                },
                &state,
                submitted.clone(),
                0,
            )
            .await
            .unwrap();
            assert!(
                recovery_composer.is_empty(),
                "accepted draft cannot be retried"
            );
            std::fs::rename(&state, &backup).unwrap();
            let error = super::reconcile_failed_submission(
                super::run::RunUi {
                    terminal: &mut terminal,
                    view: &mut recovery_view,
                    composer: &mut recovery_composer,
                },
                &state,
                submitted,
                view.last_sequence,
            )
            .await
            .unwrap_err();
            assert!(error.contains("Acceptance is unknown"));
            assert!(
                recovery_composer.is_empty(),
                "unknown draft cannot be retried"
            );
            std::fs::rename(&backup, &state).unwrap();
            assert_eq!(
                observed.lock().unwrap().len(),
                2,
                "recovery makes no Provider call"
            );
            assert_eq!(recovery_view.runs.len(), 2);
        }
    }
    let choice = match arany::WorkspacePermissions::open(&workspace) {
        Ok(access) => Some(
            tokio::time::timeout(
                Duration::from_secs(3),
                super::permissions::choose(
                    &mut terminal,
                    access.prepare_profile_selection(account.path()),
                    &mut composer,
                    false,
                ),
            )
            .await
            .unwrap(),
        ),
        Err(_) => None,
    };
    #[cfg(debug_assertions)]
    assert!(matches!(
        choice,
        Some(Ok(super::permissions::PermissionChoice::Exit))
    ));
    #[cfg(not(debug_assertions))]
    assert!(choice.is_none(), "release rejects the test account root");
    assert_eq!(
        super::permissions::cycle(&mut composer),
        if permissions.is_some() {
            "Request approvals · Shift+Tab changes mode"
        } else {
            "Read only · use /permissions to trust this folder"
        }
    );
    assert_eq!(composer.text(), "x");
    assert_eq!(
        composer.approval_mode(),
        permissions.map(|_| ApprovalMode::Request)
    );
    assert_eq!(std::fs::read(&trust_record).unwrap(), remembered);
    drop(terminal);
}
