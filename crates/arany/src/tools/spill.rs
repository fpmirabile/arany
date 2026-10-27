use super::{ToolCall, ToolError, ToolObservation};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

pub(crate) const READ_PREFIX: &str = "spill:";

pub(crate) fn read_reference(path: &str) -> Option<&str> {
    path.strip_prefix(READ_PREFIX)
        .filter(|reference| crate::validate::valid_digest(reference))
}

pub(crate) const MAX_SPILL_RESULT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_SPILL_RUN_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const MAX_REPORT_BYTES: usize =
    super::types::MAX_TOOL_OBSERVATION_BYTES + MAX_SPILL_RESULT_BYTES.div_ceil(3) * 4 + 128;
pub(crate) const SPILL_PERSIST_TIMEOUT_MS: u64 = crate::session::STORE_ACTION_OVERHEAD_MS;
pub(crate) const MAX_SPILL_WRITERS: usize = 1;
static WRITERS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| {
        std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_SPILL_WRITERS))
    });

pub(super) struct Writer {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(super) result: tokio::sync::oneshot::Receiver<Result<(), crate::StoreError>>,
}

impl Writer {
    pub(super) fn start(
        access: std::sync::Arc<crate::store::SpillAccess>,
        reference: String,
        bytes: Vec<u8>,
        mut gate: crate::fault::SpillWriteGate,
    ) -> Option<Self> {
        let permit = std::sync::Arc::clone(&WRITERS).try_acquire_owned().ok()?;
        if !access.reserve(bytes.len()).ok()? {
            return None;
        }
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = std::sync::Arc::clone(&cancelled);
        let release = std::sync::Arc::clone(&access);
        let reserved_bytes = bytes.len();
        let (send, result) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("arany-spill".into())
            .spawn(move || {
                let result = access.write(&reference, &bytes, &stop, &mut gate);
                drop(bytes);
                drop(access);
                drop(permit);
                let _ = send.send(result);
                drop(gate);
            })
            .map_err(|_| release.release(reserved_bytes))
            .ok()?;
        Some(Self { cancelled, result })
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

const UTF8_EDGE_BYTES: usize = char::MAX.len_utf8() - 1;
pub(crate) const CUT_MARKER: &[u8] = b"\n[spill truncated]\n";

pub(super) fn report_cap(intent: &super::EffectIntent) -> usize {
    if intent.target == crate::session::worker::EffectTarget::Workspace
        && matches!(
            intent.call,
            ToolCall::Command { .. } | ToolCall::McpCall { .. } | ToolCall::McpList { .. }
        )
    {
        MAX_REPORT_BYTES
    } else {
        super::types::MAX_TOOL_OBSERVATION_BYTES
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpill {
    pub r#ref: String,
    pub bytes: u64,
    pub truncated: bool,
    #[serde(skip)]
    pub(crate) captured: Option<Vec<u8>>,
}

impl OutputSpill {
    pub(super) fn valid(&self, observation: &ToolObservation) -> bool {
        self.r#ref == crate::validate::hex_digest(&observation.intent.digest())
            && retained_total(observation, self.captured.is_none()).is_some_and(|total| {
                if total > MAX_SPILL_RESULT_BYTES as u64 {
                    self.truncated
                        && ((MAX_SPILL_RESULT_BYTES - UTF8_EDGE_BYTES) as u64
                            ..=MAX_SPILL_RESULT_BYTES as u64)
                            .contains(&self.bytes)
                } else {
                    !self.truncated && self.bytes == total
                }
            })
            && self
                .captured
                .as_ref()
                .is_none_or(|bytes| bytes.len() as u64 == self.bytes)
            && observation.attestation.is_some()
            && matches!(
                observation.intent.call,
                ToolCall::Command { .. }
                    | ToolCall::CommandUnguarded { .. }
                    | ToolCall::McpCall { .. }
                    | ToolCall::McpList { .. }
            )
            && observation.intent.target == crate::session::worker::EffectTarget::Workspace
    }

    pub(super) fn captured(
        observation: &ToolObservation,
        captured: Option<Vec<u8>>,
    ) -> Option<Self> {
        if !cfg!(target_os = "macos") && !has_excerpt(observation) {
            return None;
        }
        Self::pending(observation, captured)
    }

    pub(super) fn pending(
        observation: &ToolObservation,
        captured: Option<Vec<u8>>,
    ) -> Option<Self> {
        let mut captured = captured?;
        let total = retained_total(observation, false)?;
        if captured.len() as u64 != total.min(MAX_SPILL_RESULT_BYTES as u64) {
            return None;
        }
        let truncated = total > MAX_SPILL_RESULT_BYTES as u64;
        captured.truncate(MAX_SPILL_RESULT_BYTES);
        if truncated {
            captured.truncate(MAX_SPILL_RESULT_BYTES - CUT_MARKER.len());
            if let Err(error) = std::str::from_utf8(&captured)
                && error.error_len().is_none()
            {
                captured.truncate(error.valid_up_to());
            }
            captured.extend_from_slice(CUT_MARKER);
        }
        Some(Self {
            r#ref: crate::validate::hex_digest(&observation.intent.digest()),
            bytes: captured.len() as u64,
            truncated,
            captured: Some(captured),
        })
    }
}

fn retained_total(observation: &ToolObservation, require_excerpt: bool) -> Option<u64> {
    if matches!(
        observation.intent.call,
        ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. }
    ) {
        let result: super::types::CommandResult = serde_json::from_str(&observation.output).ok()?;
        (!require_excerpt
            || super::output::excerpted(result.stdout_total, &result.stdout)
            || super::output::excerpted(result.stderr_total, &result.stderr))
        .then(|| {
            result
                .stdout_total
                .bytes
                .saturating_add(result.stderr_total.bytes)
        })
        .filter(|total| *total > 0)
    } else {
        let result: serde_json::Value = serde_json::from_str(&observation.output).ok()?;
        result["output_total"]["bytes"]
            .as_u64()
            .filter(|total| *total > 0)
    }
}

pub(super) fn has_excerpt(observation: &ToolObservation) -> bool {
    retained_total(observation, true).is_some()
}

pub(super) fn streams(stdout: &super::output::Window, stderr: &super::output::Window) -> Vec<u8> {
    let mut bytes = stdout.captured.clone();
    bytes.extend_from_slice(
        &stderr.captured[..stderr
            .captured
            .len()
            .min(MAX_SPILL_RESULT_BYTES.saturating_sub(bytes.len()))],
    );
    bytes
}

pub(super) fn encode_report(observation: &ToolObservation) -> Result<Vec<u8>, ToolError> {
    let mut report = serde_json::to_value(observation).map_err(|_| ToolError::Operation)?;
    if let Some(bytes) = observation
        .spill
        .as_ref()
        .and_then(|spill| spill.captured.as_ref())
    {
        report["capture"] = STANDARD.encode(bytes).into();
    }
    serde_json::to_vec(&report).map_err(|_| ToolError::Operation)
}

pub(super) fn decode_report(bytes: &[u8]) -> Result<ToolObservation, ToolError> {
    let mut report: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| ToolError::Operation)?;
    let captured = report
        .as_object_mut()
        .ok_or(ToolError::Operation)?
        .remove("capture");
    let mut observation: ToolObservation =
        serde_json::from_value(report).map_err(|_| ToolError::Operation)?;
    if let Some(captured) = captured {
        let captured = STANDARD
            .decode(captured.as_str().ok_or(ToolError::Operation)?)
            .map_err(|_| ToolError::Operation)?;
        let spill = observation.spill.as_mut().ok_or(ToolError::Operation)?;
        if captured.len() as u64 != spill.bytes || captured.len() > MAX_SPILL_RESULT_BYTES {
            return Err(ToolError::Operation);
        }
        spill.captured = Some(captured);
    }
    Ok(observation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Attestation, CacheUpdate, GuardReceipt, ToolDisposition};

    fn command_observation(bytes: &[u8]) -> ToolObservation {
        let temp = tempfile::tempdir().unwrap();
        let runtime = super::super::tests::in_process_runtime(
            temp.path(),
            serde_json::json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
        );
        let intent = runtime.intent(
            crate::RunId::new(),
            crate::AgentRunId::new(),
            ToolCall::Command {
                program: "shell".into(),
                args: vec![],
                cwd: "".into(),
            },
        );
        let mut stdout = super::super::output::Window::capturing(4096);
        stdout
            .push(bytes, super::super::output::DRAIN_BYTES)
            .unwrap();
        let stderr = super::super::output::Window::default();
        let output = super::super::process::fit_command(
            Some(0),
            &stdout,
            &stderr,
            Default::default(),
            4096,
            super::super::process::Truncation::Allow,
        )
        .unwrap();
        let mut observation = ToolObservation {
            spill: None,
            attestation: Some(Attestation::Guard(GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            })),
            intent,
            disposition: ToolDisposition::Succeeded,
            enforcement_cause: None,
            limit_cause: None,
            output,
            approval_cause: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        observation.spill = OutputSpill::captured(&observation, Some(streams(&stdout, &stderr)));
        observation
    }

    #[test]
    fn spill_capture_caps_raw_bytes_and_never_serializes_content_into_observations() {
        for size in [4096, MAX_SPILL_RESULT_BYTES, MAX_SPILL_RESULT_BYTES + 1] {
            let observation = command_observation(&vec![b'!'; size]);
            let spill = observation.spill.as_ref().unwrap();
            assert!(observation.valid());
            let mut forged = observation.clone();
            forged.spill.as_mut().unwrap().bytes += 1;
            assert!(!forged.valid());
            forged = observation.clone();
            forged.spill.as_mut().unwrap().truncated = !spill.truncated;
            assert!(!forged.valid());
            forged = observation.clone();
            forged.spill.as_mut().unwrap().r#ref = "f".repeat(64);
            assert!(!forged.valid());
            assert_eq!(spill.bytes, size.min(MAX_SPILL_RESULT_BYTES) as u64);
            assert_eq!(spill.truncated, size > MAX_SPILL_RESULT_BYTES);
            if spill.truncated {
                assert!(spill.captured.as_ref().unwrap().ends_with(CUT_MARKER));
            }
            let canonical = serde_json::to_vec(&observation).unwrap();
            let replay: ToolObservation = serde_json::from_slice(&canonical).unwrap();
            assert!(replay.spill.unwrap().captured.is_none());
            let wire = encode_report(&observation).unwrap();
            assert!(wire.len() <= MAX_REPORT_BYTES);
            assert_eq!(decode_report(&wire).unwrap(), observation);
        }
    }

    #[test]
    fn spill_cut_keeps_a_complete_utf8_prefix_before_the_marker() {
        let bytes = "🙂".repeat(MAX_SPILL_RESULT_BYTES / "🙂".len() + 1);
        let observation = command_observation(bytes.as_bytes());
        assert!(observation.valid());
        let spill = observation.spill.unwrap();
        assert!(spill.truncated);
        assert!(std::str::from_utf8(spill.captured.as_ref().unwrap()).is_ok());
        assert!(spill.bytes <= MAX_SPILL_RESULT_BYTES as u64);
    }

    #[test]
    fn spill_content_never_becomes_changeset_or_hook_input() {
        let raw = [
            vec![b'!'; 8192],
            b"\nPRIVATE_SPILL_HOOK_CANARY\n".to_vec(),
            vec![b'!'; 8192],
        ]
        .concat();
        let observation = command_observation(&raw);
        assert!(observation.spill.is_some());
        let model = super::super::types::model_observation(&observation);
        let input = super::super::permissions::post_tool_use_input(
            "session",
            super::super::permissions::PermissionMode::Default,
            &observation.intent.call,
            observation.intent.id,
            model,
            super::super::permissions::MAX_HOOK_INPUT_BYTES,
        )
        .unwrap();
        assert!(!input.contains("PRIVATE_SPILL_HOOK_CANARY"));
        let result: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(result["changeset"]["files"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn spill_write_failure_keeps_the_completed_effect_and_inline_excerpt() {
        let temp = tempfile::tempdir().unwrap();
        let state = crate::StateRoot::admit(&temp.path().join("state")).unwrap();
        std::fs::write(state.path().join("spill"), b"blocked").unwrap();
        let mut runtime = super::super::tests::in_process_runtime(
            temp.path(),
            serde_json::json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
        );
        let mut observation = command_observation(&vec![b'!'; 8192]);
        let inline = observation.output.clone();
        runtime.admit_spills(&state, observation.intent.run_id, &[]);
        runtime
            .persist_spill(&mut observation, super::super::Cancellation::new())
            .await;
        assert!(observation.spill.is_none());
        assert_eq!(observation.output, inline);
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.attestation.is_some());
    }

    #[tokio::test]
    async fn spill_read_grammar_rejects_traversal_and_unknown_references_stay_attested() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = super::super::tests::in_process_runtime(
            temp.path(),
            serde_json::json!({"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}),
        );
        for path in [
            "spill:",
            "spill:../secret",
            "spill:/secret",
            "spill:ABC",
            &format!("spill:{}/extra", "a".repeat(64)),
        ] {
            assert!(!super::super::fs::valid(&ToolCall::Read {
                path: path.into(),
                offset: None,
                limit: None
            }));
        }
        let intent = runtime.intent(
            crate::RunId::new(),
            crate::AgentRunId::new(),
            ToolCall::Read {
                path: format!("spill:{}", "a".repeat(64)),
                offset: None,
                limit: None,
            },
        );
        let observation = runtime
            .execute(intent, super::super::Cancellation::new())
            .await;
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert_eq!(observation.output, "no spill for this Run");
        assert!(matches!(
            observation.attestation,
            Some(Attestation::InProcess { .. })
        ));
    }
}
