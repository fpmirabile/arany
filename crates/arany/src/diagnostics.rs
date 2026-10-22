use crate::store::StateRoot;
use std::fmt::{self, Write};
use std::sync::{Mutex, OnceLock};

const MAX_RECORD_BYTES: usize = 24 * 1024;
static ROOT: OnceLock<Mutex<StateRoot>> = OnceLock::new();

#[cfg(all(test, unix))]
use crate::test_process;

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

pub(crate) fn enabled() -> bool {
    cfg!(debug_assertions) && ROOT.get().is_some()
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
            ToolCall::List { path }
            | ToolCall::Search { path, .. }
            | ToolCall::Glob { path, .. } => Some((path, true)),
            ToolCall::Read { path, .. }
            | ToolCall::Mkdir { path }
            | ToolCall::Write { path, .. }
            | ToolCall::Edit { path, .. }
            | ToolCall::MultiEdit {
                file_path: path, ..
            } => Some((path, false)),
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
            }
            | ToolCall::MultiEdit {
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
                shape.read_offset = *offset;
                shape.read_limit = *limit;
            }
            ToolCall::Edit { old, new, .. } => {
                shape.old_bytes = Some(old.len());
                shape.new_bytes = Some(new.len());
            }
            ToolCall::Write { content, .. } => shape.text_bytes = Some(content.len()),
            ToolCall::Search { query, .. } => shape.text_bytes = Some(query.len()),
            ToolCall::Glob { pattern, .. } => shape.text_bytes = Some(pattern.len()),
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
            "response_shape messages={} commentary={} finals={} unphased={} done_messages={} done_finals={}",
            shape.messages,
            shape.commentary,
            shape.finals,
            shape.unphased,
            shape.done_messages,
            shape.done_finals,
        );
    }
    if let Some(shape) = counts.tool_arguments {
        let _ = writeln!(record, "tool_arguments {shape:?}");
    }
    write_record(root, record);
}

pub(crate) fn tool_context_full(
    request: &crate::provider::ProviderRequest,
    window_tokens: Option<u32>,
    tool_allowance_bytes: usize,
    pending_turn_bytes: usize,
) {
    if enabled() {
        record_local(context_record(
            request,
            window_tokens,
            tool_allowance_bytes,
            pending_turn_bytes,
        ));
    }
}

fn context_record(
    request: &crate::provider::ProviderRequest,
    window_tokens: Option<u32>,
    tool_allowance_bytes: usize,
    pending_turn_bytes: usize,
) -> BoundedRecord {
    let input = &request.input;
    let mut components = vec![
        ("objective", input.objective.len()),
        (
            "instructions",
            input
                .workspace_guidance
                .iter()
                .map(|value| crate::provider::instructions_text(&value.path, &value.text).len())
                .sum(),
        ),
        ("includes", input.includes.iter().map(String::len).sum()),
        (
            "history",
            input
                .history
                .iter()
                .map(|turn| turn.user.len() + turn.assistant.len() + 64)
                .sum(),
        ),
        (
            "summary",
            input.context_summary.as_ref().map_or(0, String::len),
        ),
        (
            "tools",
            input
                .tools
                .as_ref()
                .map_or(0, crate::tools::ToolContext::rendered_len),
        ),
        ("pending_turn", pending_turn_bytes),
        (
            "images",
            input
                .images
                .iter()
                .map(|value| value.image.context_bytes())
                .sum(),
        ),
        (
            "steering",
            input.steering_notes.iter().map(String::len).sum(),
        ),
        (
            "goal",
            serde_json::to_string(&(
                &input.goal_budget,
                &input.goal_plan_contract,
                &input.goal_context_facts,
            ))
            .map_or(0, |value| value.len()),
        ),
    ];
    let estimated_bytes = components.iter().map(|(_, bytes)| bytes).sum::<usize>();
    components.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    let mut record = BoundedRecord(String::new());
    // Only a bounded model identifier is permitted; all conversation and Tool values stay private.
    let model = if request.model.len() <= 128
        && request
            .model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
    {
        request.model.as_str()
    } else {
        "unreported"
    };
    let _ = writeln!(
        record,
        "context cause=ToolContextFull model={model} window_tokens={window_tokens:?} ceiling_bytes={} tool_allowance_bytes={tool_allowance_bytes} estimated_content_bytes={estimated_bytes} estimated_content_tokens={} max_output_tokens={}",
        crate::session::content_ceiling(window_tokens),
        estimated_bytes.div_ceil(3),
        request.max_output_tokens,
    );
    for (label, bytes) in components {
        let _ = writeln!(record, "component={label} bytes={bytes}");
    }
    if let Some(context) = &input.tools {
        let largest_turn = context
            .steps
            .iter()
            .map(|step| step.turn.len())
            .max()
            .unwrap_or(0);
        let largest_result = context
            .steps
            .iter()
            .flat_map(|step| {
                (0..step.observations.len())
                    .map(move |index| context.model_result(step, index).to_string().len())
            })
            .max()
            .unwrap_or(0);
        let _ = writeln!(
            record,
            "catalog_bytes={} largest_turn_bytes={largest_turn} largest_result_bytes={largest_result}",
            context.catalog().len()
        );
    }
    record
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
                "provider phase={:?} disposition={:?} reason={:?} refusal={:?}",
                call.phase, call.disposition, call.failure_reason, call.refusal
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
                observation.attestation.is_some(),
                observation.stops_run()
            );
        }
        Event::RunFinished {
            disposition,
            failure,
            ..
        } if *disposition != RunDisposition::Finished => {
            let _ = writeln!(record, "run disposition={disposition:?} cause={failure:?}");
        }
        Event::ContextCompacted {
            record: compaction, ..
        } => {
            let CompactionStatus::Failed { reason } = compaction.status else {
                return;
            };
            let _ = writeln!(record, "compaction reason={reason:?}");
        }
        _ => return,
    }
    record_local(record);
}

