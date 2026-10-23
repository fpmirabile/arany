use super::*;
use crate::process::BoundedOutput;

const FINAL_MESSAGE_FAILURES: [(&str, &[Option<&str>]); 4] = [
    ("ResponseMissingFinalMessage", &[Some("commentary")]),
    (
        "ResponseDuplicateFinalMessage",
        &[Some("final_answer"), Some("final_answer")],
    ),
    (
        "ResponseAmbiguousFinalMessage",
        &[None, Some("final_answer")],
    ),
    (
        "ResponseLateCommentary",
        &[Some("final_answer"), Some("commentary")],
    ),
];

fn final_message_failure_stream(phases: &[Option<&str>]) -> String {
    let output = phases
        .iter()
        .map(|phase| {
            serde_json::json!({
                "type": "message", "role": "assistant", "status": "completed", "phase": phase,
                "content": [{"type": "output_text", "text": serde_json::json!(FAILURE_BODY_CANARY).to_string()}]
            })
        })
        .collect::<Vec<_>>();
    format!(
        "data: {{\"type\":\"response.created\"}}\n\ndata: {}\n\n",
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_final_message_rejection", "status": "completed", "model": "gpt-6.1-sol",
                "output": output, "usage": {"input_tokens": 12, "output_tokens": 8}
            }
        })
    )
}

const OBJECTIVE: &str = "offline chatgpt objective";
const UNCHECKED_OBJECTIVE: &str = "offline direct Run without a model check";
const CATALOG_FAILURE_OBJECTIVE: &str = "offline direct Run without a model catalog";
const ANSWER: &str = "offline chatgpt answer";
const FOLLOWUP_OBJECTIVE: &str = "offline chatgpt follow-up";
const FOLLOWUP_ANSWER: &str = "offline follow-up answer";
const TEAM_OBJECTIVE: &str = "offline chatgpt team objective";
const TEAM_CHILD: &str = "offline child assignment";
const TEAM_CHILD_ANSWER: &str = "offline child answer";
const TEAM_ANSWER: &str = "offline team answer";
const FAILED_OBJECTIVE: &str = "offline usage-limit objective";
const COMMENTARY_CANARY: &str = "UNCOMMITTED_COMMENTARY_CANARY";
pub(super) const FAILURE_BODY_CANARY: &str = "UPSTREAM_FAILURE_BODY_CANARY";

fn checked_request(output: &mut impl Read, method: &str) -> Option<serde_json::Value> {
    let request = read_request(output);
    let header_end = request
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("HTTP request headers")
        + 4;
    let headers = std::str::from_utf8(&request[..header_end]).expect("HTTPS request headers");
    assert!(
        headers.starts_with(&format!("{method} HTTP/1.1\r\n")),
        "unexpected ChatGPT route"
    );
    assert!(
        headers.lines().any(|line| line
            .trim_end_matches('\r')
            .eq_ignore_ascii_case(&format!("authorization: Bearer {ACCESS}"))),
        "selected ChatGPT token missing from its compiled route"
    );
    assert_eq!(
        request
            .windows(ACCESS.len())
            .filter(|part| *part == ACCESS.as_bytes())
            .count(),
        1,
        "access token appeared outside its bearer header"
    );
    let body = &request[header_end..];
    assert!(
        !body
            .windows(ACCESS.len())
            .any(|part| part == ACCESS.as_bytes()),
        "access token entered request body"
    );
    assert!(
        !body
            .windows(REFRESH.len())
            .any(|part| part == REFRESH.as_bytes()),
        "refresh token entered request body"
    );
    if method.starts_with("GET") {
        assert!(body.is_empty(), "catalog request had a body");
        None
    } else {
        assert!(
            headers.lines().any(|line| line
                .trim_end_matches('\r')
                .eq_ignore_ascii_case("accept: text/event-stream")),
            "Responses request must negotiate SSE"
        );
        Some(serde_json::from_slice(body).expect("bounded Responses request"))
    }
}

