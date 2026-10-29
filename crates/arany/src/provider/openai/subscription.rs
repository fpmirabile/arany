use super::limits;
use super::responses::StreamDecoder;
use super::{OPENAI_ENDPOINT, wire};
use crate::provider::conversation::Conversation;
use crate::provider::diagnostics::subscription_failure;
use crate::provider::diagnostics::{StreamCounts, SubscriptionFailureStage as Stage};
use crate::provider::{
    CallLimits, ChatGptProvenance, CompactionRequest, CompactionResponse, Effort,
    MAX_REPORTED_INPUT_TOKENS, MAX_REPORTED_OUTPUT_TOKENS, OutputTokenBound, Provenance, Provider,
    ProviderError, ProviderIdentity, ProviderRequest, ProviderResponse, Usage,
    catalog::valid_model_id, render,
};
use crate::provider::{RateLimitSlot, ResponsePreview};
use crate::session::MAX_COMPACTION_OUTPUT_TOKENS;
#[cfg(test)]
use reqwest::redirect::Policy;
use reqwest::{Client, Url, header};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::{Instant, timeout, timeout_at};

const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;
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
            max_output_tokens: crate::provider::MAX_OUTPUT_TOKEN_CAP,
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
            RunCall {
                client: &self.client,
                endpoint: Url::parse(OPENAI_ENDPOINT).expect("compiled Responses endpoint"),
                access_token: &self.access_token,
                effort: self.effort,
                max_output_tokens: self.identity.limits.max_output_tokens,
            },
            &request,
            crate::provider::request_ceiling(request.input.context_window_tokens),
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
            RunCall {
                client: &self.client,
                endpoint: Url::parse(OPENAI_ENDPOINT).expect("compiled Responses endpoint"),
                access_token: &self.access_token,
                effort: self.effort,
                max_output_tokens: request.max_output_tokens,
            },
            &request,
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

fn client() -> Result<Client, ProviderError> {
    super::super::http_client(CALL_CEILING)
        .https_only(true)
        .build()
        .map_err(|_| ProviderError::Unavailable)
}

struct RunCall<'a> {
    client: &'a Client,
    endpoint: Url,
    access_token: &'a str,
    effort: Effort,
    max_output_tokens: u32,
}

#[cfg(test)]
async fn invoke_at(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    request: &ProviderRequest,
    effort: Effort,
) -> Result<ProviderResponse, ProviderError> {
    invoke_at_observed(
        RunCall {
            client,
            endpoint,
            access_token,
            effort,
            max_output_tokens: crate::provider::SMALL_OUTPUT_CAP,
        },
        request,
        crate::provider::MAX_PROVIDER_REQUEST_BYTES,
        (None, RateLimitSlot::default()),
    )
    .await
}

