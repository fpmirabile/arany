use crate::{
    AgentPhase, Answer, CallLimits, ChatGptAdmission, ChatGptProvenance, CollaborationPolicy,
    CompactionRequest, CompactionResponse, Delegate, Effort, ImageAttachment, OutputTokenBound,
    Provenance, Provider, ProviderCallDisposition, ProviderError, ProviderIdentity,
    ProviderOutcome, ProviderRequest, ProviderResponse, RunCancellation, RunRequest,
};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// A scripted one-request Tool step; the id only has to be unique within its step.
pub fn tool_outcome(call: crate::ToolCall) -> crate::ProviderOutcome {
    crate::ProviderOutcome::Tools(vec![crate::ToolRequest {
        id: crate::ToolUseId::new("call_1").expect("scripted Tool id"),
        call,
    }])
}

pub fn plan_outcome(contract: crate::GoalPlanContract) -> crate::ProviderOutcome {
    crate::ProviderOutcome::Plan(Box::new(crate::PlanRequest {
        id: crate::ToolUseId::new("plan_1").expect("scripted plan id"),
        contract,
    }))
}

pub struct ScriptedProvider {
    pub identity: ProviderIdentity,
    pub preview: Option<crate::PreviewText>,
    pub preview_gate: Option<Arc<tokio::sync::Notify>>,
    pub responses: Mutex<VecDeque<Result<ProviderOutcome, ProviderError>>>,
    pub cancel_at_completion: Option<(AgentPhase, RunCancellation)>,
    pub cancel_at_compaction: Option<RunCancellation>,
    pub before_call: Vec<(usize, Box<dyn Fn() + Send + Sync>)>,
    pub clock_advance: Option<(usize, Duration)>,
    pub active_time_per_call: Option<Duration>,
    pub known_input_tokens: bool,
    pub cached_input_tokens: u32,
    pub usage_override: Option<crate::Usage>,
    pub context_window_tokens: Option<u32>,
    pub input_tokens_at: Vec<(usize, u32)>,
    pub requests: Arc<Mutex<Vec<ProviderRequest>>>,
    pub compact_responses: Mutex<VecDeque<Result<CompactionResponse, ProviderError>>>,
    pub compact_requests: Arc<Mutex<Vec<CompactionRequest>>>,
}