fn send_sse(input: &mut impl Write, id: &str, text: serde_json::Value) {
    let mut response = serde_json::json!({
        "id": id,
        "status": "completed",
        "model": "gpt-6.1-sol",
        "output": arany::semantic::responses_output(&text),
        "usage": {"input_tokens": 12, "output_tokens": 8}
    });
    if id == "resp_without_check" {
        let text = text.as_str().expect("plain answer").to_owned();
        let split = text.find(ANSWER).expect("unchecked direct answer") + ANSWER.len() / 2;
        let (first, last) = text.split_at(split);
        response["output"] = serde_json::json!([
            {
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "phase": "commentary",
                "content": [{"type": "output_text", "text": COMMENTARY_CANARY}]
            },
            {
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "phase": "final_answer",
                "content": [
                    {"type": "output_text", "text": first},
                    {"type": "output_text", "text": last}
                ]
            }
        ]);
    }
    let mut stream = format!(
        "event: response.created\r\ndata: {}\r\n\r\n",
        serde_json::json!({"type":"response.created", "response":{"id":id}})
    );
    let output = response["output"].take().as_array().unwrap().clone();
    for (index, mut item) in output.into_iter().enumerate() {
        item["id"] = serde_json::json!(format!("msg_{index}"));
        if item["type"] == "message" && item["phase"].is_null() {
            item["phase"] = serde_json::json!("final_answer");
        }
        stream.push_str(&format!("event: response.output_item.done\r\ndata: {}\r\n\r\n",
            serde_json::json!({"type":"response.output_item.done", "output_index":index, "item":item})));
    }
    response["output"] = serde_json::json!([]);
    stream.push_str(&format!(
        "event: response.completed\r\ndata: {}\r\n\r\n",
        serde_json::json!({"type": "response.completed", "response": response})
    ));
    let stream = if id == "resp_without_check" {
        stream
            .split_inclusive("\r\n")
            .filter(|line| !line.starts_with("event:"))
            .collect::<String>()
    } else {
        stream
    };
    let content_type = if id == "resp_without_check" {
        "application/json"
    } else {
        "text/event-stream"
    };
    input
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nx-codex-primary-used-percent: 25.5\r\nx-codex-primary-window-minutes: 300\r\nx-codex-primary-reset-at: 2000000000\r\nx-codex-secondary-used-percent: 70\r\nx-codex-secondary-window-minutes: 10080\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                stream.len()
            )
            .as_bytes(),
        )
        .expect("Responses headers");
    input.write_all(stream.as_bytes()).expect("Responses event");
}

pub(super) fn respond_after_catalog(
    input: &mut impl Write,
    output: &mut impl Read,
    catalog: &[u8],
    refresh_ready: Receiver<()>,
) {
    assert!(checked_request(output, "GET /v1/models").is_none());
    refresh_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("cached picker must open while refresh is held");
    let refreshed = br#"{"models":[{"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","visibility":"list"},{"slug":"model-new","display_name":"New model","visibility":"list"}]}"#;
    send_json(input, refreshed);
    assert!(checked_request(output, "GET /v1/models").is_none());
    input
        .write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .expect("failed cached refresh");
    respond_unchecked(input, output, catalog);
    respond_without_catalog(input, output);
    respond_probes(input, output, catalog);
    respond_team(input, output, catalog);
    respond_failure(input, output, catalog);
    respond_diagnostics(input, output, catalog);
}

fn serve_catalog(input: &mut impl Write, output: &mut impl Read, catalog: &[u8]) {
    assert!(checked_request(output, "GET /v1/models").is_none());
    send_json(input, catalog);
}

fn serve_unavailable_catalog(input: &mut impl Write, output: &mut impl Read) {
    assert!(checked_request(output, "GET /v1/models").is_none());
    input
        .write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .expect("unavailable Run catalog");
}

fn respond_without_catalog(input: &mut impl Write, output: &mut impl Read) {
    serve_unavailable_catalog(input, output);
    let body = checked_request(output, "POST /v1/responses").expect("unchecked Run body");
    assert_eq!(
        crate::semantic::input(&body)["objective"],
        CATALOG_FAILURE_OBJECTIVE
    );
    send_sse(input, "resp_without_catalog", serde_json::json!(ANSWER));
}

fn respond_unchecked(input: &mut impl Write, output: &mut impl Read, catalog: &[u8]) {
    serve_catalog(input, output, catalog);
    let direct = checked_request(output, "POST /v1/responses").expect("first direct Run body");
    assert_eq!(direct["model"], "gpt-6.1-sol");
    assert_eq!(direct["reasoning"]["effort"], "low");
    assert_eq!(direct["store"], false);
    assert_eq!(direct["stream"], true);
    assert!(direct.get("max_output_tokens").is_none());
    assert!(direct.get("previous_response_id").is_none());
    assert!(direct.get("text").is_none());
    assert!(direct.get("tool_choice").is_none() && direct.get("tools").is_none());
    let context: serde_json::Value = crate::semantic::input(&direct);
    assert_eq!(
        context,
        serde_json::json!({
            "phase": "root_plan",
            "collaboration": {"mode": "single"},
            "objective": UNCHECKED_OBJECTIVE,
            "workspace_guidance": null,
            "includes": [],
            "history": [],
            "derived_context_summary": null,
            "child_results": []
        })
    );
    send_sse(input, "resp_without_check", serde_json::json!(ANSWER));
}

