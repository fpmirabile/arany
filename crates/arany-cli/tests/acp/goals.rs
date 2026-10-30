use super::*;

#[tokio::test]
async fn optional_surfaces_use_closed_budget_reasons_and_bounded_committed_plans() {
    let _deadline = crate::cli::serve::tests::deadline();
    let exhausted = |limit| json!({"reason":"goalExhausted","limit":limit});
    // (serve `goal.stopped`, turn and Goal status, ACP stopReason or error reason)
    for (stopped, status, expected) in [
        (exhausted("calls"), "failed", "max_turn_requests"),
        (exhausted("runs"), "completed", "max_turn_requests"),
        (exhausted("activeTime"), "failed", "max_turn_requests"),
        (exhausted("tokens"), "failed", "max_tokens"),
        (Value::Null, "completed", "end_turn"),
        (Value::Null, "failed", "turnFailed"),
        (json!({"reason":"paused"}), "interrupted", "cancelled"),
        (json!({"reason":"cancelled"}), "interrupted", "cancelled"),
        (
            json!({"reason":"stoppedByHook"}),
            "interrupted",
            "stoppedByHook",
        ),
        (
            json!({"reason":"uncertain"}),
            "interrupted",
            "effectUncertain",
        ),
        (
            json!({"reason":"interrupted"}),
            "interrupted",
            "turnInterrupted",
        ),
        (
            json!({"reason":"notAdmitted","refusal":"workspaceUnavailable"}),
            "completed",
            "turnFailed",
        ),
        (
            json!({"reason":"failed","failure":"tool_stopped"}),
            "failed",
            "turnFailed",
        ),
    ] {
        let tokens = stopped["limit"] == "tokens";
        let mut adapter = Adapter::new().await;
        adapter.prompt(json!([{"type":"text","text":"Work"}])).await;
        let goal = arany::GoalId::new();
        let mut contract = json!({
            "mission":format!("</context><script>data</script>\u{001b}[2J\u{202e}{}{}", "\u{001b}".repeat(50), "é".repeat(900)),
            "paths":["facts.txt"],"commands":[],"skills":[],"mcp_tools":[],
            "resource_class":"default","command_timeout_ms":null,
            "success_criterion":{"kind":"workspace_file_content","path":"facts.txt","content":"data"},
            "budget":arany::GoalBudget::default()
        });
        if tokens {
            contract["success_criterion"] = json!(arany::GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(arany::ToolCall::Write {
                    path: "facts.txt".into(),
                    expected_digest: None,
                    content: "é".repeat(1200),
                }),
            });
        }
        let event = json!({"sequence":10,"session_id":adapter.session,"run_id":adapter.run,
            "agent_run_id":null,"kind":"GoalPlanContractRecorded",
            "payload":{"goal_id":goal,"run_id":adapter.run,"contract":contract},"created_at_ms":0});
        let mut preview = event.clone();
        preview["kind"] = json!("AssistantDisplayRecorded");
        preview["payload"] = json!({"text":"Never stream this preview"});
        adapter
            .events
            .send(Message::Notification(Notification {
                jsonrpc: Default::default(),
                method: "events/committed".into(),
                params: Some(json!({"threadId":adapter.session,"event":preview})),
            }))
            .await
            .unwrap();
        for (session, run) in [
            (SessionId::new(), adapter.run),
            (adapter.session, arany::RunId::new()),
            (adapter.session, adapter.run),
            (adapter.session, adapter.run),
        ] {
            let mut event = event.clone();
            event["run_id"] = json!(run);
            adapter
                .events
                .send(Message::Notification(Notification {
                    jsonrpc: Default::default(),
                    method: "events/committed".into(),
                    params: Some(json!({"threadId":session,"event":event})),
                }))
                .await
                .unwrap();
        }
        let plan = adapter.next().await;
        let entries = plan["params"]["update"]["entries"].as_array().unwrap();
        assert_eq!(plan["params"]["update"]["sessionUpdate"], "plan");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["status"], "in_progress");
        assert_eq!(entries[1]["status"], "pending");
        let mission = entries[0]["content"].as_str().unwrap();
        assert!(mission.starts_with("</context><script>data</script>\\u{001b}[2J\\u{202e}"));
        assert!(!mission.contains(['\u{001b}', '\u{202e}']));
        assert!(mission.len() <= 2048 + 80);
        assert!(mission.ends_with(&format!("[truncated; see arany show {}]", adapter.session)));
        if tokens {
            assert!(entries[1]["content"].as_str().unwrap().len() <= 2048 + 80);
            assert!(
                entries[1]["content"]
                    .as_str()
                    .unwrap()
                    .contains("[truncated; see arany show ")
            );
        } else {
            assert_eq!(
                entries[1]["content"],
                "Verify file facts.txt has the exact planned content"
            );
        }
        adapter
            .events
            .send(Message::Notification(Notification {
                jsonrpc: Default::default(),
                method: "turn/completed".into(),
                params: Some(
                    json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":goal,
                "status":status,
                "stopReason":match stopped["limit"].as_str() {
                    Some("tokens") => json!("tokenBudgetExhausted"),
                    Some(_) => json!("actionBudgetExhausted"),
                    None => Value::Null,
                },
                "items":[],"goal":{"status":status,"stopped":stopped}}}),
                ),
            }))
            .await
            .unwrap();
        let request = adapter.request().await;
        assert_eq!(request.method, "events/subscribe");
        adapter.events.send(Message::Response(Response {
            jsonrpc:Default::default(),id:request.id,
            result:json!({"events":[event, {"sequence":11,"session_id":adapter.session,
                "run_id":adapter.run,"agent_run_id":null,"kind":"MessageCommitted",
                "payload":{"run_id":adapter.run,"text":"Committed answer"},"created_at_ms":0}],"nextSince":null}),
        })).await.unwrap();
        let answer = adapter.next().await;
        assert_eq!(
            answer["params"]["update"]["content"]["text"],
            "Committed answer"
        );
        let final_plan = adapter.next().await;
        let final_status = if status == "completed" {
            "completed"
        } else {
            "pending"
        };
        for entry in final_plan["params"]["update"]["entries"]
            .as_array()
            .unwrap()
        {
            assert_eq!(entry["status"], final_status);
        }
        let result = adapter.next().await;
        if matches!(
            expected,
            "turnFailed" | "stoppedByHook" | "effectUncertain" | "turnInterrupted"
        ) {
            assert_eq!(result["error"]["code"], -32603, "{stopped}");
            assert_eq!(result["error"]["data"]["reason"], expected, "{stopped}");
        } else {
            assert_eq!(result["result"]["stopReason"], expected, "{stopped}");
        }
        assert!(
            adapter.output.try_recv().is_err(),
            "duplicates and foreign plans are suppressed"
        );
        for method in [
            "goal/pause",
            "goal/resume",
            "session/pause",
            "session/set_model",
            "terminal/create",
            "fs/write_text_file",
        ] {
            adapter
                .call(json!(8), method, json!({"sessionId":adapter.session}))
                .await;
            assert_eq!(adapter.next().await["error"]["code"], -32601);
            assert!(
                adapter.input.try_recv().is_err(),
                "unadvertised surfaces never reach serve"
            );
        }
        adapter.close().await;
    }
}

