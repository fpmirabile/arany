use crate::session::events::*;

#[test]
fn saved_account_binding_requires_a_native_selected_provider() {
    let id = Uuid::now_v7();
    let mut defaults = SessionDefaults {
        provider: Some("openai".into()),
        model: Some("gpt-5.4".into()),
        effort: None,
        account_id: Some(id),
        policy: CollaborationPolicy::Single,
    };
    assert!(defaults.validate().is_ok());
    defaults.provider = Some("custom:local".into());
    assert!(defaults.validate().is_err());
    defaults.provider = Some("openai".into());
    defaults.model = None;
    assert!(defaults.validate().is_err());
    defaults.model = Some("gpt-5.4".into());
    defaults.account_id = Some(Uuid::nil());
    assert!(defaults.validate().is_err());
    defaults.account_id = Some(id);
    defaults.provider = Some("chatgpt".into());
    defaults.model = None;
    assert!(defaults.validate().is_ok());
    defaults.account_id = None;
    assert!(defaults.validate().is_err());
}

#[test]
fn current_defaults_require_explicit_nullable_fields() {
    let defaults = SessionDefaults::default();
    let encoded = serde_json::to_value(&defaults).unwrap();
    assert_eq!(
        serde_json::from_value::<SessionDefaults>(encoded.clone()).unwrap(),
        defaults
    );
    for field in ["provider", "model", "effort", "account_id"] {
        let mut missing = encoded.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<SessionDefaults>(missing).is_err());
    }
}

