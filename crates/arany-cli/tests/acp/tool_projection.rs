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
            guard_stage: None,
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

#[tokio::test]
async fn move_live_and_permission_projection_preserves_file_change_details() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Review changes"}]))
        .await;
    let intent = adapter.intent(arany::ToolCall::Move {
        source: "source\nname.txt".into(),
        destination: "destination.txt".into(),
        expected_digest: Some("a".repeat(64)),
    });

    adapter.item(&intent, None).await;
    let started = adapter.next().await["params"]["update"].clone();
    assert_eq!(started["kind"], "edit");
    assert_eq!(started["title"], "Change files source name.txt");
    assert_eq!(
        started["locations"][0]["path"],
        "/var/workspace/source\nname.txt"
    );
    assert_eq!(
        started["locations"][1]["path"],
        "/var/workspace/destination.txt"
    );

    adapter.permission(&intent, 10).await;
    let request = adapter.next().await;
    let permission = request["params"]["toolCall"].clone();
    assert_eq!(permission["kind"], "edit");
    assert_eq!(permission["title"], "Change files source name.txt");
    assert_eq!(
        permission["locations"][0]["path"],
        "/var/workspace/source\nname.txt"
    );
    assert_eq!(
        permission["locations"][1]["path"],
        "/var/workspace/destination.txt"
    );
    let uncertain = arany::ToolObservation {
        spill: None,
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Uncertain,
        enforcement_cause: None,
        limit_cause: None,
        output: String::new(),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter.item(&intent, Some(&uncertain)).await;
    let withdrawal = adapter.next().await;
    assert_eq!(withdrawal["method"], "$/cancel_request");
    assert_eq!(withdrawal["params"]["requestId"], request["id"]);
    let completed = adapter.next().await["params"]["update"].clone();
    assert_eq!(completed["kind"], "edit");
    assert_eq!(completed["status"], "failed");
    assert_eq!(completed["_meta"]["arany.status"], "uncertain");
    adapter.close().await;
}

#[tokio::test]
async fn move_completion_preserves_denial_projection() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Review changes"}]))
        .await;
    let intent = adapter.intent(arany::ToolCall::Move {
        source: "denied.txt".into(),
        destination: "denied-new.txt".into(),
        expected_digest: Some("b".repeat(64)),
    });
    adapter.item(&intent, None).await;
    let _ = adapter.next().await;
    let observation = arany::ToolObservation {
        spill: None,
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Denied,
        enforcement_cause: None,
        limit_cause: None,
        output: String::new(),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter.item(&intent, Some(&observation)).await;
    let update = adapter.next().await["params"]["update"].clone();
    assert_eq!(update["kind"], "edit");
    assert_eq!(update["status"], "failed");
    assert!(
        update["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("denied")
    );
    adapter.close().await;
}

#[tokio::test]
async fn move_history_projection_preserves_file_change_details() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut replay = Adapter::new().await;
    let intent = replay.intent(arany::ToolCall::Move {
        source: "old.txt".into(),
        destination: "new.txt".into(),
        expected_digest: Some("b".repeat(64)),
    });
    replay
        .call(
            json!(3),
            "session/load",
            json!({"sessionId":replay.session,"cwd":"/var/workspace","mcpServers":[]}),
        )
        .await;
    let request = replay.request().await;
    assert_eq!(request.method, "thread/resume");
    replay
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: request.id,
            result: json!({
                "thread":{"id":replay.session,"title":"History","workspace":"/private/var/workspace","createdAtMs":0},
                "turns":[{"id":replay.run,"goalId":arany::GoalId::new(),"status":"interrupted","items":[
                    {"type":"userMessage","id":format!("{}:user", replay.run),"text":"Move","images":[]},
                    serve::items::tool_item(&intent, None, true)
                ]}]
            }),
        }))
        .await
        .unwrap();
    let _ = replay.next().await;
    let started = replay.next().await["params"]["update"].clone();
    assert_eq!(started["sessionUpdate"], "tool_call");
    assert_eq!(started["kind"], "edit");
    assert_eq!(started["title"], "Change files old.txt");
    assert_eq!(started["locations"][0]["path"], "/var/workspace/old.txt");
    assert_eq!(started["locations"][1]["path"], "/var/workspace/new.txt");
    let completed = replay.next().await["params"]["update"].clone();
    assert_eq!(completed["sessionUpdate"], "tool_call_update");
    assert_eq!(completed["status"], "failed");
    assert_eq!(completed["_meta"]["arany.status"], "uncertain");
    assert_eq!(replay.next().await["id"], 3);
    replay.close().await;
}

