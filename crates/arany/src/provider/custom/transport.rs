use super::{CustomProfile, CustomProfileError, MessagesAuth, PinnedDestination, Protocol};
use crate::provider::ProviderError;
use crate::provider::{
    CompactionRequest, CompactionResponse, Effort, MAX_REPORTED_INPUT_TOKENS, MAX_RESPONSE_BYTES,
    ProviderRequest, ProviderResponse, Usage,
    anthropic::wire as messages,
    openai::{chat, wire},
    raw_reflects_secret, render,
};
use reqwest::{
    Client,
    header::{self, HeaderValue},
};
use serde_json::Value;

pub(super) struct CustomTransport {
    client: Client,
    endpoint: String,
    key: String,
    protocol: Protocol,
}

struct TransportResponse {
    bytes: Vec<u8>,
    counts: crate::provider::diagnostics::StreamCounts,
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
    InvalidResponseContract,
    InvalidOutcomeContract,
}

impl CustomTransport {
    pub(super) fn new(
        profile: &CustomProfile,
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
            protocol: profile.protocol.clone(),
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
        if !(1..=super::CUSTOM_OUTPUT_TOKEN_CAP).contains(&request.max_output_tokens) {
            return Err(TransportError::InvalidOutcome);
        }
        let conversation = render::conversation(request).map_err(|_| TransportError::Rejected)?;
        let mut body = match &self.protocol {
            Protocol::OpenaiResponses => match effort {
                Some(value) => wire::run_body_with_effort(request, &conversation, value),
                None => wire::run_body(request, &conversation),
            },
            Protocol::AnthropicMessages { .. } => {
                messages::run_body_with_optional_effort(request, &conversation, effort)
            }
            Protocol::OpenaiChatCompletions { .. } => {
                chat::run_body(request, &conversation, effort)
            }
        }
        .map_err(|_| TransportError::Rejected)?;
        let received =
            if stream && !matches!(&self.protocol, Protocol::OpenaiChatCompletions { .. }) {
                body["stream"] = Value::Bool(true);
                self.send_streamed(body, &request.model, preview, request.max_output_tokens)
                    .await?
            } else {
                self.send(body).await?
            };
        let response = match &self.protocol {
            Protocol::OpenaiResponses => {
                let scope = wire::RunScope::new(request, &conversation, &self.key);
                if stream {
                    wire::decode_streamed_run(&received.bytes, &scope)
                        .map_err(|_| ProviderError::InvalidOutcome)
                } else {
                    wire::decode_run(&received.bytes, &scope)
                }
            }
            Protocol::AnthropicMessages { .. } => {
                let offered: Vec<_> = conversation
                    .tools
                    .iter()
                    .flatten()
                    .cloned()
                    .chain(crate::provider::tool_specs::outcome_specs(
                        &conversation.outcome,
                    ))
                    .collect();
                messages::decode_run_with_counts(
                    &received.bytes,
                    &messages::RunScope {
                        model: &request.model,
                        output_cap: request.max_output_tokens,
                        secret: &self.key,
                        offered: &offered,
                        review: request.input.role == crate::provider::PromptRole::ToolReview,
                    },
                    received.counts,
                )
            }
            Protocol::OpenaiChatCompletions { .. } => {
                let offered: Vec<_> = conversation.tools.iter().flatten().cloned().collect();
                chat::decode_run(
                    &received.bytes,
                    request,
                    &self.key,
                    &offered,
                    &conversation.outcome,
                )
            }
        }
        .map_err(|error| {
            if matches!(&self.protocol, Protocol::OpenaiChatCompletions { .. }) {
                response_decode_failure(error)
            } else {
                TransportError::InvalidOutcome
            }
        })?;
        if !valid_usage(response.usage, request.max_output_tokens) {
            self.log_messages_failure(
                crate::provider::diagnostics::SubscriptionFailureStage::UsageContract,
                received.counts,
            );
            return Err(TransportError::InvalidOutcome);
        }
        if response.reflects_secret(&self.key) {
            self.log_messages_failure(
                crate::provider::diagnostics::SubscriptionFailureStage::CredentialReflection,
                received.counts,
            );
            return Err(TransportError::InvalidOutcome);
        }
        Ok(response)
    }

