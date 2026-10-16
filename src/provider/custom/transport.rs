use super::{CustomProfileError, PinnedDestination};
use crate::provider::ProviderError;
use crate::provider::{
    CompactionRequest, CompactionResponse, Effort, MAX_REPORTED_INPUT_TOKENS, MAX_RESPONSE_BYTES,
    ProviderRequest, ProviderResponse, Usage, openai::wire, raw_reflects_secret,
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
    Rejected,
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
        if request.tools.is_some()
            || !request.images.is_empty()
            || request.validate_scope().is_err()
        {
            return Err(TransportError::Rejected);
        }
        if !(1..=super::OUTPUT_CAP).contains(&request.max_output_tokens) {
            return Err(TransportError::InvalidOutcome);
        }
        let mut body = effort.map_or_else(
            || wire::run_body(request),
            |value| wire::run_body_with_effort(request, value),
        );
        let bytes = if stream {
            body["stream"] = Value::Bool(true);
            self.send_streamed(body, &request.model, preview).await?
        } else {
            self.send(body).await?
        };
        let response = if stream {
            wire::decode_streamed_run(&bytes, &request.model)
                .map_err(|_| TransportError::InvalidOutcome)?
        } else {
            wire::decode_run(&bytes, &request.model).map_err(|_| TransportError::InvalidOutcome)?
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
            .map_err(|_| TransportError::Unavailable)?;
        if response.url().as_str() != self.endpoint || !response.status().is_success() {
            return Err(if response.status().is_server_error() {
                TransportError::Unavailable
            } else {
                TransportError::Rejected
            });
        }
        crate::provider::openai::read_streamed_reply(&mut response, &self.key, model, preview)
            .await
            .map_err(|error| {
                if error.failure_class() == crate::provider::ProviderFailureClass::Unavailable {
                    TransportError::Unavailable
                } else {
                    TransportError::InvalidOutcome
                }
            })
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
            .map_err(|_| TransportError::Unavailable)?;
        if response.url().as_str() != self.endpoint {
            return Err(TransportError::InvalidOutcome);
        }
        if !response.status().is_success() {
            return Err(if response.status().is_server_error() {
                TransportError::Unavailable
            } else {
                TransportError::Rejected
            });
        }
        let bytes = super::super::read_identity_body(&mut response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|error| match error {
                ProviderError::Unavailable => TransportError::Unavailable,
                _ => TransportError::InvalidOutcome,
            })?;
        if raw_reflects_secret(&bytes, &self.key) {
            return Err(TransportError::InvalidOutcome);
        }
        Ok(bytes)
    }
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
