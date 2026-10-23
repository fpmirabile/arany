use super::{ENDPOINT, wire};
use crate::diagnostics::{
    ResponseShape, StreamCounts, SubscriptionFailureStage as Stage, subscription_failure,
};
use crate::provider::{RateLimitSlot, ResponsePreview};
mod limits;
use crate::provider::conversation::Conversation;
use crate::provider::streaming::{FramingStage, ResponsesPreview, SseEvent, SseFramer};
use crate::provider::{
    AgentPhase, CallLimits, ChatGptProvenance, CompactionItem, CompactionRequest,
    CompactionResponse, Effort, HistoryTurn, MAX_REPORTED_INPUT_TOKENS, MAX_REPORTED_OUTPUT_TOKENS,
    MAX_RESPONSE_BYTES, OutputTokenBound, PROBE_OUTPUT_CAP, PROBE_TOTAL_DEADLINE, PromptRole,
    Provenance, Provider, ProviderError, ProviderIdentity, ProviderOutcome, ProviderRequest,
    ProviderResponse, RunInput, Usage, catalog::valid_model_id, render,
};
use crate::session::{AgentRunId, MAX_COMPACTION_OUTPUT_TOKENS, RunId, SessionId};
#[cfg(test)]
use reqwest::redirect::Policy;
use reqwest::{Client, Url, header};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::{Instant, timeout, timeout_at};

const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;
const MAX_EVENTS: usize = 8192;
// Codex's `stream_idle_timeout_ms` default; each received event restarts it.
const STREAM_IDLE: Duration = Duration::from_secs(300);
const CALL_CEILING: Duration = Duration::from_secs(600);
// The route sends no cap, so the Engine's summary byte bound decides compaction acceptance.
const COMPACTION_VISIBLE_CAP: u32 = crate::provider::DEFAULT_OUTPUT_TOKEN_CAP;
const COMPACTION_TARGET_BYTES: usize = crate::session::MAX_COMPACTION_SUMMARY_BYTES * 3 / 4;

pub struct ChatGptProvider {
    client: Client,
    identity: ProviderIdentity,
    effort: Effort,
    access_token: String,
    context_window_tokens: Option<u32>,
}

impl ChatGptProvider {
    /// The caller must revalidate the selected account and current consent before Workspace input.
    /// A model probe is not required for account-consent admission.
    pub fn from_checked_account(
        model: String,
        effort: Effort,
        access_token: String,
        provenance: ChatGptProvenance,
    ) -> Result<Self, ProviderError> {
        if !valid_model_id(&model)
            || !provenance.valid()
            || access_token.is_empty()
            || access_token.len() > MAX_ACCESS_TOKEN_BYTES
            || !access_token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(ProviderError::Rejected);
        }
        let mut identity = ProviderIdentity::new("chatgpt", model, Provenance::ChatGpt(provenance));
        identity.effort = Some(effort);
        identity.limits = CallLimits {
            output_token_bound: OutputTokenBound::LocalAcceptanceOnly,
            deadline: CALL_CEILING,
            ..CallLimits::default()
        };
        Ok(Self {
            client: client()?,
            identity,
            effort,
            access_token,
            context_window_tokens: None,
        })
    }

    /// Pins the window the account's checked catalog reported; out-of-range values stay absent.
    #[must_use]
    pub fn with_context_window(mut self, tokens: Option<u32>) -> Self {
        self.context_window_tokens =
            tokens.and_then(|tokens| crate::provider::admitted_context_window(u64::from(tokens)));
        self
    }
}

