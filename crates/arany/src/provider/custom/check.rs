use super::{CustomProfile, CustomProfileError, PinnedDestination, transport::CustomTransport};
use crate::provider::evidence::ProviderEvidenceRecord;
use crate::provider::{
    AgentPhase, CompactionItem, CompactionRequest, HistoryTurn, PromptRole, ProviderOutcome,
    ProviderRequest, RunInput,
};
use crate::session::{AgentRunId, RunId, SessionId};
use crate::store::{StateRoot, Store};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TOTAL_DEADLINE: Duration = Duration::from_secs(180);
const EVIDENCE_AGE_MS: i64 = 24 * 60 * 60 * 1000;
const CHECK_VERSION: &str = "custom-protocol-schema-challenge-conformance-effort-stream-v32";

pub struct CustomProfileCheck {
    profile_name: String,
    expires_at_ms: i64,
}

impl CustomProfileCheck {
    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

pub async fn check_custom_profile(
    root: StateRoot,
    name: &str,
) -> Result<CustomProfileCheck, CustomProfileError> {
    let profile = CustomProfile::load_named(&root, name)?;
    let store = Store::open(root).map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    let result = async {
        store
            .clear_provider_evidence(profile.name().to_owned())
            .await
            .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
        let record = tokio::time::timeout(TOTAL_DEADLINE, async {
            let destination = profile.resolve_destination().await?;
            let key = std::env::var(profile.credential_env())
                .map_err(|_| CustomProfileError::CredentialUnavailable)?;
            probe(&profile, &destination, key).await
        })
        .await
        .map_err(|_| CustomProfileError::ConformanceFailed)??;
        store
            .record_provider_evidence(record.clone())
            .await
            .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
        let observed = store
            .load_provider_evidence(record.name.clone())
            .await
            .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
        if observed.as_ref() != Some(&record) {
            return Err(CustomProfileError::EvidenceUnavailable);
        }
        Ok(CustomProfileCheck {
            profile_name: record.name,
            expires_at_ms: record.expires_at_ms,
        })
    }
    .await;
    let closed = store.close().await;
    let report = result?;
    closed.map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    Ok(report)
}

async fn probe(
    profile: &CustomProfile,
    destination: &PinnedDestination,
    key: String,
) -> Result<ProviderEvidenceRecord, CustomProfileError> {
    let transport = CustomTransport::new(profile, destination, key)?;
    let session_id = SessionId::new();
    let direct = ProviderRequest {
        session_id,
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Single,
        model: profile.model().to_owned(),
        max_output_tokens: profile.max_output_tokens(),
        input: RunInput::new(PromptRole::ReadOnly, "Synthetic conformance check: finish directly with a short factual sentence. No Workspace content is supplied.".into()),
    };
    let (preview, observed) = crate::provider::ResponsePreview::new();
    let direct_response = transport
        .run_streamed(&direct, None, Some(preview))
        .await
        .map_err(|_| CustomProfileError::ConformanceFailed)?;
    if !matches!(&direct_response.outcome, ProviderOutcome::Answer(value) if !observed.borrow().answer.is_empty() && value.text.starts_with(&observed.borrow().answer))
    {
        return Err(CustomProfileError::ConformanceFailed);
    }
    let direct_id = direct_response
        .response_id
        .ok_or(CustomProfileError::ConformanceFailed)?;

    let delegate = ProviderRequest {
        session_id,
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Team { max_active_children: 1 },
        model: profile.model().to_owned(),
        max_output_tokens: 128,
        input: RunInput::new(PromptRole::ReadOnly, "Synthetic conformance check: delegate exactly one independent read-only question about the number two. No Workspace content is supplied.".into()),
    };
    let (preview, observed) = crate::provider::ResponsePreview::new();
    let delegate_response = transport
        .run_streamed(&delegate, None, Some(preview))
        .await
        .map_err(|_| CustomProfileError::ConformanceFailed)?;
    if !matches!(&delegate_response.outcome, ProviderOutcome::Delegate(value) if value.children.len() == 1 && !value.children[0].objective.trim().is_empty() && value.children[0].profile.is_none() && observed.borrow().answer.is_empty())
    {
        return Err(CustomProfileError::ConformanceFailed);
    }
    let delegate_id = delegate_response
        .response_id
        .ok_or(CustomProfileError::ConformanceFailed)?;
    if delegate_id == direct_id {
        return Err(CustomProfileError::ConformanceFailed);
    }

    let compact = CompactionRequest {
        session_id,
        covered_run_id: RunId::new(),
        model: profile.model().to_owned(),
        previous_summary: None,
        items: vec![CompactionItem::Completed(HistoryTurn {
            user: "Synthetic conformance question".into(),
            assistant: "Synthetic conformance answer".into(),
        })],
        max_output_tokens: 128,
    };
    let compact_id = probe_compaction(&transport, &compact, None).await?;
    if compact_id == direct_id || compact_id == delegate_id {
        return Err(CustomProfileError::ConformanceFailed);
    }

    let mut seen_ids = vec![direct_id, delegate_id, compact_id];
    for effort in profile.efforts() {
        let mut selected = direct.clone();
        selected.run_id = RunId::new();
        selected.agent_run_id = AgentRunId::new();
        selected.max_output_tokens = 128;
        let (preview, observed) = crate::provider::ResponsePreview::new();
        let response = transport
            .run_streamed(&selected, Some(*effort), Some(preview))
            .await
            .map_err(|_| CustomProfileError::ConformanceFailed)?;
        if !matches!(&response.outcome, ProviderOutcome::Answer(value) if !observed.borrow().answer.is_empty() && value.text.starts_with(&observed.borrow().answer))
        {
            return Err(CustomProfileError::ConformanceFailed);
        }
        let id = response
            .response_id
            .ok_or(CustomProfileError::ConformanceFailed)?;
        if seen_ids.contains(&id) {
            return Err(CustomProfileError::ConformanceFailed);
        }
        seen_ids.push(id);
        let compact_id = probe_compaction(&transport, &compact, Some(*effort)).await?;
        if seen_ids.contains(&compact_id) {
            return Err(CustomProfileError::ConformanceFailed);
        }
        seen_ids.push(compact_id);
    }

    let checked_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?
        .as_millis();
    let checked_at_ms =
        i64::try_from(checked_at_ms).map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    let expires_at_ms = checked_at_ms
        .checked_add(EVIDENCE_AGE_MS)
        .ok_or(CustomProfileError::EvidenceUnavailable)?;
    Ok(ProviderEvidenceRecord {
        name: profile.name().to_owned(),
        digest: fingerprint(profile),
        addresses: destination.addresses().to_vec(),
        checked_at_ms,
        expires_at_ms,
    })
}

async fn probe_compaction(
    transport: &CustomTransport,
    request: &CompactionRequest,
    effort: Option<crate::provider::Effort>,
) -> Result<String, CustomProfileError> {
    let mut random = [0; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| CustomProfileError::ConformanceFailed)?;
    let nonce = uuid::Uuid::from_bytes(random).simple().to_string();
    let response = transport
        .probe_compaction(request, effort, &nonce)
        .await
        .map_err(|_| CustomProfileError::ConformanceFailed)?;
    if response.summary != nonce {
        return Err(CustomProfileError::ConformanceFailed);
    }
    response
        .response_id
        .ok_or(CustomProfileError::ConformanceFailed)
}

pub(super) fn fingerprint(profile: &CustomProfile) -> [u8; 32] {
    let (protocol, base, encoding, auth) = match &profile.protocol {
        super::Protocol::OpenaiResponses => ("openai-responses", "", "json_schema", "bearer"),
        super::Protocol::AnthropicMessages { base_url, auth } => (
            "anthropic-messages",
            base_url.as_str(),
            "native_tools",
            match auth {
                super::MessagesAuth::Bearer => "bearer",
                super::MessagesAuth::ApiKey => "x-api-key",
            },
        ),
    };
    let mut digest = Sha256::new();
    for field in [
        env!("CARGO_PKG_VERSION"),
        CHECK_VERSION,
        profile.name(),
        profile.endpoint(),
        profile.model(),
        profile.credential_env(),
        "2",
        protocol,
        base,
        encoding,
        auth,
        "user_authorized",
        "4096",
    ] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    digest.update(2_u32.to_be_bytes());
    for effort in profile.efforts() {
        let value = effort.as_str();
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    digest.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Url;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const CONTRACT_RESPONSE_BYTES: usize = 1024 * 1024;

    #[derive(Clone, Copy)]
    enum ProbeReply {
        Valid,
        ValidEfforts,
        CompactionRejected,
        IgnoredFormat,
        EffortCompactionRejected,
        ZeroUsage,
        AggregateJsonNodes,
        WrongModel,
        Redirect,
        Oversize,
        ChunkedAtLimit,
        ChunkedOversize,
        EncodedBody,
        EncodedBodyAfterIdentity,
        EscapedCredential,
        MissingUsage,
        InputAtLimit,
        InputAboveLimit,
    }

    async fn serve_probe(
        listener: TcpListener,
        reply: ProbeReply,
        messages: Option<super::super::MessagesAuth>,
    ) {
        let count = if matches!(
            reply,
            ProbeReply::ValidEfforts | ProbeReply::EffortCompactionRejected
        ) {
            7
        } else if matches!(
            reply,
            ProbeReply::Valid | ProbeReply::CompactionRejected | ProbeReply::IgnoredFormat
        ) {
            3
        } else {
            1
        };
        let mut seen_nonces = std::collections::HashSet::new();
        for index in 0..count {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .expect("probe connection deadline")
                .expect("probe connection");
            let mut request = Vec::new();
            let (header_end, body_len) = loop {
                let mut chunk = [0; 4096];
                let size = stream.read(&mut chunk).await.expect("request bytes");
                assert!(size > 0 && request.len() + size <= 512 * 1024);
                request.extend_from_slice(&chunk[..size]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let header_end = end + 4;
                    let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
                    let body_len = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .expect("content length");
                    if request.len() >= header_end + body_len {
                        break (header_end, body_len);
                    }
                }
            };
            let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
            let header = header.to_ascii_lowercase();
            assert!(header.starts_with(if messages.is_some() {
                "post /anthropic/v1/messages http/1.1\r\n"
            } else {
                "post /v1/responses http/1.1\r\n"
            }));
            assert!(header.contains(concat!("user-agent: arany/", env!("CARGO_PKG_VERSION"))));
            assert!(!header.contains("anthropic-workspace-id"));
            if messages == Some(super::super::MessagesAuth::ApiKey) {
                assert!(header.contains("x-api-key: test-key\r\n"));
                assert!(!header.contains("authorization:"));
            } else {
                assert!(header.contains("authorization: bearer test-key\r\n"));
                assert!(!header.contains("x-api-key:"));
            }
            assert_eq!(
                header.contains("anthropic-version: 2023-06-01\r\n"),
                messages.is_some()
            );
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end..header_end + body_len])
                    .expect("JSON request");
            let streaming = body["stream"] == true;
            let content_type = if streaming {
                "text/event-stream"
            } else {
                "application/json"
            };
            let compaction = if messages.is_some() {
                body["output_config"]["format"]["type"] == "json_schema"
            } else {
                body["text"]["format"]["name"] == "arany_compaction"
            };
            let schema = if messages.is_some() {
                &body["output_config"]["format"]["schema"]
            } else {
                &body["text"]["format"]["schema"]
            };
            let nonce = schema
                .pointer("/properties/summary/enum/0")
                .and_then(serde_json::Value::as_str);
            if let Some(nonce) = nonce {
                assert!(
                    compaction
                        && nonce.len() == 32
                        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
                );
                assert!(
                    seen_nonces.insert(nonce.to_owned()),
                    "every compaction probe needs a fresh challenge"
                );
                let mut without_schema = body.clone();
                if messages.is_some() {
                    without_schema["output_config"]["format"]
                        .as_object_mut()
                        .unwrap()
                        .remove("schema");
                } else {
                    without_schema["text"]["format"]
                        .as_object_mut()
                        .unwrap()
                        .remove("schema");
                }
                assert!(
                    !without_schema.to_string().contains(nonce),
                    "the challenge occurs only in the schema, never in a prompt"
                );
            }
            assert_eq!(body["model"], "model-1");
            if messages.is_some() {
                assert!(body.get("thinking").is_none());
                if compaction {
                    let mut expected = crate::provider::summary_schema();
                    if let Some(nonce) = nonce {
                        expected["properties"]["summary"]["enum"] = json!([nonce]);
                    }
                    assert_eq!(schema, &expected);
                } else {
                    assert_eq!(body.get("output_config").is_some(), index >= 3);
                }
            } else {
                assert_eq!(body["store"], false);
                assert_eq!(body["truncation"], "disabled");
                assert_eq!(body.get("text").is_some(), compaction);
            }
            if let Some(delegate) =
                body["text"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
                    .as_array()
                    .and_then(|branches| {
                        branches
                            .iter()
                            .find(|branch| branch["properties"]["type"]["enum"][0] == "delegate")
                    })
            {
                let assignment = &delegate["properties"]["children"]["items"];
                assert_eq!(assignment["type"], "object");
                assert_eq!(assignment["required"], json!(["objective", "profile"]));
                assert_eq!(assignment["additionalProperties"], false);
                assert_eq!(assignment["properties"]["profile"]["enum"], json!([null]));
            }
            assert_eq!(
                body.get(if messages.is_some() {
                    "output_config"
                } else {
                    "reasoning"
                })
                .and_then(|value| value["effort"].as_str()),
                match index {
                    3 | 4 => Some("low"),
                    5 | 6 => Some("high"),
                    _ => None,
                }
            );
            assert_eq!(
                body[if messages.is_some() {
                    "max_tokens"
                } else {
                    "max_output_tokens"
                }],
                if index == 0 && !compaction { 4096 } else { 128 }
            );
            assert!(
                !request[header_end..]
                    .windows(b"OMITTED_WORKSPACE_CANARY".len())
                    .any(|window| window == b"OMITTED_WORKSPACE_CANARY")
            );

            if compaction
                && (matches!(reply, ProbeReply::CompactionRejected)
                    || (matches!(reply, ProbeReply::EffortCompactionRejected) && index == 6))
            {
                stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                continue;
            }

            if matches!(
                reply,
                ProbeReply::EncodedBody | ProbeReply::EncodedBodyAfterIdentity
            ) {
                let encoding = if matches!(reply, ProbeReply::EncodedBodyAfterIdentity) {
                    "identity\r\nContent-Encoding: gzip"
                } else {
                    "gzip"
                };
                stream
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Encoding: {encoding}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").as_bytes())
                    .await
                    .expect("encoded response headers");
                let mut next = [0];
                let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut next))
                    .await
                    .expect("encoded response was rejected without reading its body")
                    .expect("encoded response peer state");
                assert_eq!(read, 0);
                continue;
            }

            let (status, response_body) = match reply {
                ProbeReply::Redirect => ("302 Found", String::new()),
                ProbeReply::Oversize => ("200 OK", String::new()),
                ProbeReply::EncodedBody | ProbeReply::EncodedBodyAfterIdentity => unreachable!(),
                ProbeReply::Valid
                | ProbeReply::CompactionRejected
                | ProbeReply::IgnoredFormat
                | ProbeReply::EffortCompactionRejected
                | ProbeReply::ZeroUsage
                | ProbeReply::AggregateJsonNodes
                | ProbeReply::ValidEfforts
                | ProbeReply::WrongModel
                | ProbeReply::ChunkedAtLimit
                | ProbeReply::ChunkedOversize
                | ProbeReply::EscapedCredential
                | ProbeReply::MissingUsage
                | ProbeReply::InputAtLimit
                | ProbeReply::InputAboveLimit => {
                    let text = match index {
                        _ if compaction => json!(r#"{"summary":"synthetic summary"}"#),
                        0 if matches!(reply, ProbeReply::EscapedCredential) => {
                            json!("test-key")
                        }
                        1 if streaming => {
                            json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                        }
                        _ if streaming && index != 0 => {
                            json!("ok")
                        }
                        0 | 3 | 4 => {
                            json!("ok")
                        }
                        1 => {
                            json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                        }
                        _ => json!(r#"{"summary":"synthetic summary"}"#),
                    };
                    let text = if matches!(reply, ProbeReply::IgnoredFormat) {
                        text
                    } else {
                        crate::provider::render::semantic::compaction_probe_reply(&body, text)
                    };
                    let model = if matches!(reply, ProbeReply::WrongModel) {
                        "drifted-model"
                    } else {
                        "model-1"
                    };
                    let mut response = json!({
                        "id": format!("resp_{}", index + 1),
                        "status": "completed",
                        "model": model,
                        "output": crate::provider::render::semantic::responses_output(&text),
                        "usage": {"input_tokens":20,"output_tokens":10}
                    });
                    if messages.is_some() {
                        response = json!({
                            "id": format!("msg_{}", index + 1), "type":"message", "role":"assistant",
                            "model":model, "stop_reason":if text.is_object() { "tool_use" } else { "end_turn" },
                            "content":crate::provider::render::semantic::messages_content(&text),
                            "usage":{"input_tokens":20,"output_tokens":10}
                        });
                    }
                    if matches!(reply, ProbeReply::ZeroUsage) {
                        response["usage"] = json!({"input_tokens":0,"output_tokens":0});
                    }
                    if matches!(reply, ProbeReply::MissingUsage) {
                        response.as_object_mut().unwrap().remove("usage");
                    }
                    if matches!(
                        reply,
                        ProbeReply::InputAtLimit | ProbeReply::InputAboveLimit
                    ) {
                        response["usage"]["input_tokens"] = json!(
                            crate::provider::MAX_REPORTED_INPUT_TOKENS
                                + u32::from(matches!(reply, ProbeReply::InputAboveLimit))
                        );
                    }
                    if !streaming
                        && matches!(
                            reply,
                            ProbeReply::ChunkedAtLimit | ProbeReply::ChunkedOversize
                        )
                    {
                        response["padding"] = json!("");
                        let target = CONTRACT_RESPONSE_BYTES
                            + usize::from(matches!(reply, ProbeReply::ChunkedOversize));
                        let padding = target - response.to_string().len();
                        response["padding"] = json!("x".repeat(padding));
                        assert_eq!(response.to_string().len(), target);
                    }
                    ("200 OK", response.to_string())
                }
            };
            if matches!(
                reply,
                ProbeReply::ChunkedAtLimit | ProbeReply::ChunkedOversize
            ) {
                let response_body = if streaming {
                    assert!(messages.is_some());
                    let response: serde_json::Value = serde_json::from_str(&response_body).unwrap();
                    let events = crate::provider::render::semantic::messages_stream(&response);
                    let target = CONTRACT_RESPONSE_BYTES
                        + usize::from(matches!(reply, ProbeReply::ChunkedOversize));
                    let padding = target - events.len() - 3;
                    let body = format!(":{}\n\n{events}", "x".repeat(padding));
                    assert_eq!(body.len(), target);
                    body
                } else {
                    response_body
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{response_body}\r\n0\r\n\r\n",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("chunked response");
                continue;
            }
            if matches!(reply, ProbeReply::AggregateJsonNodes) {
                assert!(streaming && messages.is_some());
                let mut start: serde_json::Value = serde_json::from_str(&response_body).unwrap();
                start["content"] = json!([]);
                start["stop_reason"] = serde_json::Value::Null;
                start["usage"]["output_tokens"] = json!(0);
                start["usage"]["first_extra"] = json!(vec![0; 132_000]);
                let events = [
                    json!({"type":"message_start","message":start}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10,"second_extra":vec![0;132_000]}}),
                    json!({"type":"message_stop"}),
                ];
                let body: String = events
                    .iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().unwrap()
                        )
                    })
                    .collect();
                assert!(body.len() < CONTRACT_RESPONSE_BYTES);
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                continue;
            }
            if streaming && messages.is_some() && !response_body.is_empty() {
                let response: serde_json::Value = serde_json::from_str(&response_body).unwrap();
                let body = crate::provider::render::semantic::messages_stream(&response);
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                continue;
            }
            if streaming && !response_body.is_empty() {
                let mut response: serde_json::Value = serde_json::from_str(&response_body).unwrap();
                response["output"][0]["id"] = json!("msg_stream");
                let item = response["output"][0].clone();
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                let delta = if item["type"] == "function_call" {
                    added["arguments"] = json!("");
                    json!({"type":"response.function_call_arguments.delta", "item_id":"msg_stream", "output_index":0, "delta":item["arguments"]})
                } else {
                    added["content"] = json!([]);
                    json!({"type":"response.output_text.delta", "item_id":"msg_stream", "output_index":0, "content_index":0, "delta":item["content"][0]["text"]})
                };
                let events = [
                    json!({"type":"response.created","response":{"id":response["id"],"model":"model-1"}}),
                    json!({"type":"response.output_item.added","output_index":0,"item":added}),
                    delta,
                    json!({"type":"response.output_item.done","output_index":0,"item":response["output"][0]}),
                    json!({"type":"response.completed","response":response}),
                ];
                let body = events
                    .iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().unwrap()
                        )
                    })
                    .collect::<String>();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                continue;
            }
            let declared_len = if matches!(reply, ProbeReply::Oversize) {
                CONTRACT_RESPONSE_BYTES + 1
            } else {
                response_body.len()
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {declared_len}\r\nConnection: close\r\n\r\n{response_body}"
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("probe response");
        }
    }

    async fn run_case(reply: ProbeReply) -> Result<ProviderEvidenceRecord, CustomProfileError> {
        run_case_protocol(reply, None).await
    }

    async fn run_case_protocol(
        reply: ProbeReply,
        messages: Option<super::super::MessagesAuth>,
    ) -> Result<ProviderEvidenceRecord, CustomProfileError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let mut profile = CustomProfile {
            name: "local".into(),
            protocol: super::super::Protocol::OpenaiResponses,
            endpoint: format!("http://127.0.0.1:{port}/v1/responses"),
            model: "model-1".into(),
            credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
            capability_evidence_version: 2,
            efforts: if matches!(
                reply,
                ProbeReply::ValidEfforts | ProbeReply::EffortCompactionRejected
            ) {
                vec![crate::provider::Effort::Low, crate::provider::Effort::High]
            } else {
                Vec::new()
            },
        };
        if let Some(auth) = messages {
            let base_url = format!("http://127.0.0.1:{port}/anthropic");
            profile.endpoint = format!("{base_url}/v1/messages");
            profile.protocol = super::super::Protocol::AnthropicMessages { base_url, auth };
        }
        assert_eq!(Url::parse(profile.endpoint()).unwrap().port(), Some(port));
        let destination = profile
            .resolve_destination()
            .await
            .expect("pinned loopback");
        let server = tokio::spawn(serve_probe(listener, reply, messages));
        let result = probe(&profile, &destination, "test-key".into()).await;
        server.await.expect("server completed");
        result
    }

    async fn run_transport_case(
        reply: ProbeReply,
        compaction: bool,
        messages: bool,
        streaming: bool,
    ) -> Result<(), super::super::transport::TransportError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let mut profile = CustomProfile {
            name: "local".into(),
            protocol: super::super::Protocol::OpenaiResponses,
            endpoint: format!("http://127.0.0.1:{port}/v1/responses"),
            model: "model-1".into(),
            credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
            capability_evidence_version: 2,
            efforts: Vec::new(),
        };
        if messages {
            let base_url = format!("http://127.0.0.1:{port}/anthropic");
            profile.endpoint = format!("{base_url}/v1/messages");
            profile.protocol = super::super::Protocol::AnthropicMessages {
                base_url,
                auth: super::super::MessagesAuth::ApiKey,
            };
        }
        let destination = profile
            .resolve_destination()
            .await
            .expect("pinned loopback");
        let server = tokio::spawn(serve_probe(
            listener,
            reply,
            messages.then_some(super::super::MessagesAuth::ApiKey),
        ));
        let transport =
            CustomTransport::new(&profile, &destination, "test-key".into()).expect("transport");
        let request = ProviderRequest {
            session_id: crate::session::SessionId::new(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            phase: AgentPhase::RootPlan,
            collaboration: crate::session::CollaborationPolicy::Single,
            model: "model-1".into(),
            max_output_tokens: 4096,
            input: RunInput {
                ..RunInput::new(
                    PromptRole::ReadOnly,
                    "Synthetic conformance check: finish directly".into(),
                )
            },
        };
        let result = if compaction {
            let request = CompactionRequest {
                session_id: SessionId::new(),
                covered_run_id: RunId::new(),
                model: request.model,
                previous_summary: None,
                items: vec![CompactionItem::Completed(HistoryTurn {
                    user: "Synthetic question".into(),
                    assistant: "Synthetic answer".into(),
                })],
                max_output_tokens: 128,
            };
            tokio::time::timeout(Duration::from_secs(10), transport.compact(&request, None))
                .await
                .expect("compaction transport deadline")
                .map(|_| ())
        } else if streaming {
            tokio::time::timeout(
                Duration::from_secs(10),
                transport.run_streamed(&request, None, None),
            )
            .await
            .expect("Messages stream rejection deadline")
            .map(|_| ())
        } else {
            tokio::time::timeout(Duration::from_secs(10), transport.run(&request, None))
                .await
                .expect("transport rejection deadline")
                .map(|_| ())
        };
        server.await.expect("server completed");
        result
    }

    #[tokio::test]
    async fn transport_accepts_exact_cap_and_rejects_excess_or_encoding() {
        assert!(
            matches!(
                run_transport_case(ProbeReply::AggregateJsonNodes, false, true, true).await,
                Err(super::super::transport::TransportError::InvalidOutcome)
            ),
            "assembled Messages JSON retains the aggregate node bound"
        );
        assert!(
            run_transport_case(ProbeReply::ChunkedAtLimit, false, false, false)
                .await
                .is_ok()
        );
        assert!(
            run_transport_case(ProbeReply::ChunkedAtLimit, false, true, false)
                .await
                .is_ok()
        );
        for streaming in [false, true] {
            for reply in [
                ProbeReply::Oversize,
                ProbeReply::ChunkedOversize,
                ProbeReply::EncodedBody,
                ProbeReply::EncodedBodyAfterIdentity,
                ProbeReply::InputAboveLimit,
            ] {
                assert!(matches!(
                    run_transport_case(reply, false, true, streaming).await,
                    Err(super::super::transport::TransportError::InvalidOutcome)
                ));
            }
        }
        assert!(
            run_transport_case(ProbeReply::ChunkedAtLimit, false, true, true)
                .await
                .is_ok()
        );
        for compaction in [false, true] {
            assert!(
                run_transport_case(ProbeReply::InputAtLimit, compaction, false, false)
                    .await
                    .is_ok(),
                "maximum reported input remains valid; compaction={compaction}"
            );
            assert!(
                matches!(
                    run_transport_case(ProbeReply::InputAboveLimit, compaction, false, false).await,
                    Err(super::super::transport::TransportError::InvalidOutcome)
                ),
                "reported input above shared ceiling must reject; compaction={compaction}"
            );
        }
        for reply in [
            ProbeReply::Oversize,
            ProbeReply::ChunkedOversize,
            ProbeReply::EncodedBody,
            ProbeReply::EncodedBodyAfterIdentity,
        ] {
            assert!(matches!(
                run_transport_case(reply, false, false, false).await,
                Err(super::super::transport::TransportError::InvalidOutcome)
            ));
        }
    }

    #[tokio::test]
    async fn messages_evidence_binds_protocol_base_and_auth() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let root = StateRoot::admit(&path).unwrap();
        let original = json!({"version":2,"profiles":[{
            "name":"local","protocol":"anthropic-messages","base_url":"http://127.0.0.1:9321/anthropic",
            "auth":"x-api-key","model":"model-1","credential_env":"ARANY_PROVIDER_EVIDENCE_TEST_ABSENT",
            "outcome_encoding":"native_tools","privacy":"user_authorized","max_output_tokens":4096,
            "capability_evidence_version":2,"efforts":[]
        }]});
        let write = |document: &serde_json::Value| {
            use std::os::unix::fs::PermissionsExt;
            let file = path.join("provider-profiles.json");
            std::fs::write(&file, serde_json::to_vec(document).unwrap()).unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        write(&original);
        let profile = CustomProfile::load_named(&root, "local").unwrap();
        let destination = profile.resolve_destination().await.unwrap();
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let evidence = ProviderEvidenceRecord {
            name: "local".into(),
            digest: fingerprint(&profile),
            addresses: destination.addresses().to_vec(),
            checked_at_ms: now,
            expires_at_ms: now + EVIDENCE_AGE_MS,
        };
        let store = Store::open(root.try_clone().unwrap()).unwrap();
        store
            .record_provider_evidence(evidence.clone())
            .await
            .unwrap();
        store.close().await.unwrap();
        assert!(matches!(
            super::super::CustomProvider::admit(&root, "local", "model-1", None).await,
            Err(CustomProfileError::CredentialUnavailable)
        ));
        for (field, value) in [
            ("base_url", json!("http://127.0.0.1:9321/other")),
            ("base_url", json!("http://127.0.0.1:9322/anthropic")),
            ("auth", json!("bearer")),
            ("efforts", json!(["low"])),
            ("credential_env", json!("ARANY_PROVIDER_OTHER_ABSENT")),
        ] {
            let mut changed = original.clone();
            changed["profiles"][0][field] = value;
            write(&changed);
            assert!(
                matches!(
                    super::super::CustomProvider::admit(&root, "local", "model-1", None).await,
                    Err(CustomProfileError::EvidenceUnavailable)
                ),
                "{field} must refuse before key lookup"
            );
        }
        let mut responses = original.clone();
        let entry = responses["profiles"][0].as_object_mut().unwrap();
        entry.remove("base_url");
        entry.remove("auth");
        entry.insert("protocol".into(), json!("openai-responses"));
        entry.insert(
            "endpoint".into(),
            json!("http://127.0.0.1:9321/v1/responses"),
        );
        entry.insert("outcome_encoding".into(), json!("json_schema"));
        write(&responses);
        assert!(matches!(
            super::super::CustomProvider::admit(&root, "local", "model-1", None).await,
            Err(CustomProfileError::EvidenceUnavailable)
        ));
        let responses_profile = CustomProfile::load_named(&root, "local").unwrap();
        for fields in [
            vec![
                env!("CARGO_PKG_VERSION"),
                "openai-responses-goal-contract-conformance-effort-stream-v30",
                responses_profile.name(),
                responses_profile.endpoint(),
                responses_profile.model(),
                responses_profile.credential_env(),
                "openai-responses",
                "json_schema",
                "user_authorized",
                "4096",
            ],
            vec![
                env!("CARGO_PKG_VERSION"),
                "custom-protocol-goal-contract-conformance-effort-stream-v31",
                responses_profile.name(),
                responses_profile.endpoint(),
                responses_profile.model(),
                responses_profile.credential_env(),
                "2",
                "openai-responses",
                "",
                "json_schema",
                "bearer",
                "user_authorized",
                "4096",
            ],
        ] {
            let mut legacy = Sha256::new();
            for field in fields {
                legacy.update((field.len() as u64).to_be_bytes());
                legacy.update(field.as_bytes());
            }
            legacy.update(2_u32.to_be_bytes());
            let old = ProviderEvidenceRecord {
                digest: legacy.finalize().into(),
                ..evidence.clone()
            };
            let store = Store::open(root.try_clone().unwrap()).unwrap();
            store.record_provider_evidence(old).await.unwrap();
            store.close().await.unwrap();
            responses["version"] = json!(1);
            write(&responses);
            assert!(matches!(
                super::super::CustomProvider::admit(&root, "local", "model-1", None).await,
                Err(CustomProfileError::EvidenceUnavailable)
            ));
        }
        write(&original);
        assert!(matches!(
            super::super::CustomProvider::admit(&root, "local", "model-1", None).await,
            Err(CustomProfileError::EvidenceUnavailable)
        ));
        let store = Store::open(root.try_clone().unwrap()).unwrap();
        let mut drifted = evidence;
        drifted.addresses[0].set_port(9322);
        store.record_provider_evidence(drifted).await.unwrap();
        store.close().await.unwrap();
        assert!(matches!(
            super::super::CustomProvider::admit(&root, "local", "model-1", None).await,
            Err(CustomProfileError::EvidenceUnavailable)
        ));
    }

    #[tokio::test]
    async fn custom_compaction_requires_schema_only_challenges_at_every_effort() {
        for messages in [
            None,
            Some(super::super::MessagesAuth::Bearer),
            Some(super::super::MessagesAuth::ApiKey),
        ] {
            for reply in [
                ProbeReply::IgnoredFormat,
                ProbeReply::EffortCompactionRejected,
            ] {
                assert!(matches!(
                    run_case_protocol(reply, messages).await,
                    Err(CustomProfileError::ConformanceFailed)
                ));
            }
        }
    }

    #[tokio::test]
    async fn messages_compaction_rejection_clears_admission() {
        for auth in [
            super::super::MessagesAuth::Bearer,
            super::super::MessagesAuth::ApiKey,
        ] {
            assert!(matches!(
                run_case_protocol(ProbeReply::CompactionRejected, Some(auth)).await,
                Err(CustomProfileError::ConformanceFailed)
            ));
        }
    }

    #[tokio::test]
    async fn synthetic_conformance_checks_exact_wire_and_reopens_evidence() {
        for auth in [
            super::super::MessagesAuth::Bearer,
            super::super::MessagesAuth::ApiKey,
        ] {
            for reply in [ProbeReply::Valid, ProbeReply::ValidEfforts] {
                let record = run_case_protocol(reply, Some(auth))
                    .await
                    .expect("Messages conformance");
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("state");
                let store = Store::open(StateRoot::admit(&path).unwrap()).unwrap();
                store
                    .record_provider_evidence(record.clone())
                    .await
                    .unwrap();
                store.close().await.unwrap();
                let store =
                    Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
                assert_eq!(
                    store
                        .load_provider_evidence(record.name.clone())
                        .await
                        .unwrap(),
                    Some(record)
                );
                store.close().await.unwrap();
            }
            for reply in [
                ProbeReply::WrongModel,
                ProbeReply::Redirect,
                ProbeReply::EscapedCredential,
                ProbeReply::MissingUsage,
                ProbeReply::ZeroUsage,
            ] {
                assert!(matches!(
                    run_case_protocol(reply, Some(auth)).await,
                    Err(CustomProfileError::ConformanceFailed)
                ));
            }
        }
        let effort_record = run_case(ProbeReply::ValidEfforts)
            .await
            .expect("strict effort conformance");
        let record = run_case(ProbeReply::Valid)
            .await
            .expect("strict conformance");
        assert_ne!(effort_record.digest, record.digest);
        assert_eq!(record.name, "local");
        assert_eq!(record.addresses.len(), 1);
        assert_eq!(record.expires_at_ms - record.checked_at_ms, EVIDENCE_AGE_MS);

        let temp = tempfile::tempdir().expect("private test root");
        let path = temp.path().join("state");
        let store = Store::open(StateRoot::admit(&path).expect("state")).expect("store");
        store
            .record_provider_evidence(record.clone())
            .await
            .expect("record evidence");
        store.close().await.expect("close store");
        let store = Store::open_read_only(StateRoot::open_existing(&path).expect("state reopen"))
            .expect("read-only store");
        assert_eq!(
            store
                .load_provider_evidence("local".into())
                .await
                .expect("load evidence"),
            Some(record.clone())
        );
        store.close().await.expect("close read-only store");

        let expired = ProviderEvidenceRecord {
            checked_at_ms: 0,
            expires_at_ms: 1,
            ..record
        };
        let store = Store::open(StateRoot::open_existing(&path).expect("state reopen"))
            .expect("writable store");
        store
            .record_provider_evidence(expired)
            .await
            .expect("record expired evidence");
        store.close().await.expect("close writable store");
        let store = Store::open_read_only(StateRoot::open_existing(&path).expect("state reopen"))
            .expect("read-only store");
        assert_eq!(
            store
                .load_provider_evidence("local".into())
                .await
                .expect("expired evidence lookup"),
            None
        );
        store.close().await.expect("close read-only store");

        for reply in [
            ProbeReply::WrongModel,
            ProbeReply::Redirect,
            ProbeReply::EscapedCredential,
            ProbeReply::MissingUsage,
        ] {
            assert!(matches!(
                run_case(reply).await,
                Err(CustomProfileError::ConformanceFailed)
            ));
        }
    }
}
