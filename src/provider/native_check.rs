use super::{
    AgentPhase, AnthropicProvider, CompactionItem, CompactionRequest, Effort, HistoryTurn,
    MAX_REPORTED_INPUT_TOKENS, NativeApiCredentials, OpenAiProvider, Provider, ProviderError,
    ProviderOutcome, ProviderRequest, catalog::valid_model_id, list_native_models_with_credentials,
    valid_saved_api_account_id, validate_native_api_key,
};
use crate::session::{AgentRunId, RunId, SessionId};
use crate::store::{NativeEvidenceRecord, StateRoot, Store};
use aws_lc_rs::hmac;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const CHECK_VERSION: &str = "native-strict-outcome-conformance-v7";
const TOTAL_DEADLINE: Duration = Duration::from_secs(180);
const PROBE_OUTPUT_CAP: u32 = 1024;
const EVIDENCE_AGE_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum NativeCheckError {
    #[error("native Provider, model, or effort is invalid")]
    InvalidSelection,
    #[error("selected native API key is unavailable or invalid")]
    CredentialUnavailable,
    #[error("native model catalog is unavailable or invalid")]
    CatalogUnavailable,
    #[error("model is not available to the selected account")]
    ModelUnavailable,
    #[error("synthetic native conformance failed")]
    ConformanceFailed,
    #[error("native conformance evidence is unavailable")]
    EvidenceUnavailable,
}

/// Makes up to three potentially billable synthetic inference calls. The caller must obtain explicit cost consent.
pub async fn check_native_model_from_env(
    root: StateRoot,
    profile: &str,
    model: &str,
    effort: Effort,
) -> Result<(), NativeCheckError> {
    if !matches!(profile, "openai" | "anthropic") || !valid_model_id(model) {
        return Err(NativeCheckError::InvalidSelection);
    }
    let credentials = NativeApiCredentials::from_env(profile)
        .map_err(|_| NativeCheckError::CredentialUnavailable)?;
    check_native_model_with_credentials(root, profile, model, effort, credentials, None).await
}

/// Makes up to three potentially billable synthetic inference calls with the selected API key.
/// `account_id` binds a saved account; `None` binds an environment-backed selection.
/// The caller must obtain explicit cost consent before invoking this function.
pub async fn check_native_model_with_api_key(
    root: StateRoot,
    profile: &str,
    model: &str,
    effort: Effort,
    key: String,
    account_id: Option<Uuid>,
) -> Result<(), NativeCheckError> {
    if !matches!(profile, "openai" | "anthropic")
        || !valid_model_id(model)
        || !valid_saved_api_account_id(profile, account_id)
    {
        return Err(NativeCheckError::InvalidSelection);
    }
    let credentials =
        NativeApiCredentials::new(profile, key, None).map_err(|error| match error {
            ProviderError::Rejected => NativeCheckError::InvalidSelection,
            _ => NativeCheckError::CredentialUnavailable,
        })?;
    check_native_model_with_credentials(root, profile, model, effort, credentials, account_id).await
}

/// Checks one explicitly selected native credential scope after the caller obtains cost consent.
pub async fn check_native_model_with_credentials(
    root: StateRoot,
    profile: &str,
    model: &str,
    effort: Effort,
    credentials: NativeApiCredentials,
    account_id: Option<Uuid>,
) -> Result<(), NativeCheckError> {
    let fingerprint = fingerprint_credentials(profile, model, effort, &credentials, account_id)?;
    let store = Store::open(root).map_err(|_| NativeCheckError::EvidenceUnavailable)?;
    let result = async {
        store
            .clear_native_evidence(fingerprint)
            .await
            .map_err(|_| NativeCheckError::EvidenceUnavailable)?;
        tokio::time::timeout(TOTAL_DEADLINE, async move {
            let catalog = list_native_models_with_credentials(profile, &credentials)
                .await
                .map_err(|_| NativeCheckError::CatalogUnavailable)?;
            if !catalog.iter().any(|entry| entry.id == model) {
                return Err(NativeCheckError::ModelUnavailable);
            }
            match profile {
                "openai" => {
                    let provider = OpenAiProvider::for_conformance(
                        model.to_owned(),
                        effort,
                        credentials.into_api_key(),
                    )
                    .map_err(|_| NativeCheckError::ConformanceFailed)?;
                    probe(&provider).await
                }
                "anthropic" => {
                    let provider =
                        AnthropicProvider::for_conformance(model.to_owned(), effort, credentials)
                            .map_err(|_| NativeCheckError::ConformanceFailed)?;
                    probe(&provider).await
                }
                _ => Err(NativeCheckError::InvalidSelection),
            }
        })
        .await
        .map_err(|_| NativeCheckError::ConformanceFailed)??;
        let checked_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| NativeCheckError::EvidenceUnavailable)?
            .as_millis();
        let checked_at_ms =
            i64::try_from(checked_at_ms).map_err(|_| NativeCheckError::EvidenceUnavailable)?;
        let record = NativeEvidenceRecord {
            fingerprint,
            checked_at_ms,
            expires_at_ms: checked_at_ms
                .checked_add(EVIDENCE_AGE_MS)
                .ok_or(NativeCheckError::EvidenceUnavailable)?,
        };
        store
            .record_native_evidence(record.clone())
            .await
            .map_err(|_| NativeCheckError::EvidenceUnavailable)?;
        if store
            .load_native_evidence(fingerprint)
            .await
            .map_err(|_| NativeCheckError::EvidenceUnavailable)?
            != Some(record)
        {
            return Err(NativeCheckError::EvidenceUnavailable);
        }
        Ok(())
    }
    .await;
    let closed = store.close().await;
    result?;
    closed.map_err(|_| NativeCheckError::EvidenceUnavailable)
}