async fn invoke_at_observed(
    call: RunCall<'_>,
    request: &ProviderRequest,
    max_request_bytes: usize,
    observations: (Option<ResponsePreview>, RateLimitSlot),
) -> Result<ProviderResponse, ProviderError> {
    let access_token = call.access_token;
    if !(1..=call.max_output_tokens).contains(&request.max_output_tokens) {
        return Err(ProviderError::Rejected);
    }
    let (preview, limits) = observations;
    let conversation = render::conversation(request)?;
    let body = subscription_body(request, &conversation, call.effort, max_request_bytes)?;
    let CompletedStream {
        body: completed,
        counts,
        reasoning_tokens,
    } = completed_at_with_preview(
        RunCall {
            max_output_tokens: request.max_output_tokens,
            ..call
        },
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

#[cfg(test)]
async fn compact_at(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    request: &CompactionRequest,
    effort: Effort,
) -> Result<CompactionResponse, ProviderError> {
    compact_at_observed(
        RunCall {
            client,
            endpoint,
            access_token,
            effort,
            max_output_tokens: request.max_output_tokens,
        },
        request,
        RateLimitSlot::default(),
    )
    .await
}

async fn compact_at_observed(
    call: RunCall<'_>,
    request: &CompactionRequest,
    limits: RateLimitSlot,
) -> Result<CompactionResponse, ProviderError> {
    if !(1..=MAX_COMPACTION_OUTPUT_TOKENS).contains(&request.max_output_tokens) {
        return Err(ProviderError::Rejected);
    }
    let body = subscription_compaction_body(request, call.effort)?;
    let access_token = call.access_token;
    let CompletedStream {
        body: completed,
        counts,
        reasoning_tokens,
    } = completed_at_with_preview(call, body, None, limits).await?;
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
    call: RunCall<'_>,
    body: Vec<u8>,
    preview: Option<(ResponsePreview, &str)>,
    limits: RateLimitSlot,
) -> Result<CompletedStream, ProviderError> {
    let mut response = send_stream_request(&call, body).await?;
    let counts = StreamCounts {
        http_status: Some(response.status().as_u16()),
        ..StreamCounts::default()
    };
    admit_stream_headers(&mut response, &call, &limits, counts).await?;
    let mut decoder = match preview {
        Some((preview, model)) => StreamDecoder::with_preview(
            call.access_token,
            model,
            Some(preview),
            call.max_output_tokens,
        ),
        None => StreamDecoder::new(call.access_token, call.max_output_tokens),
    };
    decoder.set_limits(limits);
    let body = read_events(&mut decoder, &mut response).await?;
    Ok(CompletedStream {
        body,
        counts: decoder.counts(),
        reasoning_tokens: reasoning_tokens(decoder.output_tokens_details())
            .map_err(|error| failure(Stage::UsageContract, error, decoder.counts()))?,
    })
}

async fn send_stream_request(
    call: &RunCall<'_>,
    body: Vec<u8>,
) -> Result<reqwest::Response, ProviderError> {
    let access_token = call.access_token;
    if access_token.is_empty()
        || access_token.len() > MAX_ACCESS_TOKEN_BYTES
        || !access_token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ProviderError::Rejected);
    }
    let send = call
        .client
        .post(call.endpoint.clone())
        .bearer_auth(access_token)
        .header(header::ACCEPT, "text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .body(body)
        .send();
    match timeout(STREAM_IDLE, send).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => Err(failure(
            if error.is_timeout() {
                Stage::Deadline
            } else {
                Stage::RequestTransport
            },
            crate::provider::send_failure(&error),
            StreamCounts::default(),
        )),
        // An unanswered request is a send timeout, which never retries.
        Err(_) => Err(failure(
            Stage::Deadline,
            ProviderError::Unavailable,
            StreamCounts::default(),
        )),
    }
}

async fn admit_stream_headers(
    response: &mut reqwest::Response,
    call: &RunCall<'_>,
    limits: &RateLimitSlot,
    counts: StreamCounts,
) -> Result<(), ProviderError> {
    if response.url() != &call.endpoint {
        return Err(failure(
            Stage::Destination,
            ProviderError::InvalidStream,
            counts,
        ));
    }
    limits::headers(response.headers(), call.access_token, limits)?;
    let status = response.status();
    if status != reqwest::StatusCode::OK {
        let error = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            crate::provider::rate_limit_failure(response).await
        } else if status.is_server_error() {
            ProviderError::RemoteServerError(
                status.as_u16(),
                crate::provider::server_retry_after(response),
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
    if response.content_length().is_some_and(|length| {
        length > crate::provider::max_stream_bytes(call.max_output_tokens) as u64
    }) {
        crate::provider::diagnostics::stream_limit(0, 0);
        return Err(failure(
            Stage::ContentLength,
            ProviderError::InvalidStream,
            counts,
        ));
    }
    Ok(())
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
                    decoder.end_error(),
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
        let events = decoder.event_count();
        if let Some(completed) = decoder
            .feed(&chunk)
            .map_err(|error| failure(decoder.stage(), error, decoder.counts()))?
        {
            return Ok(completed);
        }
        if decoder.event_count() > events {
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
        request.prompts.compaction_instructions()
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

#[cfg(test)]
mod tests;
