use super::*;

#[tokio::test]
async fn tools_permissions_wait_and_withdraw_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
    let intent = adapter.intent(arany::ToolCall::Read {
        path: "notes/\u{001b}[2J\nfile".into(),
        offset: None,
        limit: Some(32),
    });
    adapter.item(&intent, None).await;
    let started = adapter.next().await;
    assert_eq!(started["params"]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(started["params"]["update"]["kind"], "read");
    assert_eq!(started["params"]["update"]["status"], "pending");
    assert_eq!(
        started["params"]["update"]["locations"][0]["path"],
        "/var/workspace/notes/\u{001b}[2J\nfile"
    );
    assert!(
        !started["params"]["update"]["title"]
            .as_str()
            .unwrap()
            .chars()
            .any(char::is_control)
    );
    for (index, outcome, decision) in [
        (
            10,
            json!({"outcome":"selected","optionId":"allow_once"}),
            "accept",
        ),
        (
            11,
            json!({"outcome":"selected","optionId":"deny"}),
            "decline",
        ),
        (
            12,
            json!({"outcome":"selected","optionId":"allow_always"}),
            "unavailable",
        ),
        (13, json!({"outcome":"selected"}), "unavailable"),
        (14, Value::Null, "unavailable"),
        (
            15,
            json!({"outcome":"selected","optionId":"reject_once"}),
            "unavailable",
        ),
        (
            16,
            json!({"outcome":"selected","optionId":"unknown"}),
            "unavailable",
        ),
    ] {
        adapter.permission(&intent, index).await;
        let request = adapter.next().await;
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(
            request["params"]["toolCall"]["rawInput"],
            serde_json::to_value(&intent).unwrap()
        );
        assert_eq!(
            request["params"]["options"],
            json!([
                {"optionId":"deny","kind":"reject_once","name":"Deny"},
                {"optionId":"allow_once","kind":"allow_once","name":"Allow once"}
            ])
        );
        let content = request["params"]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(content.starts_with("````\n"));
        assert!(!content.contains('\u{001b}'));
        let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":{"outcome":outcome}});
        translate::input(
            wire::decode(&encode_line(&reply).unwrap()).unwrap(),
            &adapter.config,
            &adapter.shared,
            &adapter.serve,
            &adapter.outbound,
        )
        .await
        .unwrap();
        match adapter.input.recv().await.unwrap() {
            serve::server::ClientMessage::Message(Message::Response(response)) => {
                assert_eq!(response.id, RequestId::Integer(index));
                assert_eq!(response.result["decision"], decision);
            }
            serve::server::ClientMessage::Message(Message::Error(error)) => {
                assert_eq!(decision, "unavailable");
                assert_eq!(error.id, Some(RequestId::Integer(index)));
                assert_eq!(error.error.message, "Approval channel unavailable");
            }
            _ => panic!("approval reply"),
        }
        // Duplicate and unknown IDs cannot resolve another intent.
        for id in [request["id"].clone(), json!(999)] {
            let reply = json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
            translate::input(
                wire::decode(&encode_line(&reply).unwrap()).unwrap(),
                &adapter.config,
                &adapter.shared,
                &adapter.serve,
                &adapter.outbound,
            )
            .await
            .unwrap();
        }
        assert!(adapter.input.try_recv().is_err());
    }
    adapter.permission(&intent, 20).await;
    let permission = adapter.next().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3600)).await;
    tokio::time::resume();
    assert!(adapter.output.try_recv().is_err());
    let late = json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
    translate::input(
        wire::decode(&encode_line(&late).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Response(reply))) =
        adapter.input.recv().await
    else {
        panic!("late allow")
    };
    assert_eq!(reply.result["decision"], "accept");
    for _ in 0..256 {
        adapter
            .outbound
            .try_send(wire::response(RequestId::Integer(99), json!({})))
            .unwrap();
    }
    let blocked = adapter.intent(intent.call.clone());
    adapter.item(&blocked, None).await;
    adapter.permission(&blocked, 21).await;
    let reply = tokio::time::timeout(Duration::from_secs(5), adapter.input.recv())
        .await
        .unwrap()
        .unwrap();
    let serve::server::ClientMessage::Message(Message::Error(error)) = reply else {
        panic!("unsent approval must be unavailable");
    };
    assert_eq!(error.id, Some(RequestId::Integer(21)));
    assert_eq!(error.error.message, "Approval channel unavailable");
    assert_eq!(adapter.outbound.capacity(), 0);
    let late = json!({"jsonrpc":"2.0","id":permission["id"].as_i64().unwrap()+1,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
    translate::input(
        wire::decode(&encode_line(&late).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    assert!(adapter.input.try_recv().is_err());
    for _ in 0..256 {
        assert_eq!(adapter.output.recv().await.unwrap()["id"], 99);
    }
    let update = adapter.next().await;
    assert_eq!(update["params"]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(
        update["params"]["update"]["toolCallId"],
        blocked.id.to_string()
    );
    assert!(adapter.output.try_recv().is_err());
    let fresh = adapter.intent(intent.call.clone());
    adapter.permission(&fresh, 22).await;
    let permission = adapter.next().await;
    assert_eq!(
        permission["params"]["toolCall"]["rawInput"]["id"],
        fresh.id.to_string(),
        "an unavailable request must free the permission slot"
    );
    let error = json!({"jsonrpc":"2.0","id":permission["id"],"error":{"code":-32603,"message":"Client refused"}});
    translate::input(
        wire::decode(&encode_line(&error).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Error(reply))) =
        adapter.input.recv().await
    else {
        panic!("client error is unavailable")
    };
    assert_eq!(reply.id, Some(RequestId::Integer(22)));
    let observation = arany::ToolObservation {
        spill: None,
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Uncertain,
        enforcement_cause: None,
        limit_cause: None,
        output: "````\n<script>data</script>\u{001b}[2J".into(),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter.item(&intent, Some(&observation)).await;
    let completed = adapter.next().await;
    assert_eq!(completed["params"]["update"]["status"], "failed");
    assert_eq!(
        completed["params"]["update"]["_meta"]["arany.status"],
        "uncertain"
    );
    let text = completed["params"]["update"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(text.starts_with("`````\nOutcome unknown:"));
    assert!(!text.contains('\u{001b}'));
    adapter
        .events
        .send(Message::Notification(Notification {
            jsonrpc: Default::default(),
            method: "events/resync".into(),
            params: Some(json!({"threadId":adapter.session,"missed":1,"lastSequence":0})),
        }))
        .await
        .unwrap();
    adapter.events.send(Message::Notification(Notification { jsonrpc:Default::default(),method:"turn/completed".into(),params:Some(json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":arany::GoalId::new(),"status":"failed","items":[],"goal":{"status":"failed"}}})) })).await.unwrap();
    let events:Vec<Value> = [("ToolStarted",json!({"intent":intent,"active_ceiling_ms":1,"verdict":null})),("ToolFinished",{let mut p=serde_json::to_value(&observation).unwrap();p["checkpoint"]=Value::Null;p})].into_iter().enumerate().map(|(index,(kind,payload))| json!({
        "sequence":index+1,"session_id":adapter.session,"run_id":adapter.run,"agent_run_id":intent.agent_run_id,"kind":kind,"payload":payload,"created_at_ms":0
    })).collect();
    adapter
        .respond(json!({"events":events,"nextSince":null}))
        .await;
    let response = adapter.next().await;
    assert_eq!(
        response["error"]["data"]["reason"], "effectUncertain",
        "reconciliation must suppress duplicate Tool phases and preserve uncertainty"
    );
    adapter.close().await;
}

#[tokio::test]
async fn closed_permission_output_reports_unavailable_without_a_client_reply() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
    adapter.output.close();
    let intent = adapter.intent(arany::ToolCall::Read {
        path: "file".into(),
        offset: None,
        limit: Some(32),
    });
    adapter.permission(&intent, 1).await;
    let reply = tokio::time::timeout(Duration::from_secs(5), adapter.input.recv())
        .await
        .unwrap()
        .unwrap();
    let serve::server::ClientMessage::Message(Message::Error(error)) = reply else {
        panic!("unavailable permission")
    };
    assert_eq!(error.id, Some(RequestId::Integer(1)));
    assert_eq!(error.error.message, "Approval channel unavailable");
    let late = json!({"jsonrpc":"2.0","id":1,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
    translate::input(
        wire::decode(&encode_line(&late).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    assert!(adapter.input.try_recv().is_err());
    adapter.close().await;
}

#[tokio::test]
async fn blocked_projection_retires_withdrawn_permission_before_accepting_late_answers() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
    let intent = adapter.intent(arany::ToolCall::Read {
        path: "file".into(),
        offset: None,
        limit: Some(32),
    });
    adapter.permission(&intent, 1).await;
    let permission = adapter.next().await;
    assert_eq!(permission["method"], "session/request_permission");
    for _ in 0..serve::OUTBOUND_CAPACITY {
        adapter
            .outbound
            .try_send(wire::response(RequestId::Integer(99), json!({})))
            .unwrap();
    }
    adapter.item(&intent, None).await;
    adapter
        .events
        .send(Message::Notification(Notification {
            jsonrpc: Default::default(),
            method: "$/cancel_request".into(),
            params: Some(json!({"requestId":1})),
        }))
        .await
        .unwrap();
    adapter.permission(&intent, 2).await;
    let Some(serve::server::ClientMessage::Message(Message::Error(error))) =
        adapter.input.recv().await
    else {
        panic!("full output refuses the next permission without waiting")
    };
    assert_eq!(error.id, Some(RequestId::Integer(2)));
    assert_eq!(error.error.message, "Approval channel unavailable");
    let late = json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
    translate::input(
        wire::decode(&encode_line(&late).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    assert!(adapter.input.try_recv().is_err());
    for _ in 0..serve::OUTBOUND_CAPACITY {
        assert_eq!(adapter.next().await["id"], 99);
    }
    let messages = [adapter.next().await, adapter.next().await];
    let withdrawal = messages
        .iter()
        .find(|message| message["method"] == "$/cancel_request")
        .expect("withdrawal reaches the client after output drains");
    assert_eq!(withdrawal["params"]["requestId"], permission["id"]);
    assert!(
        messages
            .iter()
            .any(|message| message["method"] == "session/update")
    );
    adapter.permission(&intent, 3).await;
    let next = adapter.next().await;
    assert_eq!(next["method"], "session/request_permission");
    assert_ne!(next["id"], permission["id"]);
    let reply = json!({"jsonrpc":"2.0","id":next["id"],"result":{"outcome":{"outcome":"selected","optionId":"deny"}}});
    translate::input(
        wire::decode(&encode_line(&reply).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Response(reply))) =
        adapter.input.recv().await
    else {
        panic!("next permission accepts its own answer")
    };
    assert_eq!(reply.id, RequestId::Integer(3));
    assert_eq!(reply.result["decision"], "decline");
    adapter.close().await;
}

#[tokio::test]
async fn permission_cancel_and_server_withdrawal_ignore_late_answers_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    for cancel in ["client", "session", "server", "before_start_reply"] {
        let mut adapter = Adapter::new().await;
        if cancel == "before_start_reply" {
            adapter
                .call(
                    json!("prompt"),
                    "session/prompt",
                    json!({"sessionId":adapter.session,"prompt":[{"type":"text","text":"Read"}]}),
                )
                .await;
            assert_eq!(adapter.request().await.method, "turn/start");
        } else {
            adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
        }

        let intent = adapter.intent(arany::ToolCall::Read {
            path: "file".into(),
            offset: None,
            limit: Some(32),
        });
        adapter.permission(&intent, 1).await;
        let permission = adapter.next().await;
        if cancel == "client" {
            let reply = json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"cancelled"}}});
            translate::input(
                wire::decode(&encode_line(&reply).unwrap()).unwrap(),
                &adapter.config,
                &adapter.shared,
                &adapter.serve,
                &adapter.outbound,
            )
            .await
            .unwrap();
            let Some(serve::server::ClientMessage::Message(Message::Response(response))) =
                adapter.input.recv().await
            else {
                panic!("cancel reply")
            };
            assert_eq!(response.result["decision"], "cancel");
        } else {
            if cancel == "session" {
                let note = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":adapter.session}});
                translate::input(
                    wire::decode(&encode_line(&note).unwrap()).unwrap(),
                    &adapter.config,
                    &adapter.shared,
                    &adapter.serve,
                    &adapter.outbound,
                )
                .await
                .unwrap();
                assert_eq!(adapter.request().await.method, "turn/interrupt");
            } else {
                adapter
                    .events
                    .send(Message::Notification(Notification {
                        jsonrpc: Default::default(),
                        method: "$/cancel_request".into(),
                        params: Some(json!({"requestId":1})),
                    }))
                    .await
                    .unwrap();
            }
            assert_eq!(
                adapter.next().await["params"]["requestId"],
                permission["id"]
            );
        }
        let late = json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
        translate::input(
            wire::decode(&encode_line(&late).unwrap()).unwrap(),
            &adapter.config,
            &adapter.shared,
            &adapter.serve,
            &adapter.outbound,
        )
        .await
        .unwrap();
        assert!(adapter.input.try_recv().is_err());
        adapter.close().await;
    }
}

#[tokio::test]
async fn permission_round_trip_dispatch_and_denial_survive_reopen() {
    let _deadline = crate::cli::serve::tests::deadline();
    for allow in [true, false] {
        permission_scenario(allow, None, PermissionCase::Read).await;
    }
    permission_scenario(true, None, PermissionCase::ModeSwitch).await;
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native ACP Guard journey; requires the arany executable and private State"
)]
fn guarded_acp_permissions_publish_once_and_refuse_hook_asks() {
    let _gate = crate::test_process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = crate::cli::serve::tests::deadline();
    // Cargo's integration targets build the product beside the deps directory in CI.
    let guard = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("arany");
    assert!(
        guard.is_file(),
        "build the arany binary before this native gate"
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            permission_scenario(false, Some(guard.clone()), PermissionCase::HookAsk).await;
            permission_scenario(true, Some(guard.clone()), PermissionCase::Write).await;
            permission_scenario(true, Some(guard), PermissionCase::LiveEdits).await;
        });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PermissionCase {
    Read,
    Write,
    HookAsk,
    ModeSwitch,
    LiveEdits,
}

async fn permission_scenario(allow: bool, guard: Option<PathBuf>, case: PermissionCase) {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("facts.txt"), "Fact").unwrap();
    let state = root.path().join("state");
    StateRoot::admit(&state).unwrap();
    let live_edits = case == PermissionCase::LiveEdits;
    let write = matches!(case, PermissionCase::Write | PermissionCase::LiveEdits);
    let hook_ask = case == PermissionCase::HookAsk;
    let commands = if hook_ask {
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        let digest: String = Sha256::digest(std::fs::read(&shell).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        json!([{"name":"sh","executable":shell,"sha256":digest,"interpreter":true,"inputs":[]}])
    } else {
        json!([])
    };
    let permissions = if hook_ask {
        json!({"permissions":{"defaultMode":"auto"},"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{
            "type":"command",
            "command":"printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"ask\"}}'"
        }]}]}})
    } else if live_edits {
        json!({"permissions":{"defaultMode":"acceptEdits"}})
    } else {
        json!({"permissions":{"ask":["Read","Edit"]}})
    };
    for (name, value) in [
        (
            "tools.json",
            json!({"workspace_paths":if write {json!(["."])} else {json!(["facts.txt"])},"write":write || hook_ask,"commands":commands,"skills":[],"mcp":[]}),
        ),
        ("permissions.json", permissions),
    ] {
        let path = state.join(name);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let call = if write {
        arany::ToolCall::Write {
            path: "new.txt".into(),
            expected_digest: None,
            content: "Written".into(),
        }
    } else {
        arany::ToolCall::Read {
            path: "facts.txt".into(),
            offset: None,
            limit: Some(32),
        }
    };
    let second = arany::ToolCall::Write {
        path: "next.txt".into(),
        expected_digest: None,
        content: "Next".into(),
    };
    let outcomes = permission_outcomes(case, allow, call, second);
    let provider = Arc::new(ScriptedProvider::new(outcomes).with_profile("openai", "gpt-5.4"));
    let (mut writer, mut reader, mut task) = if let Some(guard) = guard {
        start_with_guard(&state, provider.clone(), true, guard)
    } else {
        start_configured(&state, provider.clone(), true)
    };
    initialize(&mut writer, &mut reader).await;
    let session = session(&mut writer, &mut reader, &workspace).await;
    send(
        &mut writer,
        json!(3),
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"Read facts"}]}),
    )
    .await;
    let mut intent = None;
    let mut permissions = 0;
    let mut projections = 0;
    loop {
        let value = next_permission_update(&mut reader, &mut task, case).await;
        if value["method"] == "session/request_permission" {
            permissions += 1;
            intent = Some(value["params"]["toolCall"]["rawInput"].clone());
            if matches!(case, PermissionCase::ModeSwitch | PermissionCase::LiveEdits)
                && permissions == 1
            {
                send(&mut writer, json!(4), "session/set_mode", json!({"sessionId":session,"modeId":if live_edits {"acceptEdits"} else {"auto"}})).await;
                let mut updates = 0;
                let mut seen = Vec::new();
                let mut omitted = 0_usize;
                loop {
                    let switched = next(&mut reader).await;
                    if seen.len() < 32 {
                        seen.push(switched.to_string().chars().take(256).collect::<String>());
                    } else {
                        omitted = omitted.saturating_add(1);
                    }
                    if switched["id"] == 4 {
                        assert_eq!(switched["result"], json!({}));
                        break;
                    }
                    assert_eq!(switched["method"], "session/update");
                    match switched["params"]["update"]["sessionUpdate"].as_str() {
                        Some("current_mode_update" | "config_option_update") => updates += 1,
                        Some("tool_call") => {
                            assert_eq!(switched["params"]["update"]["status"], "pending");
                            projections += 1;
                        }
                        _ => panic!(
                            "mode switch must leave the pending approval unanswered: {:?}; seen since switch: {:?}; omitted: {}",
                            switched.to_string().chars().take(2048).collect::<String>(),
                            seen,
                            omitted
                        ),
                    }
                }
                assert_eq!(updates, 2);
            }
            writer.write_all(&encode_line(&json!({"jsonrpc":"2.0","id":value["id"],"result":{"outcome":{"outcome":"selected","optionId":if allow {"allow_once"} else {"deny"}}}})).unwrap()).await.unwrap();
        } else if value["method"] == "session/update"
            && value["params"]["update"]["sessionUpdate"]
                .as_str()
                .unwrap()
                .starts_with("tool_call")
        {
            projections += 1;
        }
        if value["id"] == 3 {
            assert_eq!(value["result"]["stopReason"], "end_turn", "{case:?}");
            break;
        }
    }
    let calls = if matches!(case, PermissionCase::ModeSwitch | PermissionCase::LiveEdits) {
        2
    } else {
        1
    };
    assert_eq!(
        permissions,
        if hook_ask {
            0
        } else if live_edits {
            1
        } else {
            calls
        },
        "{case:?}"
    );
    assert_eq!(projections, calls * 2);
    writer.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    assert_eq!(
        arany::SessionView::replay(session, &events)
            .unwrap()
            .unwrap(),
        view
    );
    if hook_ask {
        let hook = view.runs[0].tools[0]
            .hooks
            .first()
            .expect("hook invocation");
        let outcome = hook.outcome.as_ref().expect("hook completion");
        assert!(
            matches!(
                outcome.result,
                arany::permissions::HookResult::Restricted {
                    decision: arany::permissions::Decision::Ask,
                    ..
                }
            ),
            "{case:?}: hook result={:?}, disposition={:?}, enforcement={:?}, failure={:?}",
            outcome.result,
            outcome.observation.disposition,
            outcome.observation.enforcement_cause,
            arany::test_support::recorded_tool_failure(outcome.observation.intent.id),
        );
    }
    assert_permission_continuation(&view, &provider, allow, case);
    assert_eq!(view.runs[0].tools.len(), calls);
    if case == PermissionCase::ModeSwitch {
        assert_eq!(provider.requests.lock().unwrap().len(), 3);
        for tool in &view.runs[0].tools {
            let verdict = tool.permission_verdict.as_ref().unwrap();
            assert_eq!(verdict.source, arany::permissions::PermissionSource::Human);
            assert!(verdict.approved);
        }
    }
    let tool = view.runs[0].tools.last().unwrap();
    if hook_ask {
        assert!(intent.is_none());
        let verdict = tool.permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.decision, arany::permissions::Decision::Ask);
        assert_eq!(
            verdict.hook_restriction,
            Some(arany::permissions::Decision::Ask)
        );
        assert_eq!(verdict.hooks, 1);
        assert_eq!(tool.hooks.len(), 1);
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::ToolStarted { verdict: Some(verdict), .. }
                if verdict.source == arany::permissions::PermissionSource::UnavailableApproval
                    && !verdict.approved
        )));
        assert!(tool.observation.as_ref().unwrap().attestation.is_none());
    } else {
        let asked = if live_edits {
            &view.runs[0].tools[0]
        } else {
            tool
        };
        assert_eq!(Some(serde_json::to_value(&asked.intent).unwrap()), intent);
        assert_eq!(
            asked.permission_verdict.as_ref().unwrap().source,
            arany::permissions::PermissionSource::Human
        );
    }
    assert_eq!(tool.permission_verdict.as_ref().unwrap().approved, allow);
    assert_eq!(
        tool.permission_verdict.as_ref().unwrap().source,
        if hook_ask {
            arany::permissions::PermissionSource::UnavailableApproval
        } else if live_edits {
            arany::permissions::PermissionSource::Mode
        } else {
            arany::permissions::PermissionSource::Human
        }
    );
    assert_eq!(
        tool.observation.as_ref().unwrap().disposition,
        if allow {
            arany::ToolDisposition::Succeeded
        } else {
            arany::ToolDisposition::Denied
        }
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::ToolFinished { .. }))
            .count(),
        calls
    );
    assert_eq!(provider.requests.lock().unwrap().len(), calls + 1);
    assert!(provider.responses.lock().unwrap().is_empty());
    if live_edits {
        assert_eq!(std::fs::read(workspace.join("next.txt")).unwrap(), b"Next");
    }
    if write {
        assert_eq!(
            std::fs::read(workspace.join("new.txt")).unwrap(),
            b"Written"
        );
        let intent_digest: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&tool.intent).unwrap()).into();
        assert!(matches!(
            &tool.observation.as_ref().unwrap().attestation,
            Some(arany::Attestation::InProcess { receipt: Some(receipt), .. })
                if receipt.intent_digest == intent_digest
        ));
    }
}