pub(super) async fn verify_for_run(
    root: StateRoot,
    profile: &str,
    model: &str,
    effort: Effort,
    key: &str,
    account_id: Option<Uuid>,
) -> Result<(), ProviderError> {
    let credentials = NativeApiCredentials::new(profile, key.to_owned(), None)?;
    verify_credentials_for_run(root, profile, model, effort, &credentials, account_id).await
}

pub(super) async fn verify_credentials_for_run(
    root: StateRoot,
    profile: &str,
    model: &str,
    effort: Effort,
    credentials: &NativeApiCredentials,
    account_id: Option<Uuid>,
) -> Result<(), ProviderError> {
    let fingerprint = fingerprint_credentials(profile, model, effort, credentials, account_id)
        .map_err(|error| {
            if matches!(error, NativeCheckError::InvalidSelection) {
                ProviderError::Rejected
            } else {
                ProviderError::Unavailable
            }
        })?;
    let store = Store::open_read_only(root).map_err(|_| ProviderError::Unavailable)?;
    let result = store
        .load_native_evidence(fingerprint)
        .await
        .map_err(|_| ProviderError::Unavailable)?
        .ok_or(ProviderError::Rejected);
    let closed = store.close().await;
    result?;
    closed.map_err(|_| ProviderError::Unavailable)
}

