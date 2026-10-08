use super::*;
use crate::process::BoundedOutput;
use arany::{CompactionStatus, Effort, OutputTokenBound, ProviderWireProvenance, RunStatus};
use serde_json::{Value, json};

const WORKSPACE_ID: &str = "wrkspc_Selected123";
const MODEL: &str = "claude-sonnet-5";
const CHECKED_MODEL: &str = "claude-offline-new";
const OBJECTIVE: &str = "scoped offline objective";

pub(super) fn respond(input: &mut impl Write, output: &mut impl Read) {
    for index in 0..9 {
        let request = chatgpt::read_request(output);
        let header_end = request
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .expect("bounded header boundary")
            + 4;
        let headers = std::str::from_utf8(&request[..header_end]).expect("ASCII headers");
        let catalog = matches!(index, 0 | 1 | 5);
        assert!(
            headers.starts_with(if catalog {
                "GET /v1/models?limit=1000 HTTP/1.1\r\n"
            } else {
                "POST /v1/messages HTTP/1.1\r\n"
            }),
            "unexpected scoped request path at phase {index}"
        );
        for expected in [
            "host: api.anthropic.com".to_owned(),
            format!("anthropic-workspace-id: {WORKSPACE_ID}"),
            if catalog {
                format!("x-api-key: {KEY}")
            } else {
                format!("authorization: Bearer {KEY}")
            },
        ] {
            assert!(
                headers
                    .lines()
                    .filter(|line| line.eq_ignore_ascii_case(&expected))
                    .count()
                    == 1,
                "missing or duplicate selected scope/authentication header at phase {index}"
            );
        }
        assert!(
            !request
                .windows(AMBIENT_WORKSPACE.len())
                .any(|part| part == AMBIENT_WORKSPACE.as_bytes()),
            "saved account inherited ambient workspace"
        );
        if catalog {
            chatgpt::send_json(
                input,
                &serde_json::to_vec(&json!({
                    "data": [
                        {"id": MODEL, "type": "model", "display_name": "Reviewed fixture"},
                        {"id": CHECKED_MODEL, "type": "model", "display_name": "Checked fixture"}
                    ],
                    "has_more": false,
                    "first_id": MODEL,
                    "last_id": CHECKED_MODEL
                }))
                .expect("synthetic catalog"),
            );
            continue;
        }
        let payload = &request[header_end..];
        assert!(
            !payload
                .windows(KEY.len())
                .any(|part| part == KEY.as_bytes()),
            "selected credential entered the request body"
        );
        let body: Value = serde_json::from_slice(payload).expect("bounded Messages request");
        let model = if index < 4 { MODEL } else { CHECKED_MODEL };
        assert_eq!(body["model"], model);
        assert_eq!(
            body["max_tokens"],
            if matches!(index, 2 | 4) { 4096 } else { 1024 }
        );
        assert_eq!(body["stream"], false);
        assert_eq!(
            body["output_config"]["effort"],
            if index < 4 { "low" } else { "high" }
        );
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        let content: Value = serde_json::from_str(
            body["messages"][0]["content"]
                .as_str()
                .expect("structured input"),
        )
        .expect("input JSON");
        match index {
            2 | 4 => assert_eq!(content["objective"], OBJECTIVE),
            3 => {
                assert_eq!(content["items"][0]["user"], OBJECTIVE);
                assert_eq!(content["items"][0]["assistant"], "scoped answer");
            }
            6 => assert_eq!(
                content["objective"],
                "Synthetic conformance check: finish directly with a short factual sentence. No Workspace content is supplied."
            ),
            7 => assert_eq!(
                content["objective"],
                "Synthetic conformance check: delegate exactly one independent read-only question about the number two. No Workspace content is supplied."
            ),
            8 => assert_eq!(
                content["items"][0]["user"],
                "Synthetic conformance question"
            ),
            _ => unreachable!("catalog phases returned before Messages decoding"),
        }
        if matches!(index, 6 | 7) {
            assert!(content["workspace_guidance"].is_null());
            assert_eq!(content["includes"], json!([]));
            assert_eq!(content["history"], json!([]));
            assert_eq!(content["child_results"], json!([]));
            assert!(content["derived_context_summary"].is_null());
        }
        let outcome = match index {
            3 | 8 => json!({"summary": "scoped compacted"}),
            7 => json!({"outcome": {"type": "delegate", "children": ["Explain two"]}}),
            _ => {
                json!({"outcome": {"type": "finish", "summary": "scoped summary", "result": "scoped answer"}})
            }
        };
        chatgpt::send_json(
            input,
            &serde_json::to_vec(&json!({
                "id": format!("msg_scoped_{index}"), "type": "message", "role": "assistant",
                "model": model, "stop_reason": "end_turn", "stop_details": null,
                "stop_sequence": null, "content": [{"type": "text", "text": outcome.to_string()}],
                "usage": {"input_tokens": 12, "output_tokens": 8}
            }))
            .expect("synthetic Messages reply"),
        );
    }
}

