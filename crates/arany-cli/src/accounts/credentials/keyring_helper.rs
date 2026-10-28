use super::{CredentialError, DEFAULT_ACCOUNT, SERVICE, SavedAccount, admit_transport};
use crate::accounts::chatgpt::{
    MAX_TOKEN_RECORD_BYTES, valid_keyring_record, valid_written_keyring_record,
};
use arany::{ProviderKind, RecordKind};
#[cfg(not(target_os = "macos"))]
use keyring::Entry;
use keyring::Error;
#[cfg(target_os = "macos")]
pub(super) mod macos;
#[cfg(target_os = "macos")]
use macos::Entry;
use std::{
    io::{IsTerminal, Read, Write},
    process::{Child, Command, ExitCode, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

#[cfg(test)]
pub(crate) static TEST_HELPER: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

const DEADLINE: Duration = Duration::from_secs(5);
const AUTHORIZATION_DEADLINE: Duration = Duration::from_secs(120);
const PROBE_SLOT: &str = "credential-backend-probe";
const EXIT_NOT_FOUND: u8 = 2;
const EXIT_LOCKED: u8 = 3;
const EXIT_IDENTITY_MISMATCH: u8 = 4;
const LOCKED_CATEGORIES: [&str; 8] = [
    "ERR_SEC_WR_PERM",
    "ERR_SEC_INVALID_OWNER_EDIT",
    "ERR_SEC_NOT_AVAILABLE",
    "ERR_SEC_READ_ONLY",
    "ERR_SEC_AUTH_FAILED",
    "ERR_SEC_NO_SUCH_KEYCHAIN",
    "ERR_SEC_INVALID_KEYCHAIN",
    "ERR_SEC_INTERACTION_NOT_ALLOWED",
];

pub(super) fn probe_backend() -> Result<(), CredentialError> {
    invoke("probe", PROBE_SLOT, None).map(|_| ())
}

struct CancelAuthorization(Arc<AtomicBool>);

impl Drop for CancelAuthorization {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(super) async fn authorize(
    slot: String,
    expected: Option<(Uuid, String)>,
) -> Result<bool, CredentialError> {
    let (mut command, limit) = helper_command("authorize", &slot)?;
    if let Some((id, provider)) = expected {
        if !valid_native_identity(&slot, id, &provider) {
            return Err(CredentialError::InvalidAccount);
        }
        command.args([id.to_string(), provider]);
    } else if !slot.starts_with("chatgpt-") {
        return Err(CredentialError::InvalidAccount);
    }
    interactive_command(command, "authorize", limit)
        .await
        .map(|record| record.is_some())
}

pub(super) async fn read_interactive(slot: &str) -> Result<Option<Vec<u8>>, CredentialError> {
    let (command, limit) = helper_command("read", slot)?;
    interactive_command(command, "read", limit).await
}

async fn interactive_command(
    command: Command,
    operation: &'static str,
    limit: usize,
) -> Result<Option<Vec<u8>>, CredentialError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let _owner = CancelAuthorization(Arc::clone(&cancelled));
    tokio::task::spawn_blocking(move || {
        invoke_command(
            command,
            operation,
            None,
            limit,
            AUTHORIZATION_DEADLINE,
            &cancelled,
        )
    })
    .await
    .map_err(|_| CredentialError::Unavailable)?
}

pub(super) fn read(slot: &str) -> Result<Option<Vec<u8>>, CredentialError> {
    invoke("read", slot, None)
}

pub(super) fn write(slot: &str, record: &str) -> Result<(), CredentialError> {
    if record.len() > slot_record_limit(slot).ok_or(CredentialError::InvalidAccount)?
        || !valid_written_record(slot, record.as_bytes())
    {
        return Err(CredentialError::InvalidAccount);
    }
    invoke("write", slot, Some(record.as_bytes())).map(|_| ())
}

pub(super) fn delete(slot: &str) -> Result<(), CredentialError> {
    if (slot != DEFAULT_ACCOUNT && !slot.starts_with("chatgpt-")) || !valid_slot(slot) {
        return Err(CredentialError::InvalidAccount);
    }
    invoke("delete", slot, None).map(|_| ())
}

fn invoke(
    operation: &str,
    slot: &str,
    record: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, CredentialError> {
    let (command, limit) = helper_command(operation, slot)?;
    invoke_command(
        command,
        operation,
        record,
        limit,
        DEADLINE,
        &AtomicBool::new(false),
    )
}

fn helper_command(operation: &str, slot: &str) -> Result<(Command, usize), CredentialError> {
    admit_transport()?;
    let limit = slot_record_limit(slot).ok_or(CredentialError::InvalidAccount)?;
    if operation != "probe" && slot == PROBE_SLOT {
        return Err(CredentialError::InvalidAccount);
    }
    #[cfg(all(not(test), target_os = "linux"))]
    // Spawn /proc/self/exe: a rebuilt binary's deleted pathname could resolve to another image.
    let executable = std::path::Path::new("/proc/self/exe");
    #[cfg(all(not(test), not(target_os = "linux")))]
    let executable = std::env::current_exe().map_err(|_| CredentialError::Unavailable)?;
    #[cfg(test)]
    let executable = TEST_HELPER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .map(std::path::PathBuf::into_os_string)
        .or_else(|| std::env::var_os("ARANY_TEST_EXE"))
        .filter(|path| std::path::Path::new(path).is_absolute())
        .ok_or(CredentialError::Unavailable)?;
    let mut command = Command::new(executable);
    command
        .args(["--internal-credential-helper", operation, slot])
        .env_clear();
    #[cfg(unix)]
    command.current_dir("/");
    #[cfg(target_os = "linux")]
    let forwarded = [
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "HOME",
        "USER",
        "LOGNAME",
    ];
    #[cfg(not(target_os = "linux"))]
    let forwarded = ["HOME", "USER", "LOGNAME"];
    for name in forwarded {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(all(target_os = "macos", debug_assertions))]
    if let Some(root) = std::env::var_os(macos::TEST_KEYCHAIN_ROOT_ENV) {
        command.env(macos::TEST_KEYCHAIN_ROOT_ENV, root);
    }
    #[cfg(all(target_os = "macos", not(debug_assertions)))]
    if std::env::var_os(macos::TEST_KEYCHAIN_ROOT_ENV).is_some() {
        return Err(CredentialError::Unavailable);
    }
    Ok((command, limit))
}

fn invoke_command(
    mut command: Command,
    operation: &str,
    record: Option<&[u8]>,
    limit: usize,
    duration: Duration,
    cancelled: &AtomicBool,
) -> Result<Option<Vec<u8>>, CredentialError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CredentialError::Unavailable);
    }
    command
        .stdin(if record.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = OwnedChild(command.spawn().map_err(|_| CredentialError::Unavailable)?);
    let input = child.0.stdin.take();
    let output = child
        .0
        .stdout
        .take()
        .ok_or_else(|| unconfirmed_error(operation, CredentialError::Unavailable))?;
    let deadline = Instant::now() + duration;
    std::thread::scope(|scope| {
        let writer = record.map(|record| {
            scope.spawn(move || {
                input
                    .ok_or(CredentialError::Unavailable)?
                    .write_all(record)
                    .map_err(|_| CredentialError::Unavailable)
            })
        });
        let reader = scope.spawn(move || {
            let mut bytes = Vec::new();
            let capture_limit = LOCKED_CATEGORIES
                .iter()
                .map(|category| category.len())
                .fold(limit, usize::max);
            output
                .take((capture_limit + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| CredentialError::Unavailable)?;
            Ok::<_, CredentialError>(bytes)
        });
        let status = loop {
            if cancelled.load(Ordering::Acquire) {
                child.stop();
                return Err(CredentialError::Unavailable);
            }
            match child.0.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    child.stop();
                    return Err(CredentialError::TimedOut);
                }
                Err(_) => {
                    child.stop();
                    return Err(CredentialError::Unavailable);
                }
            }
        };
        let write_result = writer.map(|writer| writer.join());
        let output = reader.join().map_err(|_| CredentialError::Unavailable)??;
        helper_reply(
            operation,
            status,
            output,
            write_result.is_none_or(|result| matches!(result, Ok(Ok(())))),
            limit,
        )
    })
    .map_err(|error| unconfirmed_error(operation, error))
}

