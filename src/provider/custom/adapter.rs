use super::{
    CustomProfile, CustomProfileError, check::fingerprint, transport::CustomTransport,
    transport::TransportError,
};
use crate::provider::{
    CompactionRequest, CompactionResponse, CustomProfileProvenance, Effort, Provider,
    ProviderError, ProviderRequest, ProviderResponse,
};
use crate::store::{StateRoot, Store};

pub struct CustomProvider {
    profile_label: String,
    model: String,
    endpoint: String,
    evidence_digest: [u8; 32],
    evidence_version: u32,
    effort: Option<Effort>,
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
        let transport = CustomTransport::new(&destination, key)?;
        Ok(Self {
            profile_label: format!("custom:{}", profile.name()),
            model: profile.model().to_owned(),
            endpoint: profile.endpoint().to_owned(),
            evidence_digest: expected_digest,
            evidence_version: profile.capability_evidence_version(),
            effort: selected_effort,
            transport,
        })
    }
}

impl Provider for CustomProvider {
    fn profile_name(&self) -> &str {
        &self.profile_label
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    fn reasoning_effort(&self) -> Option<Effort> {
        self.effort
    }

    fn custom_profile_provenance(&self) -> Option<CustomProfileProvenance> {
        Some(CustomProfileProvenance {
            endpoint: self.endpoint.clone(),
            evidence_digest: self.evidence_digest,
            capability_evidence_version: self.evidence_version,
        })
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        request.validate_scope()?;
        if request.tools.is_some()
            || !request.images.is_empty()
            || request.model != self.model
            || !(1..=4096).contains(&request.max_output_tokens)
        {
            return Err(ProviderError::Rejected);
        }
        self.transport
            .run(&request, self.effort)
            .await
            .map_err(transport_error)
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        if request.model != self.model || !(1..=1024).contains(&request.max_output_tokens) {
            return Err(ProviderError::Rejected);
        }
        self.transport
            .compact(&request, self.effort)
            .await
            .map_err(transport_error)
    }
}

fn transport_error(error: TransportError) -> ProviderError {
    match error {
        TransportError::Unavailable => ProviderError::Unavailable,
        TransportError::Rejected => ProviderError::Rejected,
        TransportError::InvalidOutcome => ProviderError::InvalidOutcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentPhase, AgentRunId, CollaborationPolicy, RunId};

    #[tokio::test]
    async fn readonly_custom_admission_cannot_dispatch_tools_or_child_authority() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let profile = CustomProfile {
            name: "local".into(),
            endpoint: format!("http://127.0.0.1:{port}/v1/responses"),
            model: "model-1".into(),
            credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
            capability_evidence_version: 1,
            efforts: Vec::new(),
        };
        let destination = profile.resolve_destination().await.unwrap();
        let provider = CustomProvider {
            profile_label: "custom:local".into(),
            model: "model-1".into(),
            endpoint: profile.endpoint().into(),
            evidence_digest: fingerprint(&profile),
            evidence_version: 1,
            effort: None,
            transport: CustomTransport::new(&destination, "synthetic-key".into()).unwrap(),
        };
        for (phase, collaboration, tools) in [
            (
                AgentPhase::RootPlan,
                CollaborationPolicy::Single,
                Some(crate::tools::ToolContext {
                    catalog: "NO_DISCLOSURE".into(),
                    observations: Vec::new(),
                }),
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
                run_id: RunId::new(),
                agent_run_id: AgentRunId::new(),
                phase,
                collaboration,
                model: "model-1".into(),
                instructions: None,
                objective: "NO_DISCLOSURE".into(),
                images: Vec::new(),
                includes: Vec::new(),
                history: Vec::new(),
                context_summary: None,
                child_results: Vec::new(),
                tools,
                max_output_tokens: 4096,
            };
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(5), provider.invoke(request))
                    .await
                    .unwrap(),
                Err(ProviderError::Rejected)
            ));
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
