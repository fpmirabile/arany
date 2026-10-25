use super::{
    CustomProfile, CustomProfileError, check::fingerprint, transport::CustomTransport,
    transport::TransportError,
};
use crate::provider::{
    CompactionRequest, CompactionResponse, CustomProfileProvenance, Effort, Provenance, Provider,
    ProviderError, ProviderIdentity, ProviderKind, ProviderRequest, ProviderResponse,
};
use crate::store::{StateRoot, Store};

pub struct CustomProvider {
    identity: ProviderIdentity,
    transport: CustomTransport,
}

impl CustomProvider {
    pub async fn admit(
        root: &StateRoot,
        profile_name: &str,
        selected_model: &str,
        selected_effort: Option<Effort>,
    ) -> Result<Self, CustomProfileError> {
        let profile = CustomProfile::load_named(root, profile_name)?;
        if profile.model() != selected_model {
            return Err(CustomProfileError::ModelMismatch);
        }
        if !profile.admits_effort(selected_effort) {
            return Err(CustomProfileError::EffortUnavailable);
        }
        let destination = profile.resolve_destination().await?;
        let store = Store::open_read_only(
            root.try_clone()
                .map_err(|_| CustomProfileError::EvidenceUnavailable)?,
        )
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
        let evidence = store
            .load_provider_evidence(profile.name().to_owned())
            .await;
        let closed = store.close().await;
        let evidence = evidence.map_err(|_| CustomProfileError::EvidenceUnavailable)?;
        closed.map_err(|_| CustomProfileError::EvidenceUnavailable)?;
        let expected_digest = fingerprint(&profile);
        let evidence = evidence.ok_or(CustomProfileError::EvidenceUnavailable)?;
        if evidence.digest != expected_digest || evidence.addresses != destination.addresses() {
            return Err(CustomProfileError::EvidenceUnavailable);
        }
        let key = std::env::var(profile.credential_env())
            .map_err(|_| CustomProfileError::CredentialUnavailable)?;
        let transport = CustomTransport::new(&profile, &destination, key)?;
        let mut identity = ProviderIdentity::new(
            ProviderKind::custom_profile(profile.name()),
            profile.model(),
            Provenance::Custom(CustomProfileProvenance {
                endpoint: profile.endpoint().to_owned(),
                evidence_digest: expected_digest,
                capability_evidence_version: profile.capability_evidence_version(),
            }),
        );
        identity.effort = selected_effort;
        Ok(Self {
            identity,
            transport,
        })
    }
}

#[async_trait::async_trait]
impl Provider for CustomProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        request.validate_scope()?;
        if request.input.tools.is_some()
            || !request.input.images.is_empty()
            || request.model != self.identity.model
            || !(1..=super::CUSTOM_OUTPUT_TOKEN_CAP).contains(&request.max_output_tokens)
        {
            return Err(ProviderError::Rejected);
        }
        self.transport
            .run(&request, self.identity.effort)
            .await
            .map_err(transport_error)
    }

    async fn invoke_with_preview(
        &self,
        request: ProviderRequest,
        preview: crate::provider::ResponsePreview,
    ) -> Result<ProviderResponse, ProviderError> {
        request.validate_scope()?;
        if request.input.tools.is_some()
            || !request.input.images.is_empty()
            || request.model != self.identity.model
            || !(1..=super::CUSTOM_OUTPUT_TOKEN_CAP).contains(&request.max_output_tokens)
        {
            return Err(ProviderError::Rejected);
        }
        self.transport
            .run_streamed(&request, self.identity.effort, Some(preview))
            .await
            .map_err(transport_error)
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        if request.model != self.identity.model
            || !(1..=crate::session::MAX_COMPACTION_OUTPUT_TOKENS)
                .contains(&request.max_output_tokens)
        {
            return Err(ProviderError::Rejected);
        }
        self.transport
            .compact(&request, self.identity.effort)
            .await
            .map_err(transport_error)
    }
}

