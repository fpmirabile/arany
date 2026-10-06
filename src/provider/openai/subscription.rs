use super::{DEADLINE, ENDPOINT, wire};
use crate::diagnostics::{
    ResponseShape, StreamCounts, SubscriptionFailureStage as Stage, subscription_failure,
};
use crate::provider::{
    AgentPhase, ChatGptProvenance, CompactionItem, CompactionRequest, CompactionResponse, Effort,
    HistoryTurn, MAX_REPORTED_INPUT_TOKENS, MAX_RESPONSE_BYTES, OutputTokenBound, Provider,
    ProviderError, ProviderOutcome, ProviderRequest, ProviderResponse, catalog::valid_model_id,
};
use crate::session::{AgentRunId, RunId, SessionId};
#[cfg(test)]
use reqwest::redirect::Policy;
use reqwest::{Client, Url, header};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::timeout;

const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;
const MAX_EVENTS: usize = 8192;
const PROBE_DEADLINE: Duration = Duration::from_secs(180);
const PROBE_OUTPUT_CAP: u32 = 1024;

pub struct ChatGptProvider {
    client: Client,
    model: String,
    effort: Effort,
    access_token: String,
    provenance: ChatGptProvenance,
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
        Ok(Self {
            client: client()?,
            model,
            effort,
            access_token,
            provenance,
        })
    }
}

