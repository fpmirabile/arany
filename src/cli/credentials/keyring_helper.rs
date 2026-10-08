use super::{CredentialError, DEFAULT_ACCOUNT, SERVICE, SavedAccount, admit_transport};
use crate::cli::chatgpt::{MAX_TOKEN_RECORD_BYTES, valid_keyring_record};
use arany::StateRoot;
#[cfg(not(target_os = "macos"))]
use keyring::Entry;
use keyring::Error;
#[cfg(target_os = "macos")]
mod macos;
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

const DEADLINE: Duration = Duration::from_secs(5);
const AUTHORIZATION_DEADLINE: Duration = Duration::from_secs(120);
const CHATGPT_PROBE_SLOT: &str = "chatgpt-backend-probe";

pub(super) fn probe_chatgpt_backend() -> Result<(), CredentialError> {
    invoke("probe", CHATGPT_PROBE_SLOT, None).map(|_| ())
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
        || !valid_record(slot, record.as_bytes())
    {
        return Err(CredentialError::InvalidAccount);
    }
    invoke("write", slot, Some(record.as_bytes())).map(|_| ())
}

pub(super) fn delete(slot: &str) -> Result<(), CredentialError> {
    if !slot.starts_with("chatgpt-") || !valid_slot(slot) {
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
    if operation != "probe" && slot == CHATGPT_PROBE_SLOT {
        return Err(CredentialError::InvalidAccount);
    }
    #[cfg(all(not(test), target_os = "linux"))]
    let executable = std::path::Path::new("/proc/self/exe");
    #[cfg(all(not(test), not(target_os = "linux")))]
    let executable = std::env::current_exe().map_err(|_| CredentialError::Unavailable)?;
    #[cfg(test)]
    let executable = std::env::var_os("ARANY_TEST_EXE")
        .filter(|path| std::path::Path::new(path).is_absolute())
        .ok_or(CredentialError::Unavailable)?;
    let mut command = Command::new(executable);
    command
        .args(["--internal-credential-helper", operation, slot])
        .env_clear();
    #[cfg(unix)]
    command.current_dir("/");
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
    #[cfg(all(target_os = "macos", debug_assertions))]
    if let Some(root) = std::env::var_os("ARANY_TEST_KEYCHAIN_ROOT") {
        command.env("ARANY_TEST_KEYCHAIN_ROOT", root);
    }
    #[cfg(all(target_os = "macos", not(debug_assertions)))]
    if std::env::var_os("ARANY_TEST_KEYCHAIN_ROOT").is_some() {
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
    let output = child.0.stdout.take().ok_or(CredentialError::Unavailable)?;
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
            output
                .take((limit + 1) as u64)
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
                    return Err(match operation {
                        "write" => CredentialError::WriteOutcomeUnknown,
                        "delete" => CredentialError::DeleteOutcomeUnknown,
                        _ => CredentialError::TimedOut,
                    });
                }
                Err(_) => {
                    child.stop();
                    return Err(CredentialError::Unavailable);
                }
            }
        };
        let write_result = writer.map(|writer| writer.join());
        let output = reader.join().map_err(|_| CredentialError::Unavailable)??;
        match status.code() {
            Some(0) => {
                if write_result.is_some_and(|result| !matches!(result, Ok(Ok(())))) {
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
            Some(2) if matches!(operation, "read" | "probe" | "authorize") => Ok(None),
            Some(3) => Err(CredentialError::Locked),
            Some(4) if operation == "authorize" => Err(CredentialError::InvalidAccount),
            _ => Err(CredentialError::Unavailable),
        }
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixListener, thread};

    #[test]
    fn keyring_slots_are_bounded_and_do_not_accept_arbitrary_accounts() {
        assert_eq!(slot_record_limit(CHATGPT_PROBE_SLOT), Some(0));
        assert!(write(CHATGPT_PROBE_SLOT, "synthetic").is_err());
        assert_eq!(
            delete(DEFAULT_ACCOUNT),
            Err(CredentialError::InvalidAccount)
        );
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
            Some(StateRoot::MAX_ACCOUNT_RECORD_BYTES)
        );
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(arany::Effort::Low),
            "synthetic-key".into(),
        )
        .unwrap();
        let record = serde_json::to_string(&account).unwrap();
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
                "cli::credentials::keyring_helper::tests::held_authorization_helper",
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
    #[ignore = "explicit human-interaction deadline gate; six-second synthetic helper, no OS store access"]
    async fn authorization_wait_accepts_human_latency() {
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
    #[ignore = "explicit Unix credential write-timeout gate; no OS store access"]
    fn timed_out_write_is_uncertain_and_reaps_helper() {
        const SOCKET: &str = "ARANY_TEST_STALLED_WRITE_SOCKET";
        if let Some(path) = std::env::var_os(SOCKET) {
            let mut record = Vec::new();
            std::io::stdin()
                .take((StateRoot::MAX_ACCOUNT_RECORD_BYTES + 1) as u64)
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
                "cli::credentials::keyring_helper::tests::timed_out_write_is_uncertain_and_reaps_helper",
                "--ignored",
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
                StateRoot::MAX_ACCOUNT_RECORD_BYTES,
                DEADLINE,
                &AtomicBool::new(false),
            )
        });

        let accept_deadline = Instant::now() + Duration::from_secs(4);
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
            "OS credential store write timed out; the saved account may have changed. Check it before retrying"
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
    if slot == CHATGPT_PROBE_SLOT {
        Some(0)
    } else if slot == DEFAULT_ACCOUNT
        || ["native-test-", "native-client-test-"]
            .iter()
            .any(|prefix| {
                slot.strip_prefix(prefix)
                    .is_some_and(|id| Uuid::parse_str(id).is_ok())
            })
    {
        Some(StateRoot::MAX_ACCOUNT_RECORD_BYTES)
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
        && matches!(provider, "openai" | "anthropic")
}

