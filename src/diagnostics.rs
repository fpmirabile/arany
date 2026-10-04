use crate::store::StateRoot;
use std::fmt::{self, Write};
use std::sync::{Mutex, OnceLock};

const MAX_RECORD_BYTES: usize = 24 * 1024;
static ROOT: OnceLock<Mutex<StateRoot>> = OnceLock::new();

/// Enables bounded local diagnostics in debug builds after ordinary state admission.
pub fn enable_development_diagnostics(root: &StateRoot) {
    if cfg!(debug_assertions)
        && let Ok(root) = root.try_clone()
    {
        let _ = ROOT.set(Mutex::new(root));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubscriptionFailureStage {
    RequestTransport,
    Deadline,
    HttpStatus,
    Destination,
    ContentEncoding,
    ContentLength,
    ReadTransport,
    EndBeforeCompleted,
    StreamLimit,
    EventFraming,
    EventJson,
    EventType,
    CredentialReflection,
    RemoteFailure,
    IncompleteResponse,
    CompletedResponse,
    ResponseEnvelope,
    ResponseModel,
    ResponseStatus,
    ResponseIdentifier,
    ResponseMessage,
    ResponseContent,
    ResponsePhase,
    ResponseFinalMessage,
    OutcomeContract,
    UsageContract,
    LocalOutputLimit,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct StreamCounts {
    pub(crate) bytes: usize,
    pub(crate) events: usize,
    pub(crate) http_status: Option<u16>,
}

pub(crate) fn subscription_failure(stage: SubscriptionFailureStage, counts: StreamCounts) {
    let Some(root) = ROOT.get() else { return };
    let Ok(root) = root.try_lock() else { return };
    write_failure(&root, stage, counts);
}

fn write_failure(root: &StateRoot, stage: SubscriptionFailureStage, counts: StreamCounts) {
    let mut record = BoundedRecord(String::new());
    let _ = writeln!(
        record,
        "arany {} development diagnostic",
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(
        record,
        "subscription stage={stage:?} bytes={} events={} http_status={:?}",
        counts.bytes, counts.events, counts.http_status
    );
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let _ = writeln!(record, "unix_ms={timestamp} pid={}", std::process::id());
    let mut trace = BoundedRecord(String::new());
    let _ = write!(trace, "{}", std::backtrace::Backtrace::force_capture());
    let _ = writeln!(
        record,
        "Local failure call stack (not an upstream or async causal stack; source locations omitted):"
    );
    for line in trace.0.lines() {
        if line.trim_start().starts_with("at ") || line.contains('/') || line.contains('\\') {
            continue;
        }
        if writeln!(record, "{line}").is_err() {
            break;
        }
    }
    let _ = root.append_development_diagnostic(record.0.as_bytes());
}

struct BoundedRecord(String);

impl Write for BoundedRecord {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for character in text.chars() {
            let character = if character.is_control() && character != '\n' {
                ' '
            } else {
                character
            };
            if self.0.len() + character.len_utf8() > MAX_RECORD_BYTES {
                return Err(fmt::Error);
            }
            self.0.push(character);
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    #[test]
    fn diagnostic_file_is_bounded_private_and_rejects_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let path = root.path().join("development.log");
        write_failure(
            &root,
            SubscriptionFailureStage::EndBeforeCompleted,
            StreamCounts {
                bytes: 42,
                events: 1,
                http_status: None,
            },
        );
        let bytes = std::fs::read(&path).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("stage=EndBeforeCompleted bytes=42 events=1"));
        assert!(text.contains("Local failure call stack"));
        assert!(text.contains("diagnostics::write_failure"));
        assert!(!text.contains("/home/") && !text.contains("/tmp/") && !text.contains(" at "));
        assert!(bytes.len() <= MAX_RECORD_BYTES);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let block = vec![b'x'; MAX_RECORD_BYTES];
        for _ in 0..20 {
            root.append_development_diagnostic(&block).unwrap();
            assert!(std::fs::metadata(&path).unwrap().len() <= 256 * 1024);
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(root.append_development_diagnostic(b"not written").is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let alias = temp.path().join("alias");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(root.append_development_diagnostic(b"not written").is_err());
        std::fs::remove_file(&alias).unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&alias, &path).unwrap();
        assert!(root.append_development_diagnostic(b"not written").is_err());
        assert!(!alias.exists());
        let mut record = BoundedRecord(String::new());
        assert!(write!(record, "{}", "\u{1b}é".repeat(MAX_RECORD_BYTES)).is_err());
        assert!(record.0.len() <= MAX_RECORD_BYTES && !record.0.contains('\u{1b}'));
    }
}