fn respond_probes(input: &mut impl Write, output: &mut impl Read, catalog: &[u8]) {
    serve_catalog(input, output, catalog);
    let probes = [
        (
            "resp_check_direct",
            "Synthetic conformance check: finish directly",
            serde_json::json!("done"),
        ),
        (
            "resp_check_delegate",
            "Synthetic conformance check: delegate exactly one",
            serde_json::json!({"outcome": {"type": "delegate", "children":[{"objective":"Explain two","profile":null}]}}),
        ),
        (
            "resp_check_compact",
            "Synthetic conformance question",
            serde_json::json!(r#"Condensed"#),
        ),
        ("resp_offline_chatgpt", OBJECTIVE, serde_json::json!(ANSWER)),
        (
            "resp_offline_chatgpt_followup",
            FOLLOWUP_OBJECTIVE,
            serde_json::json!(FOLLOWUP_ANSWER),
        ),
    ];
    for (index, (id, expected_input, outcome)) in probes.into_iter().enumerate() {
        if index == 3 {
            serve_unavailable_catalog(input, output);
        } else if index > 3 {
            serve_catalog(input, output, catalog);
        }
        let body = checked_request(output, "POST /v1/responses").expect("Responses body");
        assert_eq!(body["model"], "gpt-6.1-sol");
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("text").is_none());
        let rendered_input = body["input"].to_string();
        let content = rendered_input.as_str();
        assert!(content.contains(expected_input), "wrong probe or Run input");
        assert_eq!(content.contains(OBJECTIVE), index >= 3);
        assert_eq!(content.contains(FOLLOWUP_OBJECTIVE), index == 4);
        if index >= 3 {
            assert!(!content.contains("Synthetic conformance"));
            let structured = crate::semantic::input(&body);
            assert_eq!(structured["objective"], expected_input);
            assert_eq!(
                structured["history"],
                if index == 3 {
                    serde_json::json!([])
                } else {
                    serde_json::json!([{"user": OBJECTIVE, "assistant": ANSWER}])
                }
            );
        }
        send_sse(input, id, outcome);
    }
}

fn respond_team(input: &mut impl Write, output: &mut impl Read, catalog: &[u8]) {
    serve_catalog(input, output, catalog);
    for (id, phase, objective, outcome) in [
        (
            "resp_team_plan",
            "root_plan",
            TEAM_OBJECTIVE,
            serde_json::json!({"outcome": {"type": "delegate", "children": [{"objective": TEAM_CHILD, "profile": null}]}}),
        ),
        (
            "resp_team_child",
            "child_work",
            TEAM_CHILD,
            serde_json::json!(TEAM_CHILD_ANSWER),
        ),
        (
            "resp_team_synthesis",
            "root_synthesis",
            TEAM_OBJECTIVE,
            serde_json::json!(TEAM_ANSWER),
        ),
    ] {
        let body = checked_request(output, "POST /v1/responses").expect("team Responses body");
        assert_eq!(body["model"], "gpt-6.1-sol");
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("text").is_none());
        let content = body["input"].to_string();
        assert!(!content.contains("Synthetic conformance"));
        let context: serde_json::Value = crate::semantic::input(&body);
        assert_eq!(context["phase"], phase);
        assert_eq!(context["objective"], objective);
        assert_eq!(
            context["history"],
            if phase == "child_work" {
                serde_json::json!([])
            } else {
                serde_json::json!([
                    {"user": OBJECTIVE, "assistant": ANSWER},
                    {"user": FOLLOWUP_OBJECTIVE, "assistant": FOLLOWUP_ANSWER}
                ])
            }
        );
        assert_eq!(
            context["child_results"],
            if phase == "root_synthesis" {
                serde_json::json!([{
                    "objective": TEAM_CHILD,
                    "summary": "child summary",
                    "result": TEAM_CHILD_ANSWER
                }])
            } else {
                serde_json::json!([])
            }
        );
        send_sse(input, id, outcome);
    }
}

fn respond_failure(input: &mut impl Write, output: &mut impl Read, catalog: &[u8]) {
    serve_catalog(input, output, catalog);
    let body = checked_request(output, "POST /v1/responses").expect("failed Run body");
    assert_eq!(body["model"], "gpt-6.1-sol");
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("previous_response_id").is_none());
    let context: serde_json::Value = crate::semantic::input(&body);
    assert_eq!(context["objective"], FAILED_OBJECTIVE);
    assert_eq!(context["phase"], "root_plan");
    assert_eq!(
        context["history"],
        serde_json::json!([
            {"user": OBJECTIVE, "assistant": ANSWER},
            {"user": FOLLOWUP_OBJECTIVE, "assistant": FOLLOWUP_ANSWER},
            {"user": TEAM_OBJECTIVE, "assistant": TEAM_ANSWER},
        ])
    );
    let rejected_body = format!("{FAILURE_BODY_CANARY} {ACCESS} {REFRESH}");
    // A Retry-After beyond the action deadline is never waited out, so the 429 fails the Run at once.
    input.write_all(format!("HTTP/1.1 429 Too Many Requests\r\nx-codex-secondary-used-percent: 71\r\nx-codex-secondary-window-minutes: 10080\r\nRetry-After: 86400\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{rejected_body}", rejected_body.len()).as_bytes()).expect("synthetic Run rejection");
}

fn completed_diagnostic(model: &str, text: serde_json::Value) -> String {
    format!(
        "data: {{\"type\":\"response.created\"}}\n\ndata: {}\n\n",
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_contract_rejection",
                "status": "completed",
                "model": model,
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": text.to_string()}]
                }],
                "usage": {"input_tokens": 12, "output_tokens": 8}
            }
        })
    )
}