async fn probe<P: Provider>(provider: &P) -> Result<(), NativeCheckError> {
    let model = provider.model_name().to_owned();
    let direct = ProviderRequest {
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Single,
        model: model.clone(),
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
    let response = provider
        .invoke(direct.clone())
        .await
        .map_err(|_| NativeCheckError::ConformanceFailed)?;
    if !matches!(&response.outcome, ProviderOutcome::Finish(value) if !value.summary.trim().is_empty() && !value.result.trim().is_empty() && value.summary.len() <= 8 * 1024 && value.result.len() <= 8 * 1024)
        || !valid_usage(response.input_tokens, response.output_tokens)
    {
        return Err(NativeCheckError::ConformanceFailed);
    }
    let direct_id = response
        .response_id
        .ok_or(NativeCheckError::ConformanceFailed)?;

    let delegate = ProviderRequest {
        collaboration: crate::session::CollaborationPolicy::Team { max_active_children: 1 },
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        objective: "Synthetic conformance check: delegate exactly one independent read-only question about the number two. No Workspace content is supplied.".into(),
        ..direct
    };
    let response = provider
        .invoke(delegate)
        .await
        .map_err(|_| NativeCheckError::ConformanceFailed)?;
    if !matches!(&response.outcome, ProviderOutcome::Delegate(value) if value.children.len() == 1 && !value.children[0].trim().is_empty())
        || !valid_usage(response.input_tokens, response.output_tokens)
    {
        return Err(NativeCheckError::ConformanceFailed);
    }
    let delegate_id = response
        .response_id
        .ok_or(NativeCheckError::ConformanceFailed)?;
    if delegate_id == direct_id {
        return Err(NativeCheckError::ConformanceFailed);
    }

    let compact = CompactionRequest {
        session_id: SessionId::new(),
        covered_run_id: RunId::new(),
        model,
        previous_summary: None,
        items: vec![CompactionItem::Completed(HistoryTurn {
            user: "Synthetic conformance question".into(),
            assistant: "Synthetic conformance answer".into(),
        })],
        max_output_tokens: PROBE_OUTPUT_CAP,
    };
    let response = provider
        .compact(compact)
        .await
        .map_err(|_| NativeCheckError::ConformanceFailed)?;
    if response.summary.trim().is_empty()
        || response.summary.len() > 8 * 1024
        || !valid_usage(response.input_tokens, response.output_tokens)
        || response.response_id.as_deref() == Some(&direct_id)
        || response.response_id.as_deref() == Some(&delegate_id)
        || response.response_id.is_none()
    {
        return Err(NativeCheckError::ConformanceFailed);
    }
    Ok(())
}

fn valid_usage(input: Option<u32>, output: Option<u32>) -> bool {
    input.is_some_and(|count| (1..=MAX_REPORTED_INPUT_TOKENS).contains(&count))
        && output.is_some_and(|count| count > 0 && count <= PROBE_OUTPUT_CAP)
}

#[cfg(test)]
fn fingerprint(
    profile: &str,
    model: &str,
    effort: Effort,
    key: &str,
    account_id: Option<Uuid>,
) -> Result<[u8; 32], NativeCheckError> {
    fingerprint_with_workspace(profile, model, effort, key, account_id, None)
}

fn fingerprint_with_workspace(
    profile: &str,
    model: &str,
    effort: Effort,
    key: &str,
    account_id: Option<Uuid>,
    workspace_id: Option<&str>,
) -> Result<[u8; 32], NativeCheckError> {
    let protocol = match profile {
        "openai" => "https://api.openai.com/v1/responses;json_schema",
        "anthropic" => "https://api.anthropic.com/v1/messages;output_config.format",
        _ => return Err(NativeCheckError::InvalidSelection),
    };
    if !valid_model_id(model) || !valid_saved_api_account_id(profile, account_id) {
        return Err(NativeCheckError::InvalidSelection);
    }
    validate_native_api_key(key).map_err(|_| NativeCheckError::CredentialUnavailable)?;
    let mut message = Vec::with_capacity(256);
    for field in [
        CHECK_VERSION,
        env!("CARGO_PKG_VERSION"),
        protocol,
        profile,
        model,
        effort.as_str(),
        "three-probes-1024-output-tokens",
        "sequential-concurrency-one",
        if account_id.is_some() { "saved" } else { "env" },
        workspace_id.unwrap_or(""),
    ] {
        message.extend_from_slice(&(field.len() as u64).to_be_bytes());
        message.extend_from_slice(field.as_bytes());
    }
    if let Some(id) = account_id {
        message.extend_from_slice(id.as_bytes());
    }
    let signing_key = hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes());
    let tag = hmac::sign(&signing_key, &message);
    tag.as_ref()
        .try_into()
        .map_err(|_| NativeCheckError::EvidenceUnavailable)
}

