use super::{
    AgentPhase, AnthropicProvider, CompactionItem, CompactionRequest, Effort, HistoryTurn,
    MAX_REPORTED_INPUT_TOKENS, NativeApiCredentials, OpenAiProvider, PROBE_OUTPUT_CAP,
    PROBE_TOTAL_DEADLINE, PromptRole, Provider, ProviderError, ProviderKind, ProviderOutcome,
    ProviderRequest, RunInput, catalog::valid_model_id, valid_saved_api_account_id,
    validate_native_api_key,
};
use crate::provider::evidence::NativeEvidenceRecord;
use crate::session::{AgentRunId, RunId, SessionId};
use crate::store::{StateRoot, Store};
use crate::{
    CheckCallKind, CheckDisposition, CheckJournal, CheckProbe, CheckStart, CheckTarget, CheckUsage,
    CheckView,
};
use aws_lc_rs::hmac;
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const NATIVE_CHECK_VERSION: &str = "native-goal-contract-outcome-conformance-journal-v37";
const PROBE_COUNT_LABEL: &str = "three";
const EVIDENCE_AGE_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum NativeCheckError {
    #[error("Provider check could not start: {0}")]
    Journal(Box<crate::CheckOpenError>),
    #[error("{source}\n{receipt}")]
    Check {
        source: Box<NativeCheckError>,
        receipt: crate::CheckReceipt,
    },
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
) -> Result<CheckView, NativeCheckError> {
    if !ProviderKind::parse(profile).is_some_and(ProviderKind::is_native_api)
        || !valid_model_id(model)
    {
        return Err(NativeCheckError::InvalidSelection);
    }
    let credentials = NativeApiCredentials::from_env(profile)
        .map_err(|_| NativeCheckError::CredentialUnavailable)?;
    check_native_model(
        root,
        NativeCheckRequest {
            profile: profile.into(),
            model: model.into(),
            effort,
            credentials,
            account_id: None,
        },
        crate::RunCancellation::new(),
        async {
            NativeApiCredentials::from_env(profile)
                .map_err(|_| NativeCheckError::CredentialUnavailable)
        },
    )
    .await
}

pub struct NativeCheckRequest {
    pub profile: String,
    pub model: String,
    pub effort: Effort,
    pub credentials: NativeApiCredentials,
    pub account_id: Option<Uuid>,
}

