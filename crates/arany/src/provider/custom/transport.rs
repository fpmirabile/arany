use super::{CustomProfileError, PinnedDestination};
use crate::provider::ProviderError;
use crate::provider::{
    CompactionRequest, CompactionResponse, Effort, MAX_REPORTED_INPUT_TOKENS, MAX_RESPONSE_BYTES,
    ProviderRequest, ProviderResponse, Usage, openai::wire, raw_reflects_secret, render,
};
use reqwest::{Client, header};
use serde_json::Value;

pub(super) struct CustomTransport {
    client: Client,
    endpoint: String,
    key: String,
}

#[derive(Clone, Copy)]
pub(super) enum TransportError {
    Unavailable,
    PartialTransport,
    Rejected,
    HttpServerUnavailable(Option<std::time::Duration>),
    RateLimited(Option<std::time::Duration>),
    QuotaExceeded,
    Transport,
    StreamEnded,
    InvalidOutcome,
}

impl CustomTransport {
    pub(super) fn new(
        destination: &PinnedDestination,
        key: String,
    ) -> Result<Self, CustomProfileError> {
        if key.is_empty() || key.len() > 512 || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(CustomProfileError::CredentialUnavailable);
        }
        Ok(Self {
            client: destination.client()?,
            endpoint: destination.endpoint().to_owned(),
            key,
        })
    }

    pub(super) async fn run(
        &self,
        request: &ProviderRequest,
        effort: Option<Effort>,
    ) -> Result<ProviderResponse, TransportError> {
        self.run_request(request, effort, false, None).await
    }

    pub(super) async fn run_streamed(
        &self,
        request: &ProviderRequest,
        effort: Option<Effort>,
        preview: Option<crate::provider::ResponsePreview>,
    ) -> Result<ProviderResponse, TransportError> {
        self.run_request(request, effort, true, preview).await
    }

    async fn run_request(
        &self,
        request: &ProviderRequest,
        effort: Option<Effort>,
        stream: bool,
        preview: Option<crate::provider::ResponsePreview>,
    ) -> Result<ProviderResponse, TransportError> {
        if request.input.tools.is_some()
            || !request.input.images.is_empty()
            || request.validate_scope().is_err()
        {
            return Err(TransportError::Rejected);
        }
        if !(1..=super::OUTPUT_CAP).contains(&request.max_output_tokens) {
            return Err(TransportError::InvalidOutcome);
        }
        let conversation = render::conversation(request).map_err(|_| TransportError::Rejected)?;
        let mut body = match effort {
            Some(value) => wire::run_body_with_effort(request, &conversation, value),
            None => wire::run_body(request, &conversation),
        }
        .map_err(|_| TransportError::Rejected)?;
        let scope = wire::RunScope::new(request, &conversation, &self.key);
        let bytes = if stream {
            body["stream"] = Value::Bool(true);
            self.send_streamed(body, &request.model, preview).await?
        } else {
            self.send(body).await?
        };
        let response = if stream {
            wire::decode_streamed_run(&bytes, &scope).map_err(|_| TransportError::InvalidOutcome)?
        } else {
            wire::decode_run(&bytes, &scope).map_err(|_| TransportError::InvalidOutcome)?
        };
        if !valid_usage(response.usage, request.max_output_tokens) {
            return Err(TransportError::InvalidOutcome);
        }
        if response.reflects_secret(&self.key) {
            return Err(TransportError::InvalidOutcome);
        }
        Ok(response)
    }

    pub(super) async fn compact(
        &self,
        request: &CompactionRequest,
        effort: Option<Effort>,
    ) -> Result<CompactionResponse, TransportError> {
        if !(1..=1024).contains(&request.max_output_tokens) {
            return Err(TransportError::InvalidOutcome);
        }
        let body = effort.map_or_else(
            || wire::compaction_body(request),
            |value| wire::compaction_body_with_effort(request, value),
        );
        let bytes = self.send(body).await?;
        let response = wire::decode_compaction(&bytes, &request.model)
            .map_err(|_| TransportError::InvalidOutcome)?;
        if !valid_usage(response.usage, request.max_output_tokens) {
            return Err(TransportError::InvalidOutcome);
        }
        if response.reflects_secret(&self.key) {
            return Err(TransportError::InvalidOutcome);
        }
        Ok(response)
    }

    async fn send_streamed(
        &self,
        request: Value,
        model: &str,
        preview: Option<crate::provider::ResponsePreview>,
    ) -> Result<Vec<u8>, TransportError> {
        let body = wire::encode_request(&request).map_err(|_| TransportError::InvalidOutcome)?;
        let mut response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .header(header::ACCEPT_ENCODING, "identity")
            .body(body)
            .send()
            .await
            .map_err(|error| send_failure(&error))?;
        let destination_matches = response.url().as_str() == self.endpoint;
        if destination_matches && !response.status().is_success() {
            return Err(status_failure(&mut response).await);
        }
        streaming_admission(response.status(), destination_matches)?;
        crate::provider::openai::read_streamed_reply(&mut response, &self.key, model, preview)
            .await
            .map_err(stream_failure)
    }

    async fn send(&self, request: Value) -> Result<Vec<u8>, TransportError> {
        let body = wire::encode_request(&request).map_err(|_| TransportError::InvalidOutcome)?;
        let mut response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT_ENCODING, "identity")
            .body(body)
            .send()
            .await
            .map_err(|error| send_failure(&error))?;
        if response.url().as_str() != self.endpoint {
            return Err(TransportError::InvalidOutcome);
        }
        if !response.status().is_success() {
            return Err(status_failure(&mut response).await);
        }
        let bytes = super::super::read_identity_body(&mut response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|error| match error {
                ProviderError::Unavailable => TransportError::Unavailable,
                ProviderError::PartialTransport => TransportError::PartialTransport,
                _ => TransportError::InvalidOutcome,
            })?;
        if raw_reflects_secret(&bytes, &self.key) {
            return Err(TransportError::InvalidOutcome);
        }
        Ok(bytes)
    }
}