impl ScriptedProvider {
    pub fn new(responses: Vec<ProviderOutcome>) -> Self {
        Self {
            identity: ProviderIdentity {
                limits: CallLimits {
                    concurrency: 3,
                    ..CallLimits::default()
                },
                ..ProviderIdentity::new("scripted", "test-model", crate::Provenance::native())
            },
            preview: None,
            preview_gate: None,
            responses: Mutex::new(responses.into_iter().map(Ok).collect()),
            cancel_at_completion: None,
            cancel_at_compaction: None,
            before_call: Vec::new(),
            clock_advance: None,
            active_time_per_call: None,
            known_input_tokens: false,
            cached_input_tokens: 0,
            usage_override: None,
            context_window_tokens: None,
            input_tokens_at: Vec::new(),
            requests: Arc::new(Mutex::new(Vec::new())),
            compact_responses: Mutex::new(VecDeque::new()),
            compact_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Runs `hook` when the zero-based provider call `index` is invoked, before its response is returned.
    pub fn with_before_call(
        mut self,
        index: usize,
        hook: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        self.before_call.push((index, Box::new(hook)));
        self
    }

    /// Pauses Tokio time and advances it by `duration` while the zero-based provider call `index` is still in flight.
    pub fn with_clock_advance_before_call(mut self, index: usize, duration: Duration) -> Self {
        self.clock_advance = Some((index, duration));
        self
    }

    pub fn with_active_time_per_call(mut self, duration: Duration) -> Self {
        self.active_time_per_call = Some(duration);
        self
    }

    /// Reports known input usage (zero tokens) so a finite Goal token budget counts every call.
    pub fn with_reported_usage(mut self, usage: crate::Usage) -> Self {
        self.usage_override = Some(usage);
        self
    }

    /// Reports the input window an authenticated catalog would have supplied.
    pub fn with_context_window(mut self, tokens: u32) -> Self {
        self.context_window_tokens = Some(tokens);
        self
    }

    /// Reports `count` uncached input tokens for the zero-based provider call `index` only.
    pub fn with_input_tokens_at(mut self, index: usize, count: u32) -> Self {
        self.input_tokens_at.push((index, count));
        self
    }

    pub fn with_cached_input_tokens(mut self, count: u32) -> Self {
        self.known_input_tokens = true;
        self.cached_input_tokens = count;
        self
    }

    pub fn with_known_input_tokens(mut self) -> Self {
        self.known_input_tokens = true;
        self
    }

    pub fn with_compactions(
        mut self,
        responses: Vec<Result<CompactionResponse, ProviderError>>,
    ) -> Self {
        self.compact_responses = Mutex::new(responses.into());
        self
    }

    pub fn with_profile(mut self, profile: &str, model: &str) -> Self {
        self.identity.profile = profile.into();
        self.identity.model = model.into();
        self
    }

    pub fn with_effort(mut self, effort: Effort) -> Self {
        self.identity.effort = Some(effort);
        self
    }

    pub fn with_max_output_tokens(mut self, cap: u32) -> Self {
        self.identity.limits.max_output_tokens = cap;
        self
    }

    pub fn with_saved_api_account(mut self, id: Uuid) -> Self {
        self.identity.provenance = Provenance::Native {
            saved_api_account_id: Some(id),
        };
        self
    }

    pub fn with_chatgpt_account(mut self, id: Uuid, fingerprint: [u8; 32]) -> Self {
        self.identity.profile = "chatgpt".into();
        self.identity.model = "model-a".into();
        self.identity.effort = Some(Effort::High);
        self.identity.provenance = Provenance::ChatGpt(ChatGptProvenance {
            account_id: id,
            evidence_fingerprint: fingerprint,
            admission: ChatGptAdmission::AccountConsent,
        });
        self.identity.limits.output_token_bound = OutputTokenBound::LocalAcceptanceOnly;
        self.identity.limits.concurrency = 1;
        self
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    fn context_window_tokens(&self) -> Option<u32> {
        self.context_window_tokens
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let phase = request.phase;
        let index = {
            let mut requests = self.requests.lock().expect("requests lock");
            requests.push(request);
            requests.len() - 1
        };
        for (_, hook) in self
            .before_call
            .iter()
            .filter(|(target, _)| *target == index)
        {
            hook();
        }
        if let Some((target, duration)) = &self.clock_advance
            && *target == index
        {
            tokio::time::pause();
            tokio::time::advance(*duration).await;
        }
        if let Some(duration) = self.active_time_per_call {
            tokio::time::pause();
            tokio::time::advance(duration).await;
            tokio::time::resume();
        }
        let outcome = self
            .responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .expect("scripted response");
        if let Some((target, cancellation)) = &self.cancel_at_completion
            && phase == *target
        {
            assert!(cancellation.cancel(), "first completion-time cancellation");
        }
        let input = self.known_input_tokens.then_some(self.cached_input_tokens);
        let reported = self
            .input_tokens_at
            .iter()
            .find(|(target, _)| *target == index)
            .map(|(_, count)| crate::Usage {
                input_tokens: Some(*count),
                output_tokens: Some(8),
                ..Default::default()
            });
        Ok(ProviderResponse {
            turn: crate::ProviderTurn::scripted(),
            outcome: outcome?,
            response_id: None,
            usage: reported.or(self.usage_override).unwrap_or(crate::Usage {
                input_tokens: input,
                cache_read_input_tokens: input,
                output_tokens: Some(8),
                ..Default::default()
            }),
            wire_provenance: None,
        })
    }

    async fn invoke_with_preview(
        &self,
        request: ProviderRequest,
        preview: crate::ResponsePreview,
    ) -> Result<ProviderResponse, ProviderError> {
        if let Some(text) = &self.preview {
            preview.publish(text.clone())?;
            if let Some(gate) = &self.preview_gate {
                gate.notified().await;
            }
        }
        self.invoke(request).await
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        self.compact_requests
            .lock()
            .expect("compaction requests lock")
            .push(request);
        let response = self
            .compact_responses
            .lock()
            .expect("compaction responses lock")
            .pop_front()
            .expect("scripted compaction response");
        if let Some(cancellation) = &self.cancel_at_compaction {
            assert!(cancellation.cancel(), "first compaction-time cancellation");
        }
        response
    }
}

pub fn gated_identity(concurrency: u8) -> ProviderIdentity {
    ProviderIdentity {
        limits: CallLimits {
            concurrency,
            ..CallLimits::default()
        },
        ..ProviderIdentity::new("gated", "test-model", crate::Provenance::native())
    }
}

pub fn waiting_identity(deadline: Duration) -> ProviderIdentity {
    ProviderIdentity {
        limits: CallLimits {
            deadline,
            ..CallLimits::default()
        },
        ..ProviderIdentity::new("waiting", "test-model", crate::Provenance::native())
    }
}

pub struct GatedProvider {
    pub started: mpsc::UnboundedSender<(String, oneshot::Sender<()>)>,
    pub requests: Arc<Mutex<Vec<ProviderRequest>>>,
    pub fail_child: Option<&'static str>,
    pub identity: ProviderIdentity,
}

#[async_trait::async_trait]
impl Provider for GatedProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        let outcome = match request.phase {
            AgentPhase::ToolReview => return Err(ProviderError::InvalidOutcome),
            AgentPhase::RootPlan => ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![
                    crate::ChildAssignment {
                        editor: None,
                        objective: "A".into(),
                        profile: None,
                    },
                    crate::ChildAssignment {
                        editor: None,
                        objective: "B".into(),
                        profile: None,
                    },
                    crate::ChildAssignment {
                        editor: None,
                        objective: "C".into(),
                        profile: None,
                    },
                ],
            }),
            AgentPhase::ChildWork => {
                let (release, gate) = oneshot::channel();
                self.started
                    .send((request.input.objective.clone(), release))
                    .map_err(|_| ProviderError::Unavailable)?;
                gate.await.map_err(|_| ProviderError::Unavailable)?;
                if self.fail_child == Some(request.input.objective.as_str()) {
                    return Err(ProviderError::RemoteHttp(403));
                }
                ProviderOutcome::Answer(Answer {
                    text: format!("result:{}", request.input.objective),
                })
            }
            AgentPhase::RootSynthesis => ProviderOutcome::Answer(Answer {
                text: "A B C".into(),
            }),
        };
        Ok(ProviderResponse {
            turn: crate::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: crate::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
}

pub struct DropCounter(pub Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

pub struct WaitingProvider {
    pub started: mpsc::UnboundedSender<()>,
    pub dropped: Arc<AtomicUsize>,
    pub requests: Arc<Mutex<Vec<ProviderRequest>>>,
    pub identity: ProviderIdentity,
}

#[async_trait::async_trait]
impl Provider for WaitingProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().expect("requests").push(request);
        let _drop_counter = DropCounter(Arc::clone(&self.dropped));
        self.started
            .send(())
            .map_err(|_| ProviderError::Unavailable)?;
        std::future::pending().await
    }
}

pub fn request(workspace: &Path, objective: &str) -> RunRequest {
    RunRequest {
        skill: None,
        session_id: None,
        title: Some("Research".into()),
        objective: objective.into(),
        images: Vec::new(),
        workspace: workspace.to_path_buf(),
        launch_directory: None,
        include_paths: vec!["notes/facts.txt".into()],
        policy: CollaborationPolicy::Single,
        goal: crate::GoalRequest::default(),
    }
}

/// Context bytes the Engine reserves for an unplanned Goal's metadata: pinned budget, maximum facts, plan allowance and framing.
pub fn unplanned_goal_metadata_bytes() -> u32 {
    let facts = crate::GoalContextFacts {
        provider_calls: u32::MAX,
        known_tokens: u64::MAX,
        token_usage_unknown: true,
        active_ms: u64::MAX,
        reserved_active_ms: u64::MAX,
        uncertain_active_ms: u64::MAX,
        success_criterion_satisfied: Some(true),
    };
    let encoded = serde_json::to_vec(&(
        crate::GoalBudget::default(),
        None::<crate::GoalPlanContract>,
        facts,
    ))
    .expect("Goal metadata");
    u32::try_from(encoded.len() + 8 * 1024 + 64).expect("bounded Goal metadata")
}

pub fn include_data(path: &str, text: &str) -> String {
    let digest: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "File: {}\nSHA-256: {digest}\nContent (untrusted data):\n{text}",
        serde_json::to_string(path).unwrap()
    )
}

