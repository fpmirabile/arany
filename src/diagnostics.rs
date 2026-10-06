use crate::store::StateRoot;
use std::fmt::{self, Write};
use std::sync::{Mutex, OnceLock};

const MAX_RECORD_BYTES: usize = 24 * 1024;
static ROOT: OnceLock<Mutex<StateRoot>> = OnceLock::new();

/// Enables bounded local diagnostics in debug builds after ordinary state admission.
pub fn enable_development_diagnostics(root: &StateRoot) {
    if cfg!(debug_assertions)
        && let Ok(root) = root.try_clone()
        && ROOT.set(Mutex::new(root)).is_ok()
    {
        let mut record = BoundedRecord(String::new());
        let _ = writeln!(record, "runtime stage=DiagnosticsEnabled");
        record_local(record);
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
    ResponseMissingFinalMessage,
    ResponseDuplicateFinalMessage,
    ResponseAmbiguousFinalMessage,
    ResponseLateCommentary,
    OutcomeContract,
    OutcomeEncoding,
    OutcomeFields,
    OutcomeReadBounds,
    OutcomeToolArguments,
    OutcomeToolPath,
    OutcomeToolDigest,
    OutcomeToolEmptyEdit,
    OutcomeToolTextSize,
    OutcomeToolArgumentSize,
    OutcomeToolProgram,
    UsageContract,
    LocalOutputLimit,
}

/// Closed stages accepted by the local debug log; no error text is stored.
#[derive(Clone, Copy, Debug)]
pub enum DevelopmentFailure {
    ProviderAdmission,
    ToolAdmission,
    ModelCatalog,
    Setup,
    AttachedExit,
    ExecExit,
    Output,
}

/// Records a closed local failure after diagnostics have been enabled.
pub fn record_development_failure(stage: DevelopmentFailure) {
    let mut record = BoundedRecord(String::new());
    let _ = writeln!(record, "runtime stage={stage:?}");
    record_local(record);
}

#[derive(Clone, Copy)]
pub(crate) struct ToolArgumentShape {
    operation: &'static str,
    serialized_bytes: usize,
    path_bytes: Option<usize>,
    path_valid: Option<bool>,
    digest_chars: Option<usize>,
    digest_valid: Option<bool>,
    read_offset: Option<u32>,
    read_limit: Option<u32>,
    old_bytes: Option<usize>,
    new_bytes: Option<usize>,
    text_bytes: Option<usize>,
    program_chars: Option<usize>,
    program_valid: Option<bool>,
    args_count: Option<usize>,
    largest_arg_bytes: Option<usize>,
    nul_args_count: Option<usize>,
}

impl fmt::Debug for ToolArgumentShape {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolArgumentShape")
            .field("operation", &self.operation)
            .field("serialized_bytes", &self.serialized_bytes)
            .field("path_bytes", &self.path_bytes)
            .field("path_valid", &self.path_valid)
            .field("digest_chars", &self.digest_chars)
            .field("digest_valid", &self.digest_valid)
            .field("read_offset", &self.read_offset)
            .field("read_limit", &self.read_limit)
            .field("old_bytes", &self.old_bytes)
            .field("new_bytes", &self.new_bytes)
            .field("text_bytes", &self.text_bytes)
            .field("program_chars", &self.program_chars)
            .field("program_valid", &self.program_valid)
            .field("args_count", &self.args_count)
            .field("largest_arg_bytes", &self.largest_arg_bytes)
            .field("nul_args_count", &self.nul_args_count)
            .finish()
    }
}