#[test]
fn run_account_provenance_separates_api_keys_and_subscription_bounds() {
    let mut config = RunConfig {
        supervisor: None,
        worker_policy: None,
        agent_definitions: None,
        roster: None,
        providers: None,
        provider: crate::ProviderRecord::parse("openai").expect("Provider record"),
        model: "gpt-5.4".into(),
        effort: None,
        custom_profile_provenance: None,
        saved_api_account_id: Some(Uuid::now_v7()),
        chatgpt_provenance: None,
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        context_window_tokens: None,
        effective_context_window_tokens: None,
        policy: CollaborationPolicy::Single,
        output_token_cap: 4096,
        provider_concurrency: 1,
        workspace_device: 1,
        workspace_inode: 1,
        user_instruction_digest: None,
        instruction_sources: Vec::new(),
        prompts: crate::provider::EffectivePrompts::default().pins(),
        launch_directory: String::new(),
        include_sources: Vec::new(),
        history_run_ids: Vec::new(),
        excluded_history_runs: 0,
        context_usage: None,
        compaction_event_sequence: None,
        compaction_content_digest: None,
        tool_policy: None,
    };
    assert!(config.validate().is_ok());
    let mut inherited = config.clone();
    inherited.saved_api_account_id = None;
    let event = Event::RunStarted {
        run_id: RunId::new(),
        config: inherited,
        active_deadline_ms: Some(MAX_GOAL_ACTIVE_MS),
    };
    assert_eq!(
        event.payload().unwrap(),
        format!(
            r#"{{"config":{{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":{{"kind":"openai","custom_name":null}},"model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"effective_context_window_tokens":null,"policy":{{"mode":"single"}},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"user_instruction_digest":null,"instruction_sources":[],"prompts":{},"launch_directory":"","include_sources":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null}},"active_deadline_ms":{}}}"#,
            serde_json::to_string(&config.prompts).unwrap(),
            MAX_GOAL_ACTIVE_MS
        )
    );
    let Event::RunStarted {
        config: ordinary, ..
    } = &event
    else {
        unreachable!()
    };
    for provider in [
        serde_json::json!("openai"),
        serde_json::json!({"kind":"unknown","custom_name":null}),
        serde_json::json!({"kind":"openai"}),
        serde_json::json!({"kind":"openai","custom_name":"local"}),
        serde_json::json!({"kind":"custom","custom_name":null}),
        serde_json::json!({"kind":"custom","custom_name":""}),
        serde_json::json!({"kind":"custom","custom_name":"Upper"}),
        serde_json::json!({"kind":"custom","custom_name":"bad/name"}),
        serde_json::json!({"kind":"custom","custom_name":"a".repeat(crate::provider::MAX_CUSTOM_PROFILE_NAME_BYTES + 1)}),
        serde_json::json!({"kind":"openai","custom_name":null,"extra":true}),
    ] {
        assert!(serde_json::from_value::<crate::ProviderRecord>(provider.clone()).is_err());
        let mut payload = serde_json::json!({"config":ordinary,"active_deadline_ms":null});
        payload["config"]["provider"] = provider;
        assert!(
            Event::decode("RunStarted", &payload.to_string(), Some(RunId::new()), None).is_err()
        );
    }
    let mut roster_config = ordinary.clone();
    let mut roster = crate::provider::RosterSnapshot {
        preset: "coding".into(),
        selection_digest: [0; 32],
        profiles: vec![crate::provider::NamedProviderRoutes {
            name: "main".into(),
            routes: vec![ordinary.identity().unwrap()],
        }],
        providers: crate::provider::Providers::all("main".into()),
        child_profiles: vec!["main".into()],
    };
    roster.selection_digest = roster.digest().unwrap();
    roster_config.roster = Some(Box::new(roster));
    assert!(roster_config.validate().is_ok());
    for change in 0..4 {
        let mut forged = roster_config.clone();
        match change {
            0 => forged.roster.as_mut().unwrap().selection_digest[0] ^= 1,
            1 => forged.model = "changed".into(),
            2 => forged.roster.as_mut().unwrap().profiles[0].routes[0].model = "changed".into(),
            _ => forged.roster.as_mut().unwrap().child_profiles.clear(),
        }
        assert!(forged.validate().is_err());
    }
    let mut other_identity = ordinary.identity().unwrap();
    other_identity.model = "gpt-5.4-mini".into();
    let mut two = roster_config.roster.as_deref().unwrap().clone();
    two.profiles.push(crate::provider::NamedProviderRoutes {
        name: "other".into(),
        routes: vec![other_identity.clone()],
    });
    two.child_profiles.push("other".into());
    two.selection_digest = two.digest().unwrap();
    let mut pinned_config = roster_config.clone();
    pinned_config.roster = Some(Box::new(two));
    let main_routes = pinned_config
        .roster
        .as_ref()
        .unwrap()
        .role_routes()
        .unwrap();
    let mut pinned_base = pinned_config.clone();
    let roster = pinned_base.roster.as_mut().unwrap();
    roster.providers.review = "other".into();
    roster.selection_digest = roster.digest().unwrap();
    pinned_base.providers = Some(roster.role_routes().unwrap());
    for base in [pinned_config.clone(), pinned_base] {
        assert!(base.validate().is_ok());
        for role in 0..3 {
            let mut forged = base.clone();
            let roster = forged.roster.as_mut().unwrap();
            let name = match role {
                0 => &mut roster.providers.child,
                1 => &mut roster.providers.review,
                _ => &mut roster.providers.compaction,
            };
            *name = if name == "other" { "main" } else { "other" }.into();
            roster.selection_digest = roster.digest().unwrap();
            assert!(forged.validate().is_err(), "role {role}");
        }
    }
    let mut forged = pinned_config.clone();
    forged.providers = Some(main_routes);
    forged.providers.as_mut().unwrap().review = vec![other_identity];
    assert!(forged.validate().is_err());
    let mut missing_supervisor = serde_json::to_value(ordinary).unwrap();
    missing_supervisor
        .as_object_mut()
        .unwrap()
        .remove("supervisor");
    assert!(serde_json::from_value::<RunConfig>(missing_supervisor).is_err());
    let mut missing_user = serde_json::to_value(ordinary).unwrap();
    missing_user
        .as_object_mut()
        .unwrap()
        .remove("user_instruction_digest");
    assert!(serde_json::from_value::<RunConfig>(missing_user).is_err());
    let mut pinned_user = ordinary.clone();
    pinned_user.user_instruction_digest = Some([7; 32]);
    assert!(pinned_user.validate().is_ok());
    let mut short_user = serde_json::to_value(&pinned_user).unwrap();
    short_user["user_instruction_digest"] = serde_json::json!(vec![7; 31]);
    assert!(serde_json::from_value::<RunConfig>(short_user).is_err());
    let mut missing_roster = serde_json::to_value(ordinary).unwrap();
    missing_roster.as_object_mut().unwrap().remove("roster");
    assert!(serde_json::from_value::<RunConfig>(missing_roster).is_err());
    let mut missing_window = serde_json::to_value(ordinary).unwrap();
    missing_window
        .as_object_mut()
        .unwrap()
        .remove("context_window_tokens");
    assert!(serde_json::from_value::<RunConfig>(missing_window).is_err());

    config.output_token_cap = crate::provider::MAX_OUTPUT_TOKEN_CAP;
    assert!(config.validate().is_ok());
    for (cap, valid) in [
        (0, false),
        (4096, true),
        (crate::provider::MAX_OUTPUT_TOKEN_CAP, true),
        (crate::provider::MAX_OUTPUT_TOKEN_CAP + 1, false),
    ] {
        config.output_token_cap = cap;
        assert_eq!(config.validate().is_ok(), valid, "output cap {cap}");
    }
    config.output_token_cap = 4096;
    let source = |directory: &str| crate::session::InstructionSource {
        directory: directory.into(),
        file: Some(crate::session::InstructionFile::Agents),
        digest: [3; 32],
    };
    let deep = vec!["d"; 31].join("/");
    let too_deep = format!("{deep}/d");
    for (case, launch, sources, valid) in [
        (
            "root and nested",
            "a/b",
            vec![source(""), source("a/b")],
            true,
        ),
        ("nested only", "a/b", vec![source("a")], true),
        ("deepest allowed", deep.as_str(), vec![source(&deep)], true),
        ("too deep", too_deep.as_str(), vec![], false),
        ("deeper first", "a/b", vec![source("a"), source("")], false),
        ("repeated", "a", vec![source("a"), source("a")], false),
        ("off the path", "a/b", vec![source("c")], false),
        ("below launch", "a", vec![source("a/b")], false),
        ("traversal", "a/..", vec![], false),
        ("empty component", "a//b", vec![], false),
    ] {
        config.launch_directory = launch.into();
        config.instruction_sources = sources;
        assert_eq!(config.validate().is_ok(), valid, "instruction scope {case}");
    }
    config.launch_directory.clear();
    config.instruction_sources.clear();
    config.saved_api_account_id = Some(Uuid::nil());
    assert!(matches!(
        config.validate(),
        Err(ReplayError::InvalidPayload)
    ));
    config.saved_api_account_id = Some(Uuid::now_v7());
    config.provider = crate::ProviderRecord::parse("custom:local").unwrap();
    assert!(matches!(
        config.validate(),
        Err(ReplayError::InvalidPayload)
    ));
    config.saved_api_account_id = None;
    config.provider = ordinary.provider.clone();
    assert!(config.validate().is_ok());
    let decoded: RunConfig =
        serde_json::from_value(serde_json::to_value(&config).expect("current JSON"))
            .expect("current Run config");
    assert_eq!(decoded.saved_api_account_id, None);
    assert_eq!(decoded.chatgpt_provenance, None);
    assert_eq!(
        decoded.output_token_bound,
        crate::provider::OutputTokenBound::ProviderEnforced
    );

    config.provider = crate::ProviderRecord::parse("chatgpt").unwrap();
    config.model = "visible-model".into();
    config.effort = Some(crate::provider::Effort::High);
    config.chatgpt_provenance = Some(crate::provider::ChatGptProvenance {
        account_id: Uuid::now_v7(),
        admission_fingerprint: [7; 32],
        admission: crate::provider::ChatGptAdmission::AccountConsent,
    });
    assert!(config.validate().is_err());
    config.output_token_bound = crate::provider::OutputTokenBound::LocalAcceptanceOnly;
    assert!(config.validate().is_ok());
    config.saved_api_account_id = Some(Uuid::now_v7());
    assert!(config.validate().is_err());
    config.saved_api_account_id = None;
    config.chatgpt_provenance.as_mut().unwrap().account_id = Uuid::nil();
    assert!(config.validate().is_err());
    config.chatgpt_provenance.as_mut().unwrap().account_id = Uuid::now_v7();
    config
        .chatgpt_provenance
        .as_mut()
        .unwrap()
        .admission_fingerprint = [0; 32];
    assert!(config.validate().is_err());
    config
        .chatgpt_provenance
        .as_mut()
        .unwrap()
        .admission_fingerprint = [7; 32];
    config.provider_concurrency = 2;
    assert!(config.validate().is_err());
    config.provider_concurrency = 1;
    assert!(config.validate().is_ok());
    for (provider, window, budget, valid) in [
        ("openai", None, 376 * 1024, true),
        ("openai", None, 376 * 1024 + 1, false),
        ("chatgpt", Some(272_000), 734_400, true),
        ("chatgpt", Some(272_000), 734_401, false),
        ("chatgpt", Some(2_000_000), 1024 * 1024, true),
        ("chatgpt", Some(2_000_001), 64 * 1024, false),
        ("chatgpt", Some(32_767), 64 * 1024, false),
        ("openai", Some(272_000), 64 * 1024, false),
        ("anthropic", Some(272_000), 64 * 1024, false),
    ] {
        let mut pinned = if provider == "chatgpt" {
            config.clone()
        } else {
            ordinary.clone()
        };
        pinned.provider = crate::ProviderRecord::parse(provider).unwrap();
        pinned.context_window_tokens = window;
        pinned.effective_context_window_tokens = window;
        pinned.context_usage = Some(ContextUsage {
            used_bytes: 0,
            budget_bytes: budget,
            compactable_bytes: 0,
            tool_history_bytes: 0,
        });
        assert_eq!(
            pinned.validate().is_ok(),
            valid,
            "{provider} {window:?} {budget}"
        );
    }
    // The effective window bounds the content ceiling and never exceeds the model window.
    for (window, effective, budget, valid) in [
        (Some(272_000), Some(136_000), 367_200, true),
        (Some(272_000), Some(136_000), 367_201, false),
        (Some(272_000), Some(272_001), 64 * 1024, false),
        (Some(272_000), Some(32_767), 64 * 1024, false),
        (Some(272_000), None, 64 * 1024, false),
        (None, Some(272_000), 64 * 1024, false),
    ] {
        let mut pinned = config.clone();
        pinned.context_window_tokens = window;
        pinned.effective_context_window_tokens = effective;
        pinned.context_usage = Some(ContextUsage {
            used_bytes: 0,
            budget_bytes: budget,
            compactable_bytes: 0,
            tool_history_bytes: 0,
        });
        assert_eq!(
            pinned.validate().is_ok(),
            valid,
            "{window:?} {effective:?} {budget}"
        );
    }
    let mut fallback = config.identity().unwrap();
    fallback.model = "fallback-model".into();
    let mut custom = ordinary.identity().unwrap();
    custom.profile = "custom:fallback".into();
    custom.provenance =
        crate::provider::Provenance::Custom(crate::provider::CustomProfileProvenance {
            endpoint: "https://example.invalid/v1".into(),
            profile_digest: [1; 32],
        });
    let mut anthropic = ordinary.identity().unwrap();
    anthropic.profile = "anthropic".into();
    anthropic.model = "claude-sonnet-5".into();
    for (fallback, window_valid) in [
        (fallback, true),
        (ordinary.identity().unwrap(), false),
        (custom, false),
        (anthropic, false),
    ] {
        let mut pinned = config.clone();
        pinned.providers = Some(crate::provider::Providers::all(vec![
            config.identity().unwrap(),
            fallback,
        ]));
        assert!(pinned.validate().is_ok(), "no window: {pinned:?}");
        pinned.context_window_tokens = Some(272_000);
        pinned.effective_context_window_tokens = Some(272_000);
        assert_eq!(pinned.validate().is_ok(), window_valid, "{pinned:?}");
    }
    let encoded = serde_json::to_value(&config).unwrap();
    assert_eq!(encoded["output_token_bound"], "local_acceptance_only");
    assert_eq!(encoded["chatgpt_provenance"]["admission_fingerprint"][0], 7);
    assert_eq!(
        encoded["chatgpt_provenance"]["admission"],
        "account_consent"
    );
    assert_eq!(
        serde_json::from_value::<RunConfig>(encoded.clone()).unwrap(),
        config
    );
    for field in [
        "effort",
        "custom_profile_provenance",
        "saved_api_account_id",
        "chatgpt_provenance",
        "output_token_bound",
        "provider_concurrency",
        "effective_context_window_tokens",
        "context_usage",
        "compaction_event_sequence",
        "compaction_content_digest",
        "tool_policy",
    ] {
        let mut missing = encoded.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<RunConfig>(missing).is_err());
    }
    let mut missing_admission = encoded.clone();
    missing_admission["chatgpt_provenance"]
        .as_object_mut()
        .unwrap()
        .remove("admission");
    assert!(serde_json::from_value::<RunConfig>(missing_admission).is_err());
    for (field, value) in [
        ("admission", serde_json::json!("unchecked")),
        ("admission", serde_json::Value::Null),
        ("unknown", serde_json::json!(true)),
    ] {
        let mut invalid = encoded.clone();
        invalid["chatgpt_provenance"][field] = value;
        assert!(serde_json::from_value::<RunConfig>(invalid).is_err());
    }
}