fn unconfirmed_error(operation: &str, error: CredentialError) -> CredentialError {
    match (operation, error) {
        (_, CredentialError::Locked) => CredentialError::Locked,
        ("write", _) => CredentialError::WriteOutcomeUnknown,
        ("delete", _) => CredentialError::DeleteOutcomeUnknown,
        (_, error) => error,
    }
}

fn helper_reply(
    operation: &str,
    status: std::process::ExitStatus,
    output: Vec<u8>,
    write_complete: bool,
    limit: usize,
) -> Result<Option<Vec<u8>>, CredentialError> {
    match status.code().and_then(|code| u8::try_from(code).ok()) {
        Some(0) => {
            if !write_complete {
                return Err(CredentialError::Unavailable);
            }
            if output.len() > limit {
                return Err(CredentialError::InvalidAccount);
            }
            if operation == "read" {
                Ok(Some(output))
            } else if output.is_empty() {
                Ok((operation == "authorize").then(Vec::new))
            } else {
                Err(CredentialError::Unavailable)
            }
        }
        Some(EXIT_NOT_FOUND) if matches!(operation, "read" | "probe" | "authorize") => Ok(None),
        Some(EXIT_LOCKED) => {
            if output.is_empty() {
                return Err(CredentialError::Locked);
            }
            let category = LOCKED_CATEGORIES
                .iter()
                .find(|category| category.as_bytes() == output)
                .ok_or(CredentialError::Unavailable)?;
            if cfg!(debug_assertions) {
                let _ = writeln!(
                    std::io::stderr(),
                    "credential store refused access: {category}"
                );
            }
            Err(CredentialError::Locked)
        }
        Some(EXIT_IDENTITY_MISMATCH) if operation == "authorize" => {
            Err(CredentialError::InvalidAccount)
        }
        _ => Err(CredentialError::Unavailable),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixListener, thread};

    #[test]
    fn authorization_admits_version_one_chatgpt_items_only_at_their_retained_slot() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};
        let credentials = crate::accounts::chatgpt::VerifiedCredentials {
            client_id: "oaiapp_authorize".into(),
            host_id: Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap(),
            subject: "synthetic-subject".into(),
            chatgpt_account_id: None,
            id_token: "synthetic-id".into(),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at_unix: 1_900_000_000,
        };
        let consent = crate::accounts::chatgpt::RiskPrompt::new(
            crate::accounts::credentials::AccountStorage::Keyring,
        )
        .accept("Accept")
        .unwrap()
        .bind(&credentials)
        .unwrap();
        let slot =
            |client: &[u8]| format!("chatgpt-{}", URL_SAFE_NO_PAD.encode(Sha256::digest(client)));
        let retained = slot(credentials.client_id.as_bytes());
        let foreign = slot(b"another-client");
        let version_one = serde_json::to_string(
            &serde_json::json!({"schema":1,"credentials":credentials,"consent":consent}),
        )
        .unwrap();
        assert!(valid_authorized_record(&retained, &version_one, None));
        assert!(!valid_authorized_record(&foreign, &version_one, None));
        assert!(!valid_authorized_record(&retained, "{", None));
        for schema in [0, 3, 255] {
            let unsupported =
                version_one.replacen("\"schema\":1", &format!("\"schema\":{schema}"), 1);
            assert!(!valid_authorized_record(&retained, &unsupported, None));
        }
        let duplicated = version_one.replacen("\"schema\":1", "\"schema\":1,\"schema\":1", 1);
        assert!(!valid_authorized_record(&retained, &duplicated, None));
        let current = version_one.replacen("\"schema\":1", "\"schema\":2", 1);
        assert!(!valid_authorized_record(&retained, &current, None));
    }

    #[test]
    fn keyring_slots_are_bounded_and_do_not_accept_arbitrary_accounts() {
        assert_eq!(slot_record_limit(PROBE_SLOT), Some(0));
        assert!(write(PROBE_SLOT, "synthetic").is_err());
        assert_eq!(delete("other"), Err(CredentialError::InvalidAccount));
        let slot = format!("chatgpt-{}", "A".repeat(43));
        assert_eq!(slot_record_limit(&slot), Some(MAX_TOKEN_RECORD_BYTES));
        assert_eq!(slot_record_limit("chatgpt-A"), None);
        assert_eq!(
            slot_record_limit(&format!("chatgpt-{}!", "A".repeat(42))),
            None
        );
        assert_eq!(slot_record_limit("other"), None);
        assert_eq!(
            slot_record_limit(DEFAULT_ACCOUNT),
            Some(RecordKind::SavedAccount.limit())
        );
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(arany::Effort::Low),
            "synthetic-key".into(),
        )
        .unwrap();
        let mut legacy = serde_json::to_value(&account).unwrap();
        legacy["schema"] = 2.into();
        legacy["provider"] = "openai".into();
        for record in [
            serde_json::to_string(&account).unwrap(),
            serde_json::to_string(&legacy).unwrap(),
        ] {
            for (id, provider, accepted) in [
                (account.id, "openai", true),
                (Uuid::now_v7(), "openai", false),
                (account.id, "anthropic", false),
            ] {
                assert_eq!(
                    valid_authorized_record(DEFAULT_ACCOUNT, &record, Some((id, provider.into()))),
                    accepted,
                    "native account identity boundary"
                );
            }
        }
        assert!(!valid_record(
            DEFAULT_ACCOUNT,
            &serde_json::to_vec(&legacy).unwrap()
        ));
    }

    #[test]
    fn deletion_dispatches_only_admitted_slots_to_an_isolated_helper() {
        use std::os::unix::fs::PermissionsExt;

        const SCRIPT: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$HOME/dispatch\"\n";
        const ROOT: &str = "ARANY_TEST_DELETE_ROOT";
        if let Some(root) = std::env::var_os(ROOT) {
            let root = std::path::PathBuf::from(root);
            let helper = root.join("helper");
            assert_eq!(std::env::var_os("ARANY_TEST_EXE"), Some(helper.into()));
            assert_eq!(
                std::fs::read_to_string(root.join("helper")).unwrap(),
                SCRIPT
            );
            assert_eq!(std::env::var_os("HOME"), Some(root.into()));
            assert_eq!(delete(DEFAULT_ACCOUNT), Ok(()));
            assert_eq!(delete(&format!("chatgpt-{}", "A".repeat(43))), Ok(()));
            for slot in [
                PROBE_SLOT.to_owned(),
                "other".into(),
                "chatgpt-A".into(),
                format!("native-test-{}", Uuid::now_v7()),
                format!("native-client-test-{}", Uuid::now_v7()),
            ] {
                assert_eq!(delete(&slot), Err(CredentialError::InvalidAccount));
            }
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let helper = root.join("helper");
        std::fs::write(&helper, SCRIPT).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let thread = std::thread::current();
        let test = thread.name().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env_clear()
            .env(ROOT, &root)
            .env("HOME", &root)
            .env("ARANY_TEST_EXE", &helper)
            .current_dir(&root);
        let output = crate::test_process::file_output(&mut command, None);
        assert!(output.status.success(), "isolated deletion protocol failed");
        crate::test_process::assert_single_test_completed(&output.stdout, test);
        assert_eq!(
            std::fs::read_to_string(root.join("dispatch")).unwrap(),
            format!(
                "--internal-credential-helper\ndelete\n{DEFAULT_ACCOUNT}\n--internal-credential-helper\ndelete\nchatgpt-{}\n",
                "A".repeat(43)
            )
        );
    }

    #[test]
    fn locked_categories_cross_bounded_pipes_without_disclosing_other_output() {
        const CHILD: &str = "ARANY_TEST_LOCKED_PROTOCOL";
        const OPERATIONS: [&str; 5] = ["read", "probe", "authorize", "write", "delete"];
        const CANARY: &str = "private-protocol-canary";
        if std::env::var_os(CHILD).is_none() {
            let test = std::thread::current().name().unwrap().to_owned();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", &test, "--nocapture"])
                .env_clear()
                .env(CHILD, "1")
                .current_dir("/");
            let output = crate::test_process::file_output(&mut command, None);
            assert!(output.status.success(), "isolated Locked protocol failed");
            crate::test_process::assert_single_test_completed(&output.stdout, &test);
            let expected = LOCKED_CATEGORIES
                .iter()
                .filter(|_| cfg!(debug_assertions))
                .map(|category| {
                    format!("credential store refused access: {category}\n")
                        .repeat(OPERATIONS.len())
                })
                .collect::<String>();
            assert!(
                output.stderr == expected.as_bytes(),
                "closed diagnostics only"
            );
            assert!(
                !output
                    .stdout
                    .windows(CANARY.len())
                    .any(|part| part == CANARY.as_bytes()),
                "rejected helper output must stay private"
            );
            return;
        }
        let malformed = [
            CANARY.to_owned(),
            format!("{}\n", LOCKED_CATEGORIES[0]),
            LOCKED_CATEGORIES.concat(),
        ];
        for (payload, accepted) in LOCKED_CATEGORIES
            .iter()
            .copied()
            .map(|category| (category, true))
            .chain(malformed.iter().map(|payload| (payload.as_str(), false)))
        {
            for operation in OPERATIONS {
                let mut command = Command::new("/bin/sh");
                command
                    .args([
                        "-c",
                        "/bin/cat >/dev/null; printf '%s' \"$1\"; exit 3",
                        "helper",
                        payload,
                    ])
                    .env_clear()
                    .current_dir("/");
                let expected = match (accepted, operation) {
                    (true, _) => CredentialError::Locked,
                    (false, "write") => CredentialError::WriteOutcomeUnknown,
                    (false, "delete") => CredentialError::DeleteOutcomeUnknown,
                    (false, _) => CredentialError::Unavailable,
                };
                assert!(
                    invoke_command(
                        command,
                        operation,
                        None,
                        0,
                        DEADLINE,
                        &AtomicBool::new(false)
                    ) == Err(expected),
                    "closed Locked reply for {operation}"
                );
            }
        }
    }

    #[test]
    fn unconfirmed_mutation_helpers_never_report_a_known_failure() {
        for (operation, expected) in [
            ("write", CredentialError::WriteOutcomeUnknown),
            ("delete", CredentialError::DeleteOutcomeUnknown),
        ] {
            for script in [
                "/bin/cat >/dev/null; kill -TERM $$",
                "/bin/cat >/dev/null; exit 1",
                "/bin/cat >/dev/null; printf unexpected",
                "/bin/cat >/dev/null; printf unexpected; exit 3",
            ] {
                let mut command = Command::new("/bin/sh");
                command.args(["-c", script]).env_clear().current_dir("/");
                assert_eq!(
                    invoke_command(
                        command,
                        operation,
                        (operation == "write").then_some(b"synthetic-record".as_slice()),
                        RecordKind::SavedAccount.limit(),
                        DEADLINE,
                        &AtomicBool::new(false),
                    )
                    .as_ref(),
                    Err(&expected),
                    "unconfirmed {operation} must stop continuation"
                );
            }
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "/bin/cat >/dev/null; exit 3"])
                .env_clear()
                .current_dir("/");
            assert_eq!(
                invoke_command(
                    command,
                    operation,
                    None,
                    RecordKind::SavedAccount.limit(),
                    DEADLINE,
                    &AtomicBool::new(false),
                ),
                Err(CredentialError::Locked),
                "a valid locked-store acknowledgment is definitive"
            );
        }
    }

    #[test]
    fn large_secret_pipes_are_drained_while_input_is_written() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "head -c 65536 /dev/zero; head -c 65536 >/dev/null"])
            .env_clear()
            .current_dir("/");
        let input = vec![b'x'; MAX_TOKEN_RECORD_BYTES];
        let output = invoke_command(
            command,
            "read",
            Some(&input),
            MAX_TOKEN_RECORD_BYTES,
            DEADLINE,
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(output.len(), MAX_TOKEN_RECORD_BYTES);
        assert!(output.iter().all(|byte| *byte == 0));
    }

    #[tokio::test]
    async fn authorization_is_status_only_and_cancellation_reaps_the_helper() {
        for (script, expected) in [
            ("exit 0", Ok(Some(Vec::new()))),
            ("exit 2", Ok(None)),
            ("exit 3", Err(CredentialError::Locked)),
            ("exit 4", Err(CredentialError::InvalidAccount)),
            ("exit 1", Err(CredentialError::Unavailable)),
            ("printf synthetic-secret", Err(CredentialError::Unavailable)),
        ] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]).env_clear().current_dir("/");
            assert!(
                interactive_command(command, "authorize", MAX_TOKEN_RECORD_BYTES).await == expected,
                "authorization protocol case"
            );
        }

        let temp = tempfile::tempdir().expect("private authorization root");
        let socket = temp.path().join("authorization");
        let listener = UnixListener::bind(&socket).expect("test-owned authorization socket");
        listener.set_nonblocking(true).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "accounts::credentials::keyring_helper::tests::held_authorization_helper",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("ARANY_TEST_AUTHORIZATION_SOCKET", &socket)
            .current_dir("/");
        let mut authorization = Box::pin(interactive_command(command, "authorize", 1024));
        let ready = async {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                match listener.accept() {
                    Ok((connection, _)) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "authorization readiness deadline"
                        );
                        tokio::task::yield_now().await;
                    }
                    Err(error) => panic!("private authorization gate: {error}"),
                }
            }
        };
        let mut connection = tokio::select! {
            result = &mut authorization => panic!("helper exited before cancellation: {result:?}"),
            connection = ready => connection,
        };
        connection.set_nonblocking(false).unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut pid = [0; 4];
        connection
            .read_exact(&mut pid)
            .expect("helper readiness PID");
        drop(authorization);
        assert_eq!(connection.read(&mut pid).expect("cancelled helper EOF"), 0);
        let pid =
            rustix::process::Pid::from_raw(u32::from_be_bytes(pid).try_into().unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            match rustix::process::test_kill_process(pid) {
                Err(rustix::io::Errno::SRCH) => break,
                Ok(()) => {
                    assert!(Instant::now() < deadline, "cancelled helper was not reaped");
                    thread::yield_now();
                }
                Err(error) => panic!("helper lifecycle observation failed: {error}"),
            }
        }
    }

    #[test]
    #[ignore = "helper-only cancellation gate; requires the parent's private socket"]
    fn held_authorization_helper() {
        let path = std::env::var_os("ARANY_TEST_AUTHORIZATION_SOCKET")
            .expect("parent-owned authorization fixture");
        let mut connection = std::os::unix::net::UnixStream::connect(path).unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        connection
            .write_all(&std::process::id().to_be_bytes())
            .unwrap();
        let mut byte = [0];
        connection
            .read_exact(&mut byte)
            .expect("parent must kill the held helper");
        panic!("held helper received unexpected input");
    }

    #[tokio::test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "explicit human-interaction deadline gate; six-second synthetic helper, no OS store access"
    )]
    #[allow(clippy::await_holding_lock)]
    async fn authorization_wait_accepts_human_latency() {
        let _gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exec /bin/sleep 6"])
            .env_clear()
            .current_dir("/");
        assert_eq!(
            interactive_command(command, "authorize", 1024).await,
            Ok(Some(Vec::new()))
        );
    }

    #[test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "explicit Unix credential write-timeout gate; no OS store access"
    )]
    fn timed_out_write_is_uncertain_and_reaps_helper() {
        let _gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        const SOCKET: &str = "ARANY_TEST_STALLED_WRITE_SOCKET";
        if let Some(path) = std::env::var_os(SOCKET) {
            let mut record = Vec::new();
            std::io::stdin()
                .take((RecordKind::SavedAccount.limit() + 1) as u64)
                .read_to_end(&mut record)
                .expect("synthetic account input");
            let account: SavedAccount =
                serde_json::from_slice(&record).expect("valid synthetic account");
            account.validate().expect("admitted synthetic account");
            let mut connection = std::os::unix::net::UnixStream::connect(path)
                .expect("signal synthetic write acceptance");
            connection.write_all(b"accepted").expect("stage marker");
            thread::sleep(Duration::from_secs(12));
            return;
        }

        let temp = tempfile::tempdir().expect("private test root");
        let socket = temp.path().join("bus");
        let listener = UnixListener::bind(&socket).expect("test-owned socket");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(arany::Effort::Low),
            "synthetic-write-timeout-key".into(),
        )
        .expect("synthetic account");
        let record = serde_json::to_vec(&account).expect("bounded synthetic record");
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "accounts::credentials::keyring_helper::tests::timed_out_write_is_uncertain_and_reaps_helper",
                "--include-ignored",
                "--nocapture",
            ])
            .env_clear()
            .env(SOCKET, &socket)
            .current_dir("/");
        let worker = thread::spawn(move || {
            invoke_command(
                command,
                "write",
                Some(&record),
                RecordKind::SavedAccount.limit(),
                DEADLINE,
                &AtomicBool::new(false),
            )
        });

        // Bounds a stuck helper, not normal startup; a loaded host can take seconds to spawn it.
        let accept_deadline = Instant::now() + Duration::from_secs(30);
        let mut connection = loop {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < accept_deadline,
                        "helper did not accept record"
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("synthetic write accept failed: {error}"),
            }
        };
        connection
            .set_nonblocking(false)
            .expect("blocking stage stream");
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bounded stage read");
        let mut stage = [0; 8];
        connection.read_exact(&mut stage).expect("write acceptance");
        assert_eq!(&stage, b"accepted");
        assert_eq!(
            worker.join().expect("credential supervisor"),
            Err(CredentialError::WriteOutcomeUnknown)
        );
        assert_eq!(
            CredentialError::WriteOutcomeUnknown.to_string(),
            "saved credential write could not be confirmed; the saved account may have changed. Inspect it before retrying"
        );
        assert_eq!(
            connection.read(&mut stage).expect("helper connection EOF"),
            0
        );
    }
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.stop();
    }
}

