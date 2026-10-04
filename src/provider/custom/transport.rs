use super::{CustomProfileError, PinnedDestination};
use crate::provider::{
    CompactionRequest, CompactionResponse, Effort, MAX_REPORTED_INPUT_TOKENS, MAX_RESPONSE_BYTES,
    ProviderRequest, ProviderResponse, openai::wire, raw_reflects_secret,
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
        if request.tools.is_some()
            || !request.images.is_empty()
            || request.validate_scope().is_err()
        {
            return Err(TransportError::Rejected);
        }
        if !(1..=4096).contains(&request.max_output_tokens) {
            return Err(TransportError::InvalidOutcome);
        }
        let body = effort.map_or_else(
            || wire::run_body(request),
            |value| wire::run_body_with_effort(request, value),
        );
        let bytes = self.send(body).await?;
        let response =
            wire::decode_run(&bytes, &request.model).map_err(|_| TransportError::InvalidOutcome)?;
        if !valid_usage(
            response.input_tokens,
            response.output_tokens,
            request.max_output_tokens,
        ) {
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
        if !valid_usage(
            response.input_tokens,
            response.output_tokens,
            request.max_output_tokens,
        ) {
            return Err(TransportError::InvalidOutcome);
        }
        if response.reflects_secret(&self.key) {
            return Err(TransportError::InvalidOutcome);
        }
        Ok(response)
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
        if response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(TransportError::InvalidOutcome);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(TransportError::InvalidOutcome);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| TransportError::Unavailable)?
        {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(TransportError::InvalidOutcome);
            }
            bytes.extend_from_slice(&chunk);
        }
        if raw_reflects_secret(&bytes, &self.key) {
            return Err(TransportError::InvalidOutcome);
        }
        Ok(bytes)
    }
}

fn valid_usage(input: Option<u32>, output: Option<u32>, cap: u32) -> bool {
    input.is_some_and(|value| (1..=MAX_REPORTED_INPUT_TOKENS).contains(&value))
        && output.is_some_and(|value| value > 0 && value <= cap)
}
