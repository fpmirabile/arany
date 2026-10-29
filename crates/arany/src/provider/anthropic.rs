use super::conversation::Conversation;
use super::{
    CompactionRequest, CompactionResponse, Effort, NativeApiCredentials, Provenance, Provider,
    ProviderError, ProviderIdentity, ProviderRequest, ProviderResponse, native, render,
    resolve_native_effort_for_run, valid_saved_api_account_id, validate_native_api_key,
};
use reqwest::Client;
use serde_json::Value;
use uuid::Uuid;

pub(super) mod api_limits;
pub(super) mod wire;

const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

pub struct AnthropicProvider {
    client: Client,
    identity: ProviderIdentity,
    effort: Effort,
    key: String,
    workspace_id: Option<String>,
    context_window_tokens: Option<u32>,
}

impl AnthropicProvider {
    /// Reads the native Anthropic key and optional API workspace before Workspace input.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        Self::from_env_with_effort(model, None)
    }

    pub fn from_env_with_effort(
        model: String,
        selected_effort: Option<Effort>,
    ) -> Result<Self, ProviderError> {
        let effort = resolve_native_effort_for_run("anthropic", &model, selected_effort)?;
        let credentials = NativeApiCredentials::from_env("anthropic")?;
        Self::with_resolved_effort(model, effort, credentials)
    }

    /// Uses the selected API key without reading process credential variables.
    pub fn from_api_key_with_effort(
        model: String,
        selected_effort: Option<Effort>,
        key: String,
    ) -> Result<Self, ProviderError> {
        let effort = resolve_native_effort_for_run("anthropic", &model, selected_effort)?;
        Self::with_resolved_effort(
            model,
            effort,
            NativeApiCredentials::new("anthropic", key, None)?,
        )
    }

    /// Uses a previously validated OS-store account and pins its identity in Run history.
    pub fn from_saved_api_key_with_effort(
        model: String,
        selected_effort: Option<Effort>,
        key: String,
        account_id: Uuid,
    ) -> Result<Self, ProviderError> {
        if !valid_saved_api_account_id("anthropic", Some(account_id)) {
            return Err(ProviderError::Rejected);
        }
        let mut provider = Self::from_api_key_with_effort(model, selected_effort, key)?;
        provider.identity.provenance = Provenance::Native {
            saved_api_account_id: Some(account_id),
        };
        Ok(provider)
    }

    /// Pins an explicit native credential and optional saved-account identity without environment reads.
    pub fn from_credentials_with_effort(
        model: String,
        selected_effort: Option<Effort>,
        credentials: NativeApiCredentials,
        saved_api_account_id: Option<Uuid>,
    ) -> Result<Self, ProviderError> {
        let effort = resolve_native_effort_for_run("anthropic", &model, selected_effort)?;
        if !valid_saved_api_account_id("anthropic", saved_api_account_id) {
            return Err(ProviderError::Rejected);
        }
        let mut provider = Self::with_resolved_effort(model, effort, credentials)?;
        provider.identity.provenance = Provenance::Native {
            saved_api_account_id,
        };
        Ok(provider)
    }

    fn with_resolved_effort(
        model: String,
        effort: Effort,
        credentials: NativeApiCredentials,
    ) -> Result<Self, ProviderError> {
        credentials.require_profile("anthropic")?;
        validate_native_api_key(credentials.api_key())?;
        let client = super::http_client(native::NATIVE_CALL_DEADLINE)
            .https_only(true)
            .build()
            .map_err(|_| ProviderError::Unavailable)?;
        let identity = native::native_identity("anthropic", model, effort);
        Ok(Self {
            client,
            identity,
            effort,
            key: credentials.key,
            workspace_id: credentials.anthropic_workspace_id,
            context_window_tokens: None,
        })
    }

    /// Pins the window the Provider catalog reported; out-of-range values stay absent.
    #[must_use]
    pub fn with_context_window(mut self, tokens: Option<u32>) -> Self {
        self.context_window_tokens =
            tokens.and_then(|tokens| super::admitted_context_window(u64::from(tokens)));
        self
    }

    #[cfg(test)]
    fn request(&self, request: Value) -> Result<reqwest::RequestBuilder, ProviderError> {
        self.request_with_limit(request, super::MAX_PROVIDER_REQUEST_BYTES)
    }

    fn request_with_limit(
        &self,
        request: Value,
        max_request_bytes: usize,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let request = native::json_request(
            &self.client,
            ANTHROPIC_ENDPOINT,
            &self.key,
            wire::encode_request_within(&request, max_request_bytes)?,
        )
        .header("anthropic-version", "2023-06-01");
        Ok(match self.workspace_id.as_deref() {
            Some(id) => request.header("anthropic-workspace-id", id),
            None => request,
        })
    }

    async fn send_with_limit(
        &self,
        request: Value,
        max_request_bytes: usize,
        limits: &super::RateLimitSlot,
    ) -> Result<wire::MessageReply, ProviderError> {
        let mut response = send_response(
            self.request_with_limit(request, max_request_bytes)?,
            &self.key,
            limits,
        )
        .await?;
        let status = response.status().as_u16();
        let bytes = super::read_identity_body(&mut response, super::MAX_RESPONSE_BYTES)
            .await
            .map_err(|error| {
                let stage = if matches!(error, ProviderError::PartialTransport) {
                    crate::provider::diagnostics::SubscriptionFailureStage::ReadTransport
                } else {
                    crate::provider::diagnostics::SubscriptionFailureStage::ResponseEnvelope
                };
                crate::provider::diagnostics::messages_failure(
                    stage,
                    crate::provider::diagnostics::StreamCounts {
                        http_status: Some(status),
                        ..Default::default()
                    },
                );
                error
            })?;
        if super::raw_reflects_secret(&bytes, &self.key) {
            crate::provider::diagnostics::messages_failure(
                crate::provider::diagnostics::SubscriptionFailureStage::CredentialReflection,
                crate::provider::diagnostics::StreamCounts {
                    bytes: bytes.len(),
                    http_status: Some(status),
                    ..Default::default()
                },
            );
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(wire::MessageReply {
            counts: crate::provider::diagnostics::StreamCounts {
                bytes: bytes.len(),
                http_status: Some(status),
                ..Default::default()
            },
            bytes,
        })
    }

    #[cfg(test)]
    fn decode_run(
        &self,
        bytes: &[u8],
        request: &ProviderRequest,
        conversation: &Conversation,
    ) -> Result<ProviderResponse, ProviderError> {
        self.decode_run_with_counts(
            bytes,
            request,
            conversation,
            crate::provider::diagnostics::StreamCounts {
                bytes: bytes.len(),
                ..Default::default()
            },
        )
    }

    fn decode_run_with_counts(
        &self,
        bytes: &[u8],
        request: &ProviderRequest,
        conversation: &Conversation,
        counts: crate::provider::diagnostics::StreamCounts,
    ) -> Result<ProviderResponse, ProviderError> {
        let response = wire::decode_run_with_counts(
            bytes,
            &wire::RunScope {
                model: &self.identity.model,
                output_cap: request.max_output_tokens,
                secret: &self.key,
                offered: &conversation
                    .tools
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .chain(super::tool_specs::outcome_specs(&conversation.outcome))
                    .collect::<Vec<_>>(),
                review: request.input.role == super::PromptRole::ToolReview,
            },
            counts,
        )?;
        if response.reflects_secret(&self.key) {
            crate::provider::diagnostics::messages_failure(
                crate::provider::diagnostics::SubscriptionFailureStage::CredentialReflection,
                crate::provider::diagnostics::StreamCounts { ..counts },
            );
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(response)
    }

    #[cfg(test)]
    fn decode_compaction(
        &self,
        bytes: &[u8],
        output_cap: u32,
    ) -> Result<CompactionResponse, ProviderError> {
        self.decode_compaction_with_counts(
            bytes,
            output_cap,
            crate::provider::diagnostics::StreamCounts {
                bytes: bytes.len(),
                ..Default::default()
            },
        )
    }

    fn decode_compaction_with_counts(
        &self,
        bytes: &[u8],
        output_cap: u32,
        counts: crate::provider::diagnostics::StreamCounts,
    ) -> Result<CompactionResponse, ProviderError> {
        let response =
            wire::decode_compaction_with_counts(bytes, &self.identity.model, output_cap, counts)?;
        if response.reflects_secret(&self.key) {
            crate::provider::diagnostics::messages_failure(
                crate::provider::diagnostics::SubscriptionFailureStage::CredentialReflection,
                crate::provider::diagnostics::StreamCounts { ..counts },
            );
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(response)
    }

    async fn invoke_streamed(
        &self,
        request: ProviderRequest,
        preview: super::ResponsePreview,
        limits: super::RateLimitSlot,
    ) -> Result<ProviderResponse, ProviderError> {
        native::admit_request(&self.identity, &request)?;
        let conversation = render::conversation(&request)?;
        let mut body = wire::run_body(&request, &conversation, self.effort)?;
        body["stream"] = Value::Bool(true);
        let mut response = send_response(
            self.request_with_limit(
                body,
                super::request_ceiling(request.input.context_window_tokens),
            )?
            .header(reqwest::header::ACCEPT, "text/event-stream"),
            &self.key,
            &limits,
        )
        .await?;
        let reply = read_streamed_reply(
            &mut response,
            &self.key,
            &self.identity.model,
            Some(preview),
            request.max_output_tokens,
        )
        .await?;
        self.decode_run_with_counts(&reply.bytes, &request, &conversation, reply.counts)
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    fn context_window_tokens(&self) -> Option<u32> {
        self.context_window_tokens
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.invoke_observed(request, None, super::RateLimitSlot::default())
            .await
    }

    async fn invoke_observed(
        &self,
        request: ProviderRequest,
        preview: Option<super::ResponsePreview>,
        limits: super::RateLimitSlot,
    ) -> Result<ProviderResponse, ProviderError> {
        if let Some(preview) = preview {
            return self.invoke_streamed(request, preview, limits).await;
        }
        native::admit_request(&self.identity, &request)?;
        let conversation = render::conversation(&request)?;
        let body = wire::run_body(&request, &conversation, self.effort)?;
        let response = self
            .send_with_limit(
                body,
                super::request_ceiling(request.input.context_window_tokens),
                &limits,
            )
            .await?;
        self.decode_run_with_counts(&response.bytes, &request, &conversation, response.counts)
    }

    async fn invoke_with_preview(
        &self,
        request: ProviderRequest,
        preview: super::ResponsePreview,
    ) -> Result<ProviderResponse, ProviderError> {
        self.invoke_observed(request, Some(preview), super::RateLimitSlot::default())
            .await
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        self.compact_observed(request, super::RateLimitSlot::default())
            .await
    }

    async fn compact_observed(
        &self,
        request: CompactionRequest,
        limits: super::RateLimitSlot,
    ) -> Result<CompactionResponse, ProviderError> {
        if request.model != self.identity.model
            || !(1..=self.identity.limits.max_output_tokens).contains(&request.max_output_tokens)
        {
            return Err(ProviderError::Rejected);
        }
        let response = self
            .send_with_limit(
                wire::compaction_body(&request, self.effort),
                super::MAX_PROVIDER_REQUEST_BYTES,
                &limits,
            )
            .await?;
        self.decode_compaction_with_counts(
            &response.bytes,
            request.max_output_tokens,
            response.counts,
        )
    }
}

async fn send_response(
    request: reqwest::RequestBuilder,
    key: &str,
    limits: &super::RateLimitSlot,
) -> Result<reqwest::Response, ProviderError> {
    let mut response = request.send().await.map_err(|error| {
        let stage = if error.is_timeout() {
            crate::provider::diagnostics::SubscriptionFailureStage::Deadline
        } else {
            crate::provider::diagnostics::SubscriptionFailureStage::RequestTransport
        };
        crate::provider::diagnostics::messages_failure(
            stage,
            crate::provider::diagnostics::StreamCounts::default(),
        );
        super::send_failure(&error)
    })?;
    api_limits::headers(response.headers(), key, limits)?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        crate::provider::diagnostics::messages_failure(
            crate::provider::diagnostics::SubscriptionFailureStage::HttpStatus,
            crate::provider::diagnostics::StreamCounts {
                http_status: Some(status),
                ..Default::default()
            },
        );
        return Err(super::http_failure(&mut response).await);
    }
    Ok(response)
}

pub(super) async fn read_streamed_reply(
    response: &mut reqwest::Response,
    key: &str,
    model: &str,
    preview: Option<super::ResponsePreview>,
    max_output_tokens: u32,
) -> Result<wire::MessageReply, ProviderError> {
    let status = response.status().as_u16();
    if let Err(failure) = super::streaming::validate_stream_response(response, max_output_tokens) {
        crate::provider::diagnostics::messages_failure(
            failure.stage,
            crate::provider::diagnostics::StreamCounts {
                http_status: Some(status),
                ..Default::default()
            },
        );
        return Err(failure.error);
    }
    let (private_preview, _observed) = super::ResponsePreview::new();
    let mut decoder = wire::MessageStream::new(
        key,
        model,
        preview.unwrap_or(private_preview),
        max_output_tokens,
    );
    decoder.set_http_status(status);
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        let stage = if error.is_timeout() {
            crate::provider::diagnostics::SubscriptionFailureStage::Deadline
        } else {
            crate::provider::diagnostics::SubscriptionFailureStage::ReadTransport
        };
        crate::provider::diagnostics::messages_failure(
            stage,
            crate::provider::diagnostics::StreamCounts {
                bytes: decoder.counts().bytes,
                events: decoder.counts().events,
                http_status: Some(status),
                ..Default::default()
            },
        );
        ProviderError::PartialTransport
    })? {
        if let Some(bytes) = decoder.feed(&chunk)? {
            return Ok(wire::MessageReply {
                bytes,
                counts: decoder.counts(),
            });
        }
    }
    let error = decoder.end_error();
    crate::provider::diagnostics::messages_failure(
        crate::provider::diagnostics::SubscriptionFailureStage::EndBeforeCompleted,
        decoder.counts(),
    );
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::raw_reflects_secret;

    fn synthetic_reply(text: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "msg_safe",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-5",
            "stop_reason": "end_turn",
            "stop_details": null,
            "stop_sequence": null,
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 12, "output_tokens": 8}
        }))
        .expect("synthetic reply")
    }

    #[test]
    fn explicit_key_admission_checks_model_effort_and_key_without_network() {
        let provider = AnthropicProvider::from_api_key_with_effort(
            "claude-sonnet-5".into(),
            Some(Effort::Max),
            "synthetic-anthropic-key".into(),
        )
        .expect("selected API key");
        assert_eq!(provider.identity().model, "claude-sonnet-5");
        assert_eq!(provider.identity().effort, Some(Effort::Max));
        assert_eq!(provider.key, "synthetic-anthropic-key");
        assert_eq!(provider.identity().saved_api_account_id(), None);
        assert_eq!(
            provider.identity().limits.max_output_tokens,
            crate::provider::MAX_OUTPUT_TOKEN_CAP
        );
        assert_eq!(
            provider.identity().limits.deadline,
            native::NATIVE_CALL_DEADLINE
        );
        assert_workspace_scope(&provider);
        assert_saved_and_unreviewed_models();
        assert_invalid_credentials_rejected();
    }

    fn assert_workspace_scope(provider: &AnthropicProvider) {
        assert!(
            !provider
                .request(serde_json::json!({}))
                .unwrap()
                .build()
                .unwrap()
                .headers()
                .contains_key("anthropic-workspace-id")
        );
        let scoped = AnthropicProvider::from_credentials_with_effort(
            "claude-sonnet-5".into(),
            None,
            NativeApiCredentials::new(
                "anthropic",
                "synthetic-anthropic-key".into(),
                Some("wrkspc_Fixture".into()),
            )
            .unwrap(),
            None,
        )
        .unwrap();
        let request = scoped
            .request(serde_json::json!({}))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.url().as_str(), ANTHROPIC_ENDPOINT);
        assert_eq!(
            request
                .headers()
                .get("anthropic-workspace-id")
                .and_then(|value| value.to_str().ok()),
            Some("wrkspc_Fixture"),
            "the shared Run and compaction request must retain scope"
        );
    }

    fn assert_saved_and_unreviewed_models() {
        let account_id = Uuid::now_v7();
        let saved = AnthropicProvider::from_saved_api_key_with_effort(
            "claude-sonnet-5".into(),
            Some(Effort::Max),
            "synthetic-anthropic-key".into(),
            account_id,
        )
        .expect("saved API account");
        assert_eq!(saved.identity().saved_api_account_id(), Some(account_id));
        for effort in Effort::ALL {
            let selected = AnthropicProvider::from_credentials_with_effort(
                "future-model".into(),
                Some(effort),
                NativeApiCredentials::new(
                    "anthropic",
                    "synthetic-anthropic-key".into(),
                    Some("wrkspc_Fixture".into()),
                )
                .unwrap(),
                Some(account_id),
            )
            .expect("explicit native choice needs no synthetic check");
            assert_eq!(selected.identity().effort, Some(effort));
            assert_eq!(selected.identity().saved_api_account_id(), Some(account_id));
            assert_eq!(selected.identity().limits.concurrency, 1);
            assert_eq!(selected.workspace_id.as_deref(), Some("wrkspc_Fixture"));
        }
    }

    fn assert_invalid_credentials_rejected() {
        assert!(matches!(
            AnthropicProvider::from_saved_api_key_with_effort(
                "claude-sonnet-5".into(),
                None,
                "synthetic-anthropic-key".into(),
                Uuid::nil(),
            ),
            Err(ProviderError::Rejected)
        ));
        assert!(matches!(
            AnthropicProvider::from_api_key_with_effort("unreviewed".into(), None, String::new()),
            Err(ProviderError::Rejected)
        ));
        assert!(matches!(
            AnthropicProvider::from_api_key_with_effort(
                "claude-sonnet-5".into(),
                Some(Effort::None),
                "synthetic-key".into()
            ),
            Err(ProviderError::Rejected)
        ));
        assert!(matches!(
            AnthropicProvider::from_api_key_with_effort(
                "claude-sonnet-5".into(),
                None,
                "bad key".into()
            ),
            Err(ProviderError::Unavailable)
        ));
    }

    fn synthetic_tool_reply(arguments: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "msg_safe",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-5",
            "stop_reason": "tool_use",
            "stop_details": null,
            "stop_sequence": null,
            "content": [{"type": "tool_use", "id": "toolu_safe", "name": "arany_mcp_call", "input": {
                "server": "example", "tool": "echo", "schema_digest": "0".repeat(64),
                "arguments": arguments
            }}],
            "usage": {"input_tokens": 12, "output_tokens": 8}
        }))
        .expect("synthetic reply")
    }

    #[test]
    fn decoded_native_outcomes_reject_escaped_key_reflection() {
        let secret = "synthetic-native-secret";
        let provider = AnthropicProvider::from_api_key_with_effort(
            "claude-sonnet-5".into(),
            None,
            secret.into(),
        )
        .expect("selected key");
        let request = crate::provider::test_support::primary_request("claude-sonnet-5");
        let conversation = render::conversation(&request).unwrap();
        let decode = |bytes: &[u8]| provider.decode_run(bytes, &request, &conversation);
        let good_run = synthetic_reply(r#"answer"#);
        assert!(decode(&good_run).is_ok());
        let reflected_run = String::from_utf8(synthetic_reply(secret))
            .unwrap()
            .replace("synthetic-native-secret", "\\u0073ynthetic-native-secret")
            .into_bytes();
        assert!(!raw_reflects_secret(&reflected_run, secret));
        assert!(matches!(
            decode(&reflected_run),
            Err(ProviderError::InvalidOutcome)
        ));

        assert!(decode(&synthetic_tool_reply(r#"{"value":"safe"}"#)).is_ok());
        for (case, arguments) in [
            (
                "escaped value",
                r#"{"value":"\u0073ynthetic-native-secret"}"#,
            ),
            (
                "escaped array value",
                r#"{"value":["\u0073ynthetic-native-secret"]}"#,
            ),
            ("escaped key", r#"{"\u0073ynthetic-native-secret":"safe"}"#),
            (
                "overwritten escaped value",
                r#"{"value":"\u0073ynthetic-native-secret","value":"safe"}"#,
            ),
            (
                "duplicate decoded key",
                r#"{"value":"\u0073ynthetic-native-secret","\u0076alue":"safe"}"#,
            ),
            (
                "malformed escaped value",
                r#"{"value":"\u0073ynthetic-native-secret",}"#,
            ),
            ("non-object arguments", "[]"),
            ("null arguments", "null"),
        ] {
            let reflected_tool = synthetic_tool_reply(arguments);
            assert!(!raw_reflects_secret(&reflected_tool, secret));
            assert!(
                matches!(decode(&reflected_tool), Err(ProviderError::InvalidOutcome)),
                "MCP argument rejection: {case}"
            );
        }

        let good_compaction = synthetic_reply(r#"safe"#);
        assert!(provider.decode_compaction(&good_compaction, 1024).is_ok());
        let reflected_compaction = String::from_utf8(synthetic_reply(secret))
            .unwrap()
            .replace(secret, r#"\u0073ynthetic-native-secret"#)
            .into_bytes();
        assert!(!raw_reflects_secret(&reflected_compaction, secret));
        assert!(matches!(
            provider.decode_compaction(&reflected_compaction, 1024),
            Err(ProviderError::InvalidOutcome)
        ));
    }
}