    pub(super) async fn compact(
        &self,
        request: &CompactionRequest,
        effort: Option<Effort>,
    ) -> Result<CompactionResponse, TransportError> {
        if !(1..=crate::session::MAX_COMPACTION_OUTPUT_TOKENS.min(super::CUSTOM_OUTPUT_TOKEN_CAP))
            .contains(&request.max_output_tokens)
        {
            return Err(TransportError::InvalidOutcome);
        }
        let body = match &self.protocol {
            Protocol::OpenaiResponses => effort.map_or_else(
                || wire::compaction_body(request),
                |value| wire::compaction_body_with_effort(request, value),
            ),
            Protocol::AnthropicMessages { .. } => {
                messages::compaction_body_with_optional_effort(request, effort)
            }
            Protocol::OpenaiChatCompletions { .. } => chat::compaction_body(request, effort),
        };
        let received = self.send(body).await?;
        let response = match &self.protocol {
            Protocol::OpenaiResponses => wire::decode_compaction(&received.bytes, &request.model),
            Protocol::AnthropicMessages { .. } => messages::decode_compaction_with_counts(
                &received.bytes,
                &request.model,
                request.max_output_tokens,
                received.counts,
            ),
            Protocol::OpenaiChatCompletions { .. } => {
                chat::decode_compaction(&received.bytes, &request.model, request.max_output_tokens)
            }
        }
        .map_err(|error| {
            if matches!(&self.protocol, Protocol::OpenaiChatCompletions { .. }) {
                response_decode_failure(error)
            } else {
                TransportError::InvalidOutcome
            }
        })?;
        if !valid_usage(response.usage, request.max_output_tokens) {
            self.log_messages_failure(
                crate::provider::diagnostics::SubscriptionFailureStage::UsageContract,
                received.counts,
            );
            return Err(TransportError::InvalidOutcome);
        }
        if response.reflects_secret(&self.key) {
            self.log_messages_failure(
                crate::provider::diagnostics::SubscriptionFailureStage::CredentialReflection,
                received.counts,
            );
            return Err(TransportError::InvalidOutcome);
        }
        Ok(response)
    }

    fn log_messages_failure(
        &self,
        stage: crate::provider::diagnostics::SubscriptionFailureStage,
        counts: crate::provider::diagnostics::StreamCounts,
    ) {
        if matches!(&self.protocol, Protocol::AnthropicMessages { .. }) {
            crate::provider::diagnostics::messages_failure(stage, counts);
        }
    }

