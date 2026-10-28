use super::*;
use crate::test_process::assert_single_test_completed;
use base64::Engine as _;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const REMOVED_CLIENT: &str = "oaiapp_removed_account";
const ROOT: &str = "ARANY_TEST_REMOVAL_ROOT";
const PHASE: &str = "ARANY_TEST_REMOVAL_PHASE";
const STAGE: &str = "cli::attached::setup::removal::manager_stage";
const DEADLINE: Duration = Duration::from_secs(10);
const CAPTURE_BYTES: usize = 64 * 1024;
const MAX_PAGES: usize = 128;
const READ_BUFFER_BYTES: usize = 4096;
const ACCOUNTS: &str = "Credentials belong to this OS user; type a listed choice number or exact name (1-6); empty Enter selects 1";
const HELPER: &str = r#"#!/bin/sh
case "$2" in
read) exit 2 ;;
delete)
    printf '%s\n' "$3" >> "$HOME/deletions"
    if [ -f "$HOME/retry" ]; then exit 0; fi
    : > "$HOME/retry"
    kill -TERM $$ ;;
*) exit 1 ;;
esac
"#;

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "isolated manager PTY; no OS credential store access"
)]
fn failed_nonselected_cleanup_can_be_retried_in_a_fresh_manager() {
    let _gate = crate::test_process::NATIVE_GATE.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    std::fs::write(root.join("helper"), HELPER).unwrap();
    std::fs::set_permissions(root.join("helper"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let account_root = StateRoot::admit(&root.join("account-root")).unwrap();
    let (selected, _) = seed_accounts(&account_root);
    let preferences = account_root
        .record(RecordKind::ModelPreferences)
        .read()
        .unwrap();
    for phase in ["first", "retry"] {
        let mut manager = Manager::start(&root, phase);
        manager.respond(
            "type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing",
            b"1\r",
        );
        manager.respond(ACCOUNTS, b"5\r");
        manager.respond(
            "type a choice name: 1. Native API account or 2. ChatGPT account",
            b"2\r",
        );
        manager.respond("type a choice name:", b"2\r");
        manager.respond("type a choice name: 1. Back or 2. Accept", b"Accept\r");
        if phase == "retry" {
            manager.respond(ACCOUNTS, b"6\r");
        }
        manager.finish();
        let bytes = account_root
            .record(RecordKind::ChatgptAccounts)
            .read()
            .unwrap()
            .unwrap();
        let index: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(index["selected"], selected.to_string());
        assert_eq!(index["accounts"][1]["disconnected"], true);
        assert_eq!(
            account_root
                .record(RecordKind::ModelPreferences)
                .read()
                .unwrap(),
            preferences
        );
    }
    let deleted = std::fs::read_to_string(root.join("deletions")).unwrap();
    let deleted: Vec<_> = deleted.lines().collect();
    assert_eq!(deleted.len(), 2);
    assert_eq!(deleted[0], deleted[1]);
    assert_eq!(
        deleted[0],
        format!(
            "chatgpt-{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(REMOVED_CLIENT.as_bytes()))
        )
    );
}

fn seed_accounts(root: &StateRoot) -> (uuid::Uuid, uuid::Uuid) {
    let selected = uuid::Uuid::now_v7();
    let removed = uuid::Uuid::now_v7();
    let host = "123e4567-e89b-42d3-a456-426614174000";
    let entry = |id, client| {
        serde_json::json!({
            "id":id,"host_id":host,"client_id":client,"subject":"synthetic-subject",
            "chatgpt_account_id":null,"keyring_slot":crate::setup::legacy_chatgpt_slot(client),
            "storage":"keyring","renewal_pending":false,"signout_pending":false,
            "disconnected":false,"plan_permission_missing":false,"token":null
        })
    };
    root.record(RecordKind::ChatgptAccounts).replace(&serde_json::to_vec(&serde_json::json!({
        "schema":3,"selected":selected,
        "accounts":[entry(selected,"oaiapp_selected_removal"),entry(removed,REMOVED_CLIENT)],
        "model_checks":[]
    })).unwrap()).unwrap();
    (selected, removed)
}

#[test]
#[ignore = "helper-only manager journey; requires the parent's private roots and synthetic helper"]
fn manager_stage() {
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    assert_eq!(
        std::env::var_os("ARANY_TEST_EXE"),
        Some(root.join("helper").into())
    );
    assert_eq!(
        std::fs::read_to_string(root.join("helper")).unwrap(),
        HELPER
    );
    assert_eq!(std::env::var_os("HOME"), Some(root.clone().into()));
    let uncertain = std::env::var(PHASE).unwrap() == "first";
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let workspace = root.join("workspace");
            let state = root.join("state");
            let (selected, ids, _) = chatgpt::saved_account_index(&workspace).unwrap();
            let removed = *ids.iter().find(|id| **id != selected).unwrap();
            assert_eq!(
                chatgpt::saved_account_ids().unwrap().1.contains(&removed),
                uncertain
            );
            let id = create_session(StateRoot::admit(&state).unwrap(), workspace.clone(), None)
                .await
                .unwrap();
            let defaults = SessionDefaults {
                provider: Some("chatgpt".into()),
                account_id: Some(selected),
                policy: CollaborationPolicy::Single,
                ..Default::default()
            };
            arany::set_session_defaults(
                StateRoot::open_existing(&state).unwrap(),
                workspace.clone(),
                id,
                defaults.clone(),
            )
            .await
            .unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let view = store.load_view(id).await.unwrap().unwrap();
            store.close().await.unwrap();
            let mut terminal = AttachedTerminal::acquire_with_mode(
                true,
                crate::terminal::TerminalMode::Classic,
                Default::default(),
            )
            .unwrap();
            let mut journal = SetupJournal::for_conversation(&state, &workspace, &view);
            let result = resolve(
                &mut terminal,
                &state,
                &workspace,
                &mut journal,
                &SessionDefaults::default(),
            )
            .await;
            terminal.restore().unwrap();
            assert_eq!(
                matches!(&result, Err(SetupError::Presentation(_))),
                uncertain
            );
            if !uncertain {
                assert!(matches!(result, Ok(None)));
            }
            assert_removal_journal(&state, id, removed, &defaults, uncertain).await;
        });
}

