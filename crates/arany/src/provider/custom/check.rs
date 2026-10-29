use super::{CustomProfile, CustomProfileError, PinnedDestination, transport::CustomTransport};
use crate::provider::evidence::ProviderEvidenceRecord;
use crate::provider::{
    AgentPhase, CompactionItem, CompactionRequest, HistoryTurn, PROBE_TOTAL_DEADLINE, PromptRole,
    ProviderOutcome, ProviderRequest, RunInput,
};
use crate::session::{AgentRunId, RunId, SessionId};
use crate::store::StateRoot;
#[cfg(test)]
use crate::store::Store;
use crate::{
    CheckCallKind, CheckDisposition, CheckJournal, CheckProbe, CheckReceipt, CheckStart,
    CheckTarget, CheckUsage,
};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const EVIDENCE_AGE_MS: i64 = 24 * 60 * 60 * 1000;
const CUSTOM_CHECK_VERSION: &str =
    "custom-protocol-workspace-tools-conformance-effort-stream-journal-v46";

pub struct CustomProfileCheck {
    profile_name: String,
    expires_at_ms: i64,
    receipt: CheckReceipt,
    view: crate::CheckView,
}

impl CustomProfileCheck {
    pub fn receipt(&self) -> CheckReceipt {
        self.receipt
    }
    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    pub fn view(&self) -> &crate::CheckView {
        &self.view
    }
}

pub async fn check_custom_profile(
    root: StateRoot,
    name: &str,
) -> Result<CustomProfileCheck, CustomProfileError> {
    check_custom_profile_with_cancellation(root, name, crate::RunCancellation::new()).await
}

pub async fn check_custom_profile_with_cancellation(
    root: StateRoot,
    name: &str,
    cancellation: crate::RunCancellation,
) -> Result<CustomProfileCheck, CustomProfileError> {
    let profile = CustomProfile::load_named(&root, name)?;
    check_custom_profile_value_with_cancellation(root, profile, cancellation).await
}

pub async fn check_custom_profile_value_with_cancellation(
    root: StateRoot,
    profile: CustomProfile,
    mut cancellation: crate::RunCancellation,
) -> Result<CustomProfileCheck, CustomProfileError> {
    let check_root = root
        .try_clone()
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    let mut journal = CheckJournal::start(check_root, start(&profile)?)
        .await
        .map_err(|error| CustomProfileError::Journal(Box::new(error)))?;
    let deadline =
        (tokio::time::Instant::now() + PROBE_TOTAL_DEADLINE).min(journal.request_deadline());
    let (result, cancelled) =
        run_check(&root, &profile, &mut journal, &mut cancellation, deadline).await;
    finish_check(journal, result, cancelled).await
}

async fn run_check(
    root: &StateRoot,
    profile: &CustomProfile,
    journal: &mut CheckJournal,
    cancellation: &mut crate::RunCancellation,
    deadline: tokio::time::Instant,
) -> (Result<ProviderEvidenceRecord, CustomProfileError>, bool) {
    let name = profile.name().to_owned();
    if !CustomProfile::load_named(root, &name).is_ok_and(|current| current == *profile) {
        return (Err(CustomProfileError::Changed), false);
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => (Err(CustomProfileError::ConformanceFailed), true),
        result = tokio::time::timeout_at(deadline, check_destination_and_publish(root, profile, &name, journal)) => {
            (result.unwrap_or(Err(CustomProfileError::ConformanceFailed)), false)
        }
    }
}