pub(crate) fn tap_resync(missed: u64) {
    if !cfg!(debug_assertions) {
        return;
    }
    let mut record = BoundedRecord(String::new());
    let _ = writeln!(record, "journal resync missed={missed}");
    record_local(record);
}

pub(crate) fn engine_failure(error: &crate::engine::EngineError) {
    use crate::engine::EngineError;
    if !cfg!(debug_assertions) {
        return;
    }
    let cause = match error {
        EngineError::RosterSelectionMismatch | EngineError::InvalidRequest => "InvalidRequest",
        EngineError::IncompatibleProviderChain => "IncompatibleProviderChain",
        EngineError::MissingSession => "MissingSession",
        EngineError::NoSessionForWorkspace => "NoSessionForWorkspace",
        EngineError::TooManySessions => "TooManySessions",
        EngineError::SessionBusy => "SessionBusy",
        EngineError::WorkspaceBusy => "WorkspaceBusy",
        EngineError::GoalUnavailable => "GoalUnavailable",
        EngineError::GoalAdmissionUncertain => "GoalAdmissionUncertain",
        EngineError::GoalBudgetExhausted => "GoalBudgetExhausted",
        EngineError::GoalNotContinuable => "GoalNotContinuable",
        EngineError::GoalIncludesUnavailable => "GoalIncludesUnavailable",
        EngineError::GoalInputTooLarge => "GoalInputTooLarge",
        EngineError::Busy => "Busy",
        EngineError::RewindBlocked => "RewindBlocked",
        EngineError::RewindPreviewExpired => "RewindPreviewExpired",
        EngineError::RewindTooLarge => "RewindTooLarge",
        EngineError::GuardUnavailable => "GuardUnavailable",
        EngineError::SessionNotIdle => "SessionNotIdle",
        EngineError::ForkNotAtRunBoundary => "ForkNotAtRunBoundary",
        EngineError::ConversationRewindLimit => "ConversationRewindLimit",
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
        EngineError::CompactionBlockedByHook => "CompactionBlockedByHook",
        EngineError::CompactionInputTooLarge => "CompactionInputTooLarge",
        EngineError::CompactionNotIdle => "CompactionNotIdle",
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

pub(crate) fn tool_failure(site: &'static str, error: &crate::tools::ToolError) {
    if !cfg!(debug_assertions) {
        return;
    }
    let mut record = BoundedRecord(String::new());
    let _ = writeln!(
        record,
        "{} stage={}{}{}",
        site.to_ascii_lowercase(),
        crate::tools::failure_stage(error),
        crate::tools::failure_check(error)
            .map(|check| format!(" check={check}"))
            .unwrap_or_default(),
        crate::tools::failure_detail(error)
            .map(|detail| format!(" detail={detail}"))
            .unwrap_or_default()
    );
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
    use super::test_process::BoundedOutput;
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    #[test]
    fn context_diagnostic_reports_sizes_without_conversation_values() {
        let mut input = crate::provider::RunInput::new(
            crate::provider::PromptRole::Primary,
            "private-objective-canary".into(),
        );
        input.includes.push("private-include-canary".repeat(100));
        input.tools = Some(crate::tools::ToolContext::new(serde_json::json!({
            "private-catalog-canary": "private-value-canary"
        })));
        input.history.push(crate::provider::HistoryTurn {
            user: "private-history-canary".into(),
            assistant: "private-answer-canary".into(),
        });
        let request = crate::provider::ProviderRequest {
            session_id: Default::default(),
            run_id: Default::default(),
            agent_run_id: Default::default(),
            phase: crate::provider::AgentPhase::RootPlan,
            collaboration: crate::session::CollaborationPolicy::Single,
            model: "gpt-6-luna".into(),
            max_output_tokens: 16_384,
            input,
        };
        for window in [None, Some(272_000)] {
            let record = context_record(&request, window, 16_896, 1234).0;
            assert!(record.contains("model=gpt-6-luna"));
            assert!(record.contains(&format!("window_tokens={window:?}")));
            assert!(record.contains("tool_allowance_bytes=16896"));
            assert!(record.contains("component=pending_turn bytes=1234"));
            assert!(record.contains("estimated_content_tokens="));
            assert!(
                record.find("component=includes").unwrap()
                    < record.find("component=history").unwrap()
            );
            assert!(!record.contains("canary") && !record.contains("private-"));
        }
    }

    #[test]
    fn diagnostic_file_is_bounded_private_and_rejects_aliases() {
        const CASE: &str = "ARANY_TEST_DIAGNOSTIC_PROCESS";
        const TEST: &str =
            "diagnostics::tests::diagnostic_file_is_bounded_private_and_rejects_aliases";
        if std::env::var_os(CASE).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST])
                .env_clear()
                .env(CASE, "isolated")
                .env(
                    "TMPDIR",
                    std::fs::canonicalize(std::env::temp_dir()).unwrap(),
                )
                .current_dir("/")
                .bounded_output_for(std::time::Duration::from_secs(30), 64 * 1024)
                .expect("bounded isolated diagnostic owner");
            assert!(output.status.success(), "isolated diagnostic owner failed");
            assert_eq!(output.stderr, b"");
            crate::test_process::assert_single_test_completed(&output.stdout, "diagnostic");
            return;
        }
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
        let host = crate::tools::ToolCall::GhPrCreate {
            repository: "github.com/example/project".into(),
            base: "main".into(),
            head: "topic".into(),
            title: CANARY.into(),
            body: CANARY.into(),
            draft: false,
        };
        write_failure(
            &root,
            SubscriptionFailureStage::OutcomeToolDigest,
            StreamCounts {
                tool_arguments: Some(ToolArgumentShape::from_call(&host)),
                ..StreamCounts::default()
            },
        );
        tool_failure("host", &crate::tools::ToolError::Operation);
        record_development_failure(DevelopmentFailure::ProviderAdmission);
        engine_failure(&crate::engine::EngineError::Store(
            crate::store::StoreError::Io(std::io::Error::other(CANARY)),
        ));
        tool_failure("Guard", &crate::tools::ToolError::Operation);
        event_failure(&crate::session::Event::ProviderCallRecorded {
            run_id: Default::default(),
            agent_run_id: Default::default(),
            record: crate::session::ProviderCallRecord {
                delegation: None,
                phase: crate::provider::AgentPhase::RootSynthesis,
                disposition: crate::session::ProviderCallDisposition::InvalidResponse,
                response_id: Some(CANARY.into()),
                input_tokens: None,
                output_tokens: None,
                wire_provenance: None,
                refusal: Some(crate::session::ProposalRefusal {
                    operation: "arany_write".into(),
                    cause: crate::session::ProposalRefusalCause::PlanInvalid,
                }),
                failure_reason: Some(crate::provider::ProviderFailureReason::ProposalRefused),
                requested_tools: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        });
        event_failure(&crate::session::Event::ToolFinished {
            checkpoint: None,
            run_id: Default::default(),
            agent_run_id: Default::default(),
            observation: crate::tools::ToolObservation {
                enforcement_cause: None,
                limit_cause: None,
                intent: crate::tools::EffectIntent {
                    host: None,
                    target: crate::session::worker::EffectTarget::Workspace,
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
                attestation: None,
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            },
        });
        event_failure(&crate::session::Event::RunFinished {
            run_id: Default::default(),
            disposition: crate::session::RunDisposition::Failed,
            failure: Some(crate::session::RunFailure::ProposalRefused),
            active_ms: 0,
        });
        for mut event in crate::session::worker_protocol_fixture().events {
            match &mut event {
                crate::session::Event::WorkerDataChunk { record, .. } => {
                    record.content = CANARY.into()
                }
                crate::session::Event::AgentSpawned {
                    assignment: Some(pins),
                    ..
                } => pins.request.direction = CANARY.into(),
                crate::session::Event::AgentFinished {
                    result: Some(result),
                    ..
                } => *result = CANARY.into(),
                _ => {}
            }
            event_failure(&event);
        }
        let failure_log = std::fs::read_to_string(&path).unwrap();
        for detail in [
            "runtime stage=ProviderAdmission",
            "engine cause=Store",
            "filesystem kind=Other",
            "guard stage=execution",
            r#"provider phase=RootSynthesis disposition=InvalidResponse reason=Some(ProposalRefused) refusal=Some(ProposalRefusal { operation: "arany_write", cause: PlanInvalid })"#,
            "tool operation=edit disposition=Uncertain receipt=false stops_run=true",
            "run disposition=Failed cause=Some(ProposalRefused)",
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