#[async_trait::async_trait]
impl Provider for ChatGptProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    fn context_window_tokens(&self) -> Option<u32> {
        self.context_window_tokens
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.invoke_observed(request, None, RateLimitSlot::default())
            .await
    }

    async fn invoke_with_preview(
        &self,
        request: ProviderRequest,
        preview: ResponsePreview,
    ) -> Result<ProviderResponse, ProviderError> {
        self.invoke_observed(request, Some(preview), RateLimitSlot::default())
            .await
    }

    async fn invoke_observed(
        &self,
        request: ProviderRequest,
        preview: Option<ResponsePreview>,
        limits: RateLimitSlot,
    ) -> Result<ProviderResponse, ProviderError> {
        request.validate_scope()?;
        if request.model != self.identity.model {
            return Err(ProviderError::Rejected);
        }
        invoke_at_observed(
            &self.client,
            Url::parse(ENDPOINT).expect("compiled Responses endpoint"),
            &self.access_token,
            &request,
            self.effort,
            crate::provider::request_ceiling(self.context_window_tokens),
            (preview, limits),
        )
        .await
    }

    async fn compact_observed(
        &self,
        request: CompactionRequest,
        limits: RateLimitSlot,
    ) -> Result<CompactionResponse, ProviderError> {
        if request.model != self.identity.model {
            return Err(ProviderError::Rejected);
        }
        compact_at_observed(
            &self.client,
            Url::parse(ENDPOINT).expect("compiled Responses endpoint"),
            &self.access_token,
            &request,
            self.effort,
            limits,
        )
        .await
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        self.compact_observed(request, RateLimitSlot::default())
            .await
    }
}

pub(super) async fn probe_model(
    access_token: &str,
    model: &str,
    effort: Effort,
) -> Result<(), ProviderError> {
    let client = client()?;
    let endpoint = Url::parse(ENDPOINT).expect("compiled Responses endpoint");
    timeout(
        PROBE_TOTAL_DEADLINE,
        probe_model_at(&client, endpoint, access_token, model, effort),
    )
    .await
    .map_err(|_| ProviderError::Unavailable)?
}

async fn probe_model_at(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    model: &str,
    effort: Effort,
) -> Result<(), ProviderError> {
    if !valid_model_id(model)
        || access_token.is_empty()
        || access_token.len() > MAX_ACCESS_TOKEN_BYTES
        || !access_token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ProviderError::Rejected);
    }
    let session_id = SessionId::new();
    let direct = ProviderRequest {
        session_id,
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Single,
        model: model.to_owned(),
        max_output_tokens: PROBE_OUTPUT_CAP,
        input: RunInput::new(PromptRole::ReadOnly, "Synthetic conformance check: finish directly with a short factual sentence. No Workspace content is supplied.".into()),
    };
    let response = invoke_at(client, endpoint.clone(), access_token, &direct, effort).await?;
    if !matches!(&response.outcome, ProviderOutcome::Answer(value) if !value.text.trim().is_empty() && value.text.len() <= 8 * 1024)
    {
        return Err(ProviderError::InvalidOutcome);
    }
    let direct_id = response.response_id.ok_or(ProviderError::InvalidOutcome)?;

    let delegate = ProviderRequest {
        collaboration: crate::session::CollaborationPolicy::Team { max_active_children: 1 },
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        input: RunInput::new(PromptRole::ReadOnly, "Synthetic conformance check: delegate exactly one independent read-only question about the number two. No Workspace content is supplied.".into()),
        ..direct
    };
    let response = invoke_at(client, endpoint.clone(), access_token, &delegate, effort).await?;
    if !matches!(&response.outcome, ProviderOutcome::Delegate(value) if value.children.len() == 1 && !value.children[0].objective.trim().is_empty() && value.children[0].profile.is_none())
    {
        return Err(ProviderError::InvalidOutcome);
    }
    let delegate_id = response.response_id.ok_or(ProviderError::InvalidOutcome)?;
    if delegate_id == direct_id {
        return Err(ProviderError::InvalidOutcome);
    }

    let compact = CompactionRequest {
        session_id,
        covered_run_id: RunId::new(),
        model: model.to_owned(),
        previous_summary: None,
        items: vec![CompactionItem::Completed(HistoryTurn {
            user: "Synthetic conformance question".into(),
            assistant: "Synthetic conformance answer".into(),
        })],
        max_output_tokens: PROBE_OUTPUT_CAP,
    };
    let response = compact_at(client, endpoint, access_token, &compact, effort).await?;
    if response.summary.trim().is_empty()
        || response.summary.len() > 8 * 1024
        || !response
            .response_id
            .as_deref()
            .is_some_and(|id| id != direct_id && id != delegate_id)
    {
        return Err(ProviderError::InvalidOutcome);
    }
    Ok(())
}

fn client() -> Result<Client, ProviderError> {
    super::super::http_client(CALL_CEILING)
        .https_only(true)
        .build()
        .map_err(|_| ProviderError::Unavailable)
}