    fn request(&self, request: &Value) -> Result<reqwest::RequestBuilder, TransportError> {
        let body = match &self.protocol {
            Protocol::OpenaiResponses => wire::encode_request(request),
            Protocol::AnthropicMessages { .. } => messages::encode_request_within(
                request,
                crate::provider::MAX_PROVIDER_REQUEST_BYTES,
            ),
            Protocol::OpenaiChatCompletions { .. } => wire::encode_request(request),
        }
        .map_err(|_| TransportError::InvalidOutcome)?;
        let request = self
            .client
            .post(&self.endpoint)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT_ENCODING, "identity")
            .header(
                header::USER_AGENT,
                concat!("arany/", env!("CARGO_PKG_VERSION")),
            )
            .body(body);
        Ok(match &self.protocol {
            Protocol::OpenaiResponses => request.bearer_auth(&self.key),
            Protocol::AnthropicMessages { auth, .. } => {
                let request = request.header("anthropic-version", "2023-06-01");
                match auth {
                    MessagesAuth::Bearer => request.bearer_auth(&self.key),
                    MessagesAuth::ApiKey => {
                        let mut key = HeaderValue::from_str(&self.key)
                            .map_err(|_| TransportError::Rejected)?;
                        key.set_sensitive(true);
                        request.header("x-api-key", key)
                    }
                }
            }
            Protocol::OpenaiChatCompletions { .. } => request.bearer_auth(&self.key),
        })
    }

    async fn send_streamed(
        &self,
        request: Value,
        model: &str,
        preview: Option<crate::provider::ResponsePreview>,
        max_output_tokens: u32,
    ) -> Result<TransportResponse, TransportError> {
        let mut response = self.stream_response(&request).await?;
        let received = match &self.protocol {
            Protocol::OpenaiResponses => crate::provider::openai::read_streamed_reply(
                &mut response,
                &self.key,
                model,
                preview,
                max_output_tokens,
            )
            .await
            .map(|bytes| TransportResponse {
                counts: crate::provider::diagnostics::StreamCounts {
                    bytes: bytes.len(),
                    http_status: Some(response.status().as_u16()),
                    ..Default::default()
                },
                bytes,
            }),
            Protocol::AnthropicMessages { .. } => crate::provider::anthropic::read_streamed_reply(
                &mut response,
                &self.key,
                model,
                preview,
                max_output_tokens,
            )
            .await
            .map(|reply| TransportResponse {
                bytes: reply.bytes,
                counts: reply.counts,
            }),
            Protocol::OpenaiChatCompletions { .. } => Err(ProviderError::Rejected),
        }
        .map_err(stream_failure)?;
        self.validate_streamed_messages(&received)?;
        Ok(received)
    }

    async fn stream_response(&self, request: &Value) -> Result<reqwest::Response, TransportError> {
        let mut response = self
            .dispatch(
                self.request(request)?
                    .header(header::ACCEPT, "text/event-stream"),
            )
            .await?;
        let status = response.status();
        self.admit_stream_destination(response.url().as_str(), status)?;
        if let Some(stage) = response_failure_stage(true, status) {
            self.log_messages_failure(stage, response_counts(status));
            return Err(status_failure(&mut response).await);
        }
        streaming_admission(status, true)?;
        Ok(response)
    }

    pub(super) fn admit_stream_destination(
        &self,
        response_url: &str,
        status: reqwest::StatusCode,
    ) -> Result<(), TransportError> {
        if response_url == self.endpoint {
            return Ok(());
        }
        self.log_messages_failure(
            crate::provider::diagnostics::SubscriptionFailureStage::Destination,
            response_counts(status),
        );
        streaming_admission(status, false)
    }

    fn validate_streamed_messages(
        &self,
        received: &TransportResponse,
    ) -> Result<(), TransportError> {
        if matches!(&self.protocol, Protocol::AnthropicMessages { .. }) {
            crate::provider::parse_response_json(&received.bytes).map_err(|_| {
                self.log_messages_failure(
                    crate::provider::diagnostics::SubscriptionFailureStage::ResponseEnvelope,
                    received.counts,
                );
                TransportError::InvalidOutcome
            })?;
        }
        Ok(())
    }

    async fn send(&self, request: Value) -> Result<TransportResponse, TransportError> {
        let mut response = self.dispatch(self.request(&request)?).await?;
        self.read_response(&mut response).await
    }

    async fn dispatch(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, TransportError> {
        request.send().await.map_err(|error| {
            self.log_messages_failure(
                if error.is_timeout() {
                    crate::provider::diagnostics::SubscriptionFailureStage::Deadline
                } else {
                    crate::provider::diagnostics::SubscriptionFailureStage::RequestTransport
                },
                crate::provider::diagnostics::StreamCounts::default(),
            );
            send_failure(&error)
        })
    }

    async fn read_response(
        &self,
        response: &mut reqwest::Response,
    ) -> Result<TransportResponse, TransportError> {
        if response.url().as_str() != self.endpoint {
            self.log_messages_failure(
                crate::provider::diagnostics::SubscriptionFailureStage::Destination,
                crate::provider::diagnostics::StreamCounts {
                    http_status: Some(response.status().as_u16()),
                    ..Default::default()
                },
            );
            return Err(TransportError::InvalidOutcome);
        }
        if let Some(stage) = response_failure_stage(true, response.status()) {
            self.log_messages_failure(
                stage,
                crate::provider::diagnostics::StreamCounts {
                    http_status: Some(response.status().as_u16()),
                    ..Default::default()
                },
            );
            return Err(status_failure(response).await);
        }
        if matches!(&self.protocol, Protocol::OpenaiChatCompletions { .. })
            && !response
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|value| {
                    value.to_str().is_ok_and(|value| {
                        value.split(';').next().is_some_and(|media_type| {
                            media_type.trim().eq_ignore_ascii_case("application/json")
                        })
                    })
                })
        {
            return Err(TransportError::InvalidOutcome);
        }
        let status = response.status().as_u16();
        let bytes = super::super::read_identity_body(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|error| {
                self.log_messages_failure(
                    if matches!(error, ProviderError::PartialTransport) {
                        crate::provider::diagnostics::SubscriptionFailureStage::ReadTransport
                    } else {
                        crate::provider::diagnostics::SubscriptionFailureStage::ResponseEnvelope
                    },
                    crate::provider::diagnostics::StreamCounts {
                        http_status: Some(status),
                        ..Default::default()
                    },
                );
                match error {
                    ProviderError::Unavailable => TransportError::Unavailable,
                    ProviderError::PartialTransport => TransportError::PartialTransport,
                    _ => TransportError::InvalidOutcome,
                }
            })?;
        if matches!(&self.protocol, Protocol::AnthropicMessages { .. }) {
            crate::provider::parse_response_json(&bytes).map_err(|_| {
                self.log_messages_failure(
                    crate::provider::diagnostics::SubscriptionFailureStage::ResponseEnvelope,
                    crate::provider::diagnostics::StreamCounts {
                        bytes: bytes.len(),
                        http_status: Some(status),
                        ..Default::default()
                    },
                );
                TransportError::InvalidOutcome
            })?;
        }
        if raw_reflects_secret(&bytes, &self.key) {
            self.log_messages_failure(
                crate::provider::diagnostics::SubscriptionFailureStage::CredentialReflection,
                crate::provider::diagnostics::StreamCounts {
                    bytes: bytes.len(),
                    http_status: Some(status),
                    ..Default::default()
                },
            );
            return Err(TransportError::InvalidOutcome);
        }
        Ok(TransportResponse {
            counts: crate::provider::diagnostics::StreamCounts {
                bytes: bytes.len(),
                http_status: Some(status),
                ..Default::default()
            },
            bytes,
        })
    }
}