fn respond_diagnostics(input: &mut impl Write, output: &mut impl Read, catalog: &[u8]) {
    // A stream that ends before completion is a retryable cut; the wire corpus owns it.
    for (stage, stream) in [
        (
            "IncompleteResponse",
            "data: {\"type\":\"response.incomplete\"}\n\n".to_owned(),
        ),
        (
            "ResponseModel",
            completed_diagnostic("gpt-6-astra", serde_json::json!("done")),
        ),
    ]
    .into_iter()
    .chain(
        FINAL_MESSAGE_FAILURES.map(|(stage, phases)| (stage, final_message_failure_stream(phases))),
    ) {
        serve_catalog(input, output, catalog);
        let body =
            checked_request(output, "POST /v1/responses").expect("diagnostic failure request");
        assert_eq!(body["model"], "gpt-6.1-sol");
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("text").is_none());
        let context: serde_json::Value = crate::semantic::input(&body);
        assert_eq!(
            context,
            serde_json::json!({
                "phase": "root_plan",
                "collaboration": {"mode": "single"},
                "objective": format!("offline diagnostic {stage}"),
                "workspace_guidance": null,
                "includes": [],
                "history": [],
                "derived_context_summary": null,
                "child_results": []
            })
        );
        input.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{stream}", stream.len()).as_bytes()).expect("diagnostic failure reply");
    }
}

fn replace_selected_identity() -> (StateRoot, Vec<u8>) {
    let root = StateRoot::open_existing(Path::new("/root/.local/state/arany"))
        .expect("isolated account root");
    let record = root
        .record(RecordKind::ChatgptAccounts)
        .read()
        .expect("original record")
        .expect("saved account");
    let mut changed: serde_json::Value =
        serde_json::from_slice(&record).expect("synthetic account JSON");
    let replacement = Uuid::now_v7().to_string();
    changed["selected"] = replacement.clone().into();
    changed["accounts"][0]["id"] = replacement.clone().into();
    changed["accounts"][0]["token"]["credentials"]["access_expires_at_unix"] = 1.into();
    for check in changed["model_checks"].as_array_mut().expect("checks") {
        check["account_id"] = replacement.clone().into();
    }
    root.record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&changed).expect("replacement JSON"))
        .expect("synthetic identity drift");
    (root, record)
}