async fn invoke_at(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    request: &ProviderRequest,
    effort: Effort,
) -> Result<ProviderResponse, ProviderError> {
    invoke_at_with_preview(
        client,
        endpoint,
        access_token,
        request,
        effort,
        crate::provider::MAX_PROVIDER_REQUEST_BYTES,
        None,
    )
    .await
}

async fn invoke_at_with_preview(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    request: &ProviderRequest,
    effort: Effort,
    max_request_bytes: usize,
    preview: Option<ResponsePreview>,
) -> Result<ProviderResponse, ProviderError> {
    invoke_at_observed(
        client,
        endpoint,
        access_token,
        request,
        effort,
        max_request_bytes,
        (preview, RateLimitSlot::default()),
    )
    .await
}

async fn invoke_at_observed(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    request: &ProviderRequest,
    effort: Effort,
    max_request_bytes: usize,
    observations: (Option<ResponsePreview>, RateLimitSlot),
) -> Result<ProviderResponse, ProviderError> {
    let (preview, limits) = observations;
    if !(1..=crate::provider::DEFAULT_OUTPUT_TOKEN_CAP).contains(&request.max_output_tokens) {
        return Err(ProviderError::Rejected);
    }
    let conversation = render::conversation(request)?;
    let body = subscription_body(request, &conversation, effort, max_request_bytes)?;
    let CompletedStream {
        body: completed,
        counts,
        reasoning_tokens,
    } = completed_at_with_preview(
        client,
        endpoint,
        access_token,
        body,
        preview.map(|preview| (preview, request.model.as_str())),
        limits,
    )
    .await?;
    let decoded = wire::decode_streamed_run(
        &completed,
        &wire::RunScope::new(request, &conversation, access_token),
    );
    let result = decoded.map_err(|error| {
        if cfg!(debug_assertions) && error == wire::ResponseError::Outcome {
            let (stage, shape) = wire::outcome_failure_diagnostic(&completed, &request.model);
            return failure(
                stage,
                error.provider_error(),
                StreamCounts {
                    tool_arguments: shape,
                    ..counts
                },
            );
        }
        response_failure(error, counts)
    })?;
    accept_result(
        result.reflects_secret(access_token),
        result.usage,
        reasoning_tokens,
        request.max_output_tokens,
        counts,
    )?;
    Ok(result)
}

async fn compact_at(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    request: &CompactionRequest,
    effort: Effort,
) -> Result<CompactionResponse, ProviderError> {
    compact_at_observed(
        client,
        endpoint,
        access_token,
        request,
        effort,
        RateLimitSlot::default(),
    )
    .await
}

async fn compact_at_observed(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    request: &CompactionRequest,
    effort: Effort,
    limits: RateLimitSlot,
) -> Result<CompactionResponse, ProviderError> {
    if !(1..=MAX_COMPACTION_OUTPUT_TOKENS).contains(&request.max_output_tokens) {
        return Err(ProviderError::Rejected);
    }
    let body = subscription_compaction_body(request, effort)?;
    let CompletedStream {
        body: completed,
        counts,
        reasoning_tokens,
    } = completed_at_with_preview(client, endpoint, access_token, body, None, limits).await?;
    let result = wire::decode_streamed_compaction(&completed, &request.model)
        .map_err(|error| response_failure(error, counts))?;
    accept_result(
        result.reflects_secret(access_token),
        result.usage,
        reasoning_tokens,
        COMPACTION_VISIBLE_CAP,
        counts,
    )?;
    Ok(result)
}

// The cap bounds visible output; the reported total also counts reasoning and is kept for charging.
fn accept_result(
    reflects_secret: bool,
    usage: Usage,
    reasoning_tokens: u32,
    max_output_tokens: u32,
    counts: StreamCounts,
) -> Result<(), ProviderError> {
    if reflects_secret {
        return Err(failure(
            Stage::CredentialReflection,
            ProviderError::InvalidOutcome,
            counts,
        ));
    }
    if usage
        .input_tokens
        .is_none_or(|count| count == 0 || count > MAX_REPORTED_INPUT_TOKENS)
        || usage.output_tokens.is_none_or(|count| {
            count == 0 || count > MAX_REPORTED_OUTPUT_TOKENS || count < reasoning_tokens
        })
        || !usage.cache_consistent()
    {
        return Err(failure(
            Stage::UsageContract,
            ProviderError::InvalidResponseContract,
            counts,
        ));
    }
    if usage
        .output_tokens
        .is_some_and(|count| count.saturating_sub(reasoning_tokens) > max_output_tokens)
    {
        return Err(failure(
            Stage::LocalOutputLimit,
            ProviderError::LocalOutputLimit,
            counts,
        ));
    }
    Ok(())
}

