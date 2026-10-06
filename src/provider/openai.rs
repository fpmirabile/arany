use super::{
    CompactionRequest, CompactionResponse, Effort, MAX_REPORTED_INPUT_TOKENS, MAX_RESPONSE_BYTES,
    Provider, ProviderError, ProviderRequest, ProviderResponse, raw_reflects_secret,
    resolve_native_effort, resolve_native_effort_for_run, valid_saved_api_account_id,
    validate_native_api_key,
};
use reqwest::{Client, redirect::Policy};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

mod subscription;
pub(crate) mod wire;
pub use subscription::ChatGptProvider;

const ENDPOINT: &str = "https://api.openai.com/v1/responses";
const DEADLINE: Duration = Duration::from_secs(120);

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
    model: String,
    effort: Effort,
    key: String,
    saved_api_account_id: Option<Uuid>,
    max_concurrent_calls: u8,
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
        provider.saved_api_account_id = Some(account_id);
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
        provider.saved_api_account_id = saved_api_account_id;
        provider.max_concurrent_calls = 1;
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
        let client = Client::builder()
            .https_only(true)
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .redirect(Policy::none())
            .referer(false)
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(DEADLINE)
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| ProviderError::Unavailable)?;
        let max_concurrent_calls = if resolve_native_effort("openai", &model, Some(effort)).is_ok()
        {
            3
        } else {
            1
        };
        Ok(Self {
            client,
            model,
            effort,
            key,
            saved_api_account_id: None,
            max_concurrent_calls,
        })
    }

    async fn send(&self, request: Value) -> Result<Vec<u8>, ProviderError> {
        let body = wire::encode_request(&request)?;
        let mut response = self
            .client
            .post(ENDPOINT)
            .bearer_auth(&self.key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .body(body)
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        if !response.status().is_success() {
            return Err(if response.status().is_server_error() {
                ProviderError::Unavailable
            } else {
                ProviderError::Rejected
            });
        }
        if response
            .headers()
            .get_all(reqwest::header::CONTENT_ENCODING)
            .iter()
            .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(ProviderError::InvalidOutcome);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ProviderError::InvalidOutcome);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ProviderError::Unavailable)?
        {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(ProviderError::InvalidOutcome);
            }
            bytes.extend_from_slice(&chunk);
        }
        if raw_reflects_secret(&bytes, &self.key) {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(bytes)
    }

    fn decode_run(&self, bytes: &[u8], output_cap: u32) -> Result<ProviderResponse, ProviderError> {
        let response = wire::decode_run(bytes, &self.model)?;
        if !valid_usage(response.input_tokens, response.output_tokens, output_cap)
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
        let response = wire::decode_compaction(bytes, &self.model)?;
        if !valid_usage(response.input_tokens, response.output_tokens, output_cap)
            || response.reflects_secret(&self.key)
        {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(response)
    }
}

fn valid_usage(input: Option<u32>, output: Option<u32>, output_cap: u32) -> bool {
    input.is_none_or(|count| count <= MAX_REPORTED_INPUT_TOKENS)
        && output.is_none_or(|count| count <= output_cap)
}

impl Provider for OpenAiProvider {
    fn profile_name(&self) -> &str {
        "openai"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn reasoning_effort(&self) -> Option<Effort> {
        Some(self.effort)
    }

    fn max_concurrent_calls(&self) -> u8 {
        self.max_concurrent_calls
    }

    fn saved_api_account_id(&self) -> Option<Uuid> {
        self.saved_api_account_id
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        request.validate_scope()?;
        super::image::validate_images(&request)?;
        if request.model != self.model || !(1..=4096).contains(&request.max_output_tokens) {
            return Err(ProviderError::Rejected);
        }
        let response = self
            .send(wire::run_body_with_effort(&request, self.effort))
            .await?;
        if request.tools.is_some() {
            let decoded = wire::decode_tool_run(&response, &self.model)?;
            if !valid_usage(
                decoded.input_tokens,
                decoded.output_tokens,
                request.max_output_tokens,
            ) || decoded.reflects_secret(&self.key)
            {
                return Err(ProviderError::InvalidOutcome);
            }
            Ok(decoded)
        } else {
            self.decode_run(&response, request.max_output_tokens)
        }
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        if request.model != self.model || !(1..=1024).contains(&request.max_output_tokens) {
            return Err(ProviderError::Rejected);
        }
        let response = self
            .send(wire::compaction_body_with_effort(&request, self.effort))
            .await?;
        self.decode_compaction(&response, request.max_output_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(provider.model_name(), "gpt-5.4");
        assert_eq!(provider.reasoning_effort(), Some(Effort::High));
        assert_eq!(provider.key, "synthetic-openai-key");
        assert_eq!(provider.saved_api_account_id(), None);
        let account_id = Uuid::now_v7();
        let saved = OpenAiProvider::from_saved_api_key_with_effort(
            "gpt-5.4".into(),
            Some(Effort::High),
            "synthetic-openai-key".into(),
            account_id,
        )
        .expect("saved API account");
        assert_eq!(saved.saved_api_account_id(), Some(account_id));
        for effort in Effort::ALL {
            let selected = OpenAiProvider::from_saved_api_key_with_effort(
                "future-model".into(),
                Some(effort),
                "synthetic-openai-key".into(),
                account_id,
            )
            .expect("explicit native choice needs no synthetic check");
            assert_eq!(selected.model_name(), "future-model");
            assert_eq!(selected.reasoning_effort(), Some(effort));
            assert_eq!(selected.saved_api_account_id(), Some(account_id));
            assert_eq!(selected.max_concurrent_calls(), 1);
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
        let good_run =
            synthetic_reply(r#"{"outcome":{"type":"finish","summary":"safe","result":"answer"}}"#);
        let good_compaction = synthetic_reply(r#"{"summary":"safe"}"#);
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
                    provider.decode_run(&bytes, cap).is_ok()
                } else {
                    provider.decode_compaction(&bytes, cap).is_ok()
                };
                assert_eq!(actual, accepted, "{operation}: {case}");
            }
        }
        let reflected_run = synthetic_reply(
            r#"{"outcome":{"type":"finish","summary":"\u0073ynthetic-native-secret","result":"answer"}}"#,
        );
        assert!(!raw_reflects_secret(&reflected_run, secret));
        assert!(matches!(
            provider.decode_run(&reflected_run, 4096),
            Err(ProviderError::InvalidOutcome)
        ));

        let tool_rows: Vec<Value> =
            serde_json::from_str(include_str!("../../tests/fixtures/tool-outcomes.json")).unwrap();
        let mut tool = tool_rows[9].clone();
        let good_tool = synthetic_reply(&tool.to_string());
        assert!(provider.decode_run(&good_tool, 4096).is_ok());
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
            tool["outcome"]["call"]["arguments"] = serde_json::json!(arguments);
            let reflected_tool = synthetic_reply(&tool.to_string());
            assert!(!raw_reflects_secret(&reflected_tool, secret));
            assert!(
                matches!(
                    provider.decode_run(&reflected_tool, 4096),
                    Err(ProviderError::InvalidOutcome)
                ),
                "MCP argument rejection: {case}"
            );
        }

        assert!(provider.decode_compaction(&good_compaction, 1024).is_ok());
        let reflected_compaction = synthetic_reply(r#"{"summary":"\u0073ynthetic-native-secret"}"#);
        assert!(!raw_reflects_secret(&reflected_compaction, secret));
        assert!(matches!(
            provider.decode_compaction(&reflected_compaction, 1024),
            Err(ProviderError::InvalidOutcome)
        ));
    }
}
