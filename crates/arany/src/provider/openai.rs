use super::conversation::Conversation;
use super::{
    CompactionRequest, CompactionResponse, Effort, MAX_REPORTED_INPUT_TOKENS, Provenance, Provider,
    ProviderError, ProviderIdentity, ProviderRequest, ProviderResponse, Usage, native, render,
    resolve_native_effort_for_run, valid_saved_api_account_id, validate_native_api_key,
};
use reqwest::Client;
use serde_json::Value;
use uuid::Uuid;

mod subscription;
pub(crate) mod wire;
pub use subscription::ChatGptProvider;

const ENDPOINT: &str = "https://api.openai.com/v1/responses";

/// Makes three potentially plan-consuming, data-free strict-outcome probes.
/// The caller must verify the selected account, catalog visibility, and explicit cost consent.
pub async fn probe_chatgpt_model(
    access_token: &str,
    model: &str,
    effort: Effort,
) -> Result<(), ProviderError> {
    subscription::probe_model(access_token, model, effort).await
}

pub struct OpenAiProvider {
    client: Client,
    identity: ProviderIdentity,
    effort: Effort,
    key: String,
}

impl OpenAiProvider {
    pub(super) fn for_conformance(
        model: String,
        effort: Effort,
        key: String,
    ) -> Result<Self, ProviderError> {
        if !super::catalog::valid_model_id(&model) {
            return Err(ProviderError::Rejected);
        }
        Self::with_resolved_effort(model, effort, key)
    }

    /// Reads only the native OpenAI API key. The caller must select this profile before Workspace input.
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        Self::from_env_with_effort(model, None)
    }

    pub fn from_env_with_effort(
        model: String,
        selected_effort: Option<Effort>,
    ) -> Result<Self, ProviderError> {
        let effort = resolve_native_effort_for_run("openai", &model, selected_effort)?;
        let key = std::env::var("OPENAI_API_KEY").map_err(|_| ProviderError::Unavailable)?;
        Self::with_resolved_effort(model, effort, key)
    }

    /// Uses the selected API key without reading process credential variables.
    pub fn from_api_key_with_effort(
        model: String,
        selected_effort: Option<Effort>,
        key: String,
    ) -> Result<Self, ProviderError> {
        let effort = resolve_native_effort_for_run("openai", &model, selected_effort)?;
        Self::with_resolved_effort(model, effort, key)
    }

    /// Uses a previously validated OS-store account and pins its identity in Run history.
    pub fn from_saved_api_key_with_effort(
        model: String,
        selected_effort: Option<Effort>,
        key: String,
        account_id: Uuid,
    ) -> Result<Self, ProviderError> {
        if !valid_saved_api_account_id("openai", Some(account_id)) {
            return Err(ProviderError::Rejected);
        }
        let mut provider = Self::from_api_key_with_effort(model, selected_effort, key)?;
        provider.identity.provenance = Provenance::Native {
            saved_api_account_id: Some(account_id),
        };
        Ok(provider)
    }

    /// Admits one previously checked native model/effort before Workspace input.
    pub async fn from_checked_api_key_with_effort(
        root: crate::store::StateRoot,
        model: String,
        effort: Effort,
        key: String,
        saved_api_account_id: Option<Uuid>,
    ) -> Result<Self, ProviderError> {
        super::native_check::verify_for_run(
            root,
            "openai",
            &model,
            effort,
            &key,
            saved_api_account_id,
        )
        .await?;
        let mut provider = Self::with_resolved_effort(model, effort, key)?;
        provider.identity.provenance = Provenance::Native {
            saved_api_account_id,
        };
        provider.identity.limits.concurrency = native::UNREVIEWED_MODEL_CONCURRENCY;
        Ok(provider)
    }

    pub async fn from_checked_env_with_effort(
        root: crate::store::StateRoot,
        model: String,
        effort: Effort,
    ) -> Result<Self, ProviderError> {
        if !super::catalog::valid_model_id(&model) {
            return Err(ProviderError::Rejected);
        }
        let key = std::env::var("OPENAI_API_KEY").map_err(|_| ProviderError::Unavailable)?;
        Self::from_checked_api_key_with_effort(root, model, effort, key, None).await
    }

    fn with_resolved_effort(
        model: String,
        effort: Effort,
        key: String,
    ) -> Result<Self, ProviderError> {
        validate_native_api_key(&key)?;
        let client = super::http_client(native::NATIVE_CALL_DEADLINE)
            .https_only(true)
            .build()
            .map_err(|_| ProviderError::Unavailable)?;
        let identity = native::native_identity("openai", model, effort);
        Ok(Self {
            client,
            identity,
            effort,
            key,
        })
    }

    fn request(&self, request: Value) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(native::json_request(
            &self.client,
            ENDPOINT,
            &self.key,
            wire::encode_request(&request)?,
        ))
    }

    async fn send(&self, request: Value) -> Result<Vec<u8>, ProviderError> {
        native::post_json(self.request(request)?, &self.key).await
    }

    fn decode_run(
        &self,
        bytes: &[u8],
        request: &ProviderRequest,
        conversation: &Conversation,
    ) -> Result<ProviderResponse, ProviderError> {
        let response = wire::decode_run(
            bytes,
            &wire::RunScope::new(request, conversation, &self.key),
        )?;
        if !valid_usage(response.usage, request.max_output_tokens)
            || response.reflects_secret(&self.key)
        {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(response)
    }

    fn decode_compaction(
        &self,
        bytes: &[u8],
        output_cap: u32,
    ) -> Result<CompactionResponse, ProviderError> {
        let response = wire::decode_compaction(bytes, &self.identity.model)?;
        if !valid_usage(response.usage, output_cap) || response.reflects_secret(&self.key) {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(response)
    }
}

fn valid_usage(usage: Usage, output_cap: u32) -> bool {
    usage
        .input_tokens
        .is_none_or(|count| count <= MAX_REPORTED_INPUT_TOKENS)
        && usage.output_tokens.is_none_or(|count| count <= output_cap)
        && usage.cache_consistent()
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        native::admit_request(&self.identity, &request)?;
        let conversation = render::conversation(&request)?;
        let body = wire::run_body_with_effort(&request, &conversation, self.effort)?;
        let response = self.send(body).await?;
        self.decode_run(&response, &request, &conversation)
    }

    async fn invoke_with_preview(
        &self,
        request: ProviderRequest,
        preview: super::ResponsePreview,
    ) -> Result<ProviderResponse, ProviderError> {
        native::admit_request(&self.identity, &request)?;
        let conversation = render::conversation(&request)?;
        let mut body = wire::run_body_with_effort(&request, &conversation, self.effort)?;
        body["stream"] = Value::Bool(true);
        let mut response = self
            .request(body)?
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|error| super::send_failure(&error))?;
        if !response.status().is_success() {
            return Err(super::http_failure(&mut response).await);
        }
        let bytes = read_streamed_reply(
            &mut response,
            &self.key,
            &self.identity.model,
            Some(preview),
        )
        .await?;
        let decoded = wire::decode_streamed_run(
            &bytes,
            &wire::RunScope::new(&request, &conversation, &self.key),
        )
        .map_err(wire::ResponseError::provider_error)?;
        if !valid_usage(decoded.usage, request.max_output_tokens)
            || decoded.reflects_secret(&self.key)
        {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(decoded)
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        if request.model != self.identity.model || !(1..=1024).contains(&request.max_output_tokens)
        {
            return Err(ProviderError::Rejected);
        }
        let response = self
            .send(wire::compaction_body_with_effort(&request, self.effort))
            .await?;
        self.decode_compaction(&response, request.max_output_tokens)
    }
}