struct CompletedStream {
    body: Vec<u8>,
    counts: StreamCounts,
    reasoning_tokens: u32,
}

async fn completed_at_with_preview(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    body: Vec<u8>,
    preview: Option<(ResponsePreview, &str)>,
    limits: RateLimitSlot,
) -> Result<CompletedStream, ProviderError> {
    if access_token.is_empty()
        || access_token.len() > MAX_ACCESS_TOKEN_BYTES
        || !access_token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ProviderError::Rejected);
    }
    let send = client
        .post(endpoint.clone())
        .bearer_auth(access_token)
        .header(header::ACCEPT, "text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .body(body)
        .send();
    let mut response = match timeout(STREAM_IDLE, send).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return Err(failure(
                if error.is_timeout() {
                    Stage::Deadline
                } else {
                    Stage::RequestTransport
                },
                crate::provider::send_failure(&error),
                StreamCounts::default(),
            ));
        }
        // An unanswered request is a send timeout, which never retries.
        Err(_) => {
            return Err(failure(
                Stage::Deadline,
                ProviderError::Unavailable,
                StreamCounts::default(),
            ));
        }
    };
    let counts = StreamCounts {
        http_status: Some(response.status().as_u16()),
        ..StreamCounts::default()
    };
    if response.url() != &endpoint {
        return Err(failure(
            Stage::Destination,
            ProviderError::InvalidStream,
            counts,
        ));
    }
    limits::headers(response.headers(), access_token, &limits)?;
    let status = response.status();
    if status != reqwest::StatusCode::OK {
        let error = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            crate::provider::rate_limit_failure(&mut response).await
        } else if status.is_server_error() {
            ProviderError::RemoteServerError(
                status.as_u16(),
                crate::provider::server_retry_after(&response),
            )
        } else {
            ProviderError::RemoteHttp(status.as_u16())
        };
        return Err(failure(Stage::HttpStatus, error, counts));
    }
    if response
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(failure(
            Stage::ContentEncoding,
            ProviderError::InvalidStream,
            counts,
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(failure(
            Stage::ContentLength,
            ProviderError::InvalidStream,
            counts,
        ));
    }
    let mut decoder = match preview {
        Some((preview, model)) => StreamDecoder::with_preview(access_token, model, Some(preview)),
        None => StreamDecoder::new(access_token),
    };
    decoder.limits = limits;
    let body = read_events(&mut decoder, &mut response).await?;
    Ok(CompletedStream {
        body,
        counts: decoder.counts(),
        reasoning_tokens: reasoning_tokens(decoder.output_tokens_details.as_ref())
            .map_err(|error| failure(Stage::UsageContract, error, decoder.counts()))?,
    })
}

// Only a framed event restarts the window, so comments or a trickled partial frame cannot keep a
// stalled stream alive; the Engine's per-call deadline stays the absolute ceiling.
async fn read_events(
    decoder: &mut StreamDecoder<'_>,
    source: &mut impl ChunkSource,
) -> Result<Vec<u8>, ProviderError> {
    let mut idle_until = Instant::now() + STREAM_IDLE;
    loop {
        let chunk = match timeout_at(idle_until, source.next_chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => {
                return Err(failure(
                    Stage::EndBeforeCompleted,
                    decoder.framer.end_error(),
                    decoder.counts(),
                ));
            }
            Ok(Err(stage)) => {
                return Err(failure(
                    stage,
                    ProviderError::PartialTransport,
                    decoder.counts(),
                ));
            }
            Err(_) => {
                return Err(failure(
                    Stage::Deadline,
                    ProviderError::PartialTransport,
                    decoder.counts(),
                ));
            }
        };
        let events = decoder.framer.events;
        if let Some(completed) = decoder
            .feed(&chunk)
            .map_err(|error| failure(decoder.stage, error, decoder.counts()))?
        {
            return Ok(completed);
        }
        if decoder.framer.events > events {
            idle_until = Instant::now() + STREAM_IDLE;
        }
    }
}