async fn next_permission_update(
    reader: &mut Reader,
    task: &mut tokio::task::JoinHandle<Result<(), String>>,
    case: PermissionCase,
) -> Value {
    let mut line = String::new();
    let count = tokio::time::timeout(RPC_WAIT, reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    if count == 0 {
        assert!(
            task.is_finished(),
            "{case:?}: unexpected ACP EOF; transport still pending"
        );
        let transport = task.await;
        panic!("{case:?}: unexpected ACP EOF; transport={transport:?}");
    }
    let value = serde_json::from_str(&line).unwrap();
    validate(&value);
    value
}

fn permission_answer(case: PermissionCase, allow: bool) -> &'static str {
    if allow {
        "Finished"
    } else {
        match case {
            PermissionCase::HookAsk => {
                "The requested read needs an unavailable human approval channel."
            }
            PermissionCase::Read
            | PermissionCase::Write
            | PermissionCase::ModeSwitch
            | PermissionCase::LiveEdits => "The requested operation was declined and did not run.",
        }
    }
}

fn permission_outcomes(
    case: PermissionCase,
    allow: bool,
    call: arany::ToolCall,
    second: arany::ToolCall,
) -> Vec<arany::ProviderOutcome> {
    use arany::test_support::scenario::tool_outcome;
    let mut outcomes = vec![tool_outcome(call)];
    match case {
        PermissionCase::ModeSwitch => outcomes.push(outcomes[0].clone()),
        PermissionCase::LiveEdits => outcomes.push(tool_outcome(second)),
        PermissionCase::Read | PermissionCase::Write | PermissionCase::HookAsk => {}
    }
    outcomes.push(arany::ProviderOutcome::Answer(arany::Answer {
        text: permission_answer(case, allow).into(),
    }));
    outcomes
}