async fn assert_removal_journal(
    state: &Path,
    id: SessionId,
    removed: uuid::Uuid,
    defaults: &SessionDefaults,
    uncertain: bool,
) {
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let events = store.load_session(id).await.unwrap();
    let operation = events
        .iter()
        .find_map(|event| match &event.event {
            Event::ProviderSetupEditStarted {
                operation_id,
                target,
                ..
            } => {
                assert_eq!(target.identity, removed.to_string());
                Some(*operation_id)
            }
            _ => None,
        })
        .unwrap();
    assert!(matches!(&events.last().unwrap().event,
        Event::ProviderSetupEditFinished { operation_id, disposition, .. }
            if *operation_id == operation && *disposition == if uncertain {
                ProviderSetupDisposition::Uncertain
            } else { ProviderSetupDisposition::Applied }
    ));
    let view = store.load_view(id).await.unwrap().unwrap();
    assert_eq!(&view.defaults, defaults);
    assert!(view.runs.is_empty());
    assert_eq!(view.pending_provider_setup_edit, None);
    assert_eq!(view.provider_setup_uncertain, uncertain);
    store.close().await.unwrap();
}

struct Manager {
    child: Child,
    input: std::fs::File,
    output: std::fs::File,
    transcript: Vec<u8>,
    cursor: usize,
}

impl Drop for Manager {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let pid = rustix::process::Pid::from_raw(self.child.id().try_into().unwrap()).unwrap();
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.child.wait();
    }
}

impl Manager {
    fn start(root: &Path, phase: &str) -> Self {
        let (output, slave) = crate::test_process::cloexec_pty(None);
        let input = output.try_clone().unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                STAGE,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                "--color",
                "never",
            ])
            .env_clear()
            .env("TERM", "dumb")
            .env(ROOT, root)
            .env(PHASE, phase)
            .env("ARANY_TEST_EXE", root.join("helper"))
            .env("ARANY_TEST_ACCOUNT_ROOT", root.join("account-root"))
            .env("HOME", root)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", root.join("missing-bus").display()),
            )
            .current_dir(root)
            .process_group(0)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave))
            .spawn()
            .unwrap();
        let flags = fcntl_getfl(&output).unwrap();
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
        Self {
            child,
            input,
            output,
            transcript: Vec::new(),
            cursor: 0,
        }
    }

    fn pump(&mut self) {
        let mut bytes = [0; READ_BUFFER_BYTES];
        loop {
            match self.output.read(&mut bytes) {
                Ok(0) => break,
                Ok(length) => {
                    assert!(self.transcript.len() + length <= CAPTURE_BYTES);
                    self.transcript.extend_from_slice(&bytes[..length]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error)
                    if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) =>
                {
                    break;
                }
                Err(error) => panic!("manager output: {error}"),
            }
        }
    }

    fn respond(&mut self, prompt: &str, choice: &[u8]) {
        let deadline = Instant::now() + DEADLINE;
        let mut pages = 0;
        loop {
            self.pump();
            let fresh = String::from_utf8_lossy(&self.transcript[self.cursor..]);
            if let Some(start) = fresh.find("Choose: Page ")
                && let Some(end) = fresh[start..].find('\n')
            {
                pages += 1;
                assert!(pages <= MAX_PAGES);
                self.cursor += start + end + 1;
                self.input.write_all(b"\r").unwrap();
                continue;
            }
            if fresh.contains("Input:\r\n") && fresh.contains(prompt) {
                self.cursor = self.transcript.len();
                self.input.write_all(choice).unwrap();
                return;
            }
            assert!(
                Instant::now() < deadline,
                "manager prompt missing: {prompt}; {}",
                String::from_utf8_lossy(&self.transcript)
            );
            std::thread::yield_now();
        }
    }

    fn finish(mut self) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.pump();
                assert!(
                    status.success(),
                    "manager stage failed: {}",
                    String::from_utf8_lossy(&self.transcript)
                );
                assert_single_test_completed(&self.transcript, STAGE);
                return;
            }
            assert!(Instant::now() < deadline, "manager exit deadline");
            std::thread::yield_now();
        }
    }
}