// Only a cut or an early end retries; a temporary-usage event keeps its unavailable class.
pub(super) fn stream_failure(error: ProviderError) -> TransportError {
    match error {
        ProviderError::PartialTransport => TransportError::PartialTransport,
        ProviderError::StreamEnded => TransportError::StreamEnded,
        error if error.failure_class() == crate::provider::ProviderFailureClass::Unavailable => {
            TransportError::Unavailable
        }
        _ => TransportError::InvalidOutcome,
    }
}

fn send_failure(error: &reqwest::Error) -> TransportError {
    match crate::provider::send_failure(error) {
        ProviderError::Transport => TransportError::Transport,
        _ => TransportError::Unavailable,
    }
}

async fn status_failure(response: &mut reqwest::Response) -> TransportError {
    match crate::provider::http_failure(response).await {
        ProviderError::HttpServerUnavailable(after) => TransportError::HttpServerUnavailable(after),
        ProviderError::RateLimited(after) => TransportError::RateLimited(after),
        ProviderError::QuotaExceeded => TransportError::QuotaExceeded,
        _ => TransportError::Rejected,
    }
}

// A drifted destination never retries or switches; its status keeps the legacy disposition.
pub(super) fn streaming_admission(
    status: reqwest::StatusCode,
    destination_matches: bool,
) -> Result<(), TransportError> {
    if !destination_matches {
        return Err(if status.is_server_error() {
            TransportError::Unavailable
        } else {
            TransportError::Rejected
        });
    }
    if !status.is_success() {
        return Err(TransportError::Rejected);
    }
    Ok(())
}

fn valid_usage(usage: Usage, cap: u32) -> bool {
    usage
        .input_tokens
        .is_some_and(|value| (1..=MAX_REPORTED_INPUT_TOKENS).contains(&value))
        && usage
            .output_tokens
            .is_some_and(|value| value > 0 && value <= cap)
        && usage.cache_consistent()
}