fn fingerprint_credentials(
    profile: &str,
    model: &str,
    effort: Effort,
    credentials: &NativeApiCredentials,
    account_id: Option<Uuid>,
) -> Result<[u8; 32], NativeCheckError> {
    credentials
        .require_profile(profile)
        .map_err(|_| NativeCheckError::InvalidSelection)?;
    fingerprint_with_workspace(
        profile,
        model,
        effort,
        credentials.api_key(),
        account_id,
        credentials.anthropic_workspace_id(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CompactionResponse, Delegate, Finish, ProviderError, ProviderResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn native_run_admission_rechecks_exact_private_evidence() {
        let temp = tempfile::tempdir().expect("private root");
        let path = temp.path().join("state");
        let store = Store::open(StateRoot::admit(&path).expect("state root")).expect("Store");
        let key = "synthetic-native-key";
        let checked_at_ms = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        store
            .record_native_evidence(NativeEvidenceRecord {
                fingerprint: fingerprint("openai", "example-model", Effort::High, key, None)
                    .unwrap(),
                checked_at_ms,
                expires_at_ms: checked_at_ms + 60_000,
            })
            .await
            .expect("synthetic evidence");
        store.close().await.expect("close Store");

        let open = || StateRoot::open_existing(&path).expect("state root");
        let provider = OpenAiProvider::from_checked_api_key_with_effort(
            open(),
            "example-model".into(),
            Effort::High,
            key.into(),
            None,
        )
        .await
        .expect("exact checked selection");
        assert_eq!(provider.model_name(), "example-model");
        assert_eq!(provider.reasoning_effort(), Some(Effort::High));
        assert_eq!(provider.max_concurrent_calls(), 1);
        for (model, effort, selected_key, account_id) in [
            ("other-model", Effort::High, key, None),
            ("example-model", Effort::Low, key, None),
            ("example-model", Effort::High, "other-key", None),
            ("example-model", Effort::High, key, Some(Uuid::now_v7())),
        ] {
            assert!(matches!(
                OpenAiProvider::from_checked_api_key_with_effort(
                    open(),
                    model.into(),
                    effort,
                    selected_key.into(),
                    account_id,
                )
                .await,
                Err(ProviderError::Rejected)
            ));
        }
        assert!(matches!(
            AnthropicProvider::from_checked_api_key_with_effort(
                open(),
                "example-model".into(),
                Effort::High,
                key.into(),
                None,
            )
            .await,
            Err(ProviderError::Rejected)
        ));

        let saved_account_id = Uuid::now_v7();
        let store = Store::open(open()).expect("reopen Store");
        store
            .record_native_evidence(NativeEvidenceRecord {
                fingerprint: fingerprint(
                    "openai",
                    "example-model",
                    Effort::High,
                    key,
                    Some(saved_account_id),
                )
                .unwrap(),
                checked_at_ms,
                expires_at_ms: checked_at_ms + 60_000,
            })
            .await
            .expect("saved-account evidence");
        store.close().await.expect("close Store");
        let saved_provider = OpenAiProvider::from_checked_api_key_with_effort(
            open(),
            "example-model".into(),
            Effort::High,
            key.into(),
            Some(saved_account_id),
        )
        .await
        .expect("saved account exact evidence");
        assert_eq!(
            saved_provider.saved_api_account_id(),
            Some(saved_account_id)
        );
        assert!(matches!(
            OpenAiProvider::from_checked_api_key_with_effort(
                open(),
                "example-model".into(),
                Effort::High,
                key.into(),
                Some(Uuid::now_v7()),
            )
            .await,
            Err(ProviderError::Rejected)
        ));
        let credentials = || {
            NativeApiCredentials::new("anthropic", key.into(), Some("wrkspc_One".into())).unwrap()
        };
        let store = Store::open(open()).expect("reopen Store");
        store
            .record_native_evidence(NativeEvidenceRecord {
                fingerprint: fingerprint_credentials(
                    "anthropic",
                    "example-model",
                    Effort::High,
                    &credentials(),
                    None,
                )
                .unwrap(),
                checked_at_ms,
                expires_at_ms: checked_at_ms + 60_000,
            })
            .await
            .expect("workspace-bound evidence");
        store.close().await.expect("close Store");
        let selected = AnthropicProvider::from_checked_credentials_with_effort(
            open(),
            "example-model".into(),
            Effort::High,
            credentials(),
            None,
        )
        .await
        .expect("same workspace evidence");
        assert_eq!(selected.max_concurrent_calls(), 1);
        for workspace in [None, Some("wrkspc_Two")] {
            let credentials =
                NativeApiCredentials::new("anthropic", key.into(), workspace.map(str::to_owned))
                    .unwrap();
            assert!(
                matches!(
                    AnthropicProvider::from_checked_credentials_with_effort(
                        open(),
                        "example-model".into(),
                        Effort::High,
                        credentials,
                        None
                    )
                    .await,
                    Err(ProviderError::Rejected)
                ),
                "another workspace must not reuse evidence"
            );
        }
    }

    #[derive(Clone, Copy)]
    enum ProbeCase {
        Valid,
        EmptyFinish,
        MissingUsage,
        WrongDelegation,
        DuplicateId,
        BadCompaction,
    }

    struct ProbeFixture {
        case: ProbeCase,
        calls: AtomicUsize,
    }

    impl Provider for ProbeFixture {
        fn profile_name(&self) -> &str {
            "openai"
        }

        fn model_name(&self) -> &str {
            "synthetic-model"
        }

        fn max_concurrent_calls(&self) -> u8 {
            1
        }

        async fn invoke(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            assert_eq!(request.model, "synthetic-model");
            assert_eq!(request.max_output_tokens, PROBE_OUTPUT_CAP);
            assert!(
                request.instructions.is_none()
                    && request.includes.is_empty()
                    && request.history.is_empty()
                    && request.context_summary.is_none()
                    && request.child_results.is_empty()
            );
            let step = self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = if step == 0 || matches!(self.case, ProbeCase::WrongDelegation) {
                ProviderOutcome::Finish(Finish {
                    summary: "safe".into(),
                    result: if matches!(self.case, ProbeCase::EmptyFinish) {
                        String::new()
                    } else {
                        "two".into()
                    },
                })
            } else {
                ProviderOutcome::Delegate(Delegate {
                    children: vec!["What is two?".into()],
                })
            };
            Ok(ProviderResponse {
                outcome,
                response_id: Some(
                    if step == 0 || step == 1 && matches!(self.case, ProbeCase::DuplicateId) {
                        "direct"
                    } else {
                        "delegate"
                    }
                    .into(),
                ),
                input_tokens: if matches!(self.case, ProbeCase::MissingUsage) {
                    None
                } else {
                    Some(12)
                },
                output_tokens: Some(8),
                wire_provenance: None,
            })
        }

        async fn compact(
            &self,
            request: CompactionRequest,
        ) -> Result<CompactionResponse, ProviderError> {
            assert_eq!(request.model, "synthetic-model");
            assert_eq!(request.max_output_tokens, PROBE_OUTPUT_CAP);
            assert!(request.previous_summary.is_none());
            assert_eq!(request.items.len(), 1);
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompactionResponse {
                summary: if matches!(self.case, ProbeCase::BadCompaction) {
                    String::new()
                } else {
                    "safe summary".into()
                },
                response_id: Some("compaction".into()),
                input_tokens: Some(12),
                output_tokens: Some(8),
                wire_provenance: None,
            })
        }
    }

    #[tokio::test]
    async fn synthetic_probe_accepts_only_three_distinct_bounded_strict_outcomes() {
        for (case, expected, calls) in [
            (ProbeCase::Valid, true, 3),
            (ProbeCase::EmptyFinish, false, 1),
            (ProbeCase::MissingUsage, false, 1),
            (ProbeCase::WrongDelegation, false, 2),
            (ProbeCase::DuplicateId, false, 2),
            (ProbeCase::BadCompaction, false, 3),
        ] {
            let fixture = ProbeFixture {
                case,
                calls: AtomicUsize::new(0),
            };
            assert_eq!(probe(&fixture).await.is_ok(), expected);
            assert_eq!(fixture.calls.load(Ordering::SeqCst), calls);
        }
        assert!(!valid_usage(Some(1), Some(PROBE_OUTPUT_CAP + 1)));
        assert!(!valid_usage(Some(MAX_REPORTED_INPUT_TOKENS + 1), Some(1)));
    }

    #[test]
    fn evidence_fingerprint_binds_exact_selection_and_key_without_exposing_it() {
        let key = "synthetic-native-key";
        let workspace_one =
            NativeApiCredentials::new("anthropic", key.into(), Some("wrkspc_One".into())).unwrap();
        let workspace_two =
            NativeApiCredentials::new("anthropic", key.into(), Some("wrkspc_Two".into())).unwrap();
        let scoped = fingerprint_credentials(
            "anthropic",
            "example-model",
            Effort::High,
            &workspace_one,
            None,
        )
        .unwrap();
        assert_ne!(
            scoped,
            fingerprint_credentials(
                "anthropic",
                "example-model",
                Effort::High,
                &workspace_two,
                None
            )
            .unwrap(),
            "workspace scope must bind evidence"
        );
        assert_ne!(
            scoped,
            fingerprint("anthropic", "example-model", Effort::High, key, None).unwrap(),
            "an omitted workspace is a distinct credential scope"
        );
        let baseline = fingerprint("openai", "example-model", Effort::High, key, None).unwrap();
        let saved_account = Uuid::now_v7();
        for different in [
            fingerprint("openai", "example-model", Effort::Low, key, None).unwrap(),
            fingerprint("openai", "other-model", Effort::High, key, None).unwrap(),
            fingerprint("anthropic", "example-model", Effort::High, key, None).unwrap(),
            fingerprint("openai", "example-model", Effort::High, "other-key", None).unwrap(),
            fingerprint(
                "openai",
                "example-model",
                Effort::High,
                key,
                Some(saved_account),
            )
            .unwrap(),
        ] {
            assert_ne!(baseline, different);
        }
        assert_eq!(
            baseline,
            fingerprint("openai", "example-model", Effort::High, key, None).unwrap()
        );
        assert_ne!(
            fingerprint(
                "openai",
                "example-model",
                Effort::High,
                key,
                Some(saved_account)
            )
            .unwrap(),
            fingerprint(
                "openai",
                "example-model",
                Effort::High,
                key,
                Some(Uuid::now_v7())
            )
            .unwrap()
        );
        assert!(matches!(
            fingerprint("custom", "x", Effort::High, key, None),
            Err(NativeCheckError::InvalidSelection)
        ));
        assert!(matches!(
            fingerprint("openai", "bad\nmodel", Effort::High, key, None),
            Err(NativeCheckError::InvalidSelection)
        ));
        assert!(matches!(
            fingerprint("openai", "x", Effort::High, "bad key", None),
            Err(NativeCheckError::CredentialUnavailable)
        ));
        assert!(matches!(
            fingerprint("openai", "x", Effort::High, key, Some(Uuid::nil())),
            Err(NativeCheckError::InvalidSelection)
        ));
    }
}