// The response body, or a scripted one with paused time in tests.
trait ChunkSource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, Stage>;
}

impl ChunkSource for reqwest::Response {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, Stage> {
        match self.chunk().await {
            Ok(chunk) => Ok(chunk.map(|chunk| chunk.to_vec())),
            Err(error) if error.is_timeout() => Err(Stage::Deadline),
            Err(_) => Err(Stage::ReadTransport),
        }
    }
}

fn failure(stage: Stage, error: ProviderError, counts: StreamCounts) -> ProviderError {
    subscription_failure(stage, counts);
    error
}

pub(super) async fn read_completed_response(
    response: &mut reqwest::Response,
    key: &str,
    model: &str,
    preview: Option<ResponsePreview>,
) -> Result<Vec<u8>, ProviderError> {
    crate::provider::streaming::validate_stream_response(response)?;
    let mut decoder = StreamDecoder::with_preview(key, model, preview);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProviderError::PartialTransport)?
    {
        if let Some(completed) = decoder.feed(&chunk)? {
            return Ok(completed);
        }
    }
    Err(decoder.framer.end_error())
}

fn response_failure(error: wire::ResponseError, counts: StreamCounts) -> ProviderError {
    use wire::ResponseError;
    let stage = match error {
        ResponseError::Envelope => Stage::ResponseEnvelope,
        ResponseError::Model => Stage::ResponseModel,
        ResponseError::Status => Stage::ResponseStatus,
        ResponseError::Identifier => Stage::ResponseIdentifier,
        ResponseError::Message => Stage::ResponseMessage,
        ResponseError::Content => Stage::ResponseContent,
        ResponseError::Phase => Stage::ResponsePhase,
        ResponseError::MissingFinalMessage => Stage::ResponseMissingFinalMessage,
        ResponseError::DuplicateFinalMessage => Stage::ResponseDuplicateFinalMessage,
        ResponseError::AmbiguousFinalMessage => Stage::ResponseAmbiguousFinalMessage,
        ResponseError::LateCommentary => Stage::ResponseLateCommentary,
        ResponseError::Outcome => Stage::OutcomeContract,
    };
    failure(stage, error.provider_error(), counts)
}

fn subscription_body(
    request: &ProviderRequest,
    conversation: &Conversation,
    effort: Effort,
    max_request_bytes: usize,
) -> Result<Vec<u8>, ProviderError> {
    super::super::image::validate_images(request)?;
    let mut body = wire::run_body_with_effort(request, conversation, effort)?;
    if request.input.role != PromptRole::ToolReview && body["tool_choice"] != "none" {
        let tools = conversation.tools.as_deref().unwrap_or_default();
        let allowed = wire::allowed_functions(request, tools, &conversation.outcome);
        if let Some(functions) = body.get_mut("tools").and_then(Value::as_array_mut) {
            functions.retain(|function| {
                function["name"]
                    .as_str()
                    .is_some_and(|name| allowed.iter().any(|allowed| allowed == name))
            });
            if functions.is_empty() {
                let fields = body.as_object_mut().ok_or(ProviderError::Rejected)?;
                for key in ["tools", "tool_choice", "parallel_tool_calls"] {
                    fields.remove(key);
                }
            } else {
                body["tool_choice"] = json!("auto");
            }
        }
    }
    // The ChatGPT route is stateless, so replayed items drop their server ids and keep their content.
    for item in body["input"].as_array_mut().into_iter().flatten() {
        if matches!(item["type"].as_str(), Some("reasoning" | "function_call"))
            && let Some(fields) = item.as_object_mut()
        {
            fields.remove("id");
        }
    }
    streaming_body(body, max_request_bytes)
}

fn subscription_compaction_body(
    request: &CompactionRequest,
    effort: Effort,
) -> Result<Vec<u8>, ProviderError> {
    let mut body = wire::compaction_body_with_effort(request, effort);
    body["instructions"] = json!(format!(
        "{} Keep the summary under {COMPACTION_TARGET_BYTES} UTF-8 bytes; a non-ASCII character takes up to 4 bytes.",
        crate::provider::prompts::COMPACTION_INSTRUCTIONS
    ));
    streaming_body(body, crate::provider::MAX_PROVIDER_REQUEST_BYTES)
}