fn valid_authorized_record(slot: &str, record: &str, expected: Option<(Uuid, String)>) -> bool {
    record.len() <= slot_record_limit(slot).unwrap_or(0)
        && valid_record(slot, record.as_bytes())
        && expected.is_none_or(|(id, provider)| {
            serde_json::from_str::<SavedAccount>(record)
                .is_ok_and(|account| account.id == id && account.provider == provider)
        })
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
        || (operation == "probe" && slot != CHATGPT_PROBE_SLOT)
        || (operation != "probe" && slot == CHATGPT_PROBE_SLOT)
        || (operation == "delete" && !slot.starts_with("chatgpt-"))
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
                    ExitCode::from(4)
                }
            }
            Err(Error::NoEntry) => ExitCode::from(2),
            Err(Error::NoStorageAccess(_)) => ExitCode::from(3),
            Err(_) => ExitCode::FAILURE,
        },
        "probe" => match Entry::new(SERVICE, slot).and_then(|entry| entry.get_password()) {
            Ok(_) | Err(Error::NoEntry) => ExitCode::SUCCESS,
            Err(Error::NoStorageAccess(_)) => ExitCode::from(3),
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
            Err(Error::NoEntry) => ExitCode::from(2),
            Err(Error::NoStorageAccess(_)) => ExitCode::from(3),
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
            if !valid_record(slot, record.as_bytes()) {
                return ExitCode::FAILURE;
            }
            match Entry::new(SERVICE, slot).and_then(|entry| entry.set_password(&record)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(Error::NoStorageAccess(_)) => ExitCode::from(3),
                Err(_) => ExitCode::FAILURE,
            }
        }
        "delete" => match Entry::new(SERVICE, slot).and_then(|entry| entry.delete_credential()) {
            Ok(()) | Err(Error::NoEntry) => ExitCode::SUCCESS,
            Err(Error::NoStorageAccess(_)) => ExitCode::from(3),
            Err(_) => ExitCode::FAILURE,
        },
        _ => ExitCode::FAILURE,
    }
}
