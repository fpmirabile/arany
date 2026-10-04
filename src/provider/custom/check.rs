use super::{CustomProfile, CustomProfileError, PinnedDestination, transport::CustomTransport};
use crate::provider::{
    AgentPhase, CompactionItem, CompactionRequest, HistoryTurn, ProviderOutcome, ProviderRequest,
};
use crate::session::{AgentRunId, RunId, SessionId};
use crate::store::{ProviderEvidenceRecord, StateRoot, Store};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TOTAL_DEADLINE: Duration = Duration::from_secs(180);
const EVIDENCE_AGE_MS: i64 = 24 * 60 * 60 * 1000;
const CHECK_VERSION_V1: &str = "openai-responses-conformance-v5";
const CHECK_VERSION_V2: &str = "openai-responses-conformance-effort-v5";

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
    let transport = CustomTransport::new(destination, key)?;
    let direct = ProviderRequest {
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Single,
        model: profile.model().to_owned(),
        instructions: None,
        images: Vec::new(),
        objective: "Synthetic conformance check: finish directly with a short factual sentence. No Workspace content is supplied.".into(),
        includes: Vec::new(),
        history: Vec::new(),
        context_summary: None,
        child_results: Vec::new(),
        max_output_tokens: profile.max_output_tokens(),
        tools: None,
    };
    let direct_response = transport
        .run(&direct, None)
        .await
        .map_err(|_| CustomProfileError::ConformanceFailed)?;
    if !matches!(direct_response.outcome, ProviderOutcome::Finish(_)) {
        return Err(CustomProfileError::ConformanceFailed);
    }
    let direct_id = direct_response
        .response_id
        .ok_or(CustomProfileError::ConformanceFailed)?;

    let delegate = ProviderRequest {
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Team { max_active_children: 1 },
        model: profile.model().to_owned(),
        instructions: None,
        images: Vec::new(),
        objective: "Synthetic conformance check: delegate exactly one independent read-only question about the number two. No Workspace content is supplied.".into(),
        includes: Vec::new(),
        history: Vec::new(),
        context_summary: None,
        child_results: Vec::new(),
        max_output_tokens: 128,
        tools: None,
    };
    let delegate_response = transport
        .run(&delegate, None)
        .await
        .map_err(|_| CustomProfileError::ConformanceFailed)?;
    if !matches!(&delegate_response.outcome, ProviderOutcome::Delegate(value) if value.children.len() == 1 && !value.children[0].trim().is_empty())
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
        session_id: SessionId::new(),
        covered_run_id: RunId::new(),
        model: profile.model().to_owned(),
        previous_summary: None,
        items: vec![CompactionItem::Completed(HistoryTurn {
            user: "Synthetic conformance question".into(),
            assistant: "Synthetic conformance answer".into(),
        })],
        max_output_tokens: 128,
    };
    let compact_response = transport
        .compact(&compact, None)
        .await
        .map_err(|_| CustomProfileError::ConformanceFailed)?;
    if compact_response.summary.trim().is_empty() || compact_response.summary.len() > 8 * 1024 {
        return Err(CustomProfileError::ConformanceFailed);
    }
    let compact_id = compact_response
        .response_id
        .ok_or(CustomProfileError::ConformanceFailed)?;
    if compact_id == direct_id || compact_id == delegate_id {
        return Err(CustomProfileError::ConformanceFailed);
    }

    let mut seen_ids = vec![direct_id, delegate_id, compact_id];
    for effort in profile.efforts() {
        let mut selected = direct.clone();
        selected.run_id = RunId::new();
        selected.agent_run_id = AgentRunId::new();
        selected.max_output_tokens = 128;
        let response = transport
            .run(&selected, Some(*effort))
            .await
            .map_err(|_| CustomProfileError::ConformanceFailed)?;
        if !matches!(response.outcome, ProviderOutcome::Finish(_)) {
            return Err(CustomProfileError::ConformanceFailed);
        }
        let id = response
            .response_id
            .ok_or(CustomProfileError::ConformanceFailed)?;
        if seen_ids.contains(&id) {
            return Err(CustomProfileError::ConformanceFailed);
        }
        seen_ids.push(id);
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

pub(super) fn fingerprint(profile: &CustomProfile) -> [u8; 32] {
    let mut digest = Sha256::new();
    for field in [
        env!("CARGO_PKG_VERSION"),
        if profile.capability_evidence_version() == 1 {
            CHECK_VERSION_V1
        } else {
            CHECK_VERSION_V2
        },
        profile.name(),
        profile.endpoint(),
        profile.model(),
        profile.credential_env(),
        "openai-responses",
        "json_schema",
        "user_authorized",
        "4096",
    ] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    if profile.capability_evidence_version() == 2 {
        digest.update(2_u32.to_be_bytes());
        for effort in profile.efforts() {
            let value = effort.as_str();
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
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

    async fn serve_probe(listener: TcpListener, reply: ProbeReply) {
        let count = if matches!(reply, ProbeReply::ValidEfforts) {
            5
        } else if matches!(reply, ProbeReply::Valid) {
            3
        } else {
            1
        };
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
            assert!(header.starts_with("POST /v1/responses HTTP/1.1\r\n"));
            assert!(
                header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-key")
            );
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end..header_end + body_len])
                    .expect("JSON request");
            let compaction = body["text"]["format"]["name"] == "arany_compaction";
            assert_eq!(body["model"], "model-1");
            assert_eq!(body["store"], false);
            assert_eq!(body["truncation"], "disabled");
            assert_eq!(body["text"]["format"]["strict"], true);
            assert_eq!(
                body.get("reasoning")
                    .map(|value| value["effort"].as_str().unwrap()),
                match index {
                    3 => Some("low"),
                    4 => Some("high"),
                    _ => None,
                }
            );
            assert_eq!(
                body["max_output_tokens"],
                if index == 0 && !compaction { 4096 } else { 128 }
            );
            assert!(
                !request[header_end..]
                    .windows(b"OMITTED_WORKSPACE_CANARY".len())
                    .any(|window| window == b"OMITTED_WORKSPACE_CANARY")
            );

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
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: {encoding}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").as_bytes())
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
                | ProbeReply::ValidEfforts
                | ProbeReply::WrongModel
                | ProbeReply::ChunkedAtLimit
                | ProbeReply::ChunkedOversize
                | ProbeReply::EscapedCredential
                | ProbeReply::MissingUsage
                | ProbeReply::InputAtLimit
                | ProbeReply::InputAboveLimit => {
                    let text = match index {
                        0 if compaction => json!({"summary":"synthetic summary"}),
                        0 if matches!(reply, ProbeReply::EscapedCredential) => {
                            json!({"outcome":{"type":"finish","summary":"ok","result":"test-key"}})
                        }
                        0 | 3 | 4 => {
                            json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}})
                        }
                        1 => {
                            json!({"outcome":{"type":"delegate","children":["one read-only question"]}})
                        }
                        _ => json!({"summary":"synthetic summary"}),
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
                        "output": [{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text.to_string().replace("test-key", "\\u0074est-key") }]}],
                        "usage": {"input_tokens":20,"output_tokens":10}
                    });
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
                    if matches!(
                        reply,
                        ProbeReply::ChunkedAtLimit | ProbeReply::ChunkedOversize
                    ) {
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
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{response_body}\r\n0\r\n\r\n",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("chunked response");
                continue;
            }
            let declared_len = if matches!(reply, ProbeReply::Oversize) {
                CONTRACT_RESPONSE_BYTES + 1
            } else {
                response_body.len()
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {declared_len}\r\nConnection: close\r\n\r\n{response_body}"
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("probe response");
        }
    }

    async fn run_case(reply: ProbeReply) -> Result<ProviderEvidenceRecord, CustomProfileError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let profile = CustomProfile {
            name: "local".into(),
            endpoint: format!("http://127.0.0.1:{port}/v1/responses"),
            model: "model-1".into(),
            credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
            capability_evidence_version: if matches!(reply, ProbeReply::ValidEfforts) {
                2
            } else {
                1
            },
            efforts: if matches!(reply, ProbeReply::ValidEfforts) {
                vec![crate::provider::Effort::Low, crate::provider::Effort::High]
            } else {
                Vec::new()
            },
        };
        assert_eq!(Url::parse(profile.endpoint()).unwrap().port(), Some(port));
        let destination = profile
            .resolve_destination()
            .await
            .expect("pinned loopback");
        let server = tokio::spawn(serve_probe(listener, reply));
        let result = probe(&profile, &destination, "test-key".into()).await;
        server.await.expect("server completed");
        result
    }

    async fn run_transport_case(
        reply: ProbeReply,
        compaction: bool,
    ) -> Result<(), super::super::transport::TransportError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let profile = CustomProfile {
            name: "local".into(),
            endpoint: format!("http://127.0.0.1:{port}/v1/responses"),
            model: "model-1".into(),
            credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
            capability_evidence_version: 1,
            efforts: Vec::new(),
        };
        let destination = profile
            .resolve_destination()
            .await
            .expect("pinned loopback");
        let server = tokio::spawn(serve_probe(listener, reply));
        let transport = CustomTransport::new(&destination, "test-key".into()).expect("transport");
        let request = ProviderRequest {
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            phase: AgentPhase::RootPlan,
            collaboration: crate::session::CollaborationPolicy::Single,
            model: "model-1".into(),
            instructions: None,
            images: Vec::new(),
            objective: "Synthetic conformance check: finish directly".into(),
            includes: Vec::new(),
            history: Vec::new(),
            context_summary: None,
            child_results: Vec::new(),
            max_output_tokens: 4096,
            tools: None,
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
            run_transport_case(ProbeReply::ChunkedAtLimit, false)
                .await
                .is_ok()
        );
        for compaction in [false, true] {
            assert!(
                run_transport_case(ProbeReply::InputAtLimit, compaction)
                    .await
                    .is_ok(),
                "maximum reported input remains valid; compaction={compaction}"
            );
            assert!(
                matches!(
                    run_transport_case(ProbeReply::InputAboveLimit, compaction).await,
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
                run_transport_case(reply, false).await,
                Err(super::super::transport::TransportError::InvalidOutcome)
            ));
        }
    }

    #[tokio::test]
    async fn synthetic_conformance_checks_exact_wire_and_reopens_evidence() {
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
