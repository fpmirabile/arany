use super::*;

#[tokio::test]
async fn family_projection_preserves_titles_locations_and_raw_calls() {
    let _deadline = crate::cli::serve::tests::deadline();
    for (call, kind, title, paths) in [
        (
            json!({"operation":"search","path":"src","query":"different subject","regex":false,"include":null,"case_sensitive":true,"offset":0,"limit":10}),
            "search",
            "Search src",
            json!([{"path":"/var/workspace/src"}]),
        ),
        (
            json!({"operation":"git_commit","paths":["src/lib.rs"],"staged":false,"message":"Commit subject\nBody"}),
            "edit",
            "Git write Commit subject",
            json!([]),
        ),
        (
            json!({"operation":"git_branch","name":null,"start_point":null,"scope":null,"base":null,"max_count":null}),
            "read",
            "Git read ",
            json!([]),
        ),
        (
            json!({"operation":"skill","name":"review","resource":null}),
            "read",
            "Read Skill review",
            json!([]),
        ),
        (
            json!({"operation":"command_unguarded","program":"cargo","args":["check"],"cwd":"src","justification":"Check the project"}),
            "execute",
            "Unguarded command cargo",
            json!([]),
        ),
    ] {
        let mut adapter = Adapter::new().await;
        adapter
            .prompt(json!([{"type":"text","text":"Inspect"}]))
            .await;
        let intent = adapter.intent(serde_json::from_value(call.clone()).unwrap());
        adapter.item(&intent, None).await;
        let started = adapter.next().await["params"]["update"].clone();
        assert_eq!(started["kind"], kind);
        assert_eq!(started["title"], title);
        assert_eq!(started["locations"], paths);
        assert_eq!(
            started["rawInput"],
            serde_json::to_value(&intent.call).unwrap()
        );
        let observation = arany::ToolObservation {
            spill: None,
            intent: intent.clone(),
            disposition: arany::ToolDisposition::Uncertain,
            enforcement_cause: None,
            limit_cause: None,
            output: String::new(),
            attestation: None,
            approval_cause: None,
            cache_update: arany::CacheUpdate::NotApplicable,
        };
        adapter.item(&intent, Some(&observation)).await;
        let completed = adapter.next().await["params"]["update"].clone();
        assert_eq!(completed["title"], title);
        assert_eq!(completed["locations"], paths);
        assert_eq!(completed["status"], "failed");
        assert_eq!(completed["_meta"]["arany.status"], "uncertain");
        adapter.close().await;
    }
}
