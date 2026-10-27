use super::*;

#[tokio::test]
async fn modes_and_config_options_stay_in_sync_and_refuse_invalid_values_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    for (method, mode) in [
        ("session/set_mode", "acceptEdits"),
        ("session/set_config_option", "auto"),
        ("session/set_config_option", "default"),
    ] {
        let params = if method == "session/set_mode" {
            json!({"sessionId":adapter.session,"modeId":mode,"_meta":{"tools":true}})
        } else {
            json!({"sessionId":adapter.session,"configId":"mode","value":mode})
        };
        adapter.call(json!(9), method, params).await;
        let request = adapter.request().await;
        assert_eq!(request.method, "thread/settings/update");
        assert_eq!(
            request.params.unwrap(),
            json!({"threadId":adapter.session,"permissionMode":mode})
        );
        adapter
            .call(
                json!(10),
                "session/resume",
                json!({"sessionId":adapter.session,"cwd":"/var/workspace"}),
            )
            .await;
        assert_eq!(adapter.next().await["error"]["data"]["reason"], "busy");
        assert!(adapter.input.try_recv().is_err());
        adapter
            .events
            .send(Message::Response(Response {
                jsonrpc: Default::default(),
                id: request.id,
                result: json!({"permissionMode":mode}),
            }))
            .await
            .unwrap();
        let current = adapter.next().await;
        assert_eq!(
            current["params"]["update"],
            json!({"sessionUpdate":"current_mode_update","currentModeId":mode})
        );
        let config = adapter.next().await;
        assert_eq!(
            config["params"]["update"]["configOptions"][0]["currentValue"],
            mode
        );
        let response = adapter.next().await;
        if method == "session/set_mode" {
            assert_eq!(response["result"], json!({}));
        } else {
            assert_eq!(
                response["result"]["configOptions"],
                config["params"]["update"]["configOptions"]
            );
        }
        for method in ["session/load", "session/resume"] {
            adapter
                .call(
                    json!(11),
                    method,
                    json!({"sessionId":adapter.session,"cwd":"/var/workspace","mcpServers":[]}),
                )
                .await;
            let request = adapter.request().await;
            assert_eq!(request.method, "thread/resume");
            assert_eq!(request.params.as_ref().unwrap()["permissionMode"], mode);
            adapter.events.send(Message::Response(Response {jsonrpc:Default::default(), id:request.id,
                result:json!({"thread":{"id":adapter.session,"workspace":"/private/var/workspace","title":"Test","createdAtMs":0},"turns":[]})})).await.unwrap();
            let loaded = adapter.next().await;
            validate_session_result_schema(&loaded, method);
            assert_mode_state(&loaded["result"], mode);
        }
    }
    for mode in [
        json!("plan"),
        json!("dontAsk"),
        json!("bypassPermissions"),
        json!("bypass"),
        json!("Default"),
        json!(""),
        json!("x".repeat(65536)),
        json!(true),
        json!({"auto":null}),
        json!(42),
        json!(null),
        json!("unknown"),
    ] {
        for (method, params) in [
            (
                "session/set_mode",
                json!({"sessionId":adapter.session,"modeId":mode}),
            ),
            (
                "session/set_config_option",
                json!({"sessionId":adapter.session,"configId":"mode","value":mode}),
            ),
        ] {
            adapter.call(json!(9), method, params).await;
            assert_eq!(adapter.next().await["error"]["code"], -32602);
            assert!(adapter.input.try_recv().is_err());
            assert!(adapter.output.try_recv().is_err());
            adapter
                .call(
                    json!(11),
                    "session/resume",
                    json!({"sessionId":adapter.session,"cwd":"/var/workspace"}),
                )
                .await;
            let request = adapter.request().await;
            assert_eq!(
                request.params.as_ref().unwrap()["permissionMode"],
                "default"
            );
            adapter.events.send(Message::Response(Response {jsonrpc:Default::default(), id:request.id,
                result:json!({"thread":{"id":adapter.session,"workspace":"/private/var/workspace","title":"Test","createdAtMs":0},"turns":[]})})).await.unwrap();
            assert_mode_state(&adapter.next().await["result"], "default");
        }
    }
    for params in [
        json!({"sessionId":adapter.session,"configId":"model","value":"auto"}),
        json!({"sessionId":adapter.session,"configId":"mode","value":true,"type":"boolean"}),
        json!({"sessionId":adapter.session,"configId":"mode","value":"auto","type":"boolean"}),
        json!({"sessionId":adapter.session,"configId":"x".repeat(65536),"value":"auto"}),
    ] {
        adapter
            .call(json!(9), "session/set_config_option", params)
            .await;
        assert_eq!(adapter.next().await["error"]["code"], -32602);
    }
    adapter
        .call(
            json!(9),
            "session/set_mode",
            json!({"sessionId":SessionId::new(),"modeId":"default"}),
        )
        .await;
    assert_eq!(adapter.next().await["error"]["code"], -32002);
    assert!(adapter.input.try_recv().is_err());
    adapter
        .call(
            json!(12),
            "session/set_mode",
            json!({"sessionId":adapter.session,"modeId":"auto"}),
        )
        .await;
    let request = adapter.request().await;
    adapter
        .events
        .send(Message::Error(ErrorResponse {
            jsonrpc: Default::default(),
            id: Some(request.id),
            error: ErrorObject {
                code: -32600,
                message: "unavailable".into(),
                data: Some(ErrorData {
                    reason: Reason::ThreadNotLoaded,
                }),
            },
        }))
        .await
        .unwrap();
    assert_eq!(adapter.next().await["error"]["code"], -32002);
    assert!(
        adapter.output.try_recv().is_err(),
        "failed update must not change either selector"
    );
    adapter
        .call(
            json!(13),
            "session/resume",
            json!({"sessionId":adapter.session,"cwd":"/var/workspace"}),
        )
        .await;
    let request = adapter.request().await;
    assert_eq!(request.params.unwrap()["permissionMode"], "default");
    adapter.events.send(Message::Response(Response {jsonrpc:Default::default(), id:request.id,
        result:json!({"thread":{"id":adapter.session,"workspace":"/private/var/workspace","title":"Test","createdAtMs":0},"turns":[]})})).await.unwrap();
    assert_mode_state(&adapter.next().await["result"], "default");
    for _ in 0..61 {
        adapter
            .call(
                json!(14),
                "session/set_config_option",
                json!({"sessionId":adapter.session,"configId":"mode","type":"id","value":"auto"}),
            )
            .await;
    }
    adapter
        .call(
            json!(15),
            "session/set_mode",
            json!({"sessionId":adapter.session,"modeId":"default"}),
        )
        .await;
    assert_eq!(adapter.next().await["error"]["data"]["reason"], "busy");
    for _ in 0..61 {
        let request = adapter.request().await;
        assert_eq!(request.method, "thread/settings/update");
        assert_eq!(
            request.params.unwrap(),
            json!({"threadId":adapter.session,"permissionMode":"auto"})
        );
    }
    assert!(adapter.input.try_recv().is_err());
    adapter.close().await;
}

#[test]
fn acp_cli_modes_are_closed_and_do_not_enable_tools() {
    use clap::Parser;
    for mode in [
        "default",
        "acceptEdits",
        "auto",
        "plan",
        "dontAsk",
        "bypassPermissions",
        "Auto",
    ] {
        let parsed = crate::Cli::try_parse_from([
            "arany",
            "acp",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
            "--permission-mode",
            mode,
        ]);
        if matches!(mode, "default" | "acceptEdits" | "auto") {
            let crate::Command::Acp(args) = parsed.unwrap().command.unwrap() else {
                panic!("ACP args")
            };
            assert_eq!(serde_json::to_value(args.permission_mode).unwrap(), mode);
            assert!(!args.tools);
        } else {
            assert!(parsed.is_err(), "unsupported CLI mode: {mode}");
        }
    }
}