impl Provider for ChatGptProvider {
    fn profile_name(&self) -> &str {
        "chatgpt"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn reasoning_effort(&self) -> Option<Effort> {
        Some(self.effort)
    }

    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    fn chatgpt_provenance(&self) -> Option<ChatGptProvenance> {
        Some(self.provenance.clone())
    }

    fn output_token_bound(&self) -> OutputTokenBound {
        OutputTokenBound::LocalAcceptanceOnly
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        request.validate_scope()?;
        if request.model != self.model {
            return Err(ProviderError::Rejected);
        }
        timeout(
            DEADLINE,
            invoke_at(
                &self.client,
                Url::parse(ENDPOINT).expect("compiled Responses endpoint"),
                &self.access_token,
                &request,
                self.effort,
            ),
        )
        .await
        .map_err(|_| {
            failure(
                Stage::Deadline,
                ProviderError::Unavailable,
                StreamCounts::default(),
            )
        })?
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        if request.model != self.model {
            return Err(ProviderError::Rejected);
        }
        timeout(
            DEADLINE,
            compact_at(
                &self.client,
                Url::parse(ENDPOINT).expect("compiled Responses endpoint"),
                &self.access_token,
                &request,
                self.effort,
            ),
        )
        .await
        .map_err(|_| {
            failure(
                Stage::Deadline,
                ProviderError::Unavailable,
                StreamCounts::default(),
            )
        })?
    }
}

/// Performs potentially billable synthetic calls. Account/catalog admission and cost consent belong to the caller.
pub(super) async fn probe_model(
    access_token: &str,
    model: &str,
    effort: Effort,
) -> Result<(), ProviderError> {
    let client = client()?;
    let endpoint = Url::parse(ENDPOINT).expect("compiled Responses endpoint");
    timeout(
        PROBE_DEADLINE,
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
    let direct = ProviderRequest {
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Single,
        model: model.to_owned(),
        images: Vec::new(),
        instructions: None,
        objective: "Synthetic conformance check: finish directly with a short factual sentence. No Workspace content is supplied.".into(),
        includes: Vec::new(),
        history: Vec::new(),
        context_summary: None,
        child_results: Vec::new(),
        max_output_tokens: PROBE_OUTPUT_CAP,
        tools: None,
    };
    let response = invoke_at(client, endpoint.clone(), access_token, &direct, effort).await?;
    if !matches!(&response.outcome, ProviderOutcome::Finish(value) if !value.summary.trim().is_empty() && !value.result.trim().is_empty() && value.summary.len() <= 8 * 1024 && value.result.len() <= 8 * 1024)
    {
        return Err(ProviderError::InvalidOutcome);
    }
    let direct_id = response.response_id.ok_or(ProviderError::InvalidOutcome)?;

    let delegate = ProviderRequest {
        collaboration: crate::session::CollaborationPolicy::Team { max_active_children: 1 },
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        objective: "Synthetic conformance check: delegate exactly one independent read-only question about the number two. No Workspace content is supplied.".into(),
        ..direct
    };
    let response = invoke_at(client, endpoint.clone(), access_token, &delegate, effort).await?;
    if !matches!(&response.outcome, ProviderOutcome::Delegate(value) if value.children.len() == 1 && !value.children[0].trim().is_empty())
    {
        return Err(ProviderError::InvalidOutcome);
    }
    let delegate_id = response.response_id.ok_or(ProviderError::InvalidOutcome)?;
    if delegate_id == direct_id {
        return Err(ProviderError::InvalidOutcome);
    }

    let compact = CompactionRequest {
        session_id: SessionId::new(),
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
    super::super::http_client(DEADLINE)
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
    if !(1..=4096).contains(&request.max_output_tokens) {
        return Err(ProviderError::Rejected);
    }
    let body = subscription_body(request, effort)?;
    let (completed, counts) = completed_at(client, endpoint, access_token, body).await?;
    let decoded = if request.tools.is_some() {
        wire::decode_streamed_tool_run(&completed, &request.model)
    } else {
        wire::decode_streamed_run(&completed, &request.model)
    };
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
        result.input_tokens,
        result.output_tokens,
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
    if !(1..=1024).contains(&request.max_output_tokens) {
        return Err(ProviderError::Rejected);
    }
    let body = subscription_compaction_body(request, effort)?;
    let (completed, counts) = completed_at(client, endpoint, access_token, body).await?;
    let result = wire::decode_streamed_compaction(&completed, &request.model)
        .map_err(|error| response_failure(error, counts))?;
    accept_result(
        result.reflects_secret(access_token),
        result.input_tokens,
        result.output_tokens,
        request.max_output_tokens,
        counts,
    )?;
    Ok(result)
}

fn accept_result(
    reflects_secret: bool,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
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
    if input_tokens.is_none_or(|count| count == 0 || count > MAX_REPORTED_INPUT_TOKENS)
        || output_tokens.is_none_or(|count| count == 0)
    {
        return Err(failure(
            Stage::UsageContract,
            ProviderError::InvalidResponseContract,
            counts,
        ));
    }
    if output_tokens.is_some_and(|count| count > max_output_tokens) {
        return Err(failure(
            Stage::LocalOutputLimit,
            ProviderError::LocalOutputLimit,
            counts,
        ));
    }
    Ok(())
}

async fn completed_at(
    client: &Client,
    endpoint: Url,
    access_token: &str,
    body: Vec<u8>,
) -> Result<(Vec<u8>, StreamCounts), ProviderError> {
    if access_token.is_empty()
        || access_token.len() > MAX_ACCESS_TOKEN_BYTES
        || !access_token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ProviderError::Rejected);
    }
    let mut response = client
        .post(endpoint.clone())
        .bearer_auth(access_token)
        .header(header::ACCEPT, "text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .body(body)
        .send()
        .await
        .map_err(|error| {
            failure(
                if error.is_timeout() {
                    Stage::Deadline
                } else {
                    Stage::RequestTransport
                },
                ProviderError::Unavailable,
                StreamCounts::default(),
            )
        })?;
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
    let status = response.status();
    if status != reqwest::StatusCode::OK {
        return Err(failure(
            Stage::HttpStatus,
            ProviderError::RemoteHttp(status.as_u16()),
            counts,
        ));
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
    let mut decoder = StreamDecoder::new(access_token);
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        failure(
            if error.is_timeout() {
                Stage::Deadline
            } else {
                Stage::ReadTransport
            },
            ProviderError::Unavailable,
            decoder.counts(),
        )
    })? {
        if let Some(completed) = decoder
            .feed(&chunk)
            .map_err(|error| failure(decoder.stage, error, decoder.counts()))?
        {
            return Ok((completed, decoder.counts()));
        }
    }
    Err(failure(
        Stage::EndBeforeCompleted,
        ProviderError::InvalidStream,
        decoder.counts(),
    ))
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

fn subscription_body(request: &ProviderRequest, effort: Effort) -> Result<Vec<u8>, ProviderError> {
    super::super::image::validate_images(request)?;
    streaming_body(wire::run_body_with_effort(request, effort))
}

fn subscription_compaction_body(
    request: &CompactionRequest,
    effort: Effort,
) -> Result<Vec<u8>, ProviderError> {
    streaming_body(wire::compaction_body_with_effort(request, effort))
}

fn streaming_body(mut body: Value) -> Result<Vec<u8>, ProviderError> {
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
    wire::encode_request(&body)
}

struct StreamDecoder<'a> {
    token: &'a str,
    total_bytes: usize,
    events: usize,
    line: Vec<u8>,
    after_cr: bool,
    event: Option<String>,
    data: Vec<u8>,
    stage: Stage,
    response_shape: ResponseShape,
    response_id: Option<String>,
    done_items: Vec<wire::WireItem>,
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
            total_bytes: 0,
            events: 0,
            line: Vec::new(),
            after_cr: false,
            event: None,
            data: Vec::new(),
            stage: Stage::EventFraming,
            response_shape: ResponseShape::default(),
            response_id: None,
            done_items: Vec::new(),
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Option<Vec<u8>>, ProviderError> {
        self.stage = Stage::StreamLimit;
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .filter(|count| *count <= MAX_RESPONSE_BYTES)
            .ok_or(ProviderError::InvalidStream)?;
        for &byte in chunk {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\n' || byte == b'\r' {
                self.after_cr = byte == b'\r';
                let line = std::mem::take(&mut self.line);
                if line.is_empty() {
                    if let Some(completed) = self.finish_event()? {
                        return Ok(Some(completed));
                    }
                } else {
                    self.accept_line(&line)?;
                }
            } else {
                self.stage = Stage::StreamLimit;
                if self.line.len() == MAX_RESPONSE_BYTES {
                    return Err(ProviderError::InvalidStream);
                }
                self.line.push(byte);
            }
        }
        Ok(None)
    }