fn transport_error(error: TransportError) -> ProviderError {
    match error {
        TransportError::Unavailable => ProviderError::Unavailable,
        TransportError::PartialTransport => ProviderError::PartialTransport,
        TransportError::Rejected => ProviderError::Rejected,
        TransportError::HttpServerUnavailable(after) => ProviderError::HttpServerUnavailable(after),
        TransportError::RateLimited(after) => ProviderError::RateLimited(after),
        TransportError::QuotaExceeded => ProviderError::QuotaExceeded,
        TransportError::Transport => ProviderError::Transport,
        TransportError::StreamEnded => ProviderError::StreamEnded,
        TransportError::InvalidOutcome => ProviderError::InvalidOutcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentPhase, AgentRunId, CollaborationPolicy, PromptRole, RunId, RunInput};

    #[test]
    fn only_stream_cuts_and_early_ends_retry_on_the_custom_route() {
        use crate::provider::RetryCause;
        for (error, cause) in [
            (ProviderError::PartialTransport, Some(RetryCause::StreamCut)),
            (ProviderError::StreamEnded, Some(RetryCause::StreamCut)),
            (
                ProviderError::RemoteStreamCode("subscription_sharing_usage_unavailable".into()),
                None,
            ),
            (ProviderError::InvalidStream, None),
            (ProviderError::CallTimedOut, None),
        ] {
            let label = format!("{error:?}");
            let mapped = transport_error(super::super::transport::stream_failure(error));
            assert_eq!(
                mapped.retry_cause().map(|(cause, _)| cause),
                cause,
                "{label}"
            );
        }
    }

    #[test]
    fn streaming_destination_drift_keeps_legacy_disposition_without_fallback() {
        use crate::provider::ProviderFailureClass;
        for (status, disposition) in [
            (200, ProviderFailureClass::Rejected),
            (400, ProviderFailureClass::Rejected),
            (429, ProviderFailureClass::Rejected),
            (500, ProviderFailureClass::Unavailable),
            (529, ProviderFailureClass::Unavailable),
        ] {
            let error = super::super::transport::streaming_admission(
                reqwest::StatusCode::from_u16(status).unwrap(),
                false,
            )
            .err()
            .unwrap();
            let error = transport_error(error);
            assert_eq!(error.failure_class(), disposition, "HTTP {status}");
            assert_eq!(error.failure_reason(), None);
            assert!(!error.allows_fallback());
        }
    }

    #[tokio::test]
    // Skipped in the Guard: local test IPC capability is missing.
    #[cfg_attr(arany_guard, ignore = "requires local test IPC")]
    async fn readonly_custom_admission_cannot_dispatch_tools_or_child_authority() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        for messages in [false, true] {
            let mut profile = CustomProfile {
                name: "local".into(),
                protocol: super::super::Protocol::OpenaiResponses,
                endpoint: format!("http://127.0.0.1:{port}/v1/responses"),
                model: "model-1".into(),
                credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
                capability_evidence_version: 2,
                efforts: Vec::new(),
            };
            if messages {
                let base_url = format!("http://127.0.0.1:{port}/anthropic");
                profile.endpoint = format!("{base_url}/v1/messages");
                profile.protocol = super::super::Protocol::AnthropicMessages {
                    base_url,
                    auth: super::super::MessagesAuth::ApiKey,
                };
            }
            let destination = profile.resolve_destination().await.unwrap();
            let provider = CustomProvider {
                identity: ProviderIdentity::new(
                    "custom:local",
                    "model-1",
                    Provenance::Custom(CustomProfileProvenance {
                        endpoint: profile.endpoint().into(),
                        evidence_digest: fingerprint(&profile),
                        capability_evidence_version: 2,
                    }),
                ),
                transport: CustomTransport::new(&profile, &destination, "synthetic-key".into())
                    .unwrap(),
            };
            assert_eq!(
                provider.identity().limits.deadline,
                std::time::Duration::from_secs(120)
            );
            let mut image_request = crate::provider::test_support::read_only_request("model-1");
            image_request
                .input
                .images
                .push(crate::provider::ProviderImage {
                    origin: crate::provider::ImageOrigin::Objective,
                    image: crate::provider::test_image(),
                });
            assert!(matches!(
                provider.invoke(image_request).await,
                Err(ProviderError::Rejected)
            ));
            for (phase, collaboration, tools) in [
                (
                    AgentPhase::RootPlan,
                    CollaborationPolicy::Single,
                    Some(crate::tools::ToolContext::new(serde_json::json!(
                        "NO_DISCLOSURE"
                    ))),
                ),
                (
                    AgentPhase::ChildWork,
                    CollaborationPolicy::Team {
                        max_active_children: 1,
                    },
                    None,
                ),
                (
                    AgentPhase::RootPlan,
                    CollaborationPolicy::Team {
                        max_active_children: 0,
                    },
                    None,
                ),
            ] {
                let request = ProviderRequest {
                    session_id: crate::session::SessionId::new(),
                    run_id: RunId::new(),
                    agent_run_id: AgentRunId::new(),
                    phase,
                    collaboration,
                    model: "model-1".into(),
                    max_output_tokens: 4096,
                    input: RunInput {
                        tools,
                        ..RunInput::new(PromptRole::ReadOnly, "NO_DISCLOSURE".into())
                    },
                };
                assert!(matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        provider.invoke(request)
                    )
                    .await
                    .unwrap(),
                    Err(ProviderError::Rejected)
                ));
            }
        }
        assert!(
            listener
                .into_std()
                .unwrap()
                .accept()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
            "rejected scope must not open a transport connection"
        );
    }
}
