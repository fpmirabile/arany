use super::{
    CompactionRequest, CompactionResponse, Effort, MAX_RESPONSE_BYTES, NativeApiCredentials,
    Provider, ProviderError, ProviderRequest, ProviderResponse, raw_reflects_secret,
    resolve_native_effort, resolve_native_effort_for_run, valid_saved_api_account_id,
    validate_native_api_key,
};
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

mod wire;

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const DEADLINE: Duration = Duration::from_secs(120);

pub struct AnthropicProvider {
    client: Client,
    model: String,
    effort: Effort,
    key: String,
    workspace_id: Option<String>,
    saved_api_account_id: Option<Uuid>,
    max_concurrent_calls: u8,
}

impl AnthropicProvider {
    pub(super) fn for_conformance(
        model: String,
        effort: Effort,
        credentials: NativeApiCredentials,
    ) -> Result<Self, ProviderError> {
        if !super::catalog::valid_model_id(&model) {
            return Err(ProviderError::Rejected);
        }
        Self::with_resolved_effort(model, effort, credentials)
    }

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
        provider.saved_api_account_id = Some(account_id);
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
        provider.saved_api_account_id = saved_api_account_id;
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
        if !super::catalog::valid_model_id(&model)
            || !valid_saved_api_account_id("anthropic", saved_api_account_id)
        {
            return Err(ProviderError::Rejected);
        }
        Self::from_checked_credentials_with_effort(
            root,
            model,
            effort,
            NativeApiCredentials::new("anthropic", key, None)?,
            saved_api_account_id,
        )
        .await
    }

    /// Requires evidence for this exact key, API workspace, model, effort and account source.
    pub async fn from_checked_credentials_with_effort(
        root: crate::store::StateRoot,
        model: String,
        effort: Effort,
        credentials: NativeApiCredentials,
        saved_api_account_id: Option<Uuid>,
    ) -> Result<Self, ProviderError> {
        super::native_check::verify_credentials_for_run(
            root,
            "anthropic",
            &model,
            effort,
            &credentials,
            saved_api_account_id,
        )
        .await?;
        let mut provider = Self::with_resolved_effort(model, effort, credentials)?;
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
        let credentials = NativeApiCredentials::from_env("anthropic")?;
        Self::from_checked_credentials_with_effort(root, model, effort, credentials, None).await
    }

    fn with_resolved_effort(
        model: String,
        effort: Effort,
        credentials: NativeApiCredentials,
    ) -> Result<Self, ProviderError> {
        credentials.require_profile("anthropic")?;
        validate_native_api_key(credentials.api_key())?;
        let client = super::http_client(DEADLINE)
            .https_only(true)
            .build()
            .map_err(|_| ProviderError::Unavailable)?;
        let max_concurrent_calls =
            if resolve_native_effort("anthropic", &model, Some(effort)).is_ok() {
                3
            } else {
                1
            };
        Ok(Self {
            client,
            model,
            effort,
            key: credentials.key,
            workspace_id: credentials.anthropic_workspace_id,
            saved_api_account_id: None,
            max_concurrent_calls,
        })
    }

    fn request(&self, request: Value) -> Result<reqwest::RequestBuilder, ProviderError> {
        let body = wire::encode_request(&request)?;
        let request = self
            .client
            .post(ENDPOINT)
            .bearer_auth(&self.key)
            .header("anthropic-version", "2023-06-01")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .body(body);
        Ok(match self.workspace_id.as_deref() {
            Some(id) => request.header("anthropic-workspace-id", id),
            None => request,
        })
    }

    async fn send(&self, request: Value) -> Result<Vec<u8>, ProviderError> {
        let mut response = self
            .request(request)?
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        if !response.status().is_success() {
            return Err(
                if response.status().is_server_error()
                    || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                {
                    ProviderError::Unavailable
                } else {
                    ProviderError::Rejected
                },
            );
        }
        let bytes = super::read_identity_body(&mut response, MAX_RESPONSE_BYTES).await?;
        if raw_reflects_secret(&bytes, &self.key) {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(bytes)
    }

    fn decode_run(&self, bytes: &[u8], output_cap: u32) -> Result<ProviderResponse, ProviderError> {
        let response = wire::decode_run(bytes, &self.model, output_cap)?;
        if response.reflects_secret(&self.key) {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(response)
    }

    fn decode_compaction(
        &self,
        bytes: &[u8],
        output_cap: u32,
    ) -> Result<CompactionResponse, ProviderError> {
        let response = wire::decode_compaction(bytes, &self.model, output_cap)?;
        if response.reflects_secret(&self.key) {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(response)
    }
}

impl Provider for AnthropicProvider {
    fn profile_name(&self) -> &str {
        "anthropic"
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
        let response = self.send(wire::run_body(&request, self.effort)).await?;
        self.decode_run(&response, request.max_output_tokens)
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        if request.model != self.model || !(1..=4096).contains(&request.max_output_tokens) {
            return Err(ProviderError::Rejected);
        }
        let response = self
            .send(wire::compaction_body(&request, self.effort))
            .await?;
        self.decode_compaction(&response, request.max_output_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(provider.model_name(), "claude-sonnet-5");
        assert_eq!(provider.reasoning_effort(), Some(Effort::Max));
        assert_eq!(provider.key, "synthetic-anthropic-key");
        assert_eq!(provider.saved_api_account_id(), None);
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
        assert_eq!(request.url().as_str(), ENDPOINT);
        assert_eq!(
            request
                .headers()
                .get("anthropic-workspace-id")
                .and_then(|value| value.to_str().ok()),
            Some("wrkspc_Fixture"),
            "the shared Run and compaction request must retain scope"
        );
        let account_id = Uuid::now_v7();
        let saved = AnthropicProvider::from_saved_api_key_with_effort(
            "claude-sonnet-5".into(),
            Some(Effort::Max),
            "synthetic-anthropic-key".into(),
            account_id,
        )
        .expect("saved API account");
        assert_eq!(saved.saved_api_account_id(), Some(account_id));
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
            assert_eq!(selected.reasoning_effort(), Some(effort));
            assert_eq!(selected.saved_api_account_id(), Some(account_id));
            assert_eq!(selected.max_concurrent_calls(), 1);
            assert_eq!(selected.workspace_id.as_deref(), Some("wrkspc_Fixture"));
        }
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

    #[test]
    fn decoded_native_outcomes_reject_escaped_key_reflection() {
        let secret = "synthetic-native-secret";
        let provider = AnthropicProvider::from_api_key_with_effort(
            "claude-sonnet-5".into(),
            None,
            secret.into(),
        )
        .expect("selected key");
        let good_run =
            synthetic_reply(r#"{"outcome":{"type":"finish","summary":"safe","result":"answer"}}"#);
        assert!(provider.decode_run(&good_run, 4096).is_ok());
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

        let good_compaction = synthetic_reply(r#"{"summary":"safe"}"#);
        assert!(provider.decode_compaction(&good_compaction, 1024).is_ok());
        let reflected_compaction = synthetic_reply(r#"{"summary":"\u0073ynthetic-native-secret"}"#);
        assert!(!raw_reflects_secret(&reflected_compaction, secret));
        assert!(matches!(
            provider.decode_compaction(&reflected_compaction, 1024),
            Err(ProviderError::InvalidOutcome)
        ));
    }
}