    fn accept_line(&mut self, line: &[u8]) -> Result<(), ProviderError> {
        self.stage = Stage::EventFraming;
        if line.contains(&b'\r') || line.contains(&0) {
            return Err(ProviderError::InvalidStream);
        }
        if line[0] == b':' {
            return Ok(());
        }
        if let Some(value) = line.strip_prefix(b"event:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            let name = std::str::from_utf8(value).map_err(|_| ProviderError::InvalidStream)?;
            if self.event.is_some()
                || name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            {
                return Err(ProviderError::InvalidStream);
            }
            self.event = Some(name.to_owned());
        } else if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            if value.len() > MAX_RESPONSE_BYTES - self.data.len() {
                self.stage = Stage::StreamLimit;
                return Err(ProviderError::InvalidStream);
            }
            self.data.extend_from_slice(value);
        }
        Ok(())
    }

    fn finish_event(&mut self) -> Result<Option<Vec<u8>>, ProviderError> {
        if self.event.is_none() && self.data.is_empty() {
            return Ok(None);
        }
        self.events += 1;
        self.stage = Stage::StreamLimit;
        if self.events > MAX_EVENTS {
            return Err(ProviderError::InvalidStream);
        }
        self.stage = Stage::EventFraming;
        if self.data.is_empty() {
            return Err(ProviderError::InvalidStream);
        }
        let event = self.event.take();
        let data = std::mem::take(&mut self.data);
        self.stage = Stage::CredentialReflection;
        if data
            .windows(self.token.len())
            .any(|window| window == self.token.as_bytes())
        {
            return Err(ProviderError::InvalidOutcome);
        }
        self.stage = Stage::EventJson;
        let value: Value =
            serde_json::from_slice(&data).map_err(|_| ProviderError::InvalidStream)?;
        self.stage = Stage::CredentialReflection;
        if reflects_token(&value, self.token) {
            return Err(ProviderError::InvalidOutcome);
        }
        self.stage = Stage::EventType;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ProviderError::InvalidStream)?;
        if kind.is_empty()
            || kind.len() > 128
            || !kind
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || event.as_deref().is_some_and(|event| event != kind)
        {
            return Err(ProviderError::InvalidStream);
        }
        match kind {
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
                        if let Some(content) = item.get("content").and_then(Value::as_array) {
                            let text = content
                                .iter()
                                .filter(|part| {
                                    part.get("type").and_then(Value::as_str) == Some("output_text")
                                })
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<String>();
                            self.response_shape.structured +=
                                usize::from(wire::is_structured_text(&text));
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
                        .any(|item| item.completed_id() == done.item.completed_id())
                {
                    return Err(ProviderError::InvalidResponseContract);
                }
                self.done_items.push(done.item);
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
            bytes: self.total_bytes,
            events: self.events,
            http_status: Some(200),
            response_shape: Some(self.response_shape),
            tool_arguments: None,
        }
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

fn reflects_token(value: &Value, token: &str) -> bool {
    match value {
        Value::String(text) => text.contains(token),
        Value::Array(items) => items.iter().any(|item| reflects_token(item, token)),
        Value::Object(fields) => fields
            .iter()
            .any(|(name, value)| name.contains(token) || reflects_token(value, token)),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