impl ToolArgumentShape {
    pub(crate) fn from_call(call: &crate::tools::ToolCall) -> Self {
        use crate::tools::{
            ToolCall,
            types::{valid_digest, valid_name, valid_relative},
        };
        let mut shape = Self {
            operation: call.label(),
            serialized_bytes: serde_json::to_vec(call).map_or(0, |bytes| bytes.len()),
            path_bytes: None,
            path_valid: None,
            digest_chars: None,
            digest_valid: None,
            read_offset: None,
            read_limit: None,
            old_bytes: None,
            new_bytes: None,
            text_bytes: None,
            program_chars: None,
            program_valid: None,
            args_count: None,
            largest_arg_bytes: None,
            nul_args_count: None,
        };
        let path = match call {
            ToolCall::List { path } | ToolCall::Search { path, .. } => Some((path, true)),
            ToolCall::Read { path, .. }
            | ToolCall::Mkdir { path }
            | ToolCall::Write { path, .. }
            | ToolCall::Edit { path, .. } => Some((path, false)),
            ToolCall::Command { cwd, .. } => Some((cwd, true)),
            _ => None,
        };
        if let Some((path, root)) = path {
            shape.path_bytes = Some(path.len());
            shape.path_valid = Some(valid_relative(path, root));
        }
        let digest = match call {
            ToolCall::Edit {
                expected_digest, ..
            } => Some(expected_digest),
            ToolCall::Write {
                expected_digest, ..
            } => expected_digest.as_ref(),
            ToolCall::McpCall { schema_digest, .. } => Some(schema_digest),
            _ => None,
        };
        if let Some(digest) = digest {
            shape.digest_chars = Some(digest.chars().count());
            shape.digest_valid = Some(valid_digest(digest));
        }
        match call {
            ToolCall::Read { offset, limit, .. } => {
                shape.read_offset = Some(*offset);
                shape.read_limit = Some(*limit);
            }
            ToolCall::Edit { old, new, .. } => {
                shape.old_bytes = Some(old.len());
                shape.new_bytes = Some(new.len());
            }
            ToolCall::Write { content, .. } => shape.text_bytes = Some(content.len()),
            ToolCall::Search { query, .. } => shape.text_bytes = Some(query.len()),
            ToolCall::McpCall { arguments, .. } => shape.text_bytes = Some(arguments.len()),
            ToolCall::Command { program, args, .. } => {
                shape.program_chars = Some(program.chars().count());
                shape.program_valid = Some(valid_name(program));
                shape.args_count = Some(args.len());
                shape.largest_arg_bytes = args.iter().map(String::len).max();
                shape.nul_args_count = Some(args.iter().filter(|arg| arg.contains('\0')).count());
            }
            _ => {}
        }
        shape
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct StreamCounts {
    pub(crate) bytes: usize,
    pub(crate) events: usize,
    pub(crate) http_status: Option<u16>,
    pub(crate) response_shape: Option<ResponseShape>,
    pub(crate) tool_arguments: Option<ToolArgumentShape>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ResponseShape {
    pub(crate) messages: usize,
    pub(crate) commentary: usize,
    pub(crate) finals: usize,
    pub(crate) unphased: usize,
    pub(crate) structured: usize,
    pub(crate) done_messages: usize,
    pub(crate) done_finals: usize,
}

pub(crate) fn subscription_failure(stage: SubscriptionFailureStage, counts: StreamCounts) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(root) = ROOT.get() else { return };
    let Ok(root) = root.try_lock() else { return };
    write_failure(&root, stage, counts);
}

fn write_failure(root: &StateRoot, stage: SubscriptionFailureStage, counts: StreamCounts) {
    let mut record = BoundedRecord(String::new());
    let _ = writeln!(
        record,
        "subscription stage={stage:?} bytes={} events={} http_status={:?}",
        counts.bytes, counts.events, counts.http_status
    );
    if let Some(shape) = counts.response_shape {
        let _ = writeln!(
            record,
            "response_shape messages={} commentary={} finals={} unphased={} structured={} done_messages={} done_finals={}",
            shape.messages,
            shape.commentary,
            shape.finals,
            shape.unphased,
            shape.structured,
            shape.done_messages,
            shape.done_finals,
        );
    }
    if let Some(shape) = counts.tool_arguments {
        let _ = writeln!(record, "tool_arguments {shape:?}");
    }
    write_record(root, record);
}

pub(crate) fn event_failure(event: &crate::session::Event) {
    use crate::session::{CompactionStatus, Event, ProviderCallDisposition, RunDisposition};
    use crate::tools::ToolDisposition;
    if !cfg!(debug_assertions) {
        return;
    }
    let mut record = BoundedRecord(String::new());
    match event {
        Event::ProviderCallRecorded { record: call, .. }
            if !matches!(
                call.disposition,
                ProviderCallDisposition::Finished
                    | ProviderCallDisposition::Delegated
                    | ProviderCallDisposition::ToolRequested
            ) =>
        {
            let _ = writeln!(
                record,
                "provider phase={:?} disposition={:?} reason={:?}",
                call.phase, call.disposition, call.failure_reason
            );
        }
        Event::ToolFinished { observation, .. }
            if observation.disposition != ToolDisposition::Succeeded =>
        {
            let _ = writeln!(
                record,
                "tool operation={} disposition={:?} receipt={} stops_run={}",
                observation.intent.call.label(),
                observation.disposition,
                observation.guard.is_some(),
                observation.stops_run()
            );
        }
        Event::RunFinished { disposition, .. } if *disposition != RunDisposition::Finished => {
            let _ = writeln!(record, "run disposition={disposition:?}");
        }
        Event::ContextCompacted { record: compaction } => {
            let CompactionStatus::Failed { reason } = compaction.status else {
                return;
            };
            let _ = writeln!(record, "compaction reason={reason:?}");
        }
        _ => return,
    }
    record_local(record);
}

pub(crate) fn engine_failure(error: &crate::engine::EngineError) {
    use crate::engine::EngineError;
    if !cfg!(debug_assertions) {
        return;
    }
    let cause = match error {
        EngineError::InvalidRequest => "InvalidRequest",
        EngineError::MissingSession => "MissingSession",
        EngineError::NoSessionForWorkspace => "NoSessionForWorkspace",
        EngineError::TooManySessions => "TooManySessions",
        EngineError::SessionBusy => "SessionBusy",
        EngineError::SessionNotIdle => "SessionNotIdle",
        EngineError::ForkNotAtRunBoundary => "ForkNotAtRunBoundary",
        EngineError::WorkspaceMismatch => "WorkspaceMismatch",
        EngineError::InvalidWorkspacePath => "InvalidWorkspacePath",
        EngineError::InputNotRegular => "InputNotRegular",
        EngineError::InputNotUtf8 => "InputNotUtf8",
        EngineError::InputTooLarge => "InputTooLarge",
        EngineError::MissingInclude => "MissingInclude",
        EngineError::TooManyIncludes => "TooManyIncludes",
        EngineError::StateOverlap => "StateOverlap",
        EngineError::WorkspaceUnavailable => "WorkspaceUnavailable",
        EngineError::InvalidHistory => "InvalidHistory",
        EngineError::ContextTooLarge => "ContextTooLarge",
        EngineError::ImagesNotAdmitted => "ImagesNotAdmitted",
        EngineError::CompactionInputEmpty => "CompactionInputEmpty",
        EngineError::CompactionInputTooLarge => "CompactionInputTooLarge",
        EngineError::CompactionNotIdle => "CompactionNotIdle",
        EngineError::CompactionLimit => "CompactionLimit",
        EngineError::CancellationDrainTimeout => "CancellationDrainTimeout",
        EngineError::CoordinatorFailed => "CoordinatorFailed",
        EngineError::CancelledBeforeStart => "CancelledBeforeStart",
        EngineError::Store(_) => "Store",
        EngineError::Tool(_) => "Tool",
    };
    let mut record = BoundedRecord(String::new());
    let _ = writeln!(record, "engine cause={cause}");
    if let EngineError::Store(crate::store::StoreError::Io(error)) = error {
        let _ = writeln!(record, "filesystem kind={:?}", error.kind());
    }
    record_local(record);
}

pub(crate) fn guard_failure(error: &crate::tools::ToolError) {
    if !cfg!(debug_assertions) {
        return;
    }
    let mut record = BoundedRecord(String::new());
    let _ = writeln!(record, "guard stage={}", crate::tools::failure_stage(error));
    record_local(record);
}

fn record_local(record: BoundedRecord) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(root) = ROOT.get() else { return };
    let Ok(root) = root.try_lock() else { return };
    write_record(&root, record);
}

fn write_record(root: &StateRoot, details: BoundedRecord) {
    let mut record = BoundedRecord(String::new());
    let _ = writeln!(
        record,
        "arany {} development diagnostic",
        env!("CARGO_PKG_VERSION")
    );
    let _ = write!(record, "{}", details.0);
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
        enable_development_diagnostics(&root);
        if cfg!(debug_assertions) {
            assert!(path.exists(), "development admission must create the log");
            assert!(
                std::fs::read_to_string(&path)
                    .unwrap()
                    .contains("DiagnosticsEnabled")
            );
        } else {
            assert!(!path.exists());
        }
        write_failure(
            &root,
            SubscriptionFailureStage::EndBeforeCompleted,
            StreamCounts {
                bytes: 42,
                events: 1,
                http_status: None,
                response_shape: None,
                tool_arguments: None,
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
        const CANARY: &str = "private-diagnostic-canary-do-not-record";
        let call = crate::tools::ToolCall::Edit {
            path: CANARY.into(),
            expected_digest: CANARY.into(),
            old: CANARY.into(),
            new: CANARY.into(),
        };
        write_failure(
            &root,
            SubscriptionFailureStage::OutcomeToolDigest,
            StreamCounts {
                tool_arguments: Some(ToolArgumentShape::from_call(&call)),
                ..StreamCounts::default()
            },
        );
        let shape_log = std::fs::read_to_string(&path).unwrap();
        assert!(
            shape_log.contains("operation: \"edit\"")
                && shape_log.contains("digest_valid: Some(false)")
        );
        assert!(shape_log.contains(&format!("old_bytes: Some({})", CANARY.len())));
        assert!(!shape_log.contains(CANARY));
        record_development_failure(DevelopmentFailure::ProviderAdmission);
        engine_failure(&crate::engine::EngineError::Store(
            crate::store::StoreError::Io(std::io::Error::other(CANARY)),
        ));
        guard_failure(&crate::tools::ToolError::GuardRejected(CANARY));
        event_failure(&crate::session::Event::ProviderCallRecorded {
            run_id: Default::default(),
            agent_run_id: Default::default(),
            record: crate::session::ProviderCallRecord {
                phase: crate::provider::AgentPhase::RootSynthesis,
                disposition: crate::session::ProviderCallDisposition::InvalidResponse,
                response_id: Some(CANARY.into()),
                input_tokens: None,
                output_tokens: None,
                wire_provenance: None,
                failure_reason: Some(crate::provider::ProviderFailureReason::OutcomeContract),
            },
        });
        event_failure(&crate::session::Event::ToolFinished {
            run_id: Default::default(),
            agent_run_id: Default::default(),
            observation: crate::tools::ToolObservation {
                intent: crate::tools::EffectIntent {
                    id: uuid::Uuid::now_v7(),
                    run_id: Default::default(),
                    agent_run_id: Default::default(),
                    policy_digest: [1; 32],
                    enforcement_digest: [1; 32],
                    workspace_device: 1,
                    workspace_inode: 1,
                    call,
                    limits: Default::default(),
                    expires_at_ms: 1,
                    use_count: 1,
                },
                disposition: crate::tools::ToolDisposition::Uncertain,
                output: CANARY.into(),
                guard: None,
            },
        });
        event_failure(&crate::session::Event::RunFinished {
            run_id: Default::default(),
            disposition: crate::session::RunDisposition::Failed,
        });
        let failure_log = std::fs::read_to_string(&path).unwrap();
        for detail in [
            "runtime stage=ProviderAdmission",
            "engine cause=Store",
            "filesystem kind=Other",
            "guard stage=execution",
            "provider phase=RootSynthesis disposition=InvalidResponse reason=Some(OutcomeContract)",
            "tool operation=edit disposition=Uncertain receipt=false stops_run=true",
            "run disposition=Failed",
        ] {
            assert_eq!(
                failure_log.contains(detail),
                cfg!(debug_assertions),
                "closed diagnostic stage missing"
            );
        }
        assert!(!failure_log.contains(CANARY));
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
