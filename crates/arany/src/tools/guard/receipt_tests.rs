use crate::tools::types::*;

#[tokio::test]
async fn cache_receipt_states_preserve_success_without_publication_uncertainty() {
    fn make_observation(call: ToolCall, cache_requested_bytes: u64) -> ToolObservation {
        let mut intent = EffectIntent {
            host: None,
            rewrite: Vec::new(),
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::session::RunId::new(),
            agent_run_id: crate::session::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            use_count: 1,
        };
        intent.limits.cache_requested_bytes = cache_requested_bytes;
        intent.limits.cache_bytes = cache_requested_bytes;
        intent.limits.cache_candidate_bytes = 0;
        ToolObservation {
            spill: None,
            enforcement_cause: None,
            limit_cause: None,
            attestation: Some(Attestation::for_site(&intent)),
            intent,
            disposition: ToolDisposition::Succeeded,
            output: serde_json::to_string(&super::types::CommandResult {
                selection: Some(Default::default()),
                network_refusals: Vec::new(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                stdout_total: Default::default(),
                stderr_total: Default::default(),
                changeset: Default::default(),
            })
            .unwrap(),
            approval_cause: None,
            guard_stage: None,
            cache_update: CacheUpdate::NotAttempted,
        }
    }

    let read = ToolCall::Read {
        path: "src/main.rs".into(),
        offset: None,
        limit: Some(1024),
    };
    let mut observation = make_observation(read, 0);
    super::guard::publish_cache_candidate(
        &mut observation,
        super::guard::CacheCandidate::Received,
        None,
        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
    )
    .await;
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert_eq!(observation.cache_update, CacheUpdate::NotApplicable);
    assert!(observation.valid());
    assert!(!observation.stops_run());

    for (candidate, requested, expected_update, expected_disposition) in [
        (
            super::guard::CacheCandidate::Received,
            0,
            CacheUpdate::NotApplicable,
            ToolDisposition::Succeeded,
        ),
        (
            super::guard::CacheCandidate::Received,
            1,
            CacheUpdate::Rejected,
            ToolDisposition::Succeeded,
        ),
        (
            super::guard::CacheCandidate::Rejected,
            1,
            CacheUpdate::Rejected,
            ToolDisposition::Succeeded,
        ),
        (
            super::guard::CacheCandidate::Failed,
            1,
            CacheUpdate::Failed,
            ToolDisposition::Succeeded,
        ),
        (
            super::guard::CacheCandidate::Uncertain,
            1,
            CacheUpdate::Uncertain,
            ToolDisposition::Succeeded,
        ),
    ] {
        let call = ToolCall::Command {
            program: "cargo".into(),
            args: vec!["check".into()],
            cwd: ".".into(),
        };
        let mut observation = make_observation(call, requested);
        super::guard::publish_cache_candidate(
            &mut observation,
            candidate,
            None,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await;
        assert_eq!(observation.cache_update, expected_update);
        assert_eq!(observation.disposition, expected_disposition);
        assert!(observation.valid());
        assert_eq!(
            observation.stops_run(),
            expected_update == CacheUpdate::Uncertain
        );
    }
}