fn assert_permission_continuation(
    view: &arany::SessionView,
    provider: &ScriptedProvider,
    allow: bool,
    case: PermissionCase,
) {
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Finished, "{case:?}");
    assert!(run.failure.is_none());
    assert_eq!(
        run.assistant_message.as_deref(),
        Some(permission_answer(case, allow))
    );
    assert_eq!(view.goals[0].status, arany::GoalStatus::Completed);
    let requests = provider.requests.lock().unwrap();
    let context = requests.last().unwrap().input.tools.as_ref().unwrap();
    let step = context.steps.last().unwrap();
    let tool = run.tools.last().unwrap();
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(step.requests.len(), 1, "{case:?}: original request");
    assert_eq!(step.requests[0].id.as_str(), "call_1");
    assert_eq!(step.requests[0].call, tool.intent.call);
    assert_eq!(
        step.observations.as_slice(),
        std::slice::from_ref(observation)
    );
    if !allow {
        let label = match case {
            PermissionCase::HookAsk => "channel_unavailable",
            PermissionCase::Read
            | PermissionCase::Write
            | PermissionCase::ModeSwitch
            | PermissionCase::LiveEdits => "user_declined",
        };
        assert_eq!(
            observation.output,
            format!("{label}: no operation was dispatched")
        );
        assert!(observation.attestation.is_none());
        assert_eq!(run.tools.len(), 1, "{case:?}: no automatic retry");
    }
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native ACP host policy; requires the arany executable"
)]
fn admitted_host_prefixes_follow_h1_without_acp_permission_requests() {
    let _gate = crate::test_process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let launcher = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("arany");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (mode, rule, restriction, allowed) in [
                ("default", true, None, true),
                ("dontAsk", true, None, true),
                ("auto", true, None, true),
                ("acceptEdits", true, None, true),
                ("default", false, None, false),
                ("default", true, Some("ask"), false),
                ("dontAsk", true, Some("ask"), false),
                ("default", true, Some("deny"), false),
            ] {
                let _deadline = crate::cli::serve::tests::deadline();
                host_policy_scenario(launcher.clone(), mode, rule, restriction, allowed).await;
            }
        });
}