pub fn image_fixture(bytes: usize) -> ImageAttachment {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let mut png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC").unwrap();
    if bytes > png.len() {
        let data_len = bytes
            .checked_sub(png.len() + 12)
            .expect("PNG padding length");
        let mut chunk = (data_len as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(b"arNy");
        chunk.resize(8 + data_len, 0);
        let mut crc = !0_u32;
        for &byte in &chunk[4..] {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
            }
        }
        chunk.extend_from_slice(&(!crc).to_be_bytes());
        png.splice(png.len() - 12..png.len() - 12, chunk);
    }
    assert_eq!(png.len(), bytes);
    ImageAttachment::from_png(&png).expect("synthetic PNG container")
}

pub fn assert_test_call(
    actual: &ProviderRequest,
    phase: AgentPhase,
    objective: &str,
    includes: &[&str],
    history: &[crate::HistoryTurn],
    summary: Option<&str>,
    children: &[crate::ChildResult],
) {
    assert_eq!(actual.phase, phase, "call phase");
    assert!(actual.model == "test-model", "call model");
    assert!(actual.input.objective == objective, "call objective");
    assert!(actual.input.images.is_empty(), "call images");
    assert!(
        actual.input.workspace_guidance.is_empty(),
        "call instructions"
    );
    assert!(
        actual.input.includes
            == includes
                .iter()
                .map(|text| include_data("notes/facts.txt", text))
                .collect::<Vec<_>>(),
        "call includes"
    );
    assert!(actual.input.history == history, "call history");
    assert!(
        actual.input.context_summary.as_deref() == summary,
        "call summary"
    );
    assert!(actual.input.child_results == children, "call child results");
    assert_eq!(actual.max_output_tokens, 4096, "call output cap");
}

