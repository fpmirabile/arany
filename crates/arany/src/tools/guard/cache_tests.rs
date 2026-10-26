use super::tests::*;
use super::*;

pub(super) fn timeout_observation() -> ToolObservation {
    let mut observation = command_observation();
    observation.disposition = ToolDisposition::Limit;
    observation.limit_cause = Some(LimitCause::Timeout);
    observation.intent.limits.resource_class = ToolResourceClass::Build;
    observation.intent.limits.cache_transfer_ms = observation.intent.limits.runtime_ms;
    observation.attestation = Some(Attestation::Guard(GuardReceipt {
        contract_version: 1,
        intent_digest: observation.intent.digest(),
        enforcement_digest: observation.intent.enforcement_digest,
        limits: observation.intent.limits.clone(),
    }));
    observation.output = observation
        .output
        .replace("\"exit_code\":0", "\"exit_code\":null");
    observation
}

#[tokio::test]
async fn cache_timeout_admission_rejects_ineligible_reports_and_synthetic_uncertainty() {
    let mut unknown = timeout_observation();
    let candidate = CacheCandidate::Uncertain;
    publish_cache_candidate(&mut unknown, candidate, None, far_deadline()).await;
    assert_eq!(unknown.cache_update, CacheUpdate::Uncertain);
    assert!(unknown.stops_run());
    let mutations: [fn(&mut ToolObservation); 7] = [
        |o| o.intent.limits.resource_class = ToolResourceClass::Default,
        |o| o.limit_cause = Some(LimitCause::Resource),
        |o| o.limit_cause = Some(LimitCause::OutputCeiling),
        |o| {
            o.disposition = ToolDisposition::Cancelled;
            o.limit_cause = None;
        },
        |o| {
            o.disposition = ToolDisposition::Failed;
            o.limit_cause = None;
        },
        |o| o.attestation = None,
        |o| {
            let mut result: CommandResult = serde_json::from_str(&o.output).unwrap();
            result.changeset.files.push(FileChange {
                path: "source.txt".into(),
                before: None,
                after: Some(FileVersion::from_bytes(b"", false)),
            });
            assert!(result.valid());
            o.output = serde_json::to_string(&result).unwrap();
        },
    ];
    for (case, mutate) in mutations.into_iter().enumerate() {
        let mut observation = timeout_observation();
        mutate(&mut observation);
        if let Some(Attestation::Guard(receipt)) = &mut observation.attestation {
            receipt.intent_digest = observation.intent.digest();
            receipt.limits = observation.intent.limits.clone();
        }
        for update in [
            CacheUpdate::Published,
            CacheUpdate::Rejected,
            CacheUpdate::Failed,
        ] {
            observation.cache_update = update;
            let event = crate::Event::ToolFinished {
                checkpoint: None,
                run_id: observation.intent.run_id,
                agent_run_id: observation.intent.agent_run_id,
                observation: observation.clone(),
            };
            assert!(event.payload().is_err(), "{case}: {update:?}");
        }
        observation.cache_update = if observation.attestation.is_none() {
            CacheUpdate::NotApplicable
        } else {
            CacheUpdate::NotAttempted
        };
        let before = observation.clone();
        publish_cache_candidate(&mut observation, candidate, None, far_deadline()).await;
        assert_eq!(observation, before, "{case}");
        if observation.attestation.is_none() {
            assert!(observation.valid());
            observation.cache_update = CacheUpdate::Uncertain;
            assert!(!observation.valid());
        }
    }
}