async fn check_destination_and_publish(
    root: &StateRoot,
    profile: &CustomProfile,
    name: &str,
    journal: &mut CheckJournal,
) -> Result<ProviderEvidenceRecord, CustomProfileError> {
    let destination = profile.resolve_destination().await?;
    let key = std::env::var_os(profile.credential_env())
        .ok_or(CustomProfileError::CredentialNotSet)?
        .into_string()
        .map_err(|_| CustomProfileError::CredentialUnavailable)?;
    if key.is_empty() {
        return Err(CustomProfileError::CredentialUnavailable);
    }
    let record = probe(profile, &destination, key, journal).await?;
    let current = CustomProfile::load_named(root, name)?;
    if fingerprint(&current) != record.digest
        || current.resolve_destination().await?.addresses() != record.addresses
    {
        return Err(CustomProfileError::EvidenceUnavailable);
    }
    let digest = crate::store::check::provider_digest(&record)
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    journal
        .evidence_started(digest, record.digest)
        .await
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    journal
        .publish_provider(record.clone())
        .await
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    Ok(record)
}

async fn finish_check(
    mut journal: CheckJournal,
    result: Result<ProviderEvidenceRecord, CustomProfileError>,
    cancelled: bool,
) -> Result<CustomProfileCheck, CustomProfileError> {
    if result.is_err() {
        let _ = journal
            .finish(if cancelled {
                CheckDisposition::Cancelled
            } else {
                CheckDisposition::Failed
            })
            .await;
    }
    let receipt = journal.receipt();
    let view = journal.view().clone();
    let closed = journal.close().await;
    match (result, closed) {
        (Ok(record), Ok(_)) => Ok(CustomProfileCheck {
            profile_name: record.name,
            expires_at_ms: record.expires_at_ms,
            receipt,
            view,
        }),
        (Err(source), _) => Err(CustomProfileError::Check {
            source: Box::new(source),
            receipt,
        }),
        (Ok(_), Err(_)) => Err(CustomProfileError::Check {
            source: Box::new(CustomProfileError::EvidenceUnavailable),
            receipt,
        }),
    }
}

fn start(profile: &CustomProfile) -> Result<CheckStart, CustomProfileError> {
    let mut probes = vec![
        CheckProbe {
            kind: CheckCallKind::Answer,
            effort: None,
            output_tokens: profile.max_output_tokens(),
        },
        CheckProbe {
            kind: CheckCallKind::Delegate,
            effort: None,
            output_tokens: 128,
        },
        CheckProbe {
            kind: CheckCallKind::Compaction,
            effort: None,
            output_tokens: 128,
        },
    ];
    for effort in profile.efforts() {
        for kind in [CheckCallKind::Answer, CheckCallKind::Compaction] {
            probes.push(CheckProbe {
                kind,
                effort: Some(*effort),
                output_tokens: 128,
            });
        }
    }
    CheckStart::new(
        CheckTarget::Custom {
            name: profile.name().to_owned(),
        },
        profile.model().to_owned(),
        fingerprint(profile),
        Sha256::digest(profile.endpoint().as_bytes()).into(),
        0,
        probes,
    )
    .map_err(|_| CustomProfileError::EvidenceUnavailable)
}

async fn rejected(journal: &mut CheckJournal) -> CustomProfileError {
    if journal
        .reject_pending(CheckDisposition::Rejected)
        .await
        .is_err()
    {
        CustomProfileError::EvidenceUnavailable
    } else {
        CustomProfileError::ConformanceFailed
    }
}
async fn transport_failure(
    journal: &mut CheckJournal,
    error: super::transport::TransportError,
) -> CustomProfileError {
    use super::transport::TransportError;
    let disposition = match error {
        TransportError::Transport
        | TransportError::PartialTransport
        | TransportError::StreamEnded
        | TransportError::Unavailable => CheckDisposition::Uncertain,
        TransportError::Rejected
        | TransportError::InvalidOutcome
        | TransportError::InvalidResponseContract
        | TransportError::InvalidOutcomeContract
        | TransportError::HttpServerUnavailable(_)
        | TransportError::RateLimited(_)
        | TransportError::QuotaExceeded => CheckDisposition::Rejected,
    };
    if journal.reject_pending(disposition).await.is_err() {
        CustomProfileError::EvidenceUnavailable
    } else {
        CustomProfileError::ConformanceFailed
    }
}
fn usage(usage: &crate::Usage) -> Option<CheckUsage> {
    Some(CheckUsage {
        input_tokens: usage.input_tokens?,
        output_tokens: usage.output_tokens?,
    })
}