pub async fn check_native_model(
    root: StateRoot,
    request: NativeCheckRequest,
    mut cancellation: crate::RunCancellation,
    revalidate: impl std::future::Future<Output = Result<NativeApiCredentials, NativeCheckError>>,
) -> Result<CheckView, NativeCheckError> {
    let NativeCheckRequest {
        profile,
        model,
        effort,
        credentials,
        account_id,
    } = request;
    let (profile, model) = (profile.as_str(), model.as_str());
    let fingerprint = fingerprint_credentials(profile, model, effort, &credentials, account_id)?;
    let target = CheckTarget::Native {
        profile: profile.into(),
        account: account_id,
        model: model.into(),
        effort,
        workspace: credentials.anthropic_workspace_id().map(str::to_owned),
    };
    let manifest = start(target, fingerprint)?;
    let mut journal = CheckJournal::start(root, manifest)
        .await
        .map_err(|error| NativeCheckError::Journal(Box::new(error)))?;
    let deadline =
        (tokio::time::Instant::now() + PROBE_TOTAL_DEADLINE).min(journal.request_deadline());
    let mut cancelled = false;
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => { cancelled = true; Err(NativeCheckError::ConformanceFailed) },
        result = tokio::time::timeout_at(deadline, async {
        let catalog =
            super::catalog::list_native_models_checked(profile, &credentials, &mut journal)
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
                probe(&provider, &mut journal).await?;
            }
            "anthropic" => {
                let provider =
                    AnthropicProvider::for_conformance(model.to_owned(), effort, credentials)
                        .map_err(|_| NativeCheckError::ConformanceFailed)?;
                probe(&provider, &mut journal).await?;
            }
            _ => return Err(NativeCheckError::InvalidSelection),
        }
        let refreshed = revalidate.await?;
        if fingerprint_credentials(profile, model, effort, &refreshed, account_id)? != fingerprint { return Err(NativeCheckError::EvidenceUnavailable); }
        let checked_at_ms =
            crate::journal::check::now_ms().map_err(|_| NativeCheckError::EvidenceUnavailable)?;
        let record = NativeEvidenceRecord {
            fingerprint,
            checked_at_ms,
            expires_at_ms: checked_at_ms
                .checked_add(EVIDENCE_AGE_MS)
                .ok_or(NativeCheckError::EvidenceUnavailable)?,
        };
        let digest = crate::store::check::native_digest(&record)
            .map_err(|_| NativeCheckError::EvidenceUnavailable)?;
        journal
            .evidence_started(digest, fingerprint)
            .await
            .map_err(|_| NativeCheckError::EvidenceUnavailable)?;
        journal
            .publish_native(record)
            .await
            .map_err(|_| NativeCheckError::EvidenceUnavailable)?;
        Ok::<_, NativeCheckError>(())
    }) => result.map_err(|_| NativeCheckError::ConformanceFailed).and_then(|result| result),
    };
    if result.is_err() {
        let _ = journal
            .finish(if cancelled {
                CheckDisposition::Cancelled
            } else {
                CheckDisposition::Failed
            })
            .await;
    }
    let receipt = journal.receipt();
    let closed = journal.close().await;
    match (result, closed) {
        (Ok(()), Ok(view)) => Ok(view),
        (Err(source), _) => Err(NativeCheckError::Check {
            source: Box::new(source),
            receipt,
        }),
        (Ok(()), Err(_)) => Err(NativeCheckError::Check {
            source: Box::new(NativeCheckError::EvidenceUnavailable),
            receipt,
        }),
    }
}

fn start(target: CheckTarget, fingerprint: [u8; 32]) -> Result<CheckStart, NativeCheckError> {
    let (profile, effort) = match &target {
        CheckTarget::Native {
            profile, effort, ..
        } => (profile.as_str(), *effort),
        _ => return Err(NativeCheckError::InvalidSelection),
    };
    let probes = [
        CheckCallKind::Answer,
        CheckCallKind::Delegate,
        CheckCallKind::Compaction,
    ]
    .into_iter()
    .map(|kind| CheckProbe {
        kind,
        effort: Some(effort),
        output_tokens: PROBE_OUTPUT_CAP,
    })
    .collect();
    let destination = Sha256::digest(profile.as_bytes()).into();
    let pages = target.catalog_pages();
    let model = match &target {
        CheckTarget::Native { model, .. } => model.clone(),
        _ => return Err(NativeCheckError::InvalidSelection),
    };
    CheckStart::new(target, model, fingerprint, destination, pages, probes)
        .map_err(|_| NativeCheckError::EvidenceUnavailable)
}