pub(super) fn accept_check_and_run(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    signed: &str,
    refresh_ready: &mpsc::Sender<()>,
) {
    let open_at = transcript.len();
    input
        .write_all(b"/model\r")
        .expect("open cached setup catalog");
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        open_at,
        b"Choose number [effort|default], n next, p previous, or q close:\r\n",
        Some(signed),
    );
    let frame = String::from_utf8_lossy(&transcript[open_at..]);
    assert!(frame.contains("Catalog: refreshing"));
    assert!(frame.contains("Choice 1: gpt-6.1-sol; selected effort low"));
    assert!(!frame.contains("Loading model catalog"));
    refresh_ready
        .send(())
        .expect("release refresh only after cached choices are visible");
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        open_at,
        b"these choice numbers are unchanged\r\n",
        Some(signed),
    );
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        open_at,
        b"Choose number [effort|default], n next, p previous, or q close:\r\n",
        Some(signed),
    );
    let close_at = transcript.len();
    input.write_all(b"q\r").unwrap();
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        close_at,
        b"Input:\r\n",
        Some(signed),
    );
    let reopen_at = transcript.len();
    input.write_all(b"/model\r").unwrap();
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        reopen_at,
        b"Catalog: refresh failed; cached models\r\n",
        Some(signed),
    );
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        reopen_at,
        b"Choose number [effort|default], n next, p previous, or q close:\r\n",
        Some(signed),
    );
    assert!(String::from_utf8_lossy(&transcript[reopen_at..]).contains("Choice 2: model-new"));
    let close_at = transcript.len();
    input.write_all(b"q\r").unwrap();
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        close_at,
        b"Input:\r\n",
        Some(signed),
    );
    let root = StateRoot::open_existing(Path::new("/root/.local/state/arany"))
        .expect("isolated account root");
    let record = root
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let index: serde_json::Value = serde_json::from_slice(&record).unwrap();
    assert!(index["model_checks"].as_array().unwrap().is_empty());
    let result = Command::new("/arany")
        .args([
            "exec",
            "--provider",
            "chatgpt",
            "--model",
            "gpt-6.1-sol",
            "--effort",
            "low",
            "--collaboration",
            "single",
            "--output",
            "jsonl",
            "--workspace",
            "/root/workspace",
            "--state-dir",
            "/root/state-without-check",
            UNCHECKED_OBJECTIVE,
        ])
        .env_clear()
        .env("SSL_CERT_FILE", "/fixture/ca.pem")
        .bounded_output()
        .expect("direct shipped Run before any model check");
    assert!(result.status.success(), "direct unchecked Run failed");
    assert!(result.stderr.is_empty());
    let direct = session("/root/state-without-check");
    assert_catalog_window(&direct);
    assert_eq!(direct.runs.len(), 1);
    assert_eq!(direct.runs[0].objective, UNCHECKED_OBJECTIVE);
    assert_eq!(direct.runs[0].status, arany::RunStatus::Finished);
    assert_eq!(direct.runs[0].assistant_message.as_deref(), Some(ANSWER));
    let provenance = direct.runs[0]
        .config
        .as_ref()
        .unwrap()
        .chatgpt_provenance
        .as_ref()
        .unwrap();
    assert_eq!(
        provenance.account_id.to_string(),
        index["selected"].as_str().unwrap()
    );
    assert_eq!(
        provenance.admission,
        arany::ChatGptAdmission::AccountConsent
    );
    assert_ne!(provenance.evidence_fingerprint, [0; 32]);
    assert!(
        root.record(RecordKind::ChatgptAccounts)
            .read()
            .unwrap()
            .unwrap()
            == record
    );
    let journal = std::fs::read("/root/state-without-check/events.sqlite3").unwrap();
    for secret in [ACCESS, REFRESH, signed, COMMENTARY_CANARY] {
        assert!(
            !result
                .stdout
                .windows(secret.len())
                .any(|part| part == secret.as_bytes())
        );
        assert!(
            !journal
                .windows(secret.len())
                .any(|part| part == secret.as_bytes())
        );
    }
    run_without_catalog();
    let diagnostic = Command::new("/arany")
        .args([
            "provider",
            "check",
            "chatgpt",
            "gpt-6.1-sol",
            "--effort",
            "medium",
            "--accept-cost",
        ])
        .env_clear()
        .env("SSL_CERT_FILE", "/fixture/ca.pem")
        .current_dir("/root/workspace")
        .bounded_output()
        .expect("explicit optional synthetic diagnostic after the first real Run");
    assert!(diagnostic.status.success());
    assert!(diagnostic.stderr.is_empty());
    assert!(String::from_utf8_lossy(&diagnostic.stdout).contains("Context window: 272k tokens\n"));
    assert!(
        String::from_utf8_lossy(&diagnostic.stdout)
            .contains("optional diagnostic, not Run authorization")
    );
    for effort in ["high", "low", "medium"] {
        let select_at = transcript.len();
        input
            .write_all(format!("/model gpt-6.1-sol {effort}\r").as_bytes())
            .expect("select effort without a check");
        wait_for_stage(
            output,
            input,
            transcript,
            answered,
            select_at,
            format!("Notice: Model: gpt-6.1-sol · {effort}").as_bytes(),
            Some(signed),
        );
        wait_for_stage(
            output,
            input,
            transcript,
            answered,
            select_at,
            b"Input:\r\n",
            Some(signed),
        );
        assert!(
            !transcript[select_at..]
                .windows(b"Plan-consuming check".len())
                .any(|part| part == b"Plan-consuming check")
        );
    }
    let turn_at = transcript.len();
    input
        .write_all(format!("{OBJECTIVE}\r").as_bytes())
        .expect("submit ChatGPT objective");
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        turn_at,
        format!("Arany · Answer:\r\n  {ANSWER}").as_bytes(),
        Some(signed),
    );
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        turn_at,
        b"Input:\r\n",
        Some(signed),
    );
    let followup_at = transcript.len();
    input
        .write_all(format!("{FOLLOWUP_OBJECTIVE}\r").as_bytes())
        .expect("submit ChatGPT follow-up");
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        followup_at,
        format!("Arany · Answer:\r\n  {FOLLOWUP_ANSWER}").as_bytes(),
        Some(signed),
    );
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        followup_at,
        b"Input:\r\n",
        Some(signed),
    );
    let team_at = transcript.len();
    input
        .write_all(b"/agents team 1\r")
        .expect("choose one-child team");
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        team_at,
        b"Input:\r\n",
        Some(signed),
    );
    let third_at = transcript.len();
    input
        .write_all(format!("{TEAM_OBJECTIVE}\r").as_bytes())
        .expect("submit ChatGPT team objective");
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        third_at,
        format!("Arany · Answer:\r\n  {TEAM_ANSWER}").as_bytes(),
        Some(signed),
    );
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        third_at,
        b"Input:\r\n",
        Some(signed),
    );
    let failure_at = transcript.len();
    input
        .write_all(format!("{FAILED_OBJECTIVE}\r").as_bytes())
        .expect("submit synthetic rejected Run");
    wait_for_stage(output, input, transcript, answered, failure_at,
        b"Notice: Error: Run failed. Provider rate limited the request. Wait before trying again. No answer was committed. Use /agents to inspect this Run.\r\n", Some(signed));
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        failure_at,
        b"Input:\r\n",
        Some(signed),
    );
    assert!(
        !transcript[failure_at..]
            .windows(b"Answer:".len())
            .any(|part| part == b"Answer:")
    );
    assert!(
        !transcript
            .windows(FAILURE_BODY_CANARY.len())
            .any(|part| part == FAILURE_BODY_CANARY.as_bytes())
    );
    for (stage, reason, events) in [
        (
            "IncompleteResponse",
            arany::ProviderFailureReason::StreamProtocol,
            1,
        ),
        (
            "ResponseModel",
            arany::ProviderFailureReason::ResponseContract,
            2,
        ),
    ]
    .into_iter()
    .chain(
        FINAL_MESSAGE_FAILURES
            .map(|(stage, _)| (stage, arany::ProviderFailureReason::ResponseContract, 2)),
    ) {
        let state = format!("/root/state-diagnostic-{stage}");
        let objective = format!("offline diagnostic {stage}");
        let result = Command::new("/arany")
            .args([
                "exec",
                "--provider",
                "chatgpt",
                "--model",
                "gpt-6.1-sol",
                "--effort",
                "low",
                "--collaboration",
                "single",
                "--output",
                "jsonl",
                "--workspace",
                "/root/workspace",
                "--state-dir",
                &state,
                &objective,
            ])
            .env_clear()
            .env("SSL_CERT_FILE", "/fixture/ca.pem")
            .bounded_output()
            .unwrap();
        assert!(!result.status.success());
        for secret in [ACCESS, REFRESH, signed] {
            assert!(
                !result
                    .stderr
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes()),
                "credential entered failure channel"
            );
        }
        assert_eq!(result.stderr, b"error: Run failed\n");
        let view = session(&state);
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, arany::RunStatus::Failed);
        assert!(view.runs[0].assistant_message.is_none());
        assert_eq!(
            view.runs[0].agents[0].provider_calls[0].failure_reason,
            Some(reason)
        );
        let path = Path::new(&state).join("development.log");
        if cfg!(debug_assertions) {
            let log = std::fs::read(&path)
                .expect("debug process must record its actual subscription failure");
            let text = std::str::from_utf8(&log).unwrap();
            assert!(text.contains("runtime stage=DiagnosticsEnabled"));
            assert!(text.contains("provider phase=RootPlan disposition=InvalidResponse"));
            assert!(text.contains(&format!("reason=Some({reason:?})")));
            assert!(text.contains("run disposition=Failed"));
            let headers = text
                .lines()
                .filter(|line| line.starts_with("subscription "))
                .collect::<Vec<_>>();
            assert_eq!(headers.len(), 1, "one diagnostic per rejected call");
            let fields = headers[0].split_ascii_whitespace().collect::<Vec<_>>();
            assert_eq!(fields.len(), 5);
            assert_eq!(fields[1], format!("stage={stage}"));
            let bytes = fields[2]
                .strip_prefix("bytes=")
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert!(bytes > 0 && bytes <= 1024 * 1024);
            assert_eq!(fields[3], format!("events={events}"));
            assert_eq!(fields[4], "http_status=Some(200)");
            if let Some((_, phases)) = FINAL_MESSAGE_FAILURES
                .iter()
                .find(|(name, _)| *name == stage)
            {
                assert!(text.contains(&format!(
                    "response_shape messages={} commentary={} finals={} unphased={} done_messages=0 done_finals=0",
                    phases.len(),
                    phases.iter().filter(|phase| **phase == Some("commentary")).count(),
                    phases.iter().filter(|phase| **phase == Some("final_answer")).count(),
                    phases.iter().filter(|phase| phase.is_none()).count(),
                )));
            }
            assert!(
                text.contains(if reason == arany::ProviderFailureReason::StreamProtocol {
                    "subscription::completed_at"
                } else {
                    "subscription::invoke_at"
                })
            );
            assert!(log.len() <= 256 * 1024);
            let marker = "arany 0.1.0 development diagnostic\n";
            assert!(
                text.split(marker)
                    .skip(1)
                    .all(|entry| entry.len() + marker.len() <= 24 * 1024)
            );
            for forbidden in [
                ACCESS,
                REFRESH,
                signed,
                FAILURE_BODY_CANARY,
                &objective,
                ANSWER,
            ] {
                assert!(
                    !log.windows(forbidden.len())
                        .any(|part| part == forbidden.as_bytes()),
                    "sensitive value entered diagnostic log"
                );
                assert!(
                    !result
                        .stdout
                        .windows(forbidden.len())
                        .any(|part| part == forbidden.as_bytes())
                        || forbidden == objective
                );
            }
        } else {
            assert!(
                !path.exists(),
                "release must not enable local debug logging"
            );
        }
    }
    let (root, record) = replace_selected_identity();
    let drift_at = transcript.len();
    input
        .write_all(b"rejected identity objective\r")
        .expect("reject changed identity at real Run admission");
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        drift_at,
        b"invalid authorization identity",
        Some(signed),
    );
    wait_for_stage(
        output,
        input,
        transcript,
        answered,
        drift_at,
        b"Input:\r\n",
        Some(signed),
    );
    root.record(RecordKind::ChatgptAccounts)
        .replace(&record)
        .expect("restore test-owned account");
}

