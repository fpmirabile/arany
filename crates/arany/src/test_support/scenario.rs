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
    pub responses: Mutex<VecDeque<Result<ProviderOutcome, ProviderError>>>,
    pub cancel_at_completion: Option<(AgentPhase, RunCancellation)>,
    pub cancel_at_compaction: Option<RunCancellation>,
    pub before_call: Option<(usize, Box<dyn Fn() + Send + Sync>)>,
    pub clock_advance: Option<(usize, Duration)>,
    pub known_input_tokens: bool,
    pub cached_input_tokens: u32,
    pub usage_override: Option<crate::Usage>,
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
            responses: Mutex::new(responses.into_iter().map(Ok).collect()),
            cancel_at_completion: None,
            cancel_at_compaction: None,
            before_call: None,
            clock_advance: None,
            known_input_tokens: false,
            cached_input_tokens: 0,
            usage_override: None,
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
        self.before_call = Some((index, Box::new(hook)));
        self
    }

    /// Pauses Tokio time and advances it by `duration` while the zero-based provider call `index` is still in flight.
    pub fn with_clock_advance_before_call(mut self, index: usize, duration: Duration) -> Self {
        self.clock_advance = Some((index, duration));
        self
    }

    /// Reports known input usage (zero tokens) so a finite Goal token budget counts every call.
    pub fn with_reported_usage(mut self, usage: crate::Usage) -> Self {
        self.usage_override = Some(usage);
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

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let phase = request.phase;
        let index = {
            let mut requests = self.requests.lock().expect("requests lock");
            requests.push(request);
            requests.len() - 1
        };
        if let Some((target, hook)) = &self.before_call
            && *target == index
        {
            hook();
        }
        if let Some((target, duration)) = &self.clock_advance
            && *target == index
        {
            tokio::time::pause();
            tokio::time::advance(*duration).await;
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
        Ok(ProviderResponse {
            turn: crate::ProviderTurn::scripted(),
            outcome: outcome?,
            response_id: None,
            usage: self.usage_override.unwrap_or(crate::Usage {
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
        session_id: None,
        title: Some("Research".into()),
        objective: objective.into(),
        images: Vec::new(),
        workspace: workspace.to_path_buf(),
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
        actual.input.workspace_guidance.is_none(),
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

pub fn completion_boundary_cases() -> [CompletionBoundaryCase; 11] {
    [
        (
            "rejected call",
            Err(ProviderError::RemoteHttp(403)),
            ProviderCallDisposition::Rejected,
            Some(crate::ProviderFailureReason::AccountAccess),
        ),
        (
            "usage limit",
            Err(ProviderError::RemoteHttp(429)),
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
