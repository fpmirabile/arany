use super::*;

#[test]
fn old_worker_policies_and_assignments_cannot_acquire_web_tools() {
    let mut source: Value = serde_json::from_str(CURRENT_HOST_RUNSTARTED).unwrap();
    let files = vec![crate::tools::DefinitionFilePin {
        private: true,
        filename: "editor.md".into(),
        digest: [2; 32],
    }];
    let pin = crate::tools::DefinitionRegistryPin {
        digest: crate::validate::digest(&serde_json::to_vec(&files).unwrap()),
        files,
    };
    source["config"]["agent_definitions"] = serde_json::to_value(pin).unwrap();
    let grant = serde_json::json!({
        "read_paths":["src"],"write_paths":["src"],"tools":["read","write"],
        "skills":[],"mcp_servers":[],"commands":[],"permission_mode":null
    });
    source["config"]["worker_policy"] = serde_json::json!({
        "parent_grant":grant,"concurrency":1,"max_turns":4,"max_tools":3
    });
    let mut legacy = source.clone();
    legacy["config"]["worker_policy"]["data_bytes"] = 1024.into();
    let retirement = REGISTRY.event_step("RunStarted", 24).unwrap();
    assert_eq!((retirement.transform)(legacy.clone()), Some(source.clone()));
    let migrated = REGISTRY
        .upgrade_payload("RunStarted", 24, &legacy.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&migrated).unwrap(), source);
    let step = REGISTRY.event_step("RunStarted", 25).unwrap();
    let upgraded = (step.transform)(source.clone()).unwrap();
    assert_eq!(upgraded, source);
    let event = Event::decode(
        "RunStarted",
        payload_version("RunStarted").unwrap(),
        &upgraded.to_string(),
        Some(RunId::new()),
        None,
    )
    .unwrap();
    let Event::RunStarted { config, .. } = event else {
        unreachable!()
    };
    assert!(!config.tool_policy.unwrap().child_web);
    let assignment = serde_json::json!({
        "profile":null,"definition":"editor","role":"child","ordinal":1,
        "objective":"Edit src","grant":{"edit_only":{"unguarded":"denied"}},
        "assignment":{
            "request":{
                "definition":"editor","definition_digest":vec![1; 32],"mission":"Edit src",
                "direction":"","lease":["src"],"commands":[],
                "criterion":{"kind":"workspace_file_content","path":"src/main.rs","content":"ok"},
                "mode":"foreground","max_turns":4,"wave":1,"resumes":null,"supersedes":null
            },
            "grant":grant,"policy_digest":vec![1; 32],"permissions_digest":null
        }
    });
    let spawn = REGISTRY.event_step("AgentSpawned", 17).unwrap();
    assert_eq!((spawn.transform)(assignment.clone()), Some(assignment.clone()));
    for operation in ["web_fetch", "web_search"] {
        let mut widened = source.clone();
        widened["config"]["worker_policy"]["parent_grant"]["tools"] =
            serde_json::json!(["write", operation]);
        assert!((step.canonical)(&widened.to_string()).is_none());
        assert!((step.transform)(widened).is_none());
        let mut widened = assignment.clone();
        widened["assignment"]["grant"]["tools"] = serde_json::json!(["write", operation]);
        assert!((spawn.canonical)(&widened.to_string()).is_none());
        assert!((spawn.transform)(widened).is_none());
    }
    source["config"]["tool_policy"]["child_web"] = serde_json::json!(true);
    assert!((step.canonical)(&source.to_string()).is_none());
    assert!((step.transform)(source).is_none());
}