pub struct PreviewProvider {
    pub text: crate::PreviewText,
    pub completion: Option<ProviderOutcome>,
}

#[async_trait::async_trait]
impl Provider for PreviewProvider {
    fn identity(&self) -> &crate::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<crate::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                crate::ProviderIdentity::new("preview", "test-model", crate::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, _: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        panic!("attached primary must use the preview invocation")
    }
    async fn invoke_with_preview(
        &self,
        _: ProviderRequest,
        preview: crate::ResponsePreview,
    ) -> Result<ProviderResponse, ProviderError> {
        preview.publish(self.text.clone())?;
        match &self.completion {
            Some(outcome) => Ok(ProviderResponse {
                turn: crate::ProviderTurn::scripted(),
                outcome: outcome.clone(),
                response_id: None,
                usage: crate::Usage {
                    input_tokens: None,
                    output_tokens: Some(8),
                    ..Default::default()
                },
                wire_provenance: None,
            }),
            None => std::future::pending().await,
        }
    }
}

pub type CompletionBoundaryCase = (
    &'static str,
    Result<ProviderOutcome, ProviderError>,
    crate::ProviderCallDisposition,
    Option<crate::ProviderFailureReason>,
);

pub fn completion_boundary_cases() -> [CompletionBoundaryCase; 12] {
    [
        (
            "rejected call",
            Err(ProviderError::RemoteHttp(403)),
            ProviderCallDisposition::Rejected,
            Some(crate::ProviderFailureReason::AccountAccess),
        ),
        (
            // A Retry-After past the action deadline is never waited out, so the 429 settles at once.
            "rate limit with a hostile Retry-After",
            Err(ProviderError::RateLimited(Some(Duration::from_secs(
                u64::from(u32::MAX),
            )))),
            ProviderCallDisposition::Rejected,
            Some(crate::ProviderFailureReason::RateLimited),
        ),
        (
            "quota exhausted",
            Err(ProviderError::QuotaExceeded),
            ProviderCallDisposition::Rejected,
            Some(crate::ProviderFailureReason::UsageLimit),
        ),
        (
            "service unavailable",
            Err(ProviderError::RemoteHttp(503)),
            ProviderCallDisposition::Unavailable,
            Some(crate::ProviderFailureReason::ServiceUnavailable),
        ),
        (
            "usage temporarily unavailable",
            Err(ProviderError::RemoteStreamCode(
                "subscription_sharing_usage_unavailable".into(),
            )),
            ProviderCallDisposition::Unavailable,
            Some(crate::ProviderFailureReason::UsageTemporarilyUnavailable),
        ),
        (
            "unknown stream code",
            Err(ProviderError::RemoteStreamCode(
                "OMITTED_ERROR_CANARY".into(),
            )),
            ProviderCallDisposition::Rejected,
            None,
        ),
        (
            "malformed stream",
            Err(ProviderError::InvalidStream),
            ProviderCallDisposition::InvalidResponse,
            Some(crate::ProviderFailureReason::StreamProtocol),
        ),
        (
            "response contract",
            Err(ProviderError::InvalidResponseContract),
            ProviderCallDisposition::InvalidResponse,
            Some(crate::ProviderFailureReason::ResponseContract),
        ),
        (
            "outcome contract",
            Err(ProviderError::InvalidOutcomeContract),
            ProviderCallDisposition::InvalidResponse,
            Some(crate::ProviderFailureReason::OutcomeContract),
        ),
        (
            "local output limit",
            Err(ProviderError::LocalOutputLimit),
            ProviderCallDisposition::InvalidResponse,
            Some(crate::ProviderFailureReason::LocalOutputLimit),
        ),
        (
            "invalid answer",
            Ok(ProviderOutcome::Answer(Answer { text: "".into() })),
            ProviderCallDisposition::InvalidResponse,
            Some(crate::ProviderFailureReason::ProposalRefused),
        ),
        (
            "accepted answer",
            Ok(ProviderOutcome::Answer(Answer {
                text: "Accepted result".into(),
            })),
            ProviderCallDisposition::Finished,
            None,
        ),
    ]
}