fn answer_probe_matches(
    outcome: &ProviderOutcome,
    protocol: &super::Protocol,
    preview: &str,
) -> bool {
    let ProviderOutcome::Answer(value) = outcome else {
        return false;
    };
    match protocol {
        super::Protocol::OpenaiChatCompletions { .. } => !value.text.trim().is_empty(),
        super::Protocol::OpenaiResponses | super::Protocol::AnthropicMessages { .. } => {
            !preview.is_empty() && value.text.starts_with(preview)
        }
    }
}

async fn probe(
    profile: &CustomProfile,
    destination: &PinnedDestination,
    key: String,
    journal: &mut CheckJournal,
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
    let token = journal
        .call_started(CheckCallKind::Answer)
        .await
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    let chat_completions = matches!(
        &profile.protocol,
        super::Protocol::OpenaiChatCompletions { .. }
    );
    let (preview, observed) = crate::provider::ResponsePreview::new();
    let direct_response = if chat_completions {
        transport.run(&direct, None).await
    } else {
        transport.run_streamed(&direct, None, Some(preview)).await
    };
    let direct_response = match direct_response {
        Ok(value) => value,
        Err(error) => return Err(transport_failure(journal, error).await),
    };
    if !answer_probe_matches(
        &direct_response.outcome,
        &profile.protocol,
        &observed.borrow().answer,
    ) {
        return Err(rejected(journal).await);
    }
    let direct_id = direct_response.response_id.clone();
    let Some(direct_id) = direct_id else {
        return Err(rejected(journal).await);
    };

    journal
        .call_finished(
            token,
            CheckDisposition::Succeeded,
            usage(&direct_response.usage),
            false,
        )
        .await
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;

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
    let token = journal
        .call_started(CheckCallKind::Delegate)
        .await
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    let delegate_response = transport.run_streamed(&delegate, None, Some(preview)).await;
    let delegate_response = match delegate_response {
        Ok(value) => value,
        Err(error) => return Err(transport_failure(journal, error).await),
    };
    if !matches!(&delegate_response.outcome, ProviderOutcome::Delegate(value) if value.children.len() == 1 && !value.children[0].objective.trim().is_empty() && value.children[0].profile.is_none() && observed.borrow().answer.is_empty())
    {
        return Err(rejected(journal).await);
    }
    let delegate_id = delegate_response.response_id.clone();
    let Some(delegate_id) = delegate_id else {
        return Err(rejected(journal).await);
    };
    if delegate_id == direct_id {
        return Err(rejected(journal).await);
    }

    journal
        .call_finished(
            token,
            CheckDisposition::Succeeded,
            usage(&delegate_response.usage),
            false,
        )
        .await
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;

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
        prompts: Default::default(),
    };
    let compact_id = probe_compaction(
        &transport,
        &compact,
        None,
        journal,
        &[direct_id.clone(), delegate_id.clone()],
    )
    .await?;
    if compact_id == direct_id || compact_id == delegate_id {
        return Err(rejected(journal).await);
    }

    let mut seen_ids = vec![direct_id, delegate_id, compact_id];
    for effort in profile.efforts() {
        let mut selected = direct.clone();
        selected.run_id = RunId::new();
        selected.agent_run_id = AgentRunId::new();
        selected.max_output_tokens = 128;
        let (preview, observed) = crate::provider::ResponsePreview::new();
        let token = journal
            .call_started(CheckCallKind::Answer)
            .await
            .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
        let response = if chat_completions {
            transport.run(&selected, Some(*effort)).await
        } else {
            transport
                .run_streamed(&selected, Some(*effort), Some(preview))
                .await
        };
        let response = match response {
            Ok(value) => value,
            Err(error) => return Err(transport_failure(journal, error).await),
        };
        if !answer_probe_matches(
            &response.outcome,
            &profile.protocol,
            &observed.borrow().answer,
        ) {
            return Err(rejected(journal).await);
        }
        let id = response.response_id.clone();
        let Some(id) = id else {
            return Err(rejected(journal).await);
        };
        if seen_ids.contains(&id) {
            return Err(rejected(journal).await);
        }
        journal
            .call_finished(
                token,
                CheckDisposition::Succeeded,
                usage(&response.usage),
                false,
            )
            .await
            .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
        seen_ids.push(id);
        let compact_id =
            probe_compaction(&transport, &compact, Some(*effort), journal, &seen_ids).await?;
        if seen_ids.contains(&compact_id) {
            return Err(rejected(journal).await);
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
    journal: &mut CheckJournal,
    seen: &[String],
) -> Result<String, CustomProfileError> {
    let token = journal
        .call_started(CheckCallKind::Compaction)
        .await
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    let response = match transport.compact(request, effort).await {
        Ok(value) => value,
        Err(error) => return Err(transport_failure(journal, error).await),
    };
    let Some(id) = response.response_id else {
        return Err(rejected(journal).await);
    };
    if seen.contains(&id) {
        return Err(rejected(journal).await);
    }
    journal
        .call_finished(
            token,
            CheckDisposition::Succeeded,
            usage(&response.usage),
            false,
        )
        .await
        .map_err(|_| CustomProfileError::EvidenceUnavailable)?;
    Ok(id)
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
        super::Protocol::OpenaiChatCompletions { base_url } => (
            "openai-chat-completions",
            base_url.as_str(),
            "json_schema",
            "bearer",
        ),
    };
    let mut digest = Sha256::new();
    for field in [
        env!("CARGO_PKG_VERSION"),
        CUSTOM_CHECK_VERSION,
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

    const CONTRACT_RESPONSE_BYTES: usize = crate::provider::MAX_RESPONSE_BYTES;

    #[derive(Clone, Copy)]
    struct MessagesCase {
        base_path: &'static str,
        model: &'static str,
        auth: super::super::MessagesAuth,
    }

    const MESSAGES_CASES: [MessagesCase; 5] = [
        MessagesCase {
            base_path: "/anthropic",
            model: "deepseek-flash",
            auth: super::super::MessagesAuth::ApiKey,
        },
        MessagesCase {
            base_path: "/anthropic",
            model: "kimi-k3",
            auth: super::super::MessagesAuth::Bearer,
        },
        MessagesCase {
            base_path: "/coding/",
            model: "k3-256k",
            auth: super::super::MessagesAuth::ApiKey,
        },
        MessagesCase {
            base_path: "/api/anthropic",
            model: "glm-5.3",
            auth: super::super::MessagesAuth::Bearer,
        },
        MessagesCase {
            base_path: "/api/anthropic",
            model: "glm-5.3-flash",
            auth: super::super::MessagesAuth::Bearer,
        },
    ];
    const GENERIC_MESSAGES: MessagesCase = MessagesCase {
        base_path: "/anthropic",
        model: "model-1",
        auth: super::super::MessagesAuth::ApiKey,
    };

    #[derive(Clone, Copy, Debug)]
    enum ProbeReply {
        Valid,
        ValidEfforts,
        CompactionRejected,
        EmptySummary(usize),
        EffortCompactionRejected,
        ZeroUsage,
        AggregateJsonNodes,
        WrongModel,
        Redirect,
        Unauthorized,
        RateLimited,
        QuotaExceeded,
        Oversize,
        ChunkedAtLimit,
        ChunkedOversize,
        EncodedBody,
        EncodedBodyAfterIdentity,
        #[cfg(debug_assertions)]
        Disconnect,
        #[cfg(debug_assertions)]
        MalformedJson,
        EscapedCredential,
        MissingUsage,
        InputAtLimit,
        InputAboveLimit,
    }

    fn response_byte_limit(request: &serde_json::Value) -> usize {
        if request["stream"] != true {
            return CONTRACT_RESPONSE_BYTES;
        }
        let cap = request
            .get("max_tokens")
            .or_else(|| request.get("max_output_tokens"))
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())
            .expect("stream request output cap");
        crate::provider::max_stream_bytes(cap)
    }

    fn padded_event_stream(events: &str, target: usize) -> String {
        let padding = target
            .checked_sub(events.len() + 1)
            .expect("stream padding");
        let comment = format!(":{}\n", "x".repeat(CONTRACT_RESPONSE_BYTES - 2));
        let mut body = comment.repeat(padding / comment.len());
        body.push_str(&comment[..padding % comment.len()]);
        body.push('\n');
        body.push_str(events);
        assert_eq!(body.len(), target);
        body
    }

    async fn serve_probe(
        listener: TcpListener,
        reply: ProbeReply,
        messages: Option<MessagesCase>,
    ) -> (TcpListener, Option<(usize, usize)>) {
        let mut response_shape = None;
        let count = if let ProbeReply::EmptySummary(index) = reply {
            index + 1
        } else if matches!(
            reply,
            ProbeReply::ValidEfforts | ProbeReply::EffortCompactionRejected
        ) {
            7
        } else if matches!(reply, ProbeReply::Valid | ProbeReply::CompactionRejected) {
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
            let header = header.to_ascii_lowercase();
            let path = messages.map_or_else(
                || "/v1/responses".to_owned(),
                |case| format!("{}/v1/messages", case.base_path.trim_end_matches('/')),
            );
            assert!(header.starts_with(&format!("post {path} http/1.1\r\n")));
            assert!(header.contains(concat!("user-agent: arany/", env!("CARGO_PKG_VERSION"))));
            assert!(!header.contains("anthropic-workspace-id"));
            if messages.is_some_and(|case| case.auth == super::super::MessagesAuth::ApiKey) {
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
            let instructions =
                crate::provider::EffectivePrompts::default().compaction_instructions();
            let compaction = if messages.is_some() {
                body["system"][0]["text"] == instructions
            } else {
                body["instructions"] == instructions
            };
            if compaction {
                for field in ["text", "tools", "tool_choice", "parallel_tool_calls"] {
                    assert!(
                        body.get(field).is_none(),
                        "unexpected compaction field {field}"
                    );
                }
                assert!(body["output_config"].get("format").is_none());
            }
            assert_eq!(body["model"], messages.map_or("model-1", |case| case.model));
            if messages.is_some() {
                assert!(body.get("thinking").is_none());
                assert_eq!(body.get("output_config").is_some(), index >= 3);
            } else {
                assert_eq!(body["store"], false);
                assert_eq!(body["truncation"], "disabled");
                assert!(body.get("text").is_none());
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
                ProbeReply::Unauthorized => ("401 Unauthorized", String::new()),
                ProbeReply::RateLimited => ("429 Too Many Requests", String::new()),
                ProbeReply::QuotaExceeded => (
                    "429 Too Many Requests",
                    json!({"error":{"type":"insufficient_quota"}}).to_string(),
                ),
                ProbeReply::Oversize => ("200 OK", String::new()),
                #[cfg(debug_assertions)]
                ProbeReply::MalformedJson => ("200 OK", "{".to_owned()),
                #[cfg(debug_assertions)]
                ProbeReply::Disconnect => continue,
                ProbeReply::EncodedBody | ProbeReply::EncodedBodyAfterIdentity => unreachable!(),
                ProbeReply::Valid
                | ProbeReply::CompactionRejected
                | ProbeReply::EmptySummary(_)
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
                        _ if compaction => json!(r#"plausible summary"#),
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
                        _ => json!(r#"synthetic summary"#),
                    };
                    let text = if matches!(reply, ProbeReply::EmptySummary(at) if at == index) {
                        json!(" \n\t")
                    } else {
                        text
                    };
                    let model = if matches!(reply, ProbeReply::WrongModel) {
                        "drifted-model"
                    } else {
                        messages.map_or("model-1", |case| case.model)
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
                        let target = response_byte_limit(&body)
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
                    let target = response_byte_limit(&body)
                        + usize::from(matches!(reply, ProbeReply::ChunkedOversize));
                    padded_event_stream(&events, target)
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
                    json!({"type":"message_start","message":start.clone()}),
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
                let mut assembled = start;
                assembled["content"] = json!([{"type":"text","text":"ok"}]);
                assembled["stop_reason"] = json!("end_turn");
                assembled["usage"]["output_tokens"] = json!(10);
                assembled["usage"]["second_extra"] = json!(vec![0; 132_000]);
                response_shape = Some((body.len(), serde_json::to_vec(&assembled).unwrap().len()));
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                continue;
            }
            if status == "200 OK" && streaming && messages.is_some() && !response_body.is_empty() {
                let response: serde_json::Value = serde_json::from_str(&response_body).unwrap();
                let body = crate::provider::render::semantic::messages_stream(&response);
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                continue;
            }
            if status == "200 OK" && streaming && !response_body.is_empty() {
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
                response_byte_limit(&body) + 1
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
        (listener, response_shape)
    }

    async fn run_case(reply: ProbeReply) -> Result<ProviderEvidenceRecord, CustomProfileError> {
        run_case_protocol(reply, None).await
    }

    async fn run_case_protocol(
        reply: ProbeReply,
        messages: Option<MessagesCase>,
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
                ProbeReply::ValidEfforts
                    | ProbeReply::EffortCompactionRejected
                    | ProbeReply::EmptySummary(4 | 6)
            ) {
                vec![crate::provider::Effort::Low, crate::provider::Effort::High]
            } else {
                Vec::new()
            },
        };
        if let Some(case) = messages {
            let base_url = format!(
                "http://127.0.0.1:{port}{}",
                case.base_path.trim_end_matches('/')
            );
            profile.endpoint = format!("{base_url}/v1/messages");
            profile.model = case.model.into();
            profile.protocol = super::super::Protocol::AnthropicMessages {
                base_url,
                auth: case.auth,
            };
        }
        assert_eq!(Url::parse(profile.endpoint()).unwrap().port(), Some(port));
        let destination = profile
            .resolve_destination()
            .await
            .expect("pinned loopback");
        let server = tokio::spawn(serve_probe(listener, reply, messages));
        let state = tempfile::tempdir().expect("check state");
        let mut journal = CheckJournal::start(
            StateRoot::admit(&state.path().join("state")).expect("state"),
            start(&profile).expect("manifest"),
        )
        .await
        .expect("check journal");
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            probe(&profile, &destination, "test-key".into(), &mut journal),
        )
        .await
        .expect("bounded probe");
        let (listener, _) = server.await.expect("server completed");
        assert!(
            listener
                .poll_accept(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending(),
            "checks must not retry or dispatch extra calls"
        );
        result
    }

    async fn run_transport_case(
        reply: ProbeReply,
        compaction: bool,
        messages: bool,
        streaming: bool,
    ) -> Result<(), super::super::transport::TransportError> {
        run_transport_case_observed(reply, compaction, messages, streaming)
            .await
            .0
    }

    async fn run_transport_case_observed(
        reply: ProbeReply,
        compaction: bool,
        messages: bool,
        streaming: bool,
    ) -> (
        Result<(), super::super::transport::TransportError>,
        Option<(usize, usize)>,
    ) {
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
            messages.then_some(GENERIC_MESSAGES),
        ));
        let transport =
            CustomTransport::new(&profile, &destination, "test-key".into()).expect("transport");
        let request = transport_request();
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
                prompts: Default::default(),
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
        let (_, response_body_bytes) = server.await.expect("server completed");
        (result, response_body_bytes)
    }

    fn transport_request() -> ProviderRequest {
        ProviderRequest {
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
        }
    }

    #[tokio::test]
    #[cfg(debug_assertions)]
    #[cfg_attr(arany_guard, ignore = "requires local test IPC")]
    async fn custom_messages_transport_rejections_are_logged_with_observed_shape() {
        if crate::test_process::isolate_test() {
            return;
        }
        let temp = tempfile::tempdir().expect("diagnostic state");
        let root = StateRoot::admit(&temp.path().join("state")).expect("private state root");
        crate::diagnostics::enable_development_diagnostics(&root);
        let path = root.path().join("development.log");

        let transport = run_transport_case(ProbeReply::Disconnect, false, true, false).await;
        assert!(matches!(
            transport,
            Err(super::super::transport::TransportError::Transport)
        ));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("messages stage=RequestTransport bytes=0 events=0 http_status=None")
        );

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("route listener");
        let port = listener.local_addr().expect("route address").port();
        let base_url = format!("http://127.0.0.1:{port}/anthropic");
        let profile = CustomProfile {
            name: "local".into(),
            endpoint: format!("{base_url}/v1/messages"),
            protocol: super::super::Protocol::AnthropicMessages {
                base_url: base_url.clone(),
                auth: super::super::MessagesAuth::ApiKey,
            },
            model: "model-1".into(),
            credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
            capability_evidence_version: 2,
            efforts: Vec::new(),
        };
        let pinned = profile.resolve_destination().await.expect("pinned route");
        let transport =
            CustomTransport::new(&profile, &pinned, "test-key".into()).expect("transport");
        let destination = transport
            .admit_stream_destination(&format!("{base_url}/redirected"), reqwest::StatusCode::OK);
        assert!(matches!(
            destination,
            Err(super::super::transport::TransportError::Rejected)
        ));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("messages stage=Destination bytes=0 events=0 http_status=Some(200)")
        );

        let http = run_transport_case(ProbeReply::Unauthorized, false, true, false).await;
        assert!(matches!(
            http,
            Err(super::super::transport::TransportError::Rejected)
        ));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("messages stage=HttpStatus bytes=0 events=0 http_status=Some(401)")
        );

        let malformed = run_transport_case(ProbeReply::MalformedJson, false, true, false).await;
        assert!(matches!(
            malformed,
            Err(super::super::transport::TransportError::InvalidOutcome)
        ));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("messages stage=ResponseEnvelope bytes=1 events=0 http_status=Some(200)")
        );

        let (aggregate, wire_bytes) =
            run_transport_case_observed(ProbeReply::AggregateJsonNodes, false, true, true).await;
        assert!(matches!(
            aggregate,
            Err(super::super::transport::TransportError::InvalidOutcome)
        ));
        let (wire_bytes, reconstructed_bytes) = wire_bytes.expect("aggregate stream bytes");
        assert_ne!(wire_bytes, reconstructed_bytes);
        assert!(std::fs::read_to_string(path).unwrap().contains(&format!(
            "messages stage=ResponseEnvelope bytes={wire_bytes} events=6 http_status=Some(200)"
        )));
    }

    #[tokio::test]
    // Skipped in the Guard: local test IPC capability is missing.
    #[cfg_attr(arany_guard, ignore = "requires local test IPC")]
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
                assert!(
                    matches!(
                        run_transport_case(reply, false, true, streaming).await,
                        Err(super::super::transport::TransportError::InvalidOutcome)
                    ),
                    "Messages reply={reply:?}, streaming={streaming}"
                );
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

    async fn reject_superseded_protocol_evidence(
        root: &StateRoot,
        profile: &CustomProfile,
        evidence: &ProviderEvidenceRecord,
    ) {
        let (protocol, base, encoding, auth) = match &profile.protocol {
            super::super::Protocol::OpenaiResponses => {
                ("openai-responses", "", "json_schema", "bearer")
            }
            super::super::Protocol::AnthropicMessages { base_url, auth } => (
                "anthropic-messages",
                base_url.as_str(),
                "native_tools",
                match auth {
                    super::super::MessagesAuth::Bearer => "bearer",
                    super::super::MessagesAuth::ApiKey => "x-api-key",
                },
            ),
            super::super::Protocol::OpenaiChatCompletions { base_url } => (
                "openai-chat-completions",
                base_url.as_str(),
                "json_schema",
                "bearer",
            ),
        };
        for version in [
            "custom-protocol-schema-challenge-conformance-effort-stream-v34",
            "custom-protocol-text-compaction-conformance-effort-stream-journal-v39",
            "custom-protocol-text-compaction-conformance-effort-stream-journal-v41",
            "custom-protocol-text-compaction-conformance-effort-stream-journal-v42",
            "custom-protocol-text-compaction-conformance-effort-stream-journal-v43",
            "custom-protocol-text-compaction-conformance-effort-stream-journal-v44",
            "custom-protocol-workspace-tools-conformance-effort-stream-journal-v45",
        ] {
            let mut legacy = Sha256::new();
            for field in [
                env!("CARGO_PKG_VERSION"),
                version,
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
                legacy.update((field.len() as u64).to_be_bytes());
                legacy.update(field.as_bytes());
            }
            legacy.update(2_u32.to_be_bytes());
            let store = Store::open(root.try_clone().unwrap()).unwrap();
            store
                .record_provider_evidence(ProviderEvidenceRecord {
                    digest: legacy.finalize().into(),
                    ..evidence.clone()
                })
                .await
                .unwrap();
            store.close().await.unwrap();
            assert!(
                matches!(
                    super::super::CustomProvider::admit(
                        root,
                        profile.name(),
                        profile.model(),
                        None
                    )
                    .await,
                    Err(CustomProfileError::EvidenceUnavailable)
                ),
                "superseded custom check: {version}"
            );
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
        reject_superseded_protocol_evidence(&root, &profile, &evidence).await;
        let store = Store::open(root.try_clone().unwrap()).unwrap();
        store
            .record_provider_evidence(evidence.clone())
            .await
            .unwrap();
        store.close().await.unwrap();
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
        reject_superseded_protocol_evidence(&root, &responses_profile, &evidence).await;
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
            assert!(matches!(
                super::super::CustomProvider::admit(&root, "local", "model-1", None).await,
                Err(CustomProfileError::EvidenceUnavailable)
            ));
        }
        responses["version"] = json!(1);
        write(&responses);
        assert!(matches!(
            super::super::CustomProvider::admit(&root, "local", "model-1", None).await,
            Err(CustomProfileError::InvalidConfiguration)
        ));
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
    // Skipped in the Guard: local test IPC capability is missing.
    #[cfg_attr(arany_guard, ignore = "requires local test IPC")]
    async fn custom_compaction_requires_valid_text_at_every_effort() {
        for messages in [None].into_iter().chain(MESSAGES_CASES.map(Some)) {
            for reply in [
                ProbeReply::EmptySummary(2),
                ProbeReply::EmptySummary(4),
                ProbeReply::EmptySummary(6),
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
    // Skipped in the Guard: local test IPC capability is missing.
    #[cfg_attr(arany_guard, ignore = "requires local test IPC")]
    async fn messages_compaction_rejection_clears_admission() {
        for case in MESSAGES_CASES {
            assert!(matches!(
                run_case_protocol(ProbeReply::CompactionRejected, Some(case)).await,
                Err(CustomProfileError::ConformanceFailed)
            ));
        }
    }

    #[tokio::test]
    // Skipped in the Guard: local test IPC capability is missing.
    #[cfg_attr(arany_guard, ignore = "requires local test IPC")]
    async fn synthetic_conformance_checks_exact_wire_and_reopens_evidence() {
        for case in MESSAGES_CASES {
            for reply in [ProbeReply::Valid, ProbeReply::ValidEfforts] {
                let record = run_case_protocol(reply, Some(case))
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
                ProbeReply::Unauthorized,
                ProbeReply::RateLimited,
                ProbeReply::QuotaExceeded,
                ProbeReply::EscapedCredential,
                ProbeReply::MissingUsage,
                ProbeReply::ZeroUsage,
            ] {
                assert!(matches!(
                    run_case_protocol(reply, Some(case)).await,
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