#[tokio::test]
async fn host_pr_reads_project_as_bounded_reads() {
    let _deadline = crate::cli::serve::tests::deadline();
    let repository = "github.com/team/\u{001b}[2J\nproject";
    for (call, operation, label) in [
        (
            arany::ToolCall::GhPrList {
                repository: repository.into(),
                state: arany::PrState::Open,
                base: None,
                head: None,
                limit: 10,
            },
            "gh_pr_list",
            "List GitHub pull requests",
        ),
        (
            arany::ToolCall::GhPrView {
                repository: repository.into(),
                number: 17,
            },
            "gh_pr_view",
            "Inspect GitHub pull request",
        ),
    ] {
        let mut adapter = Adapter::new().await;
        adapter
            .prompt(json!([{"type":"text","text":"Read pull requests"}]))
            .await;
        let intent = adapter.intent(call);
        adapter.item(&intent, None).await;
        let update = adapter.next().await["params"]["update"].clone();
        assert_eq!(update["kind"], "read");
        assert_eq!(
            update["title"],
            format!("{label} github.com/team/\\u{{001b}}[2J project")
        );
        assert!(
            !update["title"]
                .as_str()
                .unwrap()
                .chars()
                .any(char::is_control)
        );
        assert_eq!(update["rawInput"]["operation"], operation);
        assert_eq!(update["rawInput"]["repository"], repository);
        if operation == "gh_pr_view" {
            assert_eq!(update["rawInput"]["number"], 17);
        }
        assert_eq!(update["locations"], json!([]));
        adapter.close().await;
    }
}

#[tokio::test]
async fn blocked_projection_refuses_deferred_event_overflow() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
    for _ in 0..serve::OUTBOUND_CAPACITY {
        adapter
            .outbound
            .try_send(wire::response(RequestId::Integer(99), json!({})))
            .unwrap();
    }
    let intent = adapter.intent(arany::ToolCall::Read {
        path: "file".into(),
        offset: None,
        limit: Some(32),
    });
    adapter.item(&intent, None).await;
    for _ in 0..=serve::OUTBOUND_CAPACITY {
        adapter
            .events
            .send(Message::Notification(Notification {
                jsonrpc: Default::default(),
                method: "events/resync".into(),
                params: None,
            }))
            .await
            .unwrap();
    }
    assert!(
        adapter
            .worker
            .await
            .unwrap()
            .unwrap_err()
            .contains("ACP projection queue full")
    );
    assert_eq!(adapter.outbound.capacity(), 0);
}

#[tokio::test]
async fn bounded_tool_output_and_oversized_permissions_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Inspect"}]))
        .await;
    let intent = adapter.intent(arany::ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), "echo example".into()],
        cwd: ".".into(),
        resource_class: None,
        timeout_ms: None,
        additional_reads: None,
    });
    for (call, kind) in [
        (intent.call.clone(), "execute"),
        (
            arany::ToolCall::Write {
                path: "file".into(),
                content: "x".into(),
                expected_digest: None,
            },
            "edit",
        ),
        (
            arany::ToolCall::Search {
                path: "notes".into(),
                query: "x".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
            },
            "search",
        ),
        (
            arany::ToolCall::Glob {
                pattern: "*.md".into(),
                path: "notes".into(),
                offset: 0,
                limit: 100,
            },
            "search",
        ),
        (
            arany::ToolCall::Skill {
                name: "guide".into(),
                resource: None,
            },
            "read",
        ),
        (
            arany::ToolCall::McpList {
                server: "local".into(),
            },
            "other",
        ),
    ] {
        let intent = adapter.intent(call);
        adapter.item(&intent, None).await;
        let update = adapter.next().await;
        assert_eq!(update["params"]["update"]["kind"], kind);
        assert!(update["params"]["update"].get("content").is_none());
    }
    let observation = arany::ToolObservation {
        spill: None,
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Succeeded,
        enforcement_cause: None,
        limit_cause: None,
        output: "é`\u{001b}".repeat(64 * 1024),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter.item(&intent, Some(&observation)).await;
    let update = adapter.next().await;
    let text = update["params"]["update"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("[truncated; see arany show "));
    assert!(!text.contains('\u{001b}'));
    let mut big = intent.clone();
    big.call = arany::ToolCall::Command {
        program: "sh".into(),
        args: vec!["x".repeat(MAX_RPC_LINE_BYTES)],
        cwd: ".".into(),
        resource_class: None,
        timeout_ms: None,
        additional_reads: None,
    };
    adapter.permission(&big, 100).await;
    let Some(serve::server::ClientMessage::Message(Message::Error(reply))) =
        adapter.input.recv().await
    else {
        panic!("oversized approval is unavailable")
    };
    assert_eq!(reply.id, Some(RequestId::Integer(100)));
    assert_eq!(reply.error.message, "Approval channel unavailable");
    assert!(
        adapter.output.try_recv().is_err(),
        "oversized approval must never be sent"
    );
    let unguarded = adapter.intent(arany::ToolCall::CommandUnguarded {
        program: "sh".into(),
        args: vec![],
        cwd: ".".into(),
        justification: "untrusted".into(),
        prefix_rule: None,
    });
    adapter.permission(&unguarded, 101).await;
    let Some(serve::server::ClientMessage::Message(Message::Error(reply))) =
        adapter.input.recv().await
    else {
        panic!("human-only approval is unavailable")
    };
    assert_eq!(reply.error.message, "Approval channel unavailable");
    assert!(adapter.output.try_recv().is_err());
    adapter.close().await;
}