#[cfg(unix)]
pub fn offline_cargo_fixture(home: &Path, workspace: &Path) {
    const REGISTRY: &str = "index.crates.io-1949cf8c6b5b557f";
    use std::os::unix::fs::PermissionsExt;
    let archive = include_bytes!("../../tests/fixtures/offline-cargo/dependency.crate");
    let checksum = crate::tools::types::hex_digest(archive);
    let registry = home.join(".cargo/registry");
    let package = "arany-fixture-dependency-0.1.0";
    let source = registry.join("src").join(REGISTRY).join(package);
    let files: [(&str, &[u8]); 4] = [
        (
            "Cargo.toml",
            include_bytes!("../../tests/fixtures/offline-cargo/Cargo.toml"),
        ),
        (
            "src/lib.rs",
            include_bytes!("../../tests/fixtures/offline-cargo/src/lib.rs"),
        ),
        (
            "build.rs",
            include_bytes!("../../tests/fixtures/offline-cargo/build.rs"),
        ),
        (
            "native.c",
            include_bytes!("../../tests/fixtures/offline-cargo/native.c"),
        ),
    ];
    for (path, bytes) in files {
        let path = source.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    std::fs::write(source.join(".cargo-ok"), b"{\"v\":1}").unwrap();
    let cache = registry.join("cache").join(REGISTRY);
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join(format!("{package}.crate")), archive).unwrap();
    let index = registry.join("index").join(REGISTRY);
    std::fs::create_dir_all(index.join(".cache/ar/an")).unwrap();
    std::fs::write(
        index.join("config.json"),
        br#"{"dl":"https://static.crates.io/crates","api":"https://crates.io"}"#,
    )
    .unwrap();
    let entry = serde_json::json!({
        "name": "arany-fixture-dependency", "vers": "0.1.0", "deps": [],
        "cksum": checksum, "features": {}, "yanked": false,
    });
    let mut bytes = b"\x03\x02\0\0\0etag: fixture\0\x30.1.0\0".to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(&entry).unwrap());
    bytes.push(0);
    std::fs::write(index.join(".cache/ar/an/arany-fixture-dependency"), bytes).unwrap();
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        r#"[package]
name = "offline-cargo-fixture"
version = "0.1.0"
edition = "2021"
[dependencies]
arany-fixture-dependency = "=0.1.0"
[lints.rust]
unexpected_cfgs = { level = "deny", check-cfg = ['cfg(arany_guard)'] }
"#,
    )
    .unwrap();
    std::fs::write(
        workspace.join("Cargo.lock"),
        format!(
            r#"version = 4
[[package]]
name = "arany-fixture-dependency"
version = "0.1.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{checksum}"
[[package]]
name = "offline-cargo-fixture"
version = "0.1.0"
dependencies = ["arany-fixture-dependency"]
"#
        ),
    )
    .unwrap();
    std::fs::write(
        workspace.join("src/lib.rs"),
        include_bytes!("../../tests/fixtures/offline-cargo/tests.rs"),
    )
    .unwrap();
    std::fs::write(
        workspace.join("rust-toolchain.toml"),
        b"[toolchain]\nchannel = \"unselected-repository-channel\"\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".cargo/credentials.toml"),
        b"UNSELECTED_CARGO_CREDENTIAL_CANARY",
    )
    .unwrap();
    let wrapper = home.join("unselected-wrapper");
    std::fs::write(
        &wrapper,
        br#"#!/bin/sh
printf 'UNSELECTED_CARGO_CREDENTIAL_CANARY\n' >&2
exec "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
pub async fn guarded_offline_cargo_journey(root: &Path, workspace: &Path, guard: &Path) {
    use crate::{
        ApprovalMode, CacheUpdate, Engine, EngineConfig, GoalPlanContract, GoalSuccessCriterion,
        RunControls, RunStatus, SessionView, StateRoot, Store, ToolApprovals, ToolCall, ToolGrant,
        ToolResourceClass, WorkspacePermissions,
    };
    let home = root.join("home");
    let registry = home.join(".cargo/registry");
    let index = registry
        .join("index/index.crates.io-1949cf8c6b5b557f/.cache/ar/an/arany-fixture-dependency");
    let source =
        registry.join("src/index.crates.io-1949cf8c6b5b557f/arany-fixture-dependency-0.1.0");
    let index_bytes = std::fs::read(&index).unwrap();
    let source_bytes = std::fs::read(source.join("src/lib.rs")).unwrap();
    let original_lock = std::fs::read(workspace.join("Cargo.lock")).unwrap();
    let mut permissions = WorkspacePermissions::open(workspace)
        .unwrap()
        .prepare_profile_selection();
    let choices = permissions.runtime_profile_choices();
    let mut tokens: Vec<_> = choices
        .iter()
        .filter(|choice| choice.selected)
        .map(|choice| choice.token.clone())
        .collect();
    for group in [
        "Cargo archive cache",
        "Cargo source cache",
        "Cargo index cache",
    ] {
        let choice = choices
            .iter()
            .find(|choice| choice.group == group)
            .expect("private registry candidate");
        assert!(!choice.selected);
        tokens.push(choice.token.clone());
    }
    permissions
        .remember_profiles(true, ApprovalMode::Request, &tokens)
        .unwrap();
    let state = root.join("cargo-state");
    for (case, script, success) in [
        (
            "unmarked",
            "CARGO_ENCODED_RUSTFLAGS='' cargo test --locked --lib --no-run",
            true,
        ),
        (
            "guarded",
            r#"set -eu
test "$CARGO_NET_OFFLINE" = true
test "$CARGO_BUILD_JOBS" = 1
if (printf changed > "$(command -v cargo)") 2>/dev/null; then exit 94; fi
test ! -e "$CARGO_HOME/credentials.toml"
test "$0" = "offline build 'entry"
test "$1" = "argument with ' quote"
if test -n "${SDKROOT-}"; then
    test -r /Library/Developer/CommandLineTools/usr/bin/llvm-nm
    if /Library/Developer/CommandLineTools/usr/bin/llvm-nm --version; then exit 93; else test "$?" -eq 1; fi
fi
for part in cache src index; do
    if (printf changed > "$CARGO_HOME/registry/$part/write-marker") 2>/dev/null; then exit 92; fi
done
cargo test --locked --lib
printf retained > "$CARGO_TARGET_DIR/prior-marker"
"#,
            true,
        ),
        ("warm", "cargo test --locked --lib --verbose", true),
        (
            "failed-test",
            r#"printf discarded > "$CARGO_TARGET_DIR/prior-marker"; ARANY_FIXTURE_FAIL=1 cargo test --locked --lib"#,
            false,
        ),
        (
            "forced-ipc",
            "cargo test --locked --lib -- --ignored",
            false,
        ),
        ("missing-index", "cargo test --locked --lib", false),
        ("missing-source", "cargo test --locked --lib", false),
        ("hostile-config", "cargo test --locked --lib", false),
        ("cancelled", "cargo test --locked --lib", false),
        (
            "retained",
            r#"test "$(cat "$CARGO_TARGET_DIR/prior-marker")" = retained && cargo test --locked --lib"#,
            true,
        ),
    ] {
        match case {
            "missing-index" => std::fs::remove_file(&index).unwrap(),
            "missing-source" => std::fs::rename(&source, source.with_extension("held")).unwrap(),
            "hostile-config" => {
                std::fs::create_dir_all(workspace.join(".cargo")).unwrap();
                std::fs::write(
                    workspace.join(".cargo/config.toml"),
                    format!(
                        "[build]\nrustc-wrapper = {:?}\n",
                        home.join("unselected-wrapper").to_str().unwrap()
                    ),
                )
                .unwrap();
            }
            _ => {}
        }
        let call = ToolCall::Command {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                script.into(),
                "offline build 'entry".into(),
                "argument with ' quote".into(),
            ],
            cwd: String::new(),
        };
        let provider = Arc::new(ScriptedProvider::new(vec![
            plan_outcome(GoalPlanContract {
                mission: "Run offline registry-backed tests under selected Rust".into(),
                paths: Vec::new(),
                commands: vec!["sh".into()],
                resource_class: ToolResourceClass::Build,
                command_timeout_ms: Some(60_000),
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: GoalSuccessCriterion::ToolCallSucceeded {
                    call: Box::new(call.clone()),
                },
                budget: crate::GoalBudget::default(),
            }),
            tool_outcome(call.clone()),
            ProviderOutcome::Answer(Answer {
                text: "Observed the guarded Cargo result.".into(),
            }),
        ]));
        let engine = Engine::open(
            StateRoot::admit(&state).unwrap(),
            EngineConfig {
                guard_executable: Some(guard.to_path_buf()),
                ..Default::default()
            },
        )
        .unwrap();
        let (approvals, mut inbox) = ToolApprovals::new(ApprovalMode::Request);
        let cancellation = RunCancellation::new();
        let mut request = request(workspace, "Run the offline Cargo fixture");
        request.include_paths.clear();
        request.goal.phase = Some(crate::GoalPhase::Implementation);
        let run = super::engine::run_with(
            &engine,
            &provider,
            request,
            RunControls {
                tools: ToolGrant::Trusted(WorkspacePermissions::open(workspace).unwrap()),
                approvals: Some(approvals),
                cancellation: cancellation.clone(),
                ..Default::default()
            },
        );
        let approve = async {
            let approval = inbox.next().await.expect("one exact command approval");
            assert_eq!(approval.intent.call, call);
            assert_eq!(
                approval.intent.limits.resource_class,
                ToolResourceClass::Build
            );
            if case == "cancelled" {
                cancellation.cancel();
            }
            approval.decide(case != "cancelled");
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(100), async {
            tokio::join!(run, approve)
        })
        .await
        .expect("bounded Cargo journey");
        let outcome = outcome.unwrap();
        engine.close().await.unwrap();
        let observation = outcome.run.tools[0]
            .observation
            .as_ref()
            .expect("Cargo observation");
        assert_eq!(outcome.run.tools.len(), 1);
        assert!(
            !observation
                .output
                .contains("UNSELECTED_CARGO_CREDENTIAL_CANARY"),
            "{case}: unselected data reached command output"
        );
        if case == "cancelled" {
            assert_eq!(outcome.run.status, RunStatus::Cancelled);
            assert!(observation.attestation.is_none());
        } else {
            let result: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
            assert_eq!(result["exit_code"] == 0, success, "{case}: {result}");
            assert_eq!(
                outcome.run.status,
                if success {
                    RunStatus::Finished
                } else {
                    RunStatus::Failed
                }
            );
            let receipt = observation
                .attestation
                .as_ref()
                .and_then(crate::Attestation::guard)
                .expect("native Cargo receipt");
            assert_eq!(receipt.limits, observation.intent.limits);
            assert_eq!(
                receipt.enforcement_digest,
                observation.intent.enforcement_digest
            );
            assert_eq!(
                observation.cache_update,
                if success {
                    CacheUpdate::Published
                } else {
                    CacheUpdate::NotAttempted
                }
            );
            if matches!(case, "guarded" | "warm" | "retained") {
                let stdout = result["stdout"].as_str().unwrap();
                assert!(
                    stdout.contains("1 passed; 0 failed; 1 ignored"),
                    "{case}: {result}"
                );
                assert!(stdout.contains("local_test_ipc ... ignored, requires local test IPC"));
            }
            if case == "warm" {
                assert!(
                    result["stderr"]
                        .as_str()
                        .unwrap()
                        .contains("Fresh offline-cargo-fixture"),
                    "{result}"
                );
            }
            if case == "guarded" && cfg!(target_os = "macos") {
                assert!(result["stderr"].as_str().unwrap().contains(
                    "/Library/Developer/CommandLineTools/usr/bin/llvm-nm: Operation not permitted"
                ));
            }
            if case == "forced-ipc" {
                let stdout = result["stdout"].as_str().unwrap();
                assert!(stdout.contains("local_test_ipc ... FAILED"), "{result}");
                assert!(stdout.contains("PermissionDenied"), "{result}");
            }
            if matches!(case, "failed-test" | "forced-ipc") {
                assert!(
                    result["stdout"]
                        .as_str()
                        .unwrap()
                        .contains("0 passed; 1 failed; 1 ignored")
                        || case == "forced-ipc"
                );
                assert_eq!(result["exit_code"], 101);
            }
        }
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
        let replay = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        assert_eq!(replay.runs.as_slice(), std::slice::from_ref(&outcome.run));
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), if case == "cancelled" { 2 } else { 3 });
        for text in [
            events
                .iter()
                .map(|event| event.event.payload().unwrap())
                .collect::<String>(),
            format!("{requests:?}"),
        ] {
            assert!(!text.contains("UNSELECTED_CARGO_CREDENTIAL_CANARY"));
        }
        drop(requests);
        match case {
            "missing-index" => std::fs::write(&index, &index_bytes).unwrap(),
            "missing-source" => std::fs::rename(source.with_extension("held"), &source).unwrap(),
            "hostile-config" => std::fs::remove_file(workspace.join(".cargo/config.toml")).unwrap(),
            _ => {}
        }
    }
    assert_eq!(std::fs::read(index).unwrap(), index_bytes);
    assert_eq!(
        std::fs::read(source.join("src/lib.rs")).unwrap(),
        source_bytes
    );
    assert_eq!(
        std::fs::read(workspace.join("Cargo.lock")).unwrap(),
        original_lock
    );
    for part in ["cache", "src", "index"] {
        assert!(!registry.join(part).join("write-marker").exists());
    }
}