async fn host_policy_scenario(
    launcher: PathBuf,
    mode: &str,
    rule: bool,
    restriction: Option<&str>,
    allowed: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let call =
        arany::test_support::approval::seed_host_prefix(&state, &workspace, launcher.clone()).await;
    let path = state.join("permissions.json");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    settings["permissions"]["defaultMode"] = json!(mode);
    if !rule {
        settings["permissions"]["hostPrefixes"] = json!([]);
    }
    if let Some(restriction) = restriction {
        settings["permissions"][restriction] = json!(["Bash"]);
    }
    std::fs::write(&path, serde_json::to_vec(&settings).unwrap()).unwrap();
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            arany::test_support::scenario::tool_outcome(call),
            arany::ProviderOutcome::Answer(arany::Answer {
                text: "Host policy settled".into(),
            }),
        ])
        .with_profile("openai", "gpt-5.4"),
    );
    let (mut writer, mut reader, task) = start_with_guard(&state, provider.clone(), true, launcher);
    initialize(&mut writer, &mut reader).await;
    let session = session(&mut writer, &mut reader, &workspace).await;
    send(
        &mut writer,
        json!(3),
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"Inspect host value"}]}),
    )
    .await;
    writer.write_all(&encode_line(&json!({"jsonrpc":"2.0","id":999,"result":{"outcome":{"outcome":"selected","optionId":"allow_always"}}})).unwrap()).await.unwrap();
    let mut tools = 0;
    loop {
        let message = next(&mut reader).await;
        assert_ne!(
            message["method"], "session/request_permission",
            "{mode} {restriction:?}"
        );
        if message["params"]["update"]["sessionUpdate"] == "tool_call_update" {
            tools += 1;
            assert!(
                message["params"]["update"]["title"]
                    .as_str()
                    .unwrap()
                    .contains("Unguarded command")
            );
        }
        if message["id"] == 3 {
            assert_eq!(message["result"]["stopReason"], "end_turn");
            break;
        }
    }
    assert_eq!(tools, 1);
    writer.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        serde_json::to_vec(&settings).unwrap()
    );
    assert_host_policy_history(&state, session, &provider, allowed).await;
}

async fn assert_host_policy_history(
    state: &std::path::Path,
    session: SessionId,
    provider: &ScriptedProvider,
    allowed: bool,
) {
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert!(provider.responses.lock().unwrap().is_empty());
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    for prefix in 1..=events.len() {
        arany::SessionView::replay(session, &events[..prefix]).unwrap();
    }
    let tool = &view.runs[0].tools[0];
    assert_eq!(tool.permission_verdict.as_ref().unwrap().approved, allowed);
    assert_eq!(view.runs[0].unguarded_tools(), usize::from(allowed));
    if allowed {
        assert_eq!(
            tool.permission_verdict.as_ref().unwrap().source,
            arany::permissions::PermissionSource::HostPrefixRule
        );
        assert!(
            tool.observation
                .as_ref()
                .unwrap()
                .output
                .contains("host value")
        );
        assert!(matches!(
            tool.observation.as_ref().unwrap().attestation,
            Some(arany::Attestation::Unguarded(_))
        ));
    } else {
        assert!(tool.observation.as_ref().unwrap().attestation.is_none());
        assert_eq!(
            tool.observation.as_ref().unwrap().disposition,
            arany::ToolDisposition::Denied
        );
    }
}