impl OwnedChild {
    fn stop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn slot_record_limit(slot: &str) -> Option<usize> {
    if slot == PROBE_SLOT {
        Some(0)
    } else if slot == DEFAULT_ACCOUNT
        || ["native-test-", "native-client-test-"]
            .iter()
            .any(|prefix| {
                slot.strip_prefix(prefix)
                    .is_some_and(|id| Uuid::parse_str(id).is_ok())
            })
    {
        Some(RecordKind::SavedAccount.limit())
    } else if slot.strip_prefix("chatgpt-").is_some_and(|suffix| {
        suffix.len() == 43
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }) {
        Some(MAX_TOKEN_RECORD_BYTES)
    } else {
        None
    }
}

fn valid_slot(slot: &str) -> bool {
    slot_record_limit(slot).is_some()
}

fn valid_record(slot: &str, bytes: &[u8]) -> bool {
    if slot.starts_with("chatgpt-") {
        valid_keyring_record(slot, bytes)
    } else {
        serde_json::from_slice::<SavedAccount>(bytes)
            .is_ok_and(|account| account.validate().is_ok())
    }
}

fn valid_native_identity(slot: &str, id: Uuid, provider: &str) -> bool {
    slot == DEFAULT_ACCOUNT
        && id.get_version() == Some(uuid::Version::SortRand)
        && ProviderKind::parse(provider).is_some_and(ProviderKind::is_native_api)
}

fn valid_written_record(slot: &str, bytes: &[u8]) -> bool {
    if slot.starts_with("chatgpt-") {
        valid_written_keyring_record(slot, bytes)
    } else {
        valid_record(slot, bytes)
    }
}

fn valid_authorized_record(slot: &str, record: &str, expected: Option<(Uuid, String)>) -> bool {
    record.len() <= slot_record_limit(slot).unwrap_or(0)
        && if slot.starts_with("chatgpt-") {
            valid_record(slot, record.as_bytes()) && expected.is_none()
        } else {
            super::decode_account(record.as_bytes()).is_ok_and(|account| {
                expected.is_none_or(|(id, provider)| {
                    account.id == id && account.provider.profile() == provider
                })
            })
        }
}

fn locked_reply(_error: &(dyn std::error::Error + Send + Sync + 'static)) -> ExitCode {
    #[cfg(target_os = "macos")]
    {
        let Some(category) = macos::locked_category(_error) else {
            return ExitCode::FAILURE;
        };
        if std::io::stdout().write_all(category.as_bytes()).is_err() {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::from(EXIT_LOCKED)
}

pub(super) fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(2);
    let (Some(operation), Some(slot)) = (args.next(), args.next()) else {
        return ExitCode::FAILURE;
    };
    let (Some(operation), Some(slot)) = (operation.to_str(), slot.to_str()) else {
        return ExitCode::FAILURE;
    };
    let expected = match (args.next(), args.next(), args.next()) {
        (None, None, None) if operation != "authorize" || slot.starts_with("chatgpt-") => None,
        (Some(id), Some(provider), None) if operation == "authorize" => {
            let Some((id, provider)) = id.to_str().zip(provider.to_str()) else {
                return ExitCode::FAILURE;
            };
            let Ok(id) = Uuid::parse_str(id) else {
                return ExitCode::FAILURE;
            };
            if !valid_native_identity(slot, id, provider) {
                return ExitCode::FAILURE;
            }
            Some((id, provider.to_owned()))
        }
        _ => return ExitCode::FAILURE,
    };
    if !matches!(
        operation,
        "read" | "write" | "probe" | "delete" | "authorize"
    ) || !valid_slot(slot)
        || (operation == "probe" && slot != PROBE_SLOT)
        || (operation != "probe" && slot == PROBE_SLOT)
        || (operation == "delete" && slot != DEFAULT_ACCOUNT && !slot.starts_with("chatgpt-"))
        || admit_transport().is_err()
    {
        return ExitCode::FAILURE;
    }
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        return ExitCode::FAILURE;
    }
    match operation {
        "authorize" => match Entry::new(SERVICE, slot).and_then(|entry| entry.get_password()) {
            Ok(record) => {
                if valid_authorized_record(slot, &record, expected) {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(EXIT_IDENTITY_MISMATCH)
                }
            }
            Err(Error::NoEntry) => ExitCode::from(EXIT_NOT_FOUND),
            Err(Error::NoStorageAccess(error)) => locked_reply(error.as_ref()),
            Err(_) => ExitCode::FAILURE,
        },
        "probe" => match Entry::new(SERVICE, slot).and_then(|entry| entry.get_password()) {
            Ok(_) | Err(Error::NoEntry) => ExitCode::SUCCESS,
            Err(Error::NoStorageAccess(error)) => locked_reply(error.as_ref()),
            Err(_) => ExitCode::FAILURE,
        },
        "read" => match Entry::new(SERVICE, slot).and_then(|entry| entry.get_password()) {
            Ok(record) if record.len() <= slot_record_limit(slot).unwrap_or(0) => {
                if std::io::stdout().write_all(record.as_bytes()).is_ok() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Ok(_) => ExitCode::FAILURE,
            Err(Error::NoEntry) => ExitCode::from(EXIT_NOT_FOUND),
            Err(Error::NoStorageAccess(error)) => locked_reply(error.as_ref()),
            Err(_) => ExitCode::FAILURE,
        },
        "write" => {
            let mut record = Vec::new();
            if std::io::stdin()
                .take((slot_record_limit(slot).unwrap_or(0) + 1) as u64)
                .read_to_end(&mut record)
                .is_err()
                || record.is_empty()
                || record.len() > slot_record_limit(slot).unwrap_or(0)
            {
                return ExitCode::FAILURE;
            }
            let Ok(record) = String::from_utf8(record) else {
                return ExitCode::FAILURE;
            };
            if !valid_written_record(slot, record.as_bytes()) {
                return ExitCode::FAILURE;
            }
            match Entry::new(SERVICE, slot).and_then(|entry| entry.set_password(&record)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(Error::NoStorageAccess(error)) => locked_reply(error.as_ref()),
                Err(_) => ExitCode::FAILURE,
            }
        }
        "delete" => match Entry::new(SERVICE, slot).and_then(|entry| entry.delete_credential()) {
            Ok(()) | Err(Error::NoEntry) => ExitCode::SUCCESS,
            Err(Error::NoStorageAccess(error)) => locked_reply(error.as_ref()),
            Err(_) => ExitCode::FAILURE,
        },
        _ => ExitCode::FAILURE,
    }
}
