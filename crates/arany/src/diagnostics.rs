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
        EngineError::InputPath { source, .. } => return engine_failure(source),
        EngineError::RosterSelectionMismatch | EngineError::InvalidRequest => "InvalidRequest",
        EngineError::IncompatibleProviderChain => "IncompatibleProviderChain",
        EngineError::MissingSession => "MissingSession",
        EngineError::NoSessionForWorkspace => "NoSessionForWorkspace",
        EngineError::TooManySessions => "TooManySessions",
        EngineError::SessionBusy => "SessionBusy",
        EngineError::WorkspaceBusy => "WorkspaceBusy",
        EngineError::GoalUnavailable => "GoalUnavailable",
        EngineError::GoalAdmissionUncertain => "GoalAdmissionUncertain",
        EngineError::GoalBudgetUncommitted { source, .. } => {
            if matches!(source.as_ref(), EngineError::GoalBudgetExhausted) {
                "GoalBudgetExhausted"
            } else {
                "GoalAdmissionUncertain"
            }
        }
        EngineError::GoalBudgetExhausted => "GoalBudgetExhausted",
        EngineError::GoalNotContinuable => "GoalNotContinuable",
        EngineError::GoalIncludesUnavailable => "GoalIncludesUnavailable",
        EngineError::GoalInputTooLarge => "GoalInputTooLarge",
        EngineError::Busy => "Busy",
        EngineError::RewindBlocked => "RewindBlocked",
        EngineError::RewindPreviewChanged => "RewindPreviewChanged",
        EngineError::RewindTooLarge => "RewindTooLarge",
        EngineError::GuardUnavailable => "GuardUnavailable",
        EngineError::SessionNotIdle => "SessionNotIdle",
        EngineError::ProviderSetupBlocked => "ProviderSetupBlocked",
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

pub(crate) fn record_local(record: BoundedRecord) {
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

#[derive(Default)]
pub(crate) struct BoundedRecord(String);

impl fmt::Display for BoundedRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

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
    use crate::provider::diagnostics::{
        StreamCounts, SubscriptionFailureStage, ToolArgumentShape, failure_record,
    };

    use super::test_process::BoundedOutput;
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

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
        let mut failure = BoundedRecord(String::new());
        writeln!(
            failure,
            "subscription stage=EndBeforeCompleted bytes=42 events=1 http_status=None"
        )
        .unwrap();
        write_record(&root, failure);
        let bytes = std::fs::read(&path).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("stage=EndBeforeCompleted bytes=42 events=1"));
        assert!(text.contains("Local failure call stack"));
        assert!(text.contains("diagnostics::write_record"));
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
        write_record(
            &root,
            failure_record(
                "subscription",
                SubscriptionFailureStage::OutcomeToolDigest,
                StreamCounts {
                    tool_arguments: Some(ToolArgumentShape::from_call(&call)),
                    ..StreamCounts::default()
                },
            ),
        );
        let shape_log = std::fs::read_to_string(&path).unwrap();
        assert!(
            shape_log.contains("operation: \"edit\"")
                && shape_log.contains("digest_valid: Some(false)")
        );
        assert!(shape_log.contains(&format!("old_bytes: Some({})", CANARY.len())));
        assert!(shape_log.contains(&format!("new_bytes: Some({})", CANARY.len())));
        assert!(!shape_log.contains(CANARY));
        let host = crate::tools::ToolCall::GhPrCreate {
            repository: "github.com/example/project".into(),
            base: "main".into(),
            head: "topic".into(),
            title: CANARY.into(),
            body: CANARY.into(),
            draft: false,
        };
        write_record(
            &root,
            failure_record(
                "subscription",
                SubscriptionFailureStage::OutcomeToolDigest,
                StreamCounts {
                    tool_arguments: Some(ToolArgumentShape::from_call(&host)),
                    ..StreamCounts::default()
                },
            ),
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
                rate_limits: None,
                delegation: None,
                phase: crate::provider::AgentPhase::RootSynthesis,
                disposition: crate::session::ProviderCallDisposition::Rejected,
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
                spill: None,
                enforcement_cause: None,
                limit_cause: None,
                intent: crate::tools::EffectIntent {
                    host: None,
                    host_command: None,
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
                    use_count: 1,
                },
                disposition: crate::tools::ToolDisposition::Uncertain,
                output: CANARY.into(),
                attestation: None,
                approval_cause: None,
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
            r#"provider phase=RootSynthesis disposition=Rejected reason=Some(ProposalRefused) refusal=Some(ProposalRefusal { operation: "arany_write", cause: PlanInvalid })"#,
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