fn response_decode_failure(error: ProviderError) -> TransportError {
    match error {
        ProviderError::InvalidResponseContract => TransportError::InvalidResponseContract,
        ProviderError::InvalidOutcomeContract => TransportError::InvalidOutcomeContract,
        _ => TransportError::InvalidOutcome,
    }
}

fn response_failure_stage(
    destination_matches: bool,
    status: reqwest::StatusCode,
) -> Option<crate::provider::diagnostics::SubscriptionFailureStage> {
    use crate::provider::diagnostics::SubscriptionFailureStage as Stage;
    if !destination_matches {
        Some(Stage::Destination)
    } else if !status.is_success() {
        Some(Stage::HttpStatus)
    } else {
        None
    }
}

fn response_counts(status: reqwest::StatusCode) -> crate::provider::diagnostics::StreamCounts {
    crate::provider::diagnostics::StreamCounts {
        http_status: Some(status.as_u16()),
        ..Default::default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_api_key_header_is_sensitive() {
        let key = "synthetic-private-key";
        let transport = CustomTransport {
            client: Client::new(),
            endpoint: "http://127.0.0.1:9321/v1/messages".into(),
            key: key.into(),
            protocol: Protocol::AnthropicMessages {
                base_url: "http://127.0.0.1:9321".into(),
                auth: MessagesAuth::ApiKey,
            },
        };
        let request = transport
            .request(&serde_json::json!({}))
            .unwrap_or_else(|_| panic!("synthetic request"))
            .build()
            .expect("request headers");
        assert_eq!(request.headers()["x-api-key"], key);
        assert!(request.headers()["x-api-key"].is_sensitive());
        assert!(!format!("{request:?}").contains(key));
    }
}