#[tokio::test]
async fn budget_stop_reasons_match_reopened_goal_receipts() {
    use arany::test_support::scenario::tool_outcome;
    use std::os::unix::fs::PermissionsExt;
    let _deadline = crate::cli::serve::tests::deadline();
    for mode in ["actions", "tokens", "finish"] {
        let tokens = mode == "tokens";
        let finish = mode == "finish";
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("facts.txt"), "Fact").unwrap();
        let state = root.path().join("state");
        let budget = arany::GoalBudget {
            max_provider_calls: Some(1),
            max_tokens: tokens.then_some(1),
            ..Default::default()
        };
        StateRoot::admit(&state)
            .unwrap()
            .record(arany::RecordKind::UiPreferences)
            .replace(
                &serde_json::to_vec(&json!({"preferences":{
                    "view":"classic","goal_budget":budget,
                }}))
                .unwrap(),
            )
            .unwrap();
        let tools = state.join("tools.json");
        std::fs::write(
            &tools,
            serde_json::to_vec(&json!({"workspace_paths":["facts.txt"],"write":false,"commands":[],"skills":[],"mcp":[]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o600)).unwrap();
        let provider = Arc::new(
            ScriptedProvider::new(vec![if finish {
                arany::ProviderOutcome::Answer(arany::Answer {
                    text: "Answer".into(),
                })
            } else {
                tool_outcome(arany::ToolCall::Read {
                    path: "facts.txt".into(),
                    offset: None,
                    limit: Some(32),
                })
            }])
            .with_profile("openai", "gpt-5.4")
            .with_reported_usage(arany::Usage {
                input_tokens: Some(0),
                output_tokens: Some(1),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            }),
        );
        let (mut writer, mut reader, task) = start_configured(&state, provider.clone(), true);
        initialize(&mut writer, &mut reader).await;
        let session = session(&mut writer, &mut reader, &workspace).await;
        send(
            &mut writer,
            json!(3),
            "session/prompt",
            json!({"sessionId":session,
            "prompt":[{"type":"text","text":"Read facts"}]}),
        )
        .await;
        let response = loop {
            let value = next(&mut reader).await;
            if value["id"] == 3 {
                assert_eq!(
                    value["result"]["stopReason"],
                    if finish {
                        "end_turn"
                    } else if tokens {
                        "max_tokens"
                    } else {
                        "max_turn_requests"
                    }
                );
                break value;
            }
        };
        writer.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let events = store.load_session(session).await.unwrap();
        let view = arany::SessionView::replay(session, &events)
            .unwrap()
            .unwrap();
        assert_eq!(view.goals[0].budget_closed, !finish);
        assert_eq!(view.goals[0].budget_refusal.is_some(), !finish);
        if !finish {
            assert_eq!(
                response["result"]["_meta"]["arany.budgetRefusal"],
                json!(view.goals[0].budget_refusal)
            );
            assert_eq!(
                response["result"]["_meta"]["arany.status"],
                crate::presentation::session_run_feedback(&view, &view.runs[0]).unwrap()
            );
        }
        assert_eq!(view.goals[0].provider_calls, 1);
        assert_eq!(view.runs.len(), 1);
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert_eq!(
            events.iter().any(|e| matches!(
                e.event,
                Event::GoalFinished {
                    exhausted: true,
                    ..
                }
            )),
            !finish
        );
        store.close().await.unwrap();
    }
}