fn command(args: &[&str]) -> std::process::Output {
    let output = Command::new("/usr/bin/timeout")
        .args(["-k", "1s", "15s", "/arany"])
        .args(args)
        .env_clear()
        .env("HOME", "/root")
        .env("XDG_STATE_HOME", "/root/legacy-state")
        .env("SSL_CERT_FILE", "/fixture/ca.pem")
        .env("ANTHROPIC_WORKSPACE_ID", AMBIENT_WORKSPACE)
        .current_dir("/root/workspace")
        .stdin(Stdio::null())
        .bounded_output()
        .expect("bounded scoped product command");
    assert!(output.status.success(), "scoped command failed");
    assert!(output.stderr.is_empty(), "scoped command emitted an error");
    assert!(
        !output
            .stdout
            .windows(KEY.len())
            .any(|part| part == KEY.as_bytes()),
        "credential entered headless output"
    );
    output
}

pub(super) fn stage() {
    let mut server = start_response_server(NativeReplies::ScopedAnthropic);
    let setup_output = run_pty(
        &SETUP_SHELL.replace("state-one", "state-scoped"),
        &[
            (b"type a choice name: API key or ChatGPT plan", b"API key\r"),
            (
                b"type a choice name: Cancel or Use private file",
                b"Use private file\r",
            ),
            (b"type a choice name: OpenAI or Anthropic", b"Anthropic\r"),
            (b"Setup: Enter API key", b"synthetic-offline-api-key\r"),
            (
                b"type a choice name: Key scoped to workspace or Choose API workspace",
                b"Choose API workspace\r",
            ),
            (b"Setup: API workspace ID", b"wrkspc_Selected123\r"),
        ],
        None,
    );
    let root = StateRoot::open_existing(Path::new("/root/.local/state/arany"))
        .expect("private account root");
    let record = root
        .read_saved_account_record()
        .expect("read account")
        .expect("saved account");
    let saved: Value = serde_json::from_slice(&record).expect("bounded scoped account");
    assert_eq!(saved["schema"], 1);
    assert_eq!(saved["storage"], "private_file");
    assert_eq!(saved["account"]["schema"], 2);
    assert_eq!(saved["account"]["provider"], "anthropic");
    assert_eq!(saved["account"]["anthropic_workspace_id"], WORKSPACE_ID);
    assert!(
        saved["account"]["api_key"] == KEY,
        "saved credential differs"
    );
    let id: Uuid = saved["account"]["id"]
        .as_str()
        .expect("account ID")
        .parse()
        .expect("UUID");
    unsaved("/root/state-scoped");
    assert!(
        String::from_utf8_lossy(&setup_output)
            .contains(&format!("Provider: anthropic\r\nModel: {MODEL}\r\n"))
    );

    let models = command(&["provider", "models", "anthropic", "--saved-account"]);
    assert_eq!(models.stdout, "Provider: anthropic\nModels: 2\nclaude-offline-new · availability only; choose effort; compatibility check optional\nclaude-sonnet-5 · reviewed metadata; effort low, medium, high, xhigh, max\n".as_bytes());

    let output = run_pty(
        &TURN_SHELL.replace("state-three", "state-scoped-run"),
        &[
            (b"Input:\r\n", b"scoped offline objective\r"),
            (b"Answer:\r\n  scoped answer", b"/compact\r"),
        ],
        Some(b"Notice: Compaction saved; 12 input / 8 output tokens"),
    );
    assert!(!String::from_utf8_lossy(&output).contains("scoped compacted"));
    let direct = session("/root/state-scoped-run");
    assert_eq!(direct.defaults.provider.as_deref(), Some("anthropic"));
    assert_eq!(direct.defaults.model.as_deref(), Some(MODEL));
    assert_eq!(direct.defaults.effort, Some(Effort::Low));
    assert_eq!(direct.defaults.account_id, Some(id));
    assert_eq!(direct.runs.len(), 1);
    assert_eq!(direct.compactions.len(), 1);
    let compact = &direct.compactions[0].record;
    assert_eq!(compact.provider, "anthropic");
    assert_eq!(compact.model, MODEL);
    assert_eq!(compact.response_id.as_deref(), Some("msg_scoped_3"));
    assert_eq!(compact.input_tokens, Some(12));
    assert_eq!(compact.output_tokens, Some(8));
    assert_eq!(
        compact.wire_provenance,
        Some(ProviderWireProvenance::MessagesEndTurnStorageUnspecified)
    );
    assert!(
        matches!(&compact.status, CompactionStatus::Succeeded { summary, .. } if summary == "scoped compacted")
    );

    run_pty(
        &CHECK_SHELL.replace("state-four", "state-scoped-check"),
        &[
            (b"Input:\r\n", b"/model claude-offline-new high\r"),
            (
                b"Notice: Model: claude-offline-new",
                b"scoped offline objective\r",
            ),
        ],
        Some(b"Answer:\r\n  scoped answer"),
    );
    let checked = command(&[
        "provider",
        "check",
        "anthropic",
        CHECKED_MODEL,
        "--effort",
        "high",
        "--accept-cost",
        "--saved-account",
        "--state-dir",
        "/root/state-scoped-check",
    ]);
    assert_eq!(checked.stdout, b"Provider: anthropic\nModel: claude-offline-new\nEffort: high\nCredential: saved API account\nStatus: native synthetic conformance passed\n");
    server
        .responder
        .take()
        .expect("scoped TLS peer")
        .join()
        .expect("all scoped requests");
    drop(server);
    let checked = session("/root/state-scoped-check");
    assert_eq!(checked.defaults.account_id, Some(id));
    assert_eq!(
        checked.runs.len(),
        1,
        "conformance calls must not create Runs"
    );
    for (view, model, response_id) in [
        (&direct, MODEL, "msg_scoped_2"),
        (&checked, CHECKED_MODEL, "msg_scoped_4"),
    ] {
        let run = &view.runs[0];
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(run.objective, OBJECTIVE);
        assert_eq!(run.assistant_message.as_deref(), Some("scoped answer"));
        let config = run.config.as_ref().expect("pinned configuration");
        assert_eq!(config.provider, "anthropic");
        assert_eq!(config.model, model);
        assert_eq!(
            config.effort,
            Some(if model == MODEL {
                Effort::Low
            } else {
                Effort::High
            })
        );
        assert_eq!(config.saved_api_account_id, Some(id));
        assert_eq!(
            config.output_token_bound,
            OutputTokenBound::ProviderEnforced
        );
        assert_eq!(run.agents.len(), 1);
        assert_eq!(run.agents[0].provider_calls.len(), 1);
        let call = &run.agents[0].provider_calls[0];
        assert_eq!(call.response_id.as_deref(), Some(response_id));
        assert_eq!(call.input_tokens, Some(12));
        assert_eq!(call.output_tokens, Some(8));
        assert_eq!(
            call.wire_provenance,
            Some(ProviderWireProvenance::MessagesEndTurnStorageUnspecified)
        );
    }
    assert_eq!(
        checked.runs[0]
            .config
            .as_ref()
            .unwrap()
            .provider_concurrency,
        1
    );
    assert!(
        record
            == root
                .read_saved_account_record()
                .expect("reopen account")
                .expect("saved account"),
        "catalog, conformance or turns replaced the account"
    );
}