pub(super) fn disposition(error: &ProviderError) -> CheckDisposition {
    match error {
        ProviderError::Unavailable
        | ProviderError::Transport
        | ProviderError::PartialTransport
        | ProviderError::CallTimedOut
        | ProviderError::StreamEnded
        | ProviderError::InvalidStream => CheckDisposition::Uncertain,
        ProviderError::Rejected
        | ProviderError::InvalidOutcome
        | ProviderError::InvalidResponseContract
        | ProviderError::InvalidOutcomeContract
        | ProviderError::LocalOutputLimit
        | ProviderError::HttpServerUnavailable(_)
        | ProviderError::RemoteServerError(_, _)
        | ProviderError::RateLimited(_)
        | ProviderError::QuotaExceeded
        | ProviderError::RemoteHttp(_)
        | ProviderError::RemoteStreamCode(_) => CheckDisposition::Rejected,
    }
}
async fn rejected(journal: &mut CheckJournal) -> NativeCheckError {
    if journal
        .reject_pending(CheckDisposition::Rejected)
        .await
        .is_err()
    {
        NativeCheckError::EvidenceUnavailable
    } else {
        NativeCheckError::ConformanceFailed
    }
}
async fn provider_failure(journal: &mut CheckJournal, error: ProviderError) -> NativeCheckError {
    if journal.reject_pending(disposition(&error)).await.is_err() {
        NativeCheckError::EvidenceUnavailable
    } else {
        NativeCheckError::ConformanceFailed
    }
}
fn usage(usage: &crate::Usage) -> Option<CheckUsage> {
    Some(CheckUsage {
        input_tokens: usage.input_tokens?,
        output_tokens: usage.output_tokens?,
    })
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

async fn probe<P: Provider>(
    provider: &P,
    journal: &mut CheckJournal,
) -> Result<(), NativeCheckError> {
    let model = provider.identity().model.to_owned();
    let session_id = SessionId::new();
    let direct = ProviderRequest {
        session_id,
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Single,
        model: model.clone(),
        max_output_tokens: PROBE_OUTPUT_CAP,
        input: RunInput::new(PromptRole::ReadOnly, "Synthetic conformance check: finish directly with a short factual sentence. No Workspace content is supplied.".into()),
    };
    let (preview, observed) = super::ResponsePreview::new();
    let token = journal
        .call_started(CheckCallKind::Answer)
        .await
        .map_err(|_| NativeCheckError::EvidenceUnavailable)?;
    let response = provider.invoke_with_preview(direct.clone(), preview).await;
    let response = match response {
        Ok(value) => value,
        Err(error) => return Err(provider_failure(journal, error).await),
    };
    if !matches!(&response.outcome, ProviderOutcome::Answer(value) if !observed.borrow().answer.is_empty() && value.text.starts_with(&observed.borrow().answer) && !value.text.trim().is_empty() && value.text.len() <= 8 * 1024)
        || !valid_usage(response.usage.input_tokens, response.usage.output_tokens)
    {
        return Err(rejected(journal).await);
    }
    let direct_id = response.response_id.clone();
    let Some(direct_id) = direct_id else {
        return Err(rejected(journal).await);
    };

    journal
        .call_finished(
            token,
            CheckDisposition::Succeeded,
            usage(&response.usage),
            false,
        )
        .await
        .map_err(|_| NativeCheckError::EvidenceUnavailable)?;

    let delegate = ProviderRequest {
        collaboration: crate::session::CollaborationPolicy::Team { max_active_children: 1 },
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        input: RunInput::new(PromptRole::ReadOnly, "Synthetic conformance check: delegate exactly one independent read-only question about the number two. No Workspace content is supplied.".into()),
        ..direct
    };
    let token = journal
        .call_started(CheckCallKind::Delegate)
        .await
        .map_err(|_| NativeCheckError::EvidenceUnavailable)?;
    let response = provider.invoke(delegate).await;
    let response = match response {
        Ok(value) => value,
        Err(error) => return Err(provider_failure(journal, error).await),
    };
    if !matches!(&response.outcome, ProviderOutcome::Delegate(value) if value.children.len() == 1 && !value.children[0].objective.trim().is_empty() && value.children[0].profile.is_none())
        || !valid_usage(response.usage.input_tokens, response.usage.output_tokens)
    {
        return Err(rejected(journal).await);
    }
    let delegate_id = response.response_id.clone();
    let Some(delegate_id) = delegate_id else {
        return Err(rejected(journal).await);
    };
    if delegate_id == direct_id {
        return Err(rejected(journal).await);
    }

    journal
        .call_finished(
            token,
            CheckDisposition::Succeeded,
            usage(&response.usage),
            false,
        )
        .await
        .map_err(|_| NativeCheckError::EvidenceUnavailable)?;

    let compact = CompactionRequest {
        session_id,
        covered_run_id: RunId::new(),
        model,
        previous_summary: None,
        items: vec![CompactionItem::Completed(HistoryTurn {
            user: "Synthetic conformance question".into(),
            assistant: "Synthetic conformance answer".into(),
        })],
        max_output_tokens: PROBE_OUTPUT_CAP,
    };
    let token = journal
        .call_started(CheckCallKind::Compaction)
        .await
        .map_err(|_| NativeCheckError::EvidenceUnavailable)?;
    let response = provider.compact(compact).await;
    let response = match response {
        Ok(value) => value,
        Err(error) => return Err(provider_failure(journal, error).await),
    };
    if response.summary.trim().is_empty()
        || response.summary.len() > 8 * 1024
        || !valid_usage(response.usage.input_tokens, response.usage.output_tokens)
        || response.response_id.as_deref() == Some(&direct_id)
        || response.response_id.as_deref() == Some(&delegate_id)
        || response.response_id.is_none()
    {
        return Err(rejected(journal).await);
    }
    journal
        .call_finished(
            token,
            CheckDisposition::Succeeded,
            usage(&response.usage),
            false,
        )
        .await
        .map_err(|_| NativeCheckError::EvidenceUnavailable)?;

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

fn probe_fingerprint_contract() -> String {
    format!("{PROBE_COUNT_LABEL}-probes-{PROBE_OUTPUT_CAP}-output-tokens")
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
    let contract = probe_fingerprint_contract();
    let mut message = Vec::with_capacity(256);
    for field in [
        NATIVE_CHECK_VERSION,
        env!("CARGO_PKG_VERSION"),
        protocol,
        profile,
        model,
        effort.as_str(),
        &contract,
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
    use crate::provider::{Answer, CompactionResponse, Delegate, ProviderError, ProviderResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn probe_fingerprint_contract_preserves_persisted_bytes() {
        assert_eq!(
            probe_fingerprint_contract().as_bytes(),
            b"three-probes-1024-output-tokens"
        );
    }

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
        assert_eq!(provider.identity().model, "example-model");
        assert_eq!(provider.identity().effort, Some(Effort::High));
        assert_eq!(provider.identity().limits.concurrency, 1);
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
            saved_provider.identity().saved_api_account_id(),
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
        let mut legacy = Vec::new();
        for field in [
            "native-goal-contract-outcome-conformance-v33",
            env!("CARGO_PKG_VERSION"),
            "https://api.anthropic.com/v1/messages;output_config.format",
            "anthropic",
            "example-model",
            "high",
            "three-probes-1024-output-tokens",
            "sequential-concurrency-one",
            "env",
            "wrkspc_One",
        ] {
            legacy.extend_from_slice(&u64::try_from(field.len()).unwrap().to_be_bytes());
            legacy.extend_from_slice(field.as_bytes());
        }
        let signing_key = hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes());
        let legacy: [u8; 32] = hmac::sign(&signing_key, &legacy)
            .as_ref()
            .try_into()
            .unwrap();
        let store = Store::open(open()).expect("reopen Store");
        store
            .record_native_evidence(NativeEvidenceRecord {
                fingerprint: legacy,
                checked_at_ms,
                expires_at_ms: checked_at_ms + 60_000,
            })
            .await
            .unwrap();
        store.close().await.unwrap();
        assert!(matches!(
            AnthropicProvider::from_checked_credentials_with_effort(
                open(),
                "example-model".into(),
                Effort::High,
                credentials(),
                None,
            )
            .await,
            Err(ProviderError::Rejected)
        ));
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
        assert_eq!(selected.identity().limits.concurrency, 1);
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
        EmptyAnswer,
        MissingUsage,
        WrongDelegation,
        DuplicateId,
        BadCompaction,
    }

    struct ProbeFixture {
        case: ProbeCase,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for ProbeFixture {
        fn identity(&self) -> &crate::provider::ProviderIdentity {
            static IDENTITY: std::sync::LazyLock<crate::provider::ProviderIdentity> =
                std::sync::LazyLock::new(|| {
                    crate::provider::ProviderIdentity::new(
                        "openai",
                        "synthetic-model",
                        crate::provider::Provenance::native(),
                    )
                });
            &IDENTITY
        }

        async fn invoke(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            assert_eq!(request.model, "synthetic-model");
            assert_eq!(request.max_output_tokens, PROBE_OUTPUT_CAP);
            assert!(
                request.input.workspace_guidance.is_empty()
                    && request.input.includes.is_empty()
                    && request.input.history.is_empty()
                    && request.input.context_summary.is_none()
                    && request.input.child_results.is_empty()
            );
            let step = self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = if step == 0 || matches!(self.case, ProbeCase::WrongDelegation) {
                ProviderOutcome::Answer(Answer {
                    text: if matches!(self.case, ProbeCase::EmptyAnswer) {
                        String::new()
                    } else {
                        "two".into()
                    },
                })
            } else {
                ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![crate::ChildAssignment {
                        editor: None,
                        objective: "What is two?".into(),
                        profile: None,
                    }],
                })
            };
            Ok(ProviderResponse {
                turn: crate::provider::ProviderTurn::scripted(),
                outcome,
                response_id: Some(
                    if step == 0 || step == 1 && matches!(self.case, ProbeCase::DuplicateId) {
                        "direct"
                    } else {
                        "delegate"
                    }
                    .into(),
                ),
                usage: crate::provider::Usage {
                    input_tokens: if matches!(self.case, ProbeCase::MissingUsage) {
                        None
                    } else {
                        Some(12)
                    },
                    output_tokens: Some(8),
                    ..Default::default()
                },
                wire_provenance: None,
            })
        }

        async fn invoke_with_preview(
            &self,
            request: ProviderRequest,
            preview: super::super::ResponsePreview,
        ) -> Result<ProviderResponse, ProviderError> {
            let response = self.invoke(request).await?;
            if let ProviderOutcome::Answer(finish) = &response.outcome {
                preview.publish(super::super::PreviewText {
                    answer: finish.text.clone(),
                    ..Default::default()
                })?;
            }
            Ok(response)
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
                usage: crate::provider::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(8),
                    ..Default::default()
                },
                wire_provenance: None,
            })
        }
    }

    #[tokio::test]
    async fn synthetic_probe_accepts_only_three_distinct_bounded_strict_outcomes() {
        for (case, expected, calls) in [
            (ProbeCase::Valid, true, 3),
            (ProbeCase::EmptyAnswer, false, 1),
            (ProbeCase::MissingUsage, false, 1),
            (ProbeCase::WrongDelegation, false, 2),
            (ProbeCase::DuplicateId, false, 2),
            (ProbeCase::BadCompaction, false, 3),
        ] {
            let fixture = ProbeFixture {
                case,
                calls: AtomicUsize::new(0),
            };
            let temp = tempfile::tempdir().expect("check state");
            let target = CheckTarget::Native {
                profile: "openai".into(),
                account: None,
                model: fixture.identity().model.to_owned(),
                effort: Effort::High,
                workspace: None,
            };
            let mut journal = CheckJournal::start(
                StateRoot::admit(&temp.path().join("state")).expect("state"),
                start(target, [1; 32]).expect("manifest"),
            )
            .await
            .expect("journal");
            let token = journal
                .call_started(CheckCallKind::Catalog)
                .await
                .expect("catalog intent");
            journal
                .call_finished(token, CheckDisposition::Succeeded, None, true)
                .await
                .expect("catalog result");
            assert_eq!(probe(&fixture, &mut journal).await.is_ok(), expected);
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