pub(crate) async fn read_streamed_reply(
    response: &mut reqwest::Response,
    key: &str,
    model: &str,
    preview: Option<super::ResponsePreview>,
) -> Result<Vec<u8>, ProviderError> {
    subscription::read_completed_response(response, key, model, preview).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::raw_reflects_secret;

    fn synthetic_reply(text: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "resp_safe",
            "status": "completed",
            "model": "gpt-5.4",
            "output": [{
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": text}]
            }],
            "usage": {"input_tokens": 12, "output_tokens": 8}
        }))
        .expect("synthetic reply")
    }

    #[test]
    fn explicit_key_admission_checks_model_effort_and_key_without_network() {
        let provider = OpenAiProvider::from_api_key_with_effort(
            "gpt-5.4".into(),
            Some(Effort::High),
            "synthetic-openai-key".into(),
        )
        .expect("selected API key");
        assert_eq!(provider.identity().model, "gpt-5.4");
        assert_eq!(provider.identity().effort, Some(Effort::High));
        assert_eq!(provider.key, "synthetic-openai-key");
        assert_eq!(provider.identity().saved_api_account_id(), None);
        assert_eq!(
            provider.identity().limits.max_output_tokens,
            crate::provider::MAX_OUTPUT_TOKEN_CAP
        );
        assert_eq!(
            provider.identity().limits.deadline,
            native::NATIVE_CALL_DEADLINE
        );
        let account_id = Uuid::now_v7();
        let saved = OpenAiProvider::from_saved_api_key_with_effort(
            "gpt-5.4".into(),
            Some(Effort::High),
            "synthetic-openai-key".into(),
            account_id,
        )
        .expect("saved API account");
        assert_eq!(saved.identity().saved_api_account_id(), Some(account_id));
        for effort in Effort::ALL {
            let selected = OpenAiProvider::from_saved_api_key_with_effort(
                "future-model".into(),
                Some(effort),
                "synthetic-openai-key".into(),
                account_id,
            )
            .expect("explicit native choice needs no synthetic check");
            assert_eq!(selected.identity().model, "future-model");
            assert_eq!(selected.identity().effort, Some(effort));
            assert_eq!(selected.identity().saved_api_account_id(), Some(account_id));
            assert_eq!(selected.identity().limits.concurrency, 1);
        }
        assert!(matches!(
            OpenAiProvider::from_saved_api_key_with_effort(
                "gpt-5.4".into(),
                None,
                "synthetic-openai-key".into(),
                Uuid::nil(),
            ),
            Err(ProviderError::Rejected)
        ));
        assert!(matches!(
            OpenAiProvider::from_api_key_with_effort("unreviewed".into(), None, String::new()),
            Err(ProviderError::Rejected)
        ));
        assert!(matches!(
            OpenAiProvider::from_api_key_with_effort("gpt-5.4".into(), None, "bad\nkey".into()),
            Err(ProviderError::Unavailable)
        ));
    }

    #[test]
    fn decoded_native_outcomes_reject_escaped_key_reflection() {
        let secret = "synthetic-native-secret";
        let provider =
            OpenAiProvider::from_api_key_with_effort("gpt-5.4".into(), None, secret.into())
                .expect("selected key");
        let read_only = crate::provider::test_support::read_only_request("gpt-5.4");
        let read_only_conversation = render::conversation(&read_only).unwrap();
        let decode_run = |bytes: &[u8], cap: u32| {
            let mut request = read_only.clone();
            request.max_output_tokens = cap;
            provider.decode_run(bytes, &request, &read_only_conversation)
        };
        let good_run = synthetic_reply(r#"answer"#);
        let good_compaction = synthetic_reply(r#"safe"#);
        for (case, cap, usage, accepted) in [
            ("reported output at requested cap", 8, Some((12, 8)), true),
            ("request 7 reports 8 output tokens", 7, Some((12, 8)), false),
            (
                "maximum reported input",
                8,
                Some((MAX_REPORTED_INPUT_TOKENS, 8)),
                true,
            ),
            (
                "reported input above bound",
                8,
                Some((MAX_REPORTED_INPUT_TOKENS + 1, 8)),
                false,
            ),
            ("missing native usage remains unknown", 8, None, true),
        ] {
            for (operation, bytes) in [("Run", &good_run), ("compaction", &good_compaction)] {
                let mut fixture: Value = serde_json::from_slice(bytes).unwrap();
                if let Some((input, output)) = usage {
                    fixture["usage"] = serde_json::json!({
                        "input_tokens": input,
                        "output_tokens": output
                    });
                } else {
                    fixture.as_object_mut().unwrap().remove("usage");
                }
                let bytes = serde_json::to_vec(&fixture).unwrap();
                let actual = if operation == "Run" {
                    decode_run(&bytes, cap).is_ok()
                } else {
                    provider.decode_compaction(&bytes, cap).is_ok()
                };
                assert_eq!(actual, accepted, "{operation}: {case}");
            }
        }
        let reflected_run = String::from_utf8(synthetic_reply(secret))
            .unwrap()
            .replace("synthetic-native-secret", "\\u0073ynthetic-native-secret")
            .into_bytes();
        assert!(!raw_reflects_secret(&reflected_run, secret));
        assert!(matches!(
            decode_run(&reflected_run, 4096),
            Err(ProviderError::InvalidOutcome | ProviderError::InvalidOutcomeContract)
        ));

        let tool_request = crate::provider::test_support::primary_request("gpt-5.4");
        let conversation = render::conversation(&tool_request).unwrap();
        let decode_tool = |arguments: &str| {
            let call = serde_json::json!({
                "type": "function_call", "id": "fc_safe", "status": "completed",
                "call_id": "call_safe", "name": "arany_mcp_call",
                "arguments": serde_json::json!({
                    "server": "example", "tool": "echo", "schema_digest": "0".repeat(64),
                    "arguments": arguments
                }).to_string(),
            });
            let bytes = serde_json::to_vec(&serde_json::json!({
                "id": "resp_safe", "status": "completed", "model": "gpt-5.4",
                "output": [call], "usage": {"input_tokens": 12, "output_tokens": 8}
            }))
            .unwrap();
            provider.decode_run(&bytes, &tool_request, &conversation)
        };
        assert!(decode_tool(r#"{"value":"safe"}"#).is_ok());
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
            assert!(
                matches!(
                    decode_tool(arguments),
                    Err(ProviderError::InvalidOutcome | ProviderError::InvalidOutcomeContract)
                ),
                "MCP argument rejection: {case}"
            );
        }

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

    #[test]
    fn cached_tokens_above_reported_input_are_rejected() {
        let consistent = Usage {
            input_tokens: Some(12),
            output_tokens: Some(8),
            cache_read_input_tokens: Some(12),
            cache_creation_input_tokens: None,
        };
        assert!(valid_usage(consistent, 4096));
        let inconsistent = Usage {
            cache_read_input_tokens: Some(13),
            ..consistent
        };
        assert!(!valid_usage(inconsistent, 4096));
    }
}