fn streaming_body(mut body: Value, max_request_bytes: usize) -> Result<Vec<u8>, ProviderError> {
    let object = body.as_object_mut().ok_or(ProviderError::Rejected)?;
    let input = object.remove("input").ok_or(ProviderError::Rejected)?;
    object.remove("max_output_tokens");
    object.remove("truncation");
    let input = match input {
        Value::String(_) => json!([{"role": "user", "content": input}]),
        Value::Array(_) => input,
        _ => return Err(ProviderError::Rejected),
    };
    object.insert("input".into(), input);
    object.insert("stream".into(), Value::Bool(true));
    wire::encode_request_within(&body, max_request_bytes)
}

struct StreamDecoder<'a> {
    limits: RateLimitSlot,
    token: &'a str,
    framer: SseFramer,
    preview: Option<ResponsesPreview<'a>>,
    stage: Stage,
    response_shape: ResponseShape,
    response_id: Option<String>,
    done_items: Vec<(wire::WireItem, Value)>,
    // Kept raw: only the ChatGPT completion path interprets it, so shared streams stay unchanged.
    output_tokens_details: Option<Value>,
}

#[derive(Deserialize)]
struct CreatedEvent {
    response: Option<CreatedResponse>,
}

#[derive(Deserialize)]
struct CreatedResponse {
    id: String,
}

#[derive(Deserialize)]
struct DoneEvent {
    output_index: usize,
    item: wire::WireItem,
}

impl<'a> StreamDecoder<'a> {
    fn new(token: &'a str) -> Self {
        Self {
            token,
            framer: SseFramer::new(),
            limits: RateLimitSlot::default(),
            preview: None,
            stage: Stage::EventFraming,
            response_shape: ResponseShape::default(),
            response_id: None,
            done_items: Vec::new(),
            output_tokens_details: None,
        }
    }