fn run_without_catalog() {
    let state = "/root/state-without-catalog";
    let result = Command::new("/arany")
        .args([
            "exec",
            "--provider",
            "chatgpt",
            "--model",
            "gpt-6.1-sol",
            "--effort",
            "low",
            "--collaboration",
            "single",
            "--output",
            "jsonl",
            "--workspace",
            "/root/workspace",
            "--state-dir",
            state,
            CATALOG_FAILURE_OBJECTIVE,
        ])
        .env_clear()
        .env("SSL_CERT_FILE", "/fixture/ca.pem")
        .bounded_output()
        .expect("direct Run with unavailable catalog and no check");
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    let view = session(state);
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].objective, CATALOG_FAILURE_OBJECTIVE);
    assert_eq!(view.runs[0].status, arany::RunStatus::Finished);
    assert_eq!(view.runs[0].assistant_message.as_deref(), Some(ANSWER));
    assert_eq!(
        view.runs[0].config.as_ref().unwrap().context_window_tokens,
        None
    );
}

fn assert_catalog_window(view: &SessionView) {
    for run in &view.runs {
        assert_eq!(
            run.config
                .as_ref()
                .expect("Run config")
                .context_window_tokens,
            Some(CONTEXT_WINDOW)
        );
    }
}

pub(super) fn assert_runs(view: &SessionView, account_id: Uuid, check: &serde_json::Value) {
    assert_catalog_window(view);
    assert_eq!(check["account_id"], account_id.to_string());
    let checked_fingerprint = check["fingerprint"]
        .as_array()
        .expect("checked fingerprint")
        .iter()
        .map(|byte| byte.as_u64().expect("fingerprint byte") as u8)
        .collect::<Vec<_>>();
    assert_eq!(checked_fingerprint.len(), 32);
    assert_eq!(
        view.runs.len(),
        4,
        "checked direct/team turns and rejected Run count"
    );
    assert_ne!(view.runs[0].id, view.runs[1].id);
    assert!(view.runs[0].finished_sequence.expect("first finish") < view.runs[1].accepted_sequence);
    assert!(
        view.runs[1].finished_sequence.expect("second finish") < view.runs[2].accepted_sequence
    );
    for (run, objective, answer, response_id) in [
        (&view.runs[0], OBJECTIVE, ANSWER, "resp_offline_chatgpt"),
        (
            &view.runs[1],
            FOLLOWUP_OBJECTIVE,
            FOLLOWUP_ANSWER,
            "resp_offline_chatgpt_followup",
        ),
    ] {
        assert_eq!(run.objective, objective);
        assert_eq!(run.status, arany::RunStatus::Finished);
        assert_eq!(run.assistant_message.as_deref(), Some(answer));
        let config = run.config.as_ref().expect("pinned ChatGPT Run config");
        assert_eq!(config.provider, "chatgpt");
        assert_eq!(config.model, "gpt-6.1-sol");
        assert_eq!(config.effort, Some(arany::Effort::Medium));
        assert_eq!(config.saved_api_account_id, None);
        assert_eq!(
            config.output_token_bound,
            arany::OutputTokenBound::LocalAcceptanceOnly
        );
        let provenance = config
            .chatgpt_provenance
            .as_ref()
            .expect("ChatGPT account provenance");
        assert_eq!(provenance.account_id, account_id);
        assert_eq!(
            provenance.admission,
            arany::ChatGptAdmission::AccountConsent
        );
        assert_ne!(
            provenance.evidence_fingerprint.as_slice(),
            checked_fingerprint
        );
        assert_eq!(run.agents.len(), 1);
        assert_eq!(run.agents[0].provider_calls.len(), 1);
        let call = &run.agents[0].provider_calls[0];
        assert_eq!(call.response_id.as_deref(), Some(response_id));
        let limits = call.rate_limits.as_ref().expect("committed header facts");
        assert_eq!(
            limits.primary.as_ref().unwrap().used_percent.map(f64::from),
            Some(25.5)
        );
        assert_eq!(
            limits.secondary.as_ref().unwrap().window_minutes,
            Some(10080)
        );
        assert_eq!(call.input_tokens, Some(12));
        assert_eq!(call.output_tokens, Some(8));
        assert_eq!(
            call.wire_provenance,
            Some(arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
        );
    }
    assert!(
        view.runs[0]
            .config
            .as_ref()
            .expect("first config")
            .history_run_ids
            .is_empty()
    );
    assert_eq!(
        view.runs[1]
            .config
            .as_ref()
            .expect("second config")
            .history_run_ids,
        vec![view.runs[0].id]
    );
    let team = &view.runs[2];
    assert_eq!(team.objective, TEAM_OBJECTIVE);
    assert_eq!(team.status, arany::RunStatus::Finished);
    assert_eq!(team.assistant_message.as_deref(), Some(TEAM_ANSWER));
    let config = team.config.as_ref().expect("pinned team config");
    assert_eq!(config.provider, "chatgpt");
    assert_eq!(config.model, "gpt-6.1-sol");
    assert_eq!(config.effort, Some(arany::Effort::Medium));
    assert_eq!(config.saved_api_account_id, None);
    assert_eq!(
        config.policy,
        arany::CollaborationPolicy::Team {
            max_active_children: 1
        }
    );
    assert_eq!(config.provider_concurrency, 1);
    assert_eq!(
        config.output_token_bound,
        arany::OutputTokenBound::LocalAcceptanceOnly
    );
    let provenance = config
        .chatgpt_provenance
        .as_ref()
        .expect("team account provenance");
    assert_eq!(provenance.account_id, account_id);
    assert_eq!(
        provenance.admission,
        arany::ChatGptAdmission::AccountConsent
    );
    assert_eq!(
        provenance,
        view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .chatgpt_provenance
            .as_ref()
            .unwrap()
    );
    assert_ne!(
        provenance.evidence_fingerprint.as_slice(),
        checked_fingerprint
    );
    assert_eq!(
        config.history_run_ids,
        vec![view.runs[0].id, view.runs[1].id]
    );
    assert_eq!(team.agents.len(), 2);
    assert_eq!(team.agents[0].role, arany::AgentRole::Primary);
    assert_eq!(team.agents[1].role, arany::AgentRole::Child);
    assert_eq!(team.agents[0].status, arany::AgentStatus::Finished);
    assert_eq!(team.agents[1].status, arany::AgentStatus::Finished);
    assert_eq!(team.agents[1].ordinal, 1);
    assert_eq!(team.agents[0].provider_calls.len(), 2);
    assert_eq!(team.agents[1].provider_calls.len(), 1);
    for (call, phase, response_id) in [
        (
            &team.agents[0].provider_calls[0],
            arany::AgentPhase::RootPlan,
            "resp_team_plan",
        ),
        (
            &team.agents[1].provider_calls[0],
            arany::AgentPhase::ChildWork,
            "resp_team_child",
        ),
        (
            &team.agents[0].provider_calls[1],
            arany::AgentPhase::RootSynthesis,
            "resp_team_synthesis",
        ),
    ] {
        assert_eq!(call.phase, phase);
        assert_eq!(call.response_id.as_deref(), Some(response_id));
        let limits = call.rate_limits.as_ref().expect("committed header facts");
        assert_eq!(
            limits.primary.as_ref().unwrap().used_percent.map(f64::from),
            Some(25.5)
        );
        assert_eq!(
            limits.secondary.as_ref().unwrap().window_minutes,
            Some(10080)
        );
        assert_eq!(call.input_tokens, Some(12));
        assert_eq!(call.output_tokens, Some(8));
        assert_eq!(
            call.wire_provenance,
            Some(arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
        );
    }
    let failed = &view.runs[3];
    assert!(team.finished_sequence.expect("team finish") < failed.accepted_sequence);
    assert_eq!(failed.objective, FAILED_OBJECTIVE);
    assert_eq!(failed.status, arany::RunStatus::Failed);
    assert_eq!(failed.assistant_message, None);
    let config = failed.config.as_ref().expect("failed Run config");
    assert_eq!(config.provider, "chatgpt");
    assert_eq!(config.model, "gpt-6.1-sol");
    assert_eq!(config.effort, Some(arany::Effort::Medium));
    assert_eq!(config.saved_api_account_id, None);
    assert_eq!(
        config.output_token_bound,
        arany::OutputTokenBound::LocalAcceptanceOnly
    );
    let provenance = config
        .chatgpt_provenance
        .as_ref()
        .expect("failed Run account");
    assert_eq!(provenance.account_id, account_id);
    assert_eq!(
        provenance.admission,
        arany::ChatGptAdmission::AccountConsent
    );
    assert_eq!(
        provenance,
        view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .chatgpt_provenance
            .as_ref()
            .unwrap()
    );
    assert_ne!(
        provenance.evidence_fingerprint.as_slice(),
        checked_fingerprint
    );
    assert_eq!(
        config.history_run_ids,
        vec![view.runs[0].id, view.runs[1].id, team.id]
    );
    assert_eq!(failed.agents.len(), 1);
    assert_eq!(failed.agents[0].status, arany::AgentStatus::Failed);
    assert_eq!(failed.agents[0].provider_calls.len(), 1);
    assert!(
        failed.provider_retries.is_empty(),
        "hostile Retry-After was waited out"
    );
    let call = &failed.agents[0].provider_calls[0];
    assert_eq!(
        call.rate_limits
            .as_ref()
            .unwrap()
            .secondary
            .as_ref()
            .unwrap()
            .used_percent
            .map(f64::from),
        Some(71.0)
    );
    assert_eq!(call.phase, arany::AgentPhase::RootPlan);
    assert_eq!(call.disposition, arany::ProviderCallDisposition::Rejected);
    // A 429 whose body names no quota is a rate limit, not proof of an exhausted plan.
    assert_eq!(
        call.failure_reason,
        Some(arany::ProviderFailureReason::RateLimited)
    );
    assert_eq!(
        (
            call.response_id.as_ref(),
            call.input_tokens,
            call.output_tokens,
            call.wire_provenance
        ),
        (None, None, None, None)
    );
}