    fn with_preview(token: &'a str, model: &'a str, preview: Option<ResponsePreview>) -> Self {
        let mut decoder = Self::new(token);
        decoder.preview = preview.map(|preview| ResponsesPreview::new(token, model, preview));
        decoder
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Option<Vec<u8>>, ProviderError> {
        self.framer.admit(chunk.len()).inspect_err(|_| {
            self.stage = Stage::StreamLimit;
        })?;
        for &byte in chunk {
            let event = self.framer.byte(byte, self.token).inspect_err(|_| {
                self.stage = match self.framer.stage {
                    FramingStage::Limit => Stage::StreamLimit,
                    FramingStage::Framing => Stage::EventFraming,
                    FramingStage::Json => Stage::EventJson,
                    FramingStage::Credential => Stage::CredentialReflection,
                    FramingStage::Type => Stage::EventType,
                };
            })?;
            if let Some(event) = event
                && let Some(completed) = self.finish_event(event)?
            {
                return Ok(Some(completed));
            }
        }
        Ok(None)
    }

    fn finish_event(&mut self, event: SseEvent) -> Result<Option<Vec<u8>>, ProviderError> {
        let SseEvent { value, data } = event;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ProviderError::InvalidStream)?;
        if let Some(preview) = &mut self.preview {
            preview.event(&value).inspect_err(|_| {
                self.stage = Stage::OutcomeContract;
            })?;
        }
        match kind {
            "codex.rate_limits" => {
                limits::event(&value, &self.limits);
                Ok(None)
            }
            "response.created" => {
                self.stage = Stage::CompletedResponse;
                let created: CreatedEvent = serde_json::from_slice(&data)
                    .map_err(|_| ProviderError::InvalidResponseContract)?;
                if let Some(response) = created.response {
                    if self.response_id.is_some()
                        || !self.done_items.is_empty()
                        || response.id.is_empty()
                        || response.id.len() > 128
                        || !response.id.bytes().all(|byte| byte.is_ascii_graphic())
                    {
                        return Err(ProviderError::InvalidResponseContract);
                    }
                    self.response_id = Some(response.id);
                }
                Ok(None)
            }
            "response.completed" => {
                self.stage = Stage::CompletedResponse;
                if value.get("response").is_none() {
                    return Err(ProviderError::InvalidResponseContract);
                }
                self.output_tokens_details = value
                    .pointer("/response/usage/output_tokens_details")
                    .cloned();
                if cfg!(debug_assertions)
                    && let Some(output) =
                        value.pointer("/response/output").and_then(Value::as_array)
                {
                    for item in output {
                        if item.get("type").and_then(Value::as_str) != Some("message") {
                            continue;
                        }
                        self.response_shape.messages += 1;
                        match item.get("phase").and_then(Value::as_str) {
                            Some("commentary") => self.response_shape.commentary += 1,
                            Some("final_answer") => self.response_shape.finals += 1,
                            None => self.response_shape.unphased += 1,
                            _ => {}
                        }
                    }
                }
                if !self.done_items.is_empty() {
                    let id = self
                        .response_id
                        .as_deref()
                        .ok_or(ProviderError::InvalidResponseContract)?;
                    let completed = wire::reconcile_streamed_output(
                        &data,
                        id,
                        std::mem::take(&mut self.done_items),
                    )
                    .map_err(|error| {
                        self.stage = match error {
                            wire::ResponseError::Identifier => Stage::ResponseIdentifier,
                            wire::ResponseError::Message => Stage::ResponseMessage,
                            _ => Stage::ResponseEnvelope,
                        };
                        error.provider_error()
                    })?;
                    self.stage = Stage::StreamLimit;
                    if completed.len() > MAX_RESPONSE_BYTES {
                        return Err(ProviderError::InvalidStream);
                    }
                    Ok(Some(completed))
                } else {
                    if self.response_id.as_deref().is_some_and(|id| {
                        value.pointer("/response/id").and_then(Value::as_str) != Some(id)
                    }) {
                        self.stage = Stage::ResponseIdentifier;
                        return Err(ProviderError::InvalidResponseContract);
                    }
                    Ok(Some(data))
                }
            }
            "response.output_item.done" => {
                if cfg!(debug_assertions)
                    && value.pointer("/item/type").and_then(Value::as_str) == Some("message")
                {
                    self.response_shape.done_messages += 1;
                    if value.pointer("/item/phase").and_then(Value::as_str) == Some("final_answer")
                    {
                        self.response_shape.done_finals += 1;
                    }
                }
                self.stage = Stage::CompletedResponse;
                let done: DoneEvent = serde_json::from_slice(&data)
                    .map_err(|_| ProviderError::InvalidResponseContract)?;
                if self.response_id.is_none()
                    || done.output_index != self.done_items.len()
                    || done.output_index >= MAX_EVENTS
                    || done.item.completed_id().is_none()
                    || self
                        .done_items
                        .iter()
                        .any(|(item, _)| item.completed_id() == done.item.completed_id())
                {
                    return Err(ProviderError::InvalidResponseContract);
                }
                let raw = value
                    .get("item")
                    .cloned()
                    .ok_or(ProviderError::InvalidResponseContract)?;
                self.done_items.push((done.item, raw));
                Ok(None)
            }
            "response.failed" | "error" => {
                self.stage = Stage::RemoteFailure;
                Err(remote_stream_error(&value, kind))
            }
            "response.incomplete" => {
                self.stage = Stage::IncompleteResponse;
                Err(ProviderError::InvalidStream)
            }
            _ => Ok(None),
        }
    }

    fn counts(&self) -> StreamCounts {
        StreamCounts {
            bytes: self.framer.bytes,
            events: self.framer.events,
            http_status: Some(200),
            response_shape: Some(self.response_shape),
            tool_arguments: None,
        }
    }
}

// Absent details count every output token as visible; present details must carry the count.
fn reasoning_tokens(details: Option<&Value>) -> Result<u32, ProviderError> {
    match details {
        None | Some(Value::Null) => Ok(0),
        Some(details) => details
            .get("reasoning_tokens")
            .and_then(Value::as_u64)
            .and_then(|count| u32::try_from(count).ok())
            .ok_or(ProviderError::InvalidResponseContract),
    }
}

fn remote_stream_error(value: &Value, event: &str) -> ProviderError {
    let code = match event {
        "response.failed" => value.pointer("/response/error/code"),
        "error" => value.get("code"),
        _ => None,
    }
    .and_then(Value::as_str);
    match code {
        Some(code)
            if !code.is_empty()
                && code.len() <= 128
                && code.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                }) =>
        {
            ProviderError::RemoteStreamCode(code.to_owned())
        }
        _ => ProviderError::Rejected,
    }
}

#[cfg(test)]
mod tests;
