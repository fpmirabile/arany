use arany::test_support::scenario::*;
use arany::{
    AdmittedProviders, AgentPhase, AgentStatus, Answer, ChatGptAdmission, ChatGptProvenance,
    CollaborationPolicy, CompactionFailure, CompactionItem, CompactionRequest, CompactionResponse,
    CompactionStatus, ContextUsage, Delegate, Effort, Engine, EngineConfig, EngineError, Event,
    OutputTokenBound, Provider, ProviderCallDisposition, ProviderError, ProviderOutcome,
    ProviderRequest, ProviderResponse, RunCancellation, RunControls, RunHandle, RunHints, RunId,
    RunRequest, RunResult, RunStatus, RunSteering, SessionId, SessionView, StateRoot, Store,
    ToolGrant, UnansweredStatus, rename_session,
};
use arany::{continue_session, fork_session, list_sessions, resume_session};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

#[path = "engine_run/performance.rs"]
mod performance;

#[path = "engine_run/goals.rs"]
mod goals;

#[path = "engine_run/routing.rs"]
mod routing;

#[path = "engine_run/roster_journeys.rs"]
mod roster_journeys;

#[path = "engine_run/hooks.rs"]
mod hooks;

#[path = "engine_run/permissions.rs"]
mod permissions;

#[cfg(unix)]
pub use arany::test_support::process;

use arany::test_support::engine as engine_support;

async fn next_committed(events: &mut arany::Subscription, what: &str) -> Arc<arany::EventEnvelope> {
    match tokio::time::timeout(Duration::from_secs(5), events.next()).await {
        Ok(Some(arany::Tapped::Committed(event))) => event,
        _ => panic!("{what}"),
    }
}
use engine_support::{run_to_end, run_with};

#[tokio::test]
async fn failed_write_turn_stays_in_history_after_reopen() {
    use arany::test_support::goals::{finish_with, replayed};
    use std::os::unix::fs::PermissionsExt;

    for known_reason in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_path = temp.path().join("state");
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig {
                guard_executable: Some(std::env::current_exe().expect("test executable")),
                ..Default::default()
            },
        )
        .expect("engine");
        let config = state_path.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}"#)
            .expect("tool configuration");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("private configuration");
        let refused_write = || {
            tool_outcome(arany::ToolCall::Write {
                path: String::new(),
                expected_digest: None,
                content: "hello".into(),
            })
        };
        let attempts = if known_reason { 1 } else { 3 };
        let provider = ScriptedProvider::new(
            (0..attempts)
                .map(|_| refused_write())
                .chain([finish_with(
                    "The previous Run failed before a Tool was dispatched.",
                )])
                .collect(),
        );
        if known_reason {
            *provider
                .responses
                .lock()
                .expect("responses")
                .front_mut()
                .expect("first response") = Err(ProviderError::InvalidOutcomeContract);
        }
        let observed = provider.requests.clone();
        let provider = Arc::new(provider);
        let mut input = request(&workspace, "write test.txt");
        input.include_paths.clear();
        let first = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .expect("failed Write Run");
        assert_eq!(first.run.status, RunStatus::Failed);
        assert!(first.run.tools.is_empty());
        assert!(!workspace.join("test.txt").exists());
        engine.close().await.expect("shutdown before follow-up");
        let (events, view) = replayed(&state_path, first.session_id).await;
        assert_eq!(view.runs[0], first.run);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::ToolStarted { .. }))
        );
        let call = &view.runs[0].agents[0].provider_calls[0];
        assert_eq!(call.disposition, ProviderCallDisposition::InvalidResponse);
        assert_eq!(
            call.failure_reason,
            Some(if known_reason {
                arany::ProviderFailureReason::OutcomeContract
            } else {
                arany::ProviderFailureReason::ProposalRefused
            })
        );

        let engine = engine_support::open(
            StateRoot::open_existing(&state_path).expect("state"),
            EngineConfig::default(),
        )
        .expect("reopened Engine");
        let mut follow_up = request(&workspace, "why couldn't you write it?");
        follow_up.include_paths.clear();
        follow_up.session_id = Some(first.session_id);
        follow_up.title = None;
        let second = run_to_end(&engine, &provider, follow_up)
            .await
            .expect("follow-up");
        assert_eq!(second.run.status, RunStatus::Finished);
        engine.close().await.expect("shutdown");
        let (_, view) = replayed(&state_path, first.session_id).await;
        assert_eq!(view.runs[1], second.run);
        let requests = observed.lock().expect("requests");
        assert_eq!(requests.len(), attempts + 1);
        assert!(provider.responses.lock().expect("responses").is_empty());
        let follow_up_request = &requests[attempts];
        assert_eq!(
            follow_up_request.input.objective,
            "why couldn't you write it?"
        );
        let [history] = follow_up_request.input.history.as_slice() else {
            panic!("one failed turn retained")
        };
        assert_eq!(history.user, "write test.txt");
        let fact: serde_json::Value =
            serde_json::from_str(&history.assistant).expect("typed history fact");
        assert_eq!(
            fact,
            serde_json::json!({
                "status": "failed",
                "fact": "Run failed before any primary model Tool",
                "failure": if known_reason { "provider_call" } else { "refusal_limit" },
                "primary_model_tool_execution": "nothing dispatched",
                "last_call": if known_reason {
                    serde_json::json!({"disposition": "invalid_response", "failure_reason": "outcome_contract", "refusal": null})
                } else {
                    serde_json::json!({"disposition": "invalid_response", "failure_reason": "proposal_refused", "refusal": {"operation": "arany_write", "cause": "invalid_arguments"}})
                },
            })
        );
        assert_eq!(
            second.run.config.as_ref().expect("config").history_run_ids,
            [first.run.id]
        );
    }
}

#[test]
fn account_path_uses_the_private_binary_fixture() {
    let root = arany::test_support::isolate_account_root();
    if cfg!(not(debug_assertions)) && std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").is_some() {
        assert!(matches!(
            StateRoot::account_path(),
            Err(arany::StoreError::InvalidStateDirectory)
        ));
        return;
    }
    assert_eq!(StateRoot::account_path().unwrap(), root);
    #[cfg(all(unix, debug_assertions))]
    {
        use process::BoundedOutput;

        const CHILD: &str = "ARANY_TEST_INHERITED_ACCOUNT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(
                root,
                Path::new(&std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").unwrap())
            );
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .env(CHILD, "1")
            .env("ARANY_TEST_ACCOUNT_ROOT", root)
            .env("TMPDIR", root.join("missing-temp"))
            .args(["--exact", "account_path_uses_the_private_binary_fixture"])
            .bounded_output_for(Duration::from_secs(10), 4096)
            .unwrap();
        assert!(
            output.status.success(),
            "inherited account root: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        process::assert_single_test_completed(&output.stdout, "inherited account root");
    }
    if std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").is_none() {
        assert!(root.starts_with(std::fs::canonicalize(std::env::temp_dir()).unwrap()));
    }
}

#[cfg(unix)]
#[path = "engine_run/workspace_security.rs"]
mod workspace_security;

struct WaitingCompactionProvider {
    started: mpsc::UnboundedSender<()>,
    dropped: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    compact_requests: Arc<Mutex<Vec<CompactionRequest>>>,
}

#[async_trait::async_trait]
impl Provider for WaitingCompactionProvider {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new(
                    "waiting-compaction",
                    "test-model",
                    arany::Provenance::native(),
                )
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().expect("requests").push(request);
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome: ProviderOutcome::Answer(Answer {
                text: "Seed answer".into(),
            }),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        self.compact_requests
            .lock()
            .expect("compaction requests")
            .push(request);
        let _drop_counter = DropCounter(Arc::clone(&self.dropped));
        self.started
            .send(())
            .map_err(|_| ProviderError::Unavailable)?;
        std::future::pending().await
    }
}

#[test]
fn subscription_account_and_local_bound_survive_run_and_compaction_replay() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Synthetic fact").expect("include");
        let state_path = temp.path().join("state");
        let account_id = Uuid::now_v7();
        let fingerprint = [7; 32];
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: "Synthetic answer".into(),
        })])
        .with_chatgpt_account(account_id, fingerprint)
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Compacted synthetic history".into(),
            response_id: Some("resp_compact".into()),
            usage: arany::Usage {
                input_tokens: Some(20),
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: Some(
                arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested,
            ),
        })]);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let outcome = run_to_end(
            &engine,
            &provider,
            request(&workspace, "Synthetic objective"),
        )
        .await
        .expect("committed Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        let config = outcome.run.config.as_ref().expect("Run provenance");
        assert_eq!(config.provider, "chatgpt");
        assert_eq!(config.saved_api_account_id, None);
        assert_eq!(
            config.output_token_bound,
            OutputTokenBound::LocalAcceptanceOnly
        );
        assert_eq!(
            config.chatgpt_provenance,
            Some(ChatGptProvenance {
                account_id,
                evidence_fingerprint: fingerprint,
                admission: ChatGptAdmission::AccountConsent,
            })
        );
        let compaction = engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                outcome.session_id,
                workspace,
            )
            .await
            .expect("committed compaction");
        assert!(matches!(
            compaction.status,
            CompactionStatus::Succeeded { .. }
        ));
        assert_eq!(compaction.chatgpt_provenance, config.chatgpt_provenance);
        assert_eq!(
            compaction.output_token_bound,
            OutputTokenBound::LocalAcceptanceOnly
        );
        engine.close().await.expect("engine shutdown");

        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap())
            .expect("read-only reopen");
        let events = store
            .load_session(outcome.session_id)
            .await
            .expect("canonical Events");
        let view = SessionView::replay(outcome.session_id, &events)
            .expect("strict replay")
            .expect("Session");
        assert_eq!(view.runs[0].config.as_ref(), Some(config));
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::ContextCompacted { record, .. }
                if record.chatgpt_provenance == config.chatgpt_provenance
                    && record.output_token_bound == OutputTokenBound::LocalAcceptanceOnly
        )));
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn a_tampered_compaction_reference_fails_load() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "First answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Second answer".into(),
            }),
        ])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Derived summary".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let first = run_to_end(&engine, &provider, request(&workspace, "First question"))
            .await
            .expect("first Run");
        engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
            )
            .await
            .expect("compaction");
        let mut next = request(&workspace, "Second question");
        next.session_id = Some(first.session_id);
        next.title = None;
        run_to_end(&engine, &provider, next)
            .await
            .expect("Run after compaction");
        engine.close().await.expect("engine shutdown");
        let store =
            Store::open(StateRoot::admit(&state_path).expect("state readmission")).expect("store");
        let view = store
            .load_view(first.session_id)
            .await
            .expect("honest replay")
            .expect("Session");
        let reference = view
            .runs
            .last()
            .and_then(|run| run.config.as_ref())
            .and_then(|config| config.compaction_event_sequence)
            .expect("later Run names its compaction");
        store.close().await.expect("store shutdown");
        let connection =
            rusqlite::Connection::open(state_path.join("events.sqlite3")).expect("database");
        let changed = connection
            .execute(
                "UPDATE events SET payload = replace(payload, ?1, ?2)
                 WHERE kind = 'RunStarted' AND instr(payload, ?1) > 0",
                rusqlite::params![
                    format!("\"compaction_event_sequence\":{reference}"),
                    format!("\"compaction_event_sequence\":{}", reference + 1000),
                ],
            )
            .expect("tamper");
        assert_eq!(changed, 1);
        drop(connection);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only store");
        assert!(store.load_view(first.session_id).await.is_err());
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn a_fork_before_the_snapshot_event_recompiles_from_canonical_history() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "Source answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Child answer".into(),
            }),
        ])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Source-only derived summary".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let observed = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let first = run_to_end(&engine, &provider, request(&workspace, "Source question"))
            .await
            .expect("source Run");
        engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
            )
            .await
            .expect("source compaction");
        let fork_id = engine
            .fork_session(first.session_id, workspace.clone(), None)
            .await
            .expect("fork at committed Run boundary");
        let mut branch_request = request(&workspace, "Branch question");
        branch_request.session_id = Some(fork_id);
        branch_request.title = None;
        let branch = run_to_end(&engine, &provider, branch_request)
            .await
            .expect("branch Run");
        assert!(
            branch
                .run
                .config
                .as_ref()
                .expect("config")
                .compaction_event_sequence
                .is_none()
        );
        {
            let requests = observed.lock().expect("request lock");
            assert_eq!(requests.len(), 2);
            assert!(requests[1].input.context_summary.is_none());
            assert_eq!(requests[1].input.history.len(), 1);
            assert_eq!(requests[1].input.history[0].assistant, "Source answer");
        }
        engine.close().await.expect("engine shutdown");
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let branch_view = store
            .load_view(fork_id)
            .await
            .expect("branch replay")
            .expect("branch exists");
        assert!(branch_view.compactions.is_empty());
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn invalid_compaction_snapshot_corpus_fails_before_provider_disclosure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        for fault in ["source_digest", "content_digest", "compiler_version"] {
            let temp = tempfile::tempdir().expect("temporary root");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
            std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
            let state_path = temp.path().join("state");
            let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
                text: "Source answer".into(),
            })])
            .with_compactions(vec![Ok(CompactionResponse {
                summary: "Derived summary".into(),
                response_id: None,
                usage: arany::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(8),
                    ..Default::default()
                },
                wire_provenance: None,
            })]);
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&state_path).expect("private state"),
                EngineConfig::default(),
            )
            .expect("engine");
            let first = run_to_end(&engine, &provider, request(&workspace, "Source question"))
                .await
                .expect("source Run");
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    first.session_id,
                    workspace.clone(),
                )
                .await
                .expect("source compaction");
            engine.close().await.expect("engine shutdown");

            let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
                .expect("fault injection connection");
            let payload: String = connection
                .query_row(
                    "SELECT payload FROM events WHERE session_id=?1 AND kind='ContextCompacted'",
                    [first.session_id.to_string()],
                    |row| row.get(0),
                )
                .expect("snapshot payload");
            let mut payload: serde_json::Value =
                serde_json::from_str(&payload).expect("valid snapshot JSON");
            let document = &mut payload["record"];
            match fault {
                "source_digest" => {
                    let value = document["source_digest"][0].as_u64().expect("digest byte");
                    document["source_digest"][0] = (value ^ 1).into();
                }
                "content_digest" => {
                    let value = document["status"]["content_digest"][0]
                        .as_u64()
                        .expect("digest byte");
                    document["status"]["content_digest"][0] = (value ^ 1).into();
                }
                "compiler_version" => document["compiler_version"] = 4.into(),
                _ => unreachable!("fixed corpus"),
            }
            connection
                .execute(
                    "UPDATE events SET payload=?1 WHERE session_id=?2 AND kind='ContextCompacted'",
                    [payload.to_string(), first.session_id.to_string()],
                )
                .expect("injected invalid snapshot");
            drop(connection);

            let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
                .expect("reopened store");
            assert!(
                matches!(
                    store.load_session(first.session_id).await,
                    Err(arany::StoreError::InvalidSnapshot)
                ),
                "fault: {fault}"
            );
            store.close().await.expect("store shutdown");

            let provider = ScriptedProvider::new(Vec::new());
            let calls = Arc::clone(&provider.requests);
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&state_path).expect("state readmission"),
                EngineConfig::default(),
            )
            .expect("engine after fault");
            let mut rejected = request(&workspace, "Must not disclose");
            rejected.session_id = Some(first.session_id);
            rejected.title = None;
            assert!(
                matches!(
                    run_to_end(&engine, &provider, rejected).await,
                    Err(EngineError::Store(arany::StoreError::InvalidSnapshot))
                ),
                "fault: {fault}"
            );
            assert!(calls.lock().expect("request lock").is_empty());
            engine.close().await.expect("engine shutdown after fault");
        }
    });
}

#[test]
fn compaction_input_limit_rejects_before_provider_egress() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "x".repeat(32 * 1024)
            });
            10
        ]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut session_id = None;
        for _ in 0..10 {
            let mut next = request(&workspace, &"Q".repeat(8 * 1024));
            next.session_id = session_id;
            if session_id.is_some() {
                next.title = None;
            }
            session_id = Some(
                run_to_end(&engine, &provider, next)
                    .await
                    .expect("bounded Run")
                    .session_id,
            );
        }
        let session_id = session_id.expect("Session exists");
        assert!(matches!(
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    session_id,
                    workspace.clone()
                )
                .await,
            Err(EngineError::CompactionInputTooLarge)
        ));
        assert!(compact_requests.lock().expect("request lock").is_empty());
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(session_id)
            .await
            .expect("durable Events");
        assert_eq!(events.len(), 111);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalStarted { .. }))
                .count(),
            10
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
                .count(),
            10
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::ContextCompacted { .. }))
        );
        store.close().await.expect("store shutdown");

        let maintained_path = temp.path().join("maintained-state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "x".repeat(32 * 1024)
            });
            16
        ])
        .with_compactions(
            (0..3)
                .map(|_| {
                    Ok(CompactionResponse {
                        summary: "s".repeat(8 * 1024),
                        response_id: None,
                        usage: arany::Usage {
                            input_tokens: Some(100),
                            output_tokens: Some(1024),
                            ..Default::default()
                        },
                        wire_provenance: None,
                    })
                })
                .collect(),
        );
        let requests = Arc::clone(&provider.requests);
        let compactions = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&maintained_path).unwrap(),
            EngineConfig::default(),
        )
        .expect("maintained Engine");
        let mut session_id = None;
        let mut maintenance_sequences = Vec::new();
        // The automatic threshold is 158_080, so 8 KiB-draft turns trigger maintenance every five turns and each cycle fits the input bound.
        for index in 0..16 {
            let objective = format!("{index:02}{}", "Q".repeat(8 * 1024 - 2));
            let mut next = request(&workspace, &objective);
            next.session_id = session_id;
            if session_id.is_some() {
                next.title = None;
            }
            let outcome = run_to_end(&engine, &provider, next)
                .await
                .expect("maintained bounded Run");
            session_id = Some(outcome.session_id);
            let config = outcome.run.config.as_ref().expect("context footprint");
            assert_eq!(
                config.auto_compaction_due(),
                matches!(index, 4 | 9 | 14),
                "maintenance must precede an oversized prefix at turn {}",
                index + 1
            );
            if matches!(index, 5 | 10 | 15) {
                assert_eq!(
                    config.compaction_event_sequence,
                    maintenance_sequences.last().copied()
                );
                let requests = requests.lock().expect("observed requests");
                let request = requests.last().expect("post-maintenance request");
                assert_eq!(
                    request.input.context_summary.as_deref(),
                    Some("s".repeat(8 * 1024).as_str())
                );
                assert!(
                    request.input.history.is_empty(),
                    "summary replaces covered context, not canonical history"
                );
            }
            if config.auto_compaction_due() {
                let compacted = engine
                    .auto_compact_session(
                        &AdmittedProviders::single(provider.clone()),
                        outcome.session_id,
                        workspace.clone(),
                        outcome.run.id,
                    )
                    .await
                    .expect("automatic input remains admissible")
                    .expect("new boundary");
                assert_eq!(compacted.covered_run_id, outcome.run.id);
                assert!(compacted.source_bytes <= 256 * 1024);
                assert!(matches!(
                    compacted.status,
                    CompactionStatus::Succeeded { .. }
                ));
                let observer =
                    Store::open_read_only(StateRoot::open_existing(&maintained_path).unwrap())
                        .expect("committed maintenance observer");
                let events = observer
                    .load_session(outcome.session_id)
                    .await
                    .expect("committed maintenance");
                maintenance_sequences.push(events.last().expect("summary Event").sequence);
                observer.close().await.expect("close maintenance observer");
            }
        }
        let session_id = session_id.expect("maintained Session");
        assert_eq!(maintenance_sequences.len(), 3);
        {
            let requests = requests.lock().expect("observed Run requests");
            assert_eq!(requests.len(), 16);
            for (index, request) in requests.iter().enumerate() {
                assert_eq!(request.phase, AgentPhase::RootPlan);
                assert_eq!(request.model, "test-model");
                assert_eq!(request.max_output_tokens, 4096);
                assert_eq!(
                    request.input.objective,
                    format!("{index:02}{}", "Q".repeat(8 * 1024 - 2))
                );
                assert_eq!(
                    request.input.includes,
                    [include_data("notes/facts.txt", "Fact")]
                );
                assert!(request.input.workspace_guidance.is_none());
                assert!(request.input.child_results.is_empty());
            }
            let compactions = compactions.lock().expect("observed compactions");
            assert_eq!(compactions.len(), 3);
            for (cycle, request) in compactions.iter().enumerate() {
                assert_eq!(request.model, "test-model");
                assert_eq!(request.max_output_tokens, 1024);
                assert_eq!(request.previous_summary.is_some(), cycle > 0);
                assert_eq!(request.items.len(), 5);
                for (offset, item) in request.items.iter().enumerate() {
                    let CompactionItem::Completed(turn) = item else {
                        panic!("completed source turn");
                    };
                    assert_eq!(
                        turn.user,
                        format!("{:02}{}", cycle * 5 + offset, "Q".repeat(8 * 1024 - 2))
                    );
                    assert_eq!(turn.assistant, "x".repeat(32 * 1024));
                }
            }
        }
        engine.close().await.expect("maintained Engine shutdown");
        let store = Store::open_read_only(StateRoot::open_existing(&maintained_path).unwrap())
            .expect("closed maintained Store");
        let events = store
            .load_session(session_id)
            .await
            .expect("maintained Events");
        let view = SessionView::replay(session_id, &events)
            .expect("strict maintained replay")
            .expect("Session");
        assert_eq!(view.runs.len(), 16);
        assert_eq!(view.compactions.len(), 3);
        for (index, run) in view.runs.iter().enumerate() {
            assert_eq!(run.status, RunStatus::Finished);
            assert_eq!(
                run.objective,
                format!("{index:02}{}", "Q".repeat(8 * 1024 - 2))
            );
            assert_eq!(
                run.assistant_message.as_deref(),
                Some("x".repeat(32 * 1024).as_str())
            );
        }
        store.close().await.expect("close maintained replay");
    });
}

#[test]
fn oversized_compaction_output_is_recorded_as_failure_without_a_snapshot() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "Source answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Later answer".into(),
            }),
        ])
        .with_compactions(vec![
            Ok(CompactionResponse {
                summary: "z".repeat(8 * 1024 + 1),
                response_id: None,
                usage: arany::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(1024),
                    ..Default::default()
                },
                wire_provenance: None,
            }),
            Ok(CompactionResponse {
                summary: "safe summary".into(),
                response_id: Some("bad\nidentifier".into()),
                usage: arany::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(8),
                    ..Default::default()
                },
                wire_provenance: None,
            }),
        ]);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let first = run_to_end(&engine, &provider, request(&workspace, "Source question"))
            .await
            .expect("source Run");
        let failed = engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
            )
            .await
            .expect("durable failed compaction");
        assert!(matches!(
            failed.status,
            CompactionStatus::Failed {
                reason: CompactionFailure::InvalidOutcome
            }
        ));
        let mut later_request = request(&workspace, "Later question");
        later_request.session_id = Some(first.session_id);
        later_request.title = None;
        run_to_end(&engine, &provider, later_request)
            .await
            .expect("later Run");
        let hostile_id = engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
            )
            .await
            .expect("durable hostile-ID failure");
        assert!(matches!(
            hostile_id.status,
            CompactionStatus::Failed {
                reason: CompactionFailure::InvalidOutcome
            }
        ));
        assert_eq!(hostile_id.response_id, None);
        {
            let observed = requests.lock().expect("request lock");
            assert_eq!(observed.len(), 2);
            assert!(observed[1].input.context_summary.is_none());
            assert_eq!(observed[1].input.history.len(), 1);
        }
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let view = store
            .load_view(first.session_id)
            .await
            .expect("replay")
            .expect("Session exists");
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.compactions.len(), 2);
        assert!(
            view.compactions.iter().all(|compaction| matches!(
                compaction.record.status,
                CompactionStatus::Failed { .. }
            ))
        );
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn compaction_preserves_unanswered_accepted_objectives_as_untrusted_data() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(
            (0..3)
                .map(|_| {
                    ProviderOutcome::Delegate(Delegate {
                        agent: None,
                        children: vec![arany::ChildAssignment {
                            editor: None,
                            objective: "not allowed".into(),
                            profile: None,
                        }],
                    })
                })
                .collect(),
        )
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Question remains unresolved".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let failed = run_to_end(
            &engine,
            &provider,
            request(&workspace, "Unanswered question"),
        )
        .await
        .expect("durable failed Run");
        assert_eq!(failed.run.status, RunStatus::Failed);
        let record = engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                failed.session_id,
                workspace,
            )
            .await
            .expect("compaction over failed Run");
        assert_eq!(record.covered_run_id, failed.run.id);
        {
            let requests = compact_requests.lock().expect("compaction request lock");
            assert_eq!(requests.len(), 1);
            assert!(matches!(
                &requests[0].items[..],
                [CompactionItem::Unanswered { user, status: UnansweredStatus::Failed }]
                    if user == "Unanswered question"
            ));
        }
        engine.close().await.expect("engine shutdown");
    });
}

#[test]
fn an_earlier_prestart_interruption_does_not_block_later_compaction_or_fork() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let session_id = arany::create_session(
            StateRoot::admit(&state_path).expect("private state"),
            workspace.clone(),
            Some("Interrupted Session".into()),
        )
        .await
        .expect("pinned Session start");
        use std::os::unix::fs::MetadataExt;
        let workspace_metadata = std::fs::metadata(&workspace).expect("Workspace metadata");
        let workspace_identity = arany::WorkspaceIdentity {
            device: workspace_metadata.dev(),
            inode: workspace_metadata.ino(),
        };
        let goal_id = arany::GoalId::new();
        let interrupted_run_id = arany::RunId::new();
        let store =
            Store::open(StateRoot::admit(&state_path).expect("private state")).expect("store");
        store
            .append(
                session_id,
                Event::GoalStarted {
                    goal_id,
                    objective: "Unfinished question".into(),
                    workspace_identity,
                    budget: arany::GoalBudget::default(),
                    phase: arany::GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: arany::ToolResourceSettings::default(),
                    continues: None,
                },
            )
            .await
            .expect("Goal start");
        store
            .append(
                session_id,
                Event::GoalRunLinked {
                    goal_id,
                    run_id: interrupted_run_id,
                    phase: arany::GoalPhase::Implementation,
                },
            )
            .await
            .expect("interrupted Run link");
        store
            .append(
                session_id,
                Event::MessageAccepted {
                    run_id: interrupted_run_id,
                    text: "Unfinished question".into(),
                    images: Vec::new(),
                },
            )
            .await
            .expect("accepted objective");
        store.close().await.expect("store shutdown");

        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "Finished answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Follow-up answer".into(),
            }),
        ])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Unfinished then finished".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let observed = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut resumed = request(&workspace, "Finished question");
        resumed.session_id = Some(session_id);
        resumed.title = None;
        resumed.goal = arany::GoalRequest::resume(goal_id, true);
        run_to_end(&engine, &provider, resumed)
            .await
            .expect("completed later Run");
        engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                session_id,
                workspace.clone(),
            )
            .await
            .expect("compaction after interruption");
        {
            let requests = compact_requests.lock().expect("compaction request lock");
            assert!(matches!(
                &requests[0].items[0],
                CompactionItem::Unanswered {
                    user,
                    status: UnansweredStatus::Interrupted
                } if user == "Unfinished question"
            ));
        }
        let fork_id = engine
            .fork_session(session_id, workspace.clone(), None)
            .await
            .expect("fork after interruption");
        let mut follow_up = request(&workspace, "Question after compaction");
        follow_up.session_id = Some(session_id);
        follow_up.title = None;
        run_to_end(&engine, &provider, follow_up)
            .await
            .expect("Run after compaction");
        {
            let requests = observed.lock().expect("request lock");
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[1].input.context_summary.as_deref(),
                Some("Unfinished then finished")
            );
            assert!(requests[1].input.history.is_empty());
        }
        engine.close().await.expect("engine shutdown");
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let fork = store
            .load_view(fork_id)
            .await
            .expect("fork replay")
            .expect("fork exists");
        assert_eq!(fork.runs.len(), 1);
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn persistence_capacity_is_admitted_before_provider_usage() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
        // Each admitted action reserves its worst-case Event envelope before dispatch; the boundary is the
        // largest prefix that still admits the Run: 11 Events for a single plain Run, 17 for one Team child.
        for (index, (prefix, policy, admitted, tools, branch, previews)) in [
            (9989, CollaborationPolicy::Single, false, false, false, false),
            (9988, CollaborationPolicy::Single, true, false, false, false),
            (9984, CollaborationPolicy::Team { max_active_children: 1 }, false, false, false, false),
            (9982, CollaborationPolicy::Team { max_active_children: 1 }, true, false, false, false),
            (9988, CollaborationPolicy::Single, false, true, false, false),
            (9987, CollaborationPolicy::Single, true, true, false, false),
            (9988, CollaborationPolicy::Single, false, false, true, false),
            (9987, CollaborationPolicy::Single, true, false, true, false),
            (9925, CollaborationPolicy::Single, false, false, false, true),
            (9924, CollaborationPolicy::Single, true, false, false, true),
        ].into_iter().enumerate() {
            let path = temp.path().join(format!("state-{index}"));
            let finish = || ProviderOutcome::Answer(Answer { text: "answer".into() });
            let provider = Arc::new(ScriptedProvider::new(vec![finish()]));
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let id = run_to_end(&engine, &provider, request(&workspace, "Initial")).await.unwrap().session_id;
            engine.close().await.unwrap();
            let mut connection = rusqlite::Connection::open(path.join("events.sqlite3")).unwrap();
            let transaction = connection.transaction().unwrap();
            let first_padding_sequence: i64 = transaction
                .query_row("SELECT COALESCE(MAX(sequence), 0) + 1 FROM events", [], |row| {
                    row.get(0)
                })
                .unwrap();
            for sequence in first_padding_sequence..=i64::from(prefix) {
                transaction.execute("INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',6,'{\"title\":\"x\"}',0)", rusqlite::params![sequence, id.to_string()]).unwrap();
            }
            transaction.commit().unwrap();
            drop(connection);
            let responses = if matches!(policy, CollaborationPolicy::Team { .. }) {
                vec![ProviderOutcome::Delegate(Delegate { agent: None, children: vec![arany::ChildAssignment { editor: None, objective: "Child".into(), profile: None }] }), finish(), finish()]
            } else { vec![finish()] };
            let provider = ScriptedProvider::new(responses);
            let requests = provider.requests.clone();
            let provider = Arc::new(provider);
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig { guard_executable: Some(std::env::current_exe().expect("test executable")), ..Default::default() }).unwrap();
            let id = if branch { engine.fork_session(id, workspace.clone(), None).await.unwrap() } else { id };
            if tools {
                use std::os::unix::fs::PermissionsExt;
                let config = path.join("tools.json");
                std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
                std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let max_provider_calls = match &policy {
                CollaborationPolicy::Single => 1,
                CollaborationPolicy::Auto { max_active_children }
                | CollaborationPolicy::Team { max_active_children } => {
                    u32::from(*max_active_children) + 2
                }
            };
            let mut next = request(&workspace, "Next");
            next.session_id = Some(id);
            next.title = None;
            next.policy = policy;
            next.goal.budget = Some(arany::GoalBudget {
                max_provider_calls: Some(max_provider_calls),
                max_tokens: None,
                max_active_ms: Some(30 * 60 * 1000),
                max_runs: 1,
            });
            let controls = RunControls {
                tools: if tools { ToolGrant::Configured } else { ToolGrant::None },
                hints: previews.then(RunHints::with_previews),
                ..Default::default()
            };
            let result = run_with(&engine, &provider, next, controls).await;
            if admitted {
                assert_eq!(result.unwrap().run.status, RunStatus::Finished, "case {index}");
            } else {
                assert!(matches!(result, Err(EngineError::Store(arany::StoreError::ReplayLimit))), "case {index}");
            }
            engine.close().await.unwrap();
            let phases: Vec<_> = requests.lock().unwrap().iter().map(|request| request.phase).collect();
            // Each action is admitted before its own dispatch: a Team refused at its child wave has already dispatched the root plan.
            let expected_phases = match (admitted, policy) {
                (true, CollaborationPolicy::Single) => vec![AgentPhase::RootPlan],
                (true, _) => vec![AgentPhase::RootPlan, AgentPhase::ChildWork, AgentPhase::RootSynthesis],
                (false, CollaborationPolicy::Single) => vec![],
                (false, _) => vec![AgentPhase::RootPlan],
            };
            assert_eq!(phases, expected_phases, "case {index}");
            let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
            let expected_event_count = if branch {
                // The fork's own Event plus the Single Run's admitted or refused prefix.
                if admitted { 12 } else { 1 }
            } else if admitted {
                prefix as usize
                    + if matches!(policy, CollaborationPolicy::Single) {
                        11
                    } else {
                        17
                    }
            } else {
                // A Single Run refused at Run start commits nothing; a Team refused at its child wave keeps its
                // Run start and root plan but no child AgentSpawned.
                prefix as usize
                    + match policy {
                        CollaborationPolicy::Single => 0,
                        _ => 7,
                    }
            };
            assert_eq!(
                store.load_session(id).await.unwrap().len(),
                expected_event_count,
                "case {index}: exact committed Event count"
            );
            let view = store.load_view(id).await.unwrap().unwrap();
            let linked = if admitted || !matches!(policy, CollaborationPolicy::Single) { 2 } else { 1 };
            assert_eq!(view.runs.len(), linked, "case {index}: a refused Single Run is never linked");
            assert_eq!(view.runs[0].status, RunStatus::Finished);
            if linked == 2 {
                // A refused Run is an interrupted prefix: replay labels it interrupted, never Finished.
                assert_eq!(view.runs[1].status == RunStatus::Finished, admitted, "case {index}");
            }
            store.close().await.unwrap();
        }
        // Two children in two slots form one wave with a 4N+6 = 14 Event tail: 9978 admits to 9,999 Events and the root call reserves one more for a recorded plan.
        for (prefix, admitted) in [(9978_i64, true), (9980, false)] {
            let path = temp.path().join(format!("two-child-{prefix}"));
            let finish = || ProviderOutcome::Answer(Answer { text: "answer".into() });
            let provider = Arc::new(ScriptedProvider::new(vec![finish()]));
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let id = run_to_end(&engine, &provider, request(&workspace, "Initial")).await.unwrap().session_id;
            engine.close().await.unwrap();
            let mut connection = rusqlite::Connection::open(path.join("events.sqlite3")).unwrap();
            let transaction = connection.transaction().unwrap();
            let first_padding_sequence: i64 = transaction
                .query_row("SELECT COALESCE(MAX(sequence), 0) + 1 FROM events", [], |row| row.get(0))
                .unwrap();
            for sequence in first_padding_sequence..=prefix {
                transaction.execute("INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',6,'{\"title\":\"x\"}',0)", rusqlite::params![sequence, id.to_string()]).unwrap();
            }
            transaction.commit().unwrap();
            drop(connection);
            let provider = ScriptedProvider::new(vec![
                ProviderOutcome::Delegate(Delegate { agent: None, children: vec![arany::ChildAssignment { editor: None, objective: "Child one".into(), profile: None }, arany::ChildAssignment { editor: None, objective: "Child two".into(), profile: None }] }),
                finish(),
                finish(),
                finish(),
            ]);
            let requests = provider.requests.clone();
            let provider = Arc::new(provider);
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let mut next = request(&workspace, "Next");
            next.session_id = Some(id);
            next.title = None;
            next.policy = CollaborationPolicy::Team { max_active_children: 2 };
            next.goal.budget = Some(arany::GoalBudget {
                max_provider_calls: Some(4),
                max_tokens: None,
                max_active_ms: Some(30 * 60 * 1000),
                max_runs: 1,
            });
            let result = run_to_end(&engine, &provider, next).await;
            if admitted {
                assert_eq!(result.unwrap().run.status, RunStatus::Finished, "two children at {prefix}");
            } else {
                assert!(matches!(result, Err(EngineError::Store(arany::StoreError::ReplayLimit))), "two children at {prefix}");
            }
            engine.close().await.unwrap();
            let phases: Vec<_> = requests.lock().unwrap().iter().map(|request| request.phase).collect();
            let expected_phases = if admitted {
                vec![AgentPhase::RootPlan, AgentPhase::ChildWork, AgentPhase::ChildWork, AgentPhase::RootSynthesis]
            } else {
                vec![AgentPhase::RootPlan]
            };
            assert_eq!(phases, expected_phases, "two children at {prefix}");
            let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
            let events = store.load_session(id).await.unwrap();
            let spawned = events.iter().filter(|event| matches!(event.event, Event::AgentSpawned { role: arany::AgentRole::Child, .. })).count();
            assert_eq!(spawned, if admitted { 2 } else { 0 }, "two children at {prefix}: AgentSpawned");
            assert_eq!(events.len(), if admitted { 9_999 } else { prefix as usize + 7 }, "two children at {prefix}: committed Events");
            store.close().await.unwrap();
        }
        for (index, (prefix, prior_forks, admitted)) in [
            (Some(9999), 0, true),
            (Some(10_000), 0, false),
            (None, 7, true),
            (None, 8, false),
        ].into_iter().enumerate() {
            let path = temp.path().join(format!("fork-admission-{index}"));
            let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "answer".into() })]));
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let root = run_to_end(&engine, &provider, request(&workspace, "Fork source")).await.unwrap().session_id;
            engine.close().await.unwrap();
            let mut connection = rusqlite::Connection::open(path.join("events.sqlite3")).unwrap();
            let transaction = connection.transaction().unwrap();
            let first_padding_sequence: i64 = transaction
                .query_row("SELECT COALESCE(MAX(sequence), 0) + 1 FROM events", [], |row| {
                    row.get(0)
                })
                .unwrap();
            if let Some(prefix) = prefix {
                for sequence in first_padding_sequence..=prefix {
                    transaction.execute("INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',6,'{\"title\":\"x\"}',0)", rusqlite::params![sequence, root.to_string()]).unwrap();
                }
            }
            let event_count_before_forks = prefix.unwrap_or(first_padding_sequence - 1);
            transaction.commit().unwrap();
            drop(connection);
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let mut source = root;
            for _ in 0..prior_forks {
                source = engine.fork_session(source, workspace.clone(), None).await.unwrap();
            }
            let result = engine.fork_session(source, workspace.clone(), None).await;
            let forked = if admitted {
                Some(result.expect("a boundary-admitted fork must commit"))
            } else {
                assert!(matches!(result, Err(EngineError::Store(arany::StoreError::ReplayLimit))), "fork case {index}: resulting lineage must be rejected before commitment");
                None
            };
            engine.close().await.unwrap();
            assert_eq!(provider.requests.lock().unwrap().len(), 1, "fork admission makes no Provider call");
            let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
            for id in [Some(root), Some(source), forked].into_iter().flatten() {
                let view = store.load_view(id).await.unwrap().unwrap();
                assert_eq!(view.runs.len(), 1);
                assert_eq!(view.runs[0].status, RunStatus::Finished);
                assert_eq!(view.runs[0].assistant_message.as_deref(), Some("answer"));
            }
            store.close().await.unwrap();
            let connection = rusqlite::Connection::open_with_flags(path.join("events.sqlite3"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            let count: i64 = connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0)).unwrap();
            assert_eq!(
                count,
                event_count_before_forks + i64::from(prior_forks) + i64::from(admitted),
                "fork case {index}: rejected insert must roll back"
            );
        }
        let path = temp.path().join("compactions");
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "answer".into() }), ProviderOutcome::Answer(Answer { text: "later answer".into() })])
            .with_compactions((0..66).map(|_| Err(ProviderError::RemoteHttp(403))).collect());
        let calls = provider.compact_requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
        let id = run_to_end(&engine, &provider, request(&workspace, "Source")).await.unwrap().session_id;
        for _ in 0..65 {
            assert!(matches!(engine.compact_session(&AdmittedProviders::single(provider.clone()), id, workspace.clone()).await.unwrap().status, CompactionStatus::Failed { .. }));
        }
        assert_eq!(calls.lock().unwrap().len(), 65);
        let mut later = request(&workspace, "Later");
        later.session_id = Some(id);
        later.title = None;
        run_to_end(&engine, &provider, later).await.unwrap();
        let branch = engine.fork_session(id, workspace.clone(), None).await.unwrap();
        assert!(matches!(engine.compact_session(&AdmittedProviders::single(provider.clone()), branch, workspace.clone()).await.unwrap().status, CompactionStatus::Failed { .. }), "inherited compactions do not block the fork's own compaction attempt");
        assert_eq!(calls.lock().unwrap().len(), 66);
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
        assert_eq!(store.load_view(id).await.unwrap().unwrap().compactions.len(), 65);
        assert_eq!(store.load_view(branch).await.unwrap().unwrap().compactions.len(), 66);
        store.close().await.unwrap();
    });
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    not(arany_ci),
    ignore = "file-backed 256 MiB database-capacity admission and recovery"
)]
fn database_capacity_is_admitted_before_provider_usage() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
        let state_path = temp.path().join("state");
        let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "answer".into() })]));
        let engine = engine_support::open(StateRoot::admit(&state_path).unwrap(), EngineConfig::default())
        .unwrap();
        let id = run_to_end(&engine, &provider, request(&workspace, "Source")).await.unwrap().session_id;
        engine.close().await.unwrap();
        let database = state_path.join("events.sqlite3");
        let mut connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("CREATE TABLE capacity_padding(value BLOB NOT NULL) STRICT;").unwrap();
        let target_pages = (256 * 1024 * 1024 - 4 * 1024 * 1024) / 4096 - 4;
        let transaction = connection.transaction().unwrap();
        transaction.execute(
            "WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n < ?1) INSERT INTO capacity_padding SELECT zeroblob(3000) FROM rows",
            [target_pages - 300],
        ).unwrap();
        for _ in 0..512 {
            let pages: i64 = transaction.query_row("PRAGMA page_count", [], |row| row.get(0)).unwrap();
            if pages >= target_pages {
                break;
            }
            transaction.execute("INSERT INTO capacity_padding VALUES (zeroblob(3000))", []).unwrap();
        }
        let pages: i64 = transaction.query_row("PRAGMA page_count", [], |row| row.get(0)).unwrap();
        assert!((target_pages..=target_pages + 1).contains(&pages), "fixture must reach the admission floor");
        transaction.commit().unwrap();
        drop(connection);
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "r".repeat(32 * 1024) })]).with_compactions(vec![Ok(CompactionResponse {
            summary: "source and recovered answer".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let calls = provider.requests.clone();
        let compactions = provider.compact_requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::admit(&state_path).unwrap(), EngineConfig { guard_executable: Some(std::env::current_exe().expect("test executable")), ..Default::default() }).unwrap();
        let next = || {
            let mut value = request(&workspace, "Recovered");
            value.session_id = Some(id);
            value.title = None;
            value
        };
        for policy in [CollaborationPolicy::Single, CollaborationPolicy::Team { max_active_children: 8 }] {
            let mut attempt = next();
            attempt.policy = policy;
            assert!(matches!(run_to_end(&engine, &provider, attempt).await, Err(EngineError::Store(arany::StoreError::StorageFull))));
            assert!(calls.lock().unwrap().is_empty(), "database capacity rejection must precede inference");
        }
        {
            use std::os::unix::fs::PermissionsExt;
            let config = state_path.join("tools.json");
            std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(matches!(run_with(&engine, &provider, next(), RunControls { tools: ToolGrant::Configured, ..Default::default() }).await, Err(EngineError::Store(arany::StoreError::StorageFull))));
        assert!(matches!(engine.compact_session(&AdmittedProviders::single(provider.clone()), id, workspace.clone()).await, Err(EngineError::Store(arany::StoreError::StorageFull))));
        assert!(calls.lock().unwrap().is_empty());
        assert!(compactions.lock().unwrap().is_empty());
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(id).await.unwrap();
        assert_eq!(events.len(), 12);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalFinished { .. }))
                .count(),
            1
        );
        let view = store.load_view(id).await.unwrap().unwrap();
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        assert!(view.compactions.is_empty());
        store.close().await.unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("DROP TABLE capacity_padding; VACUUM;").unwrap();
        drop(connection);
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "recovered answer".into() })]).with_compactions(vec![Ok(CompactionResponse {
            summary: "source and recovered answer".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let calls = provider.requests.clone();
        let compactions = provider.compact_requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::admit(&state_path).unwrap(), EngineConfig::default()).unwrap();
        let recovered = run_to_end(&engine, &provider, next()).await.unwrap();
        assert_eq!(recovered.run.status, RunStatus::Finished);
        engine.compact_session(&AdmittedProviders::single(provider.clone()), id, workspace.clone()).await.unwrap();
        engine.close().await.unwrap();
        {
            let requests = calls.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.run_id, recovered.run.id);
            assert_eq!(request.agent_run_id, recovered.run.agents[0].id);
            assert_eq!(request.phase, AgentPhase::RootPlan);
            assert_eq!(request.collaboration, CollaborationPolicy::Single);
            assert_eq!(request.model, "test-model");
            assert_eq!(request.input.objective, "Recovered");
            assert!(request.input.images.is_empty());
            assert!(request.input.workspace_guidance.is_none());
            assert_eq!(request.input.includes, vec![include_data("notes/facts.txt", "Fact")]);
            assert_eq!(request.input.history, vec![arany::HistoryTurn { user: "Source".into(), assistant: "answer".into() }]);
            assert!(request.input.context_summary.is_none());
            assert!(request.input.child_results.is_empty());
            assert_eq!(request.max_output_tokens, 4096);
            assert!(request.input.tools.is_none());
        }
        {
            let requests = compactions.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].session_id, id);
            assert_eq!(requests[0].covered_run_id, recovered.run.id);
            assert_eq!(requests[0].model, "test-model");
            assert!(requests[0].previous_summary.is_none());
            assert_eq!(requests[0].items, vec![
                CompactionItem::Completed(arany::HistoryTurn { user: "Source".into(), assistant: "answer".into() }),
                CompactionItem::Completed(arany::HistoryTurn { user: "Recovered".into(), assistant: "recovered answer".into() }),
            ]);
            assert_eq!(requests[0].max_output_tokens, 1024);
        }
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(id).await.unwrap();
        let goal_event_counts = [
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalStarted { .. }))
                .count(),
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalRunLinked { .. }))
                .count(),
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
                .count(),
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalFinished { .. }))
                .count(),
        ];
        assert_eq!(goal_event_counts, [2; 4]);
        let non_goal_events = events
            .iter()
            .filter(|event| {
                !matches!(
                    event.event,
                    Event::GoalStarted { .. }
                        | Event::GoalRunLinked { .. }
                        | Event::GoalProviderCallReserved { .. }
                        | Event::GoalFinished { .. }
                )
            })
            .count();
        assert_eq!(non_goal_events, 16);
        assert_eq!(events.len(), 24);
        let view = store.load_view(id).await.unwrap().unwrap();
        assert_eq!(view.runs.len(), 2);
        assert!(view.runs.iter().all(|run| run.status == RunStatus::Finished));
        assert_eq!(view.runs[1].assistant_message.as_deref(), Some("recovered answer"));
        assert_eq!(view.compactions.len(), 1);
        assert!(matches!(&view.compactions[0].record.status, CompactionStatus::Succeeded { summary, .. } if summary == "source and recovered answer"));
        store.close().await.unwrap();
    });
}

#[test]
fn bounded_answer_output_must_fit_canonical_json() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
        for (index, (bytes, child, accepted)) in [
            (8_000, false, true),
            (10_000, false, false),
            (32 * 1024, false, false),
            (16 * 1024, true, false),
        ]
        .into_iter()
        .enumerate()
        {
            let path = temp.path().join(format!("state-{index}"));
            let result = "\0".repeat(bytes);
            let mut responses = Vec::new();
            if child {
                responses.push(ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        editor: None,
                        objective: "Child".into(),
                        profile: None,
                    }],
                }));
            }
            let attempts = if accepted || child { 1 } else { 3 };
            for _ in 0..attempts {
                responses.push(ProviderOutcome::Answer(Answer {
                    text: result.clone(),
                }));
            }
            let provider = ScriptedProvider::new(responses);
            let requests = provider.requests.clone();
            let provider = Arc::new(provider);
            let engine =
                engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default())
                    .unwrap();
            let mut next = request(&workspace, "Question");
            if child {
                next.policy = CollaborationPolicy::Team {
                    max_active_children: 1,
                };
            }
            let outcome = run_to_end(&engine, &provider, next)
                .await
                .expect("durable output rejection, not StorageFull");
            assert_eq!(
                outcome.run.status,
                if accepted {
                    RunStatus::Finished
                } else {
                    RunStatus::Failed
                }
            );
            assert_eq!(
                outcome.run.assistant_message.as_deref(),
                accepted.then_some(result.as_str())
            );
            {
                let calls = requests.lock().unwrap();
                assert_eq!(calls.len(), if child { 2 } else { attempts });
                assert_eq!(calls[0].phase, AgentPhase::RootPlan);
                if child {
                    assert_eq!(calls[1].phase, AgentPhase::ChildWork);
                }
            }
            engine.close().await.unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
            let view = store.load_view(outcome.session_id).await.unwrap().unwrap();
            assert_eq!(view.runs[0], outcome.run);
            if !accepted {
                assert!(view.runs[0].agents.iter().any(|agent| {
                    agent.provider_calls.last().is_some_and(|call| {
                        call.disposition == ProviderCallDisposition::InvalidResponse
                    })
                }));
            }
            store.close().await.unwrap();
        }
    });
}

#[test]
fn inconsistent_cache_usage_closes_as_invalid_response() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: "accepted result".into(),
        })])
        .with_reported_usage(arany::Usage {
            input_tokens: Some(4),
            cache_read_input_tokens: Some(9),
            output_tokens: Some(8),
            ..Default::default()
        });
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut input = request(&workspace, "Question");
        input.include_paths.clear();
        let outcome = run_to_end(&engine, &provider, input)
            .await
            .expect("durable failed Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert_eq!(
            outcome.run.agents[0].provider_calls[0].disposition,
            ProviderCallDisposition::InvalidResponse
        );
        engine.close().await.expect("engine shutdown");
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let view = store
            .load_view(outcome.session_id)
            .await
            .expect("strict replay of the closed Run")
            .expect("Session");
        assert_eq!(view.runs[0].status, RunStatus::Failed);
    });
}

#[test]
fn invalid_delegation_fails_the_run_without_committing_an_answer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(
            (0..3)
                .map(|_| {
                    ProviderOutcome::Delegate(Delegate {
                        agent: None,
                        children: vec![arany::ChildAssignment {
                            editor: None,
                            objective: "unauthorized child".into(),
                            profile: None,
                        }],
                    })
                })
                .collect(),
        );
        let requests = provider.requests.clone();
        let state_path = temp.path().join("state");
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let outcome = run_to_end(&engine, &provider, request(&workspace, "Question"))
            .await
            .expect("durable failed Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(outcome.run.agents.len(), 1);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                3,
                "an invalid outcome is retried to the refusal limit without fake exhaustion"
            );
            assert_test_call(
                &requests[0],
                AgentPhase::RootPlan,
                "Question",
                &["Fact"],
                &[],
                None,
                &[],
            );
            assert_eq!(requests[0].collaboration, CollaborationPolicy::Single);
        }
        assert_eq!(outcome.run.failure, Some(arany::RunFailure::RefusalLimit));
        assert_eq!(outcome.run.agents[0].provider_calls.len(), 3);
        assert!(
            outcome.run.agents[0]
                .provider_calls
                .iter()
                .all(|call| { call.disposition == ProviderCallDisposition::InvalidResponse })
        );
        engine.close().await.expect("engine shutdown");
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert_eq!(
            store
                .load_view(outcome.session_id)
                .await
                .unwrap()
                .unwrap()
                .runs[0],
            outcome.run
        );
        store.close().await.unwrap();
    });
}

#[test]
fn team_policy_rejects_a_direct_primary_finish() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "unearned result".into()
            });
            3
        ]);
        let requests = provider.requests.clone();
        let state_path = temp.path().join("state");
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let outcome = run_to_end(&engine, &provider, run_request)
            .await
            .expect("durable failed Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(outcome.run.failure, Some(arany::RunFailure::RefusalLimit));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                3,
                "three identical phase refusals must stop the Run"
            );
            assert_test_call(
                &requests[0],
                AgentPhase::RootPlan,
                "Question",
                &["Fact"],
                &[],
                None,
                &[],
            );
            assert_eq!(
                requests[0].collaboration,
                CollaborationPolicy::Team {
                    max_active_children: 1
                }
            );
        }
        assert_eq!(outcome.run.agents[0].provider_calls.len(), 3);
        assert_eq!(
            outcome.run.agents[0].provider_calls[0].disposition,
            ProviderCallDisposition::InvalidResponse
        );
        engine.close().await.expect("engine shutdown");
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert_eq!(
            store
                .load_view(outcome.session_id)
                .await
                .unwrap()
                .unwrap()
                .runs[0],
            outcome.run
        );
        store.close().await.unwrap();
    });
}

#[tokio::test]
async fn team_planning_refusal_can_be_repaired_with_delegate() {
    let temp = tempfile::tempdir().expect("fixture");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state = temp.path().join("state");
    let finish = ProviderOutcome::Answer(Answer {
        text: "answer".into(),
    });
    let provider = Arc::new(ScriptedProvider::new(vec![
        finish.clone(),
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                objective: "Inspect".into(),
                profile: None,
                editor: None,
            }],
        }),
        finish.clone(),
        finish,
    ]));
    let engine = engine_support::open(
        StateRoot::admit(&state).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "Question");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Team {
        max_active_children: 1,
    };
    let result = run_to_end(&engine, &provider, input)
        .await
        .expect("team repaired");
    engine.close().await.expect("close engine");
    {
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[1].input.last_answer_refused,
            Some(arany::ProposalRefusal {
                operation: "answer".into(),
                cause: arany::ProposalRefusalCause::Phase
            })
        );
        assert!(
            requests[2..]
                .iter()
                .all(|request| request.input.last_answer_refused.is_none())
        );
    }
    assert!(provider.responses.lock().expect("script").is_empty());
    let store =
        Store::open_read_only(StateRoot::open_existing(&state).expect("state")).expect("store");
    let view = store
        .load_view(result.session_id)
        .await
        .expect("reopen")
        .expect("Session");
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert!(view.runs[0].tools.is_empty());
    store.close().await.expect("store close");
}

#[test]
fn one_child_team_finishes_only_after_child_result_is_committed() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "Check fact".into(),
                    profile: None,
                }],
            }),
            ProviderOutcome::Answer(Answer {
                text: "Fact is true".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Final answer".into(),
            }),
        ]);
        let recorded_requests = Arc::clone(&provider.requests);
        let state_path = temp.path().join("state");
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let outcome = run_to_end(&engine, &provider, run_request)
            .await
            .expect("successful team Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(outcome.run.agents.len(), 2);
        assert_eq!(
            outcome
                .run
                .config
                .as_ref()
                .expect("team config")
                .context_usage,
            Some(ContextUsage {
                used_bytes: ("Question".len() + include_data("notes/facts.txt", "Fact").len())
                    as u32,
                budget_bytes: 376 * 1024 - (20 * 1024 + 64) - unplanned_goal_metadata_bytes(),
                compactable_bytes: 0,
                tool_history_bytes: 0,
            })
        );
        assert_eq!(
            outcome.run.assistant_message.as_deref(),
            Some("Final answer")
        );
        {
            let requests = recorded_requests.lock().expect("requests lock");
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].phase, AgentPhase::RootPlan);
            assert_eq!(requests[1].phase, AgentPhase::ChildWork);
            assert_eq!(requests[1].input.objective, "Check fact");
            assert!(requests[1].input.history.is_empty());
            assert_eq!(requests[2].phase, AgentPhase::RootSynthesis);
            assert_eq!(requests[2].input.child_results.len(), 1);
            assert_eq!(requests[2].input.child_results[0].result, "Fact is true");
        }
        engine.close().await.expect("engine shutdown");
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(outcome.session_id)
            .await
            .expect("durable Events");
        // Primary, child wave and synthesis each journal their call; the Run settles active time once.
        assert_eq!(events.len(), 18);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
                .count(),
            3
        );
        let delegated_call = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::ProviderCallRecorded { record, .. }
                        if record.phase == AgentPhase::RootPlan
                            && record.disposition == ProviderCallDisposition::Delegated
                )
            })
            .expect("committed delegation");
        let interrupted = SessionView::replay(outcome.session_id, &events[..=delegated_call])
            .expect("valid post-call prefix")
            .expect("Session");
        assert_eq!(interrupted.runs[0].status, RunStatus::Interrupted);
        assert_eq!(interrupted.runs[0].agents[0].provider_calls.len(), 1);
        assert_eq!(
            interrupted.runs[0].agents[0].provider_calls[0].disposition,
            ProviderCallDisposition::Delegated
        );
        let child_id = outcome.run.agents[1].id;
        let primary_id = outcome.run.agents[0].id;
        let child_call = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::ProviderCallRecorded { agent_run_id, record, .. }
                        if *agent_run_id == child_id && record.phase == AgentPhase::ChildWork
                )
            })
            .expect("committed child call");
        let synthesis_call = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::ProviderCallRecorded { agent_run_id, record, .. }
                        if *agent_run_id == primary_id && record.phase == AgentPhase::RootSynthesis
                )
            })
            .expect("committed primary synthesis call");
        let child_finished = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::AgentFinished {
                        agent_run_id,
                        disposition: arany::AgentDisposition::Finished,
                        ..
                    } if *agent_run_id == child_id
                )
            })
            .expect("committed child result");
        let primary_finished = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::AgentFinished {
                        agent_run_id,
                        disposition: arany::AgentDisposition::Finished,
                        ..
                    } if *agent_run_id == primary_id
                )
            })
            .expect("committed primary result");
        assert!(delegated_call < child_call);
        assert!(child_call < child_finished);
        assert!(child_finished < synthesis_call);
        assert!(synthesis_call < primary_finished);
        let calls = events
            .iter()
            .filter_map(|event| match &event.event {
                Event::ProviderCallRecorded { record, .. } => Some(record),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            calls.iter().map(|call| call.phase).collect::<Vec<_>>(),
            [
                AgentPhase::RootPlan,
                AgentPhase::ChildWork,
                AgentPhase::RootSynthesis
            ]
        );
        let mut invalid_phase = events.clone();
        if let Event::ProviderCallRecorded { record, .. } = &mut invalid_phase[delegated_call].event
        {
            record.phase = AgentPhase::RootSynthesis;
        } else {
            panic!("expected committed primary delegation");
        }
        assert!(SessionView::replay(outcome.session_id, &invalid_phase).is_err());
        let mut duplicate_call = events.clone();
        duplicate_call[child_call].event = events[delegated_call].event.clone();
        duplicate_call[child_call].run_id = events[delegated_call].run_id;
        duplicate_call[child_call].agent_run_id = events[delegated_call].agent_run_id;
        assert!(SessionView::replay(outcome.session_id, &duplicate_call).is_err());
        let missing_calls: Vec<_> = events
            .iter()
            .filter(|event| !matches!(event.event, Event::ProviderCallRecorded { .. }))
            .cloned()
            .collect();
        assert!(SessionView::replay(outcome.session_id, &missing_calls).is_err());
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn provider_concurrency_ceiling_serializes_admitted_children() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let provider = GatedProvider {
            started,
            requests: Arc::new(Mutex::new(Vec::new())),
            fail_child: None,
            identity: gated_identity(1),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 3,
        };
        let mut run_task = tokio::spawn(async move {
            let outcome = run_to_end(&engine, &provider, run_request).await;
            let close = engine.close().await;
            (outcome, close)
        });
        let controlled = tokio::time::timeout(Duration::from_secs(5), async {
            for expected in ["A", "B", "C"] {
                let (objective, release) = started_rx.recv().await.expect("child started");
                assert_eq!(objective, expected);
                assert!(matches!(
                    started_rx.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
                release.send(()).expect("live child");
            }
        })
        .await;
        if controlled.is_err() {
            run_task.abort();
            let _ = run_task.await;
            panic!("serialized team calls exceeded the test deadline");
        }
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
        let (outcome, close) = match joined {
            Ok(result) => result.expect("Run task"),
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("serialized team Run exceeded the test deadline");
            }
        };
        let outcome = outcome.expect("successful team Run");
        close.expect("engine shutdown");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(
            outcome
                .run
                .config
                .as_ref()
                .expect("pinned config")
                .provider_concurrency,
            1
        );
    });
}

#[test]
fn same_session_operation_is_excluded_while_other_sessions_continue() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        for branch in [false, true] {
            let temp = tempfile::tempdir().expect("temporary root");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
            std::fs::write(workspace.join("notes/facts.txt"), "Facts").expect("include");
            let state_path = temp.path().join("state");
            let initial_engine_provider =
                Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
                    Answer {
                        text: "initial answer".into(),
                    },
                )]));
            let initial_engine = engine_support::open(
                StateRoot::admit(&state_path).expect("private state"),
                EngineConfig::default(),
            )
            .expect("initial engine");
            let initial = run_to_end(
                &initial_engine,
                &initial_engine_provider,
                request(&workspace, "Initial"),
            )
            .await
            .expect("initial Run");
            let held_session_id = if branch {
                initial_engine
                    .fork_session(initial.session_id, workspace.clone(), None)
                    .await
                    .expect("fork for ancestor-reservation proof")
            } else {
                initial.session_id
            };
            initial_engine.close().await.expect("initial close");

            let (started, mut started_rx) = mpsc::unbounded_channel();
            let dropped = Arc::new(AtomicUsize::new(0));
            let held_calls = Arc::new(Mutex::new(Vec::new()));
            let held_engine_provider = Arc::new(WaitingProvider {
                started,
                dropped: Arc::clone(&dropped),
                requests: Arc::clone(&held_calls),
                identity: waiting_identity(Duration::from_secs(120)),
            });
            let held_engine = engine_support::open(
                StateRoot::admit(&state_path).expect("state readmission"),
                EngineConfig::default(),
            )
            .expect("held engine");
            let cancellation = RunCancellation::new();
            let held_cancellation = cancellation.clone();
            let mut held_request = request(&workspace, "Held");
            held_request.session_id = Some(held_session_id);
            held_request.title = None;
            let mut held_task = tokio::spawn(async move {
                let outcome = run_with(
                    &held_engine,
                    &held_engine_provider,
                    held_request,
                    RunControls {
                        cancellation: held_cancellation,
                        ..Default::default()
                    },
                )
                .await;
                let close = held_engine.close().await;
                (outcome, close)
            });
            tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
                .await
                .expect("held Provider start deadline")
                .expect("held Provider start");

            let store = Store::open_read_only(
                StateRoot::open_existing(&state_path).expect("read-only state"),
            )
            .expect("read-only Store");
            let before = store
                .load_session(held_session_id)
                .await
                .expect("committed prefix");
            store.close().await.expect("read-only close");

            let contender_provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
                text: "later answer".into(),
            })]);
            let contender_calls = Arc::clone(&contender_provider.requests);
            let contender_provider = Arc::new(contender_provider);
            let contender = engine_support::open(
                StateRoot::admit(&state_path).expect("contender state"),
                EngineConfig::default(),
            )
            .expect("contender engine");
            let mut blocked_request = request(&workspace, "Blocked");
            blocked_request.session_id = Some(held_session_id);
            blocked_request.title = None;
            blocked_request.include_paths = vec!["missing-include.txt".into()];
            let blocked = tokio::time::timeout(
                Duration::from_secs(5),
                run_to_end(&contender, &contender_provider, blocked_request),
            )
            .await
            .expect("contender deadline");
            assert!(matches!(blocked, Err(EngineError::SessionBusy)));
            assert!(matches!(
                contender
                    .fork_session(held_session_id, workspace.clone(), None)
                    .await,
                Err(EngineError::SessionBusy)
            ));
            assert!(matches!(
                contender
                    .compact_session(
                        &AdmittedProviders::single(contender_provider.clone()),
                        held_session_id,
                        workspace.clone()
                    )
                    .await,
                Err(EngineError::SessionBusy)
            ));
            assert!(matches!(
                rename_session(
                    StateRoot::admit(&state_path).expect("rename state"),
                    workspace.clone(),
                    held_session_id,
                    "Busy rename".into(),
                )
                .await,
                Err(EngineError::SessionBusy)
            ));
            assert!(matches!(
                arany::set_session_defaults(
                    StateRoot::admit(&state_path).expect("defaults state"),
                    workspace.clone(),
                    held_session_id,
                    arany::SessionDefaults {
                        provider: Some("openai".into()),
                        model: Some("gpt-5.4".into()),
                        effort: Some(Effort::High),
                        account_id: None,
                        policy: CollaborationPolicy::Single,
                    },
                )
                .await,
                Err(EngineError::SessionBusy)
            ));
            if branch {
                let mut parent_request = request(&workspace, "Parent growth");
                parent_request.session_id = Some(initial.session_id);
                parent_request.title = None;
                assert!(matches!(
                    run_to_end(&contender, &contender_provider, parent_request).await,
                    Err(EngineError::SessionBusy)
                ));
                assert!(matches!(
                    rename_session(
                        StateRoot::admit(&state_path).unwrap(),
                        workspace.clone(),
                        initial.session_id,
                        "Parent rename".into()
                    )
                    .await,
                    Err(EngineError::SessionBusy)
                ));
            }
            assert!(contender_calls.lock().expect("contender calls").is_empty());
            let store = Store::open_read_only(
                StateRoot::open_existing(&state_path).expect("read-only state"),
            )
            .expect("read-only Store");
            let after = store
                .load_session(held_session_id)
                .await
                .expect("unchanged prefix");
            assert_eq!(after, before);
            store.close().await.expect("read-only close");

            let independent_provider =
                Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
                    Answer {
                        text: "independent answer".into(),
                    },
                )]));
            let independent = engine_support::open(
                StateRoot::admit(&state_path).expect("independent state"),
                EngineConfig::default(),
            )
            .expect("independent engine");
            let independent_outcome = run_to_end(
                &independent,
                &independent_provider,
                request(&workspace, "Independent"),
            )
            .await
            .expect("independent Session may progress");
            assert_ne!(independent_outcome.session_id, held_session_id);
            independent.close().await.expect("independent close");

            assert!(cancellation.cancel());
            let (held_outcome, held_close) =
                tokio::time::timeout(Duration::from_secs(5), &mut held_task)
                    .await
                    .expect("held cancellation deadline")
                    .expect("held task");
            assert_eq!(
                held_outcome.expect("cancelled Run").run.status,
                RunStatus::Cancelled
            );
            held_close.expect("held close");
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            {
                let calls = held_calls.lock().expect("held calls");
                assert_eq!(calls.len(), 1, "exactly one held call");
                assert_test_call(
                    &calls[0],
                    AgentPhase::RootPlan,
                    "Held",
                    &["Facts"],
                    &[arany::HistoryTurn {
                        user: "Initial".into(),
                        assistant: "initial answer".into(),
                    }],
                    None,
                    &[],
                );
            }

            let mut retry = request(&workspace, "After release");
            retry.session_id = Some(held_session_id);
            retry.title = None;
            let resumed = run_to_end(&contender, &contender_provider, retry)
                .await
                .expect("Run after release");
            assert_eq!(resumed.run.status, RunStatus::Finished);
            assert_eq!(contender_calls.lock().expect("contender calls").len(), 1);
            contender.close().await.expect("contender close");

            let store =
                Store::open_read_only(StateRoot::open_existing(&state_path).expect("final state"))
                    .expect("final Store");
            let view = store
                .load_view(held_session_id)
                .await
                .expect("final replay")
                .expect("Session exists");
            assert_eq!(view.runs.len(), 3);
            assert_eq!(view.runs[0].status, RunStatus::Finished);
            assert_eq!(view.runs[1].status, RunStatus::Cancelled);
            assert_eq!(view.runs[2].status, RunStatus::Finished);
            store.close().await.expect("final close");
        }
    });
}

#[test]
fn cancellation_before_admission_creates_no_run_or_provider_call() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = WaitingProvider {
            started,
            dropped: Arc::new(AtomicUsize::new(0)),
            requests: Arc::clone(&requests),
            identity: waiting_identity(Duration::from_secs(120)),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let cancellation = RunCancellation::new();
        assert!(cancellation.cancel());
        assert!(matches!(
            run_with(
                &engine,
                &provider,
                request(&workspace, "Question"),
                RunControls {
                    cancellation,
                    ..Default::default()
                }
            )
            .await,
            Err(EngineError::CancelledBeforeStart)
        ));
        assert!(matches!(
            started_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        engine.close().await.expect("engine shutdown");
        assert!(
            requests.lock().expect("requests").is_empty(),
            "no admission call"
        );
        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("state inspection");
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("event count");
        assert_eq!(count, 0);
    });
}

#[test]
fn team_cancellation_drains_active_and_queued_children() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        for capacity in [1_u8, 3] {
            let temp = tempfile::tempdir().expect("temporary root");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
            std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
            let state_path = temp.path().join("state");
            let (started, mut started_rx) = mpsc::unbounded_channel();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let provider = GatedProvider {
                started,
                requests: Arc::clone(&requests),
                fail_child: None,
                identity: gated_identity(capacity),
            };
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&state_path).expect("private state"),
                EngineConfig::default(),
            )
            .expect("engine");
            let cancellation = RunCancellation::new();
            let run_cancellation = cancellation.clone();
            let mut run_request = request(&workspace, "Question");
            run_request.policy = CollaborationPolicy::Team {
                max_active_children: 3,
            };
            let mut run_task = tokio::spawn(async move {
                let outcome = run_with(
                    &engine,
                    &provider,
                    run_request,
                    RunControls {
                        cancellation: run_cancellation,
                        ..Default::default()
                    },
                )
                .await;
                let close = engine.close().await;
                (outcome, close)
            });
            let started_calls = tokio::time::timeout(Duration::from_secs(5), async {
                let mut gates = Vec::new();
                for _ in 0..capacity {
                    gates.push(started_rx.recv().await.expect("child started"));
                }
                gates
            })
            .await;
            let gates = match started_calls {
                Ok(gates) => gates,
                Err(_) => {
                    run_task.abort();
                    let _ = run_task.await;
                    panic!(
                        "team calls did not start within the test deadline: capacity {capacity}"
                    );
                }
            };
            assert!(cancellation.cancel());
            let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
            let (outcome, close) = match joined {
                Ok(result) => result.expect("Run task"),
                Err(_) => {
                    run_task.abort();
                    let _ = run_task.await;
                    panic!("team cancellation exceeded the test deadline: capacity {capacity}");
                }
            };
            let outcome = outcome.expect("durable cancelled team Run");
            close.expect("engine shutdown");
            assert_eq!(
                outcome.run.status,
                RunStatus::Cancelled,
                "capacity {capacity}"
            );
            assert!(
                outcome
                    .run
                    .agents
                    .iter()
                    .all(|agent| agent.status == AgentStatus::Cancelled),
                "capacity {capacity}"
            );
            assert!(outcome.run.assistant_message.is_none());
            for (_, release) in gates {
                assert!(
                    release.send(()).is_err(),
                    "call still live after cancellation"
                );
            }
            assert!(matches!(
                started_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
            ));
            {
                let requests = requests.lock().expect("requests lock");
                assert_eq!(
                    requests
                        .iter()
                        .filter(|request| request.phase == AgentPhase::ChildWork)
                        .count(),
                    usize::from(capacity)
                );
                assert!(
                    requests
                        .iter()
                        .all(|request| request.phase != AgentPhase::RootSynthesis)
                );
            }
            let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
                .expect("reopened store");
            let events = store
                .load_session(outcome.session_id)
                .await
                .expect("durable Events");
            let replay = SessionView::replay(outcome.session_id, &events)
                .expect("valid replay")
                .expect("Session");
            assert_eq!(replay.runs[0].status, RunStatus::Cancelled);
            assert_eq!(
                events
                    .iter()
                    .filter(|envelope| matches!(envelope.event, Event::AgentFinished { .. }))
                    .count(),
                4
            );
            assert!(
                !events
                    .iter()
                    .any(|envelope| matches!(envelope.event, Event::MessageCommitted { .. }))
            );
            store.close().await.expect("store shutdown");
        }
    });
}

#[test]
fn provider_declared_output_cap_reaches_the_primary_and_caps_children() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect".into(),
                    profile: None,
                }],
            }),
            ProviderOutcome::Answer(Answer {
                text: "Child answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Answer".into(),
            }),
        ])
        .with_max_output_tokens(16_384);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut team = request(&workspace, "Question");
        team.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let outcome = run_to_end(&engine, &provider, team)
            .await
            .expect("team Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(
            outcome
                .run
                .config
                .as_ref()
                .expect("Run config")
                .output_token_cap,
            16_384
        );
        let caps: Vec<_> = requests
            .lock()
            .expect("requests lock")
            .iter()
            .map(|call| (call.phase, call.max_output_tokens))
            .collect();
        assert_eq!(
            caps,
            [
                (AgentPhase::RootPlan, 16_384),
                (AgentPhase::ChildWork, 4096),
                (AgentPhase::RootSynthesis, 16_384)
            ]
        );
        engine.close().await.expect("engine shutdown");

        let oversized_provider =
            Arc::new(ScriptedProvider::new(Vec::new()).with_max_output_tokens(16_385));
        let oversized = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("engine");
        assert!(matches!(
            run_to_end(
                &oversized,
                &oversized_provider,
                request(&workspace, "Question")
            )
            .await,
            Err(EngineError::InvalidRequest)
        ));
        oversized.close().await.expect("engine shutdown");

        // A local-only ChatGPT route admits its cap and refuses anything above the 4,096 bound.
        let chatgpt = |cap: u32, responses: Vec<ProviderOutcome>| {
            ScriptedProvider::new(responses)
                .with_chatgpt_account(Uuid::now_v7(), [1; 32])
                .with_max_output_tokens(cap)
        };
        let local_over_provider = Arc::new(chatgpt(4097, Vec::new()));
        let local_over = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("engine");
        assert!(matches!(
            run_to_end(
                &local_over,
                &local_over_provider,
                request(&workspace, "Question")
            )
            .await,
            Err(EngineError::InvalidRequest)
        ));
        local_over.close().await.expect("engine shutdown");

        let local_cap_provider = Arc::new(chatgpt(
            4096,
            vec![ProviderOutcome::Answer(Answer { text: "ok".into() })],
        ));
        let local_cap = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("engine");
        assert_eq!(
            run_to_end(
                &local_cap,
                &local_cap_provider,
                request(&workspace, "Question")
            )
            .await
            .expect("a 4,096 local-only Run is admitted")
            .run
            .status,
            RunStatus::Finished
        );
        local_cap.close().await.expect("engine shutdown");
    });
}

#[test]
fn provider_call_timeout_fails_without_a_final_message() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = WaitingProvider {
            started,
            dropped: Arc::clone(&dropped),
            requests: Arc::clone(&requests),
            identity: waiting_identity(Duration::from_secs(300)),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_task = tokio::spawn(async move {
            let outcome = run_to_end(&engine, &provider, request(&workspace, "Question")).await;
            let close = engine.close().await;
            (outcome, close)
        });
        let started = tokio::time::timeout(Duration::from_secs(5), started_rx.recv()).await;
        assert_eq!(started.expect("Provider start deadline"), Some(()));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(121)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "a call within its Provider-declared deadline stays in flight"
        );
        assert!(!run_task.is_finished());
        tokio::time::advance(Duration::from_secs(180)).await;
        tokio::time::resume();
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
        let (outcome, close) = match joined {
            Ok(result) => result.expect("Run task"),
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("timed-out Provider call did not terminate");
            }
        };
        let outcome = outcome.expect("durable failed Run");
        close.expect("engine shutdown");
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        {
            let calls = requests.lock().expect("requests");
            assert_eq!(calls.len(), 1, "exactly one timed-out call");
            assert_test_call(
                &calls[0],
                AgentPhase::RootPlan,
                "Question",
                &["Fact"],
                &[],
                None,
                &[],
            );
            assert_eq!(calls[0].run_id, outcome.run.id);
            assert_eq!(calls[0].agent_run_id, outcome.run.agents[0].id);
        }
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
    });
}

#[test]
fn provider_declared_call_deadline_times_out_and_settles_its_active_reservation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let provider = WaitingProvider {
            started,
            dropped: Arc::clone(&dropped),
            requests: Arc::new(Mutex::new(Vec::new())),
            identity: waiting_identity(Duration::from_millis(50)),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_task = tokio::spawn(async move {
            let outcome = run_to_end(&engine, &provider, request(&workspace, "Question")).await;
            let close = engine.close().await;
            (outcome, close)
        });
        let started = tokio::time::timeout(Duration::from_secs(5), started_rx.recv()).await;
        assert_eq!(started.expect("Provider start deadline"), Some(()));
        tokio::time::pause();
        tokio::time::advance(Duration::from_millis(60)).await;
        tokio::time::resume();
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
        let (outcome, close) = joined.expect("timed-out Run terminates").expect("Run task");
        let outcome = outcome.expect("durable failed Run");
        close.expect("engine shutdown");
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.run.status, RunStatus::Failed);

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(outcome.session_id)
            .await
            .expect("durable Events");
        let view = SessionView::replay(outcome.session_id, &events)
            .expect("replayed Session")
            .expect("Session view");
        let goal = view.goal(outcome.goal_id).expect("Goal");
        assert_eq!(
            goal.active_deadline, None,
            "the Run settled its active time"
        );
        assert!(
            goal.active_ms <= 1_000,
            "the call is charged its declared 50 ms, not the 120 s default"
        );
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn compaction_timeout_drops_the_call_and_records_failure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let compact_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = WaitingCompactionProvider {
            started,
            dropped: Arc::clone(&dropped),
            requests: Arc::clone(&requests),
            compact_requests: Arc::clone(&compact_requests),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let session_id = run_to_end(&engine, &provider, request(&workspace, "Seed question"))
            .await
            .expect("seed Run")
            .session_id;
        let mut compact_task = tokio::spawn(async move {
            let outcome = engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    session_id,
                    workspace,
                )
                .await;
            let close = engine.close().await;
            (outcome, close)
        });
        let started = tokio::time::timeout(Duration::from_secs(5), started_rx.recv()).await;
        assert_eq!(started.expect("Provider start deadline"), Some(()));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(121)).await;
        tokio::time::resume();
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut compact_task).await;
        let (outcome, close) = match joined {
            Ok(result) => result.expect("compaction task"),
            Err(_) => {
                compact_task.abort();
                let _ = compact_task.await;
                panic!("timed-out compaction call did not terminate");
            }
        };
        let record = outcome.expect("durable failed compaction");
        close.expect("engine shutdown");
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        {
            let calls = requests.lock().expect("requests");
            assert_eq!(calls.len(), 1, "exactly one seed call");
            assert_test_call(
                &calls[0],
                AgentPhase::RootPlan,
                "Seed question",
                &["Fact"],
                &[],
                None,
                &[],
            );
            let compacts = compact_requests.lock().expect("compaction requests");
            assert_eq!(compacts.len(), 1, "exactly one timed-out compaction");
            let compact = &compacts[0];
            assert_eq!(compact.session_id, session_id);
            assert_eq!(compact.covered_run_id, calls[0].run_id);
            assert!(compact.model == "test-model", "compaction model");
            assert!(compact.previous_summary.is_none(), "no previous summary");
            assert!(
                compact.items
                    == [CompactionItem::Completed(arany::HistoryTurn {
                        user: "Seed question".into(),
                        assistant: "Seed answer".into(),
                    })],
                "exact compaction source"
            );
            assert_eq!(compact.max_output_tokens, 1024);
        }
        assert!(matches!(
            record.status,
            CompactionStatus::Failed {
                reason: CompactionFailure::TimedOut
            }
        ));
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let view = store
            .load_view(session_id)
            .await
            .expect("durable replay")
            .expect("Session exists");
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.compactions.len(), 1);
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn eight_children_use_the_same_ordered_loop() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let objectives: Vec<_> = (1..=8).map(|index| format!("Child {index}")).collect();
        let mut responses = vec![ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: objectives
                .iter()
                .map(|objective| arany::ChildAssignment {
                    editor: None,
                    objective: objective.clone(),
                    profile: None,
                })
                .collect(),
        })];
        responses.extend((0..8).map(|_| {
            ProviderOutcome::Answer(Answer {
                text: "result".into(),
            })
        }));
        responses.push(ProviderOutcome::Answer(Answer {
            text: "final".into(),
        }));
        let provider = ScriptedProvider::new(responses);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 8,
        };
        let outcome = run_to_end(&engine, &provider, run_request)
            .await
            .expect("eight-child Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(outcome.run.agents.len(), 9);
        {
            let requests = requests.lock().expect("requests lock");
            assert_eq!(requests.len(), 10);
            let synthesis = requests.last().expect("synthesis call");
            assert_eq!(synthesis.phase, AgentPhase::RootSynthesis);
            assert_eq!(
                synthesis
                    .input
                    .child_results
                    .iter()
                    .map(|child| child.objective.as_str())
                    .collect::<Vec<_>>(),
                objectives.iter().map(String::as_str).collect::<Vec<_>>()
            );
        }
        engine.close().await.expect("engine shutdown");
    });
}

#[test]
fn collaboration_capacity_corpus_rejects_invalid_delegation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let cases = [
            (
                CollaborationPolicy::Auto {
                    max_active_children: 0,
                },
                1,
            ),
            (
                CollaborationPolicy::Team {
                    max_active_children: 1,
                },
                0,
            ),
            (
                CollaborationPolicy::Team {
                    max_active_children: 1,
                },
                2,
            ),
            (
                CollaborationPolicy::Team {
                    max_active_children: 8,
                },
                9,
            ),
        ];
        for (index, (policy, count)) in cases.into_iter().enumerate() {
            let children: Vec<_> = (0..count)
                .map(|child| arany::ChildAssignment {
                    editor: None,
                    objective: format!("Child {child}"),
                    profile: None,
                })
                .collect();
            let provider = ScriptedProvider::new(
                (0..3)
                    .map(|_| {
                        ProviderOutcome::Delegate(Delegate {
                            children: children.clone(),
                            agent: None,
                        })
                    })
                    .collect(),
            );
            let requests = provider.requests.clone();
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&temp.path().join(format!("state-{index}")))
                    .expect("private state"),
                EngineConfig::default(),
            )
            .expect("engine");
            let mut run_request = request(&workspace, "Question");
            run_request.policy = policy;
            let outcome = run_to_end(&engine, &provider, run_request)
                .await
                .expect("durable failed Run");
            assert_eq!(outcome.run.status, RunStatus::Failed, "case {index}");
            assert_eq!(outcome.run.agents.len(), 1, "case {index}");
            assert!(outcome.run.assistant_message.is_none(), "case {index}");
            {
                let requests = requests.lock().unwrap();
                assert_eq!(
                    requests.len(),
                    3,
                    "case {index}: no fake exhaustion or child call"
                );
                assert_test_call(
                    &requests[0],
                    AgentPhase::RootPlan,
                    "Question",
                    &["Fact"],
                    &[],
                    None,
                    &[],
                );
                assert_eq!(requests[0].collaboration, policy);
            }
            assert_eq!(outcome.run.failure, Some(arany::RunFailure::RefusalLimit));
            assert_eq!(outcome.run.agents[0].provider_calls.len(), 3);
            assert!(
                outcome.run.agents[0]
                    .provider_calls
                    .iter()
                    .all(|call| call.disposition == ProviderCallDisposition::InvalidResponse)
            );
            engine.close().await.expect("engine shutdown");
        }
        for (index, policy) in [
            CollaborationPolicy::Team {
                max_active_children: 0,
            },
            CollaborationPolicy::Auto {
                max_active_children: 9,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let provider = ScriptedProvider::new(vec![]);
            let requests = Arc::clone(&provider.requests);
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&temp.path().join(format!("invalid-{index}")))
                    .expect("private state"),
                EngineConfig::default(),
            )
            .expect("engine");
            let mut run_request = request(&workspace, "Question");
            run_request.policy = policy;
            assert!(
                matches!(
                    run_to_end(&engine, &provider, run_request).await,
                    Err(EngineError::InvalidRequest)
                ),
                "invalid policy case {index}"
            );
            assert!(requests.lock().expect("requests lock").is_empty());
            engine.close().await.expect("engine shutdown");
        }
    });
}

#[test]
fn delegation_during_synthesis_is_invalid_and_fails_the_run() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(
            vec![
                ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        editor: None,
                        objective: "Check fact".into(),
                        profile: None,
                    }],
                }),
                ProviderOutcome::Answer(Answer {
                    text: "Fact is true".into(),
                }),
            ]
            .into_iter()
            .chain((0..3).map(|_| {
                ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        editor: None,
                        objective: "Second round".into(),
                        profile: None,
                    }],
                })
            }))
            .collect::<Vec<_>>(),
        );
        let requests = provider.requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let outcome = run_to_end(&engine, &provider, run_request)
            .await
            .expect("durable failed Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(outcome.run.agents.len(), 2);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 5, "no second delegation round starts");
            assert!(
                requests[2..]
                    .iter()
                    .all(|r| r.phase == AgentPhase::RootSynthesis)
            );
        }
        let calls = &outcome.run.agents[0].provider_calls;
        assert_eq!(calls.len(), 4);
        assert!(calls[1..].iter().all(|call| {
            call.disposition == ProviderCallDisposition::InvalidResponse
                && call.phase == AgentPhase::RootSynthesis
        }));
        assert_eq!(outcome.run.failure, Some(arany::RunFailure::RefusalLimit));
        engine.close().await.expect("engine shutdown");
    });
}

#[cfg(unix)]
#[test]
fn workspace_paths_fail_closed_before_a_run_is_accepted() {
    use std::os::unix::fs::symlink;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(temp.path().join("outside"), "secret").expect("outside file");
        symlink(temp.path().join("outside"), workspace.join("AGENTS.md"))
            .expect("symlinked instruction");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: "Unexpected result".into(),
        })]);
        let requests = Arc::clone(&provider.requests);
        let state_path = temp.path().join("state");
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        for (objective, images) in [
            (String::new(), Vec::new()),
            (String::new(), vec![image_fixture(69); 5]),
            (String::new(), vec![image_fixture(64 * 1024); 4]),
            ("x".repeat(64 * 1024 + 1), Vec::new()),
            (
                "x".repeat(64 * 1024 + 1),
                vec![image_fixture(arany::MAX_IMAGE_BYTES)],
            ),
        ] {
            let mut invalid = request(&workspace, &objective);
            invalid.images = images;
            assert!(
                matches!(
                    run_to_end(&engine, &provider, invalid).await,
                    Err(EngineError::InvalidRequest)
                ),
                "invalid objective/images reject before hostile Workspace"
            );
        }
        assert!(matches!(
            run_to_end(&engine, &provider, request(&workspace, "Question")).await,
            Err(EngineError::WorkspaceUnavailable)
        ));
        std::fs::remove_file(workspace.join("AGENTS.md")).expect("remove test symlink");
        let mut traversal = request(&workspace, "Question");
        traversal.include_paths = vec!["../outside".into()];
        assert!(matches!(
            run_to_end(&engine, &provider, traversal).await,
            Err(EngineError::InvalidWorkspacePath)
        ));
        assert!(
            requests.lock().expect("requests lock").is_empty(),
            "rejected admission makes no Provider call"
        );
        engine.close().await.expect("engine shutdown");

        let forged_custom_provider =
            Arc::new(ScriptedProvider::new(vec![]).with_profile("custom:local", "test-model"));
        let forged_custom = engine_support::open(
            StateRoot::open_existing(&state_path).expect("existing state"),
            EngineConfig::default(),
        )
        .expect("unproven Provider opened");
        let mut invalid_workspace = request(&workspace, "Question");
        invalid_workspace.include_paths = vec!["../outside".into()];
        assert!(matches!(
            run_to_end(&forged_custom, &forged_custom_provider, invalid_workspace).await,
            Err(EngineError::InvalidRequest)
        ));
        forged_custom.close().await.expect("engine shutdown");

        let forged_chatgpt_provider =
            Arc::new(ScriptedProvider::new(vec![]).with_profile("chatgpt", "test-model"));
        let forged_chatgpt = engine_support::open(
            StateRoot::open_existing(&state_path).expect("existing state"),
            EngineConfig::default(),
        )
        .expect("unproven Provider opened");
        let mut invalid_workspace = request(&workspace, "Question");
        invalid_workspace.include_paths = vec!["../outside".into()];
        assert!(matches!(
            run_to_end(&forged_chatgpt, &forged_chatgpt_provider, invalid_workspace).await,
            Err(EngineError::InvalidRequest)
        ));
        forged_chatgpt.close().await.expect("engine shutdown");

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("state inspection");
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("event count");
        assert_eq!(count, 0);
    });
}

#[cfg(unix)]
#[test]
fn workspace_limits_and_state_overlap_fail_before_provider_disclosure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(vec![]);
        let recorded_requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");

        std::fs::write(workspace.join("AGENTS.md"), vec![b'a'; 64 * 1024 + 1])
            .expect("oversized instruction");
        assert!(matches!(
            run_to_end(&engine, &provider, request(&workspace, "Question")).await,
            Err(EngineError::InputTooLarge)
        ));
        std::fs::remove_file(workspace.join("AGENTS.md")).expect("clear test instruction");

        std::fs::write(
            workspace.join("notes/facts.txt"),
            vec![b'f'; 128 * 1024 + 1],
        )
        .expect("oversized include");
        assert!(matches!(
            run_to_end(&engine, &provider, request(&workspace, "Question")).await,
            Err(EngineError::InputTooLarge)
        ));
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("restore include");

        let mut too_many = request(&workspace, "Question");
        too_many.include_paths = vec!["notes/facts.txt".into(); 17];
        assert!(matches!(
            run_to_end(&engine, &provider, too_many).await,
            Err(EngineError::TooManyIncludes)
        ));

        std::fs::write(workspace.join("AGENTS.md"), vec![b'a'; 64 * 1024])
            .expect("maximum instruction");
        std::fs::write(workspace.join("notes/a.txt"), vec![b'a'; 128 * 1024])
            .expect("first maximum include");
        std::fs::write(workspace.join("notes/b.txt"), vec![b'b'; 128 * 1024])
            .expect("second maximum include");
        let mut too_much_context = request(&workspace, "Question");
        too_much_context.include_paths = vec!["notes/a.txt".into(), "notes/b.txt".into()];
        too_much_context.policy = CollaborationPolicy::Auto {
            max_active_children: 3,
        };
        assert!(matches!(
            run_to_end(&engine, &provider, too_much_context).await,
            Err(EngineError::ContextTooLarge)
        ));
        assert!(recorded_requests.lock().expect("requests lock").is_empty());
        engine.close().await.expect("engine shutdown");

        let provider = ScriptedProvider::new(vec![]);
        let state_inside_workspace = workspace.join("state");
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_inside_workspace).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        assert!(matches!(
            run_to_end(&engine, &provider, request(&workspace, "Question")).await,
            Err(EngineError::StateOverlap)
        ));
        engine.close().await.expect("engine shutdown");
    });
}

#[tokio::test]
async fn provisional_text_is_reconciled_before_an_answer_can_commit() {
    for valid in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let result = if valid {
            "preview complete"
        } else {
            "different answer"
        };
        let provider = PreviewProvider {
            text: arany::PreviewText {
                answer: "preview".into(),
                ..Default::default()
            },
            completion: Some(ProviderOutcome::Answer(Answer {
                text: result.into(),
            })),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).unwrap(),
            EngineConfig::default(),
        )
        .unwrap();
        let mut objective = request(&workspace, "question");
        objective.include_paths.clear();
        let outcome = run_with(
            &engine,
            &provider,
            objective,
            RunControls {
                cancellation: RunCancellation::new(),
                hints: Some(RunHints::with_previews()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.run.status,
            if valid {
                RunStatus::Finished
            } else {
                RunStatus::Failed
            }
        );
        assert_eq!(
            outcome.run.assistant_message.as_deref(),
            valid.then_some(result)
        );
        if !valid {
            assert!(outcome.run.display.is_empty());
        }
        engine.close().await.unwrap();
        if valid {
            let store = Store::open_read_only(
                StateRoot::open_existing(&temp.path().join("state")).unwrap(),
            )
            .unwrap();
            let mut events = store.load_session(outcome.session_id).await.unwrap();
            assert!(SessionView::replay(outcome.session_id, &events).is_ok());
            let candidate = events
                .iter_mut()
                .find_map(|event| match &mut event.event {
                    Event::AssistantDisplayRecorded { chunk, .. }
                        if chunk.category == arany::DisplayCategory::Answer =>
                    {
                        Some(chunk)
                    }
                    _ => None,
                })
                .unwrap();
            candidate.text = "changed".into();
            assert!(
                SessionView::replay(outcome.session_id, &events).is_err(),
                "replay must reject a contradictory accepted result"
            );
            store.close().await.unwrap();
        }
    }
}

#[cfg(unix)]
#[test]
fn preview_checkpoint_survives_abrupt_process_death() {
    const CASE: &str = "ARANY_TEST_PREVIEW_CRASH_ROOT";
    const TEST: &str = "preview_checkpoint_survives_abrupt_process_death";
    if let Some(root) = std::env::var_os(CASE) {
        let root = std::path::PathBuf::from(root);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let provider = PreviewProvider {
                text: arany::PreviewText {
                    answer: format!("DURABLE_CRASH_CANARY:{}", "x".repeat(2300)),
                    ..Default::default()
                },
                completion: None,
            };
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&root.join("state")).unwrap(),
                EngineConfig::default(),
            )
            .unwrap();
            let mut objective = request(&root.join("workspace"), "interrupted question");
            objective.include_paths.clear();
            let mut events = engine.subscribe();
            let observed = RunHints::with_previews();
            let observer =
                Store::open_read_only(StateRoot::open_existing(&root.join("state")).unwrap())
                    .unwrap();
            let _task = tokio::spawn(async move {
                run_with(
                    &engine,
                    &provider,
                    objective,
                    RunControls {
                        cancellation: RunCancellation::new(),
                        hints: Some(observed),
                        ..Default::default()
                    },
                )
                .await
            });
            loop {
                let event = next_committed(&mut events, "committed checkpoint").await;
                if matches!(event.event, Event::AssistantDisplayRecorded { .. }) {
                    let id = observer
                        .session_for_run(event.run_id.unwrap())
                        .await
                        .unwrap()
                        .unwrap();
                    let view = observer.load_view(id).await.unwrap().unwrap();
                    assert_eq!(view.runs[0].display[0].text.len(), 2048);
                    std::fs::write(root.join("checkpoint-ready"), id.to_string()).unwrap();
                    std::future::pending::<()>().await;
                }
            }
        });
        return;
    }
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .env(CASE, temp.path())
        .env(
            "TMPDIR",
            std::fs::canonicalize(std::env::temp_dir()).unwrap(),
        )
        .args(["--exact", TEST, "--nocapture", "--test-threads", "1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(debug_assertions)]
    command.env(
        "ARANY_TEST_ACCOUNT_ROOT",
        arany::test_support::isolate_account_root(),
    );
    let mut child = Child(command.spawn().unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let id = loop {
        if let Ok(text) = std::fs::read_to_string(temp.path().join("checkpoint-ready"))
            && let Ok(id) = text.parse::<SessionId>()
        {
            break id;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "durable checkpoint gate"
        );
        if let Some(status) = child.0.try_wait().unwrap() {
            use std::io::Read;

            let mut stderr = Vec::new();
            child
                .0
                .stderr
                .take()
                .unwrap()
                .take(4096)
                .read_to_end(&mut stderr)
                .unwrap();
            panic!(
                "child must reach its committed checkpoint: exit={status}; stderr={:?}",
                String::from_utf8_lossy(&stderr)
            );
        }
        std::thread::yield_now();
    };
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store =
            Store::open_read_only(StateRoot::open_existing(&temp.path().join("state")).unwrap())
                .unwrap();
        let view = store.load_view(id).await.unwrap().unwrap();
        let run = &view.runs[0];
        assert_eq!(run.status, RunStatus::Interrupted);
        assert!(run.assistant_message.is_none());
        assert_eq!(run.display.len(), 1);
        assert_eq!(run.display[0].text.len(), 2048);
        assert!(run.display[0].text.starts_with("DURABLE_CRASH_CANARY:"));
        let goal_id = view.goals[0].id;
        store.close().await.unwrap();
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: "recovered".into(),
        })]);
        let requests = provider.requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::open_existing(&temp.path().join("state")).unwrap(),
            EngineConfig::default(),
        )
        .unwrap();
        let mut next = request(&workspace, "continue");
        next.include_paths.clear();
        next.session_id = Some(id);
        next.title = None;
        next.goal = arany::GoalRequest::resume(goal_id, true);
        assert_eq!(
            run_to_end(&engine, &provider, next)
                .await
                .unwrap()
                .run
                .status,
            RunStatus::Finished
        );
        assert!(!format!("{:?}", requests.lock().unwrap()).contains("DURABLE_CRASH_CANARY"));
        engine.close().await.unwrap();
    });
}

#[tokio::test]
async fn display_limits_preserve_a_maximal_answer_and_record_omitted_commentary() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let answer = format!("{}a", "ࠀ".repeat(10922));
    let mut commentary = vec!["ࠀ".repeat(10912)];
    commentary.extend((0..31).map(|_| "x".into()));
    let provider = PreviewProvider {
        text: arany::PreviewText {
            answer: answer.clone(),
            commentary,
            commentary_limited: false,
            tentative: false,
        },
        completion: Some(ProviderOutcome::Answer(Answer {
            text: answer.clone(),
        })),
    };
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&temp.path().join("state")).unwrap(),
        EngineConfig::default(),
    )
    .unwrap();
    let mut objective = request(&workspace, "question");
    objective.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        objective,
        RunControls {
            cancellation: RunCancellation::new(),
            hints: Some(RunHints::with_previews()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert_eq!(
        outcome.run.assistant_message.as_deref(),
        Some(answer.as_str())
    );
    assert!(outcome.run.display.len() <= 64);
    assert_eq!(
        outcome
            .run
            .display
            .iter()
            .filter(|chunk| chunk.category == arany::DisplayCategory::Limit)
            .count(),
        1
    );
    engine.close().await.unwrap();
}

#[test]
fn maximal_escaped_objective_with_the_largest_image_fits_its_events() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let provider =
            ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "ok".into() })]);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut next = request(&workspace, &"\0".repeat(64 * 1024));
        next.images = vec![image_fixture(arany::MAX_IMAGE_BYTES)];
        let outcome = run_to_end(&engine, &provider, next)
            .await
            .expect("maximal objective and image Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        let session_id = outcome.session_id;
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(session_id)
            .await
            .expect("durable Events");
        let accepted = events
            .iter()
            .filter(|envelope| matches!(envelope.event, Event::MessageAccepted { .. }))
            .count();
        let started = events
            .iter()
            .filter(|envelope| matches!(envelope.event, Event::GoalStarted { .. }))
            .count();
        assert_eq!(
            (accepted, started),
            (1, 1),
            "the maximal Run is durably admitted and replayable"
        );
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn escape_heavy_session_over_the_compaction_bound_fails_closed_and_keeps_its_journal() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let provider =
            ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "ok".into() })]);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        // Each control character serializes to six bytes, so this objective exceeds the compaction bound.
        let session_id = run_to_end(
            &engine,
            &provider,
            request(&workspace, &"\u{1}".repeat(64 * 1024)),
        )
        .await
        .expect("bounded Run")
        .session_id;
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let before = store
            .load_session(session_id)
            .await
            .expect("durable Events")
            .len();
        store.close().await.expect("store shutdown");

        let provider = ScriptedProvider::new(Vec::new());
        let compact_requests = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("engine");
        assert!(matches!(
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    session_id,
                    workspace.clone()
                )
                .await,
            Err(EngineError::CompactionInputTooLarge)
        ));
        assert!(compact_requests.lock().expect("request lock").is_empty());
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let after = store
            .load_session(session_id)
            .await
            .expect("durable Events")
            .len();
        assert_eq!(after, before, "a refused compaction appends no Event");
        store.close().await.expect("store shutdown");
    });
}

#[tokio::test]
async fn one_engine_refuses_a_concurrent_submission_and_takes_a_provider_per_submission() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
    let state_path = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("private state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let answer = |text: &str| ProviderOutcome::Answer(Answer { text: text.into() });
    let first =
        Arc::new(ScriptedProvider::new(vec![answer("first")]).with_profile("scripted", "model-a"));
    let second =
        Arc::new(ScriptedProvider::new(vec![answer("second")]).with_profile("scripted", "model-b"));

    drop(
        engine
            .submit(
                AdmittedProviders::single(first.clone()),
                request(&workspace, "Abandoned"),
                RunControls::default(),
            )
            .expect("idle Engine"),
    );
    let RunHandle {
        mut events, result, ..
    } = engine
        .submit(
            AdmittedProviders::single(first.clone()),
            request(&workspace, "First"),
            RunControls::default(),
        )
        .expect("a dropped handle frees the Engine");
    assert!(matches!(
        engine.submit(
            AdmittedProviders::single(second.clone()),
            request(&workspace, "Refused"),
            RunControls::default()
        ),
        Err(EngineError::Busy)
    ));
    assert!(matches!(
        engine
            .compact_session(
                &AdmittedProviders::single(second.clone()),
                SessionId::new(),
                workspace.clone()
            )
            .await,
        Err(EngineError::Busy)
    ));
    let first_run = result.await.expect("first Run");
    assert!(first_run.stopped.is_none());
    let first_run = first_run.run;
    let streamed = tokio::time::timeout(Duration::from_secs(5), async {
        let mut kinds = Vec::new();
        while let Some(arany::Tapped::Committed(envelope)) = events.next().await {
            let finished = matches!(envelope.event, Event::GoalFinished { .. });
            kinds.push(envelope.sequence);
            if finished {
                return kinds;
            }
        }
        panic!("the handle's stream ends with the Run's Goal receipt")
    })
    .await
    .expect("handle events");
    assert_eq!(
        streamed.first(),
        Some(&1),
        "the handle sees the Run from its first Event"
    );

    let mut next = request(&workspace, "Second");
    next.session_id = Some(first_run.session_id);
    next.title = None;
    let second_run = run_to_end(&engine, &second, next)
        .await
        .expect("the finished handle frees the Engine for another Provider");
    engine.close().await.expect("engine shutdown");

    let model = |run: &arany::RunView| run.config.as_ref().expect("Run config").model.clone();
    assert_eq!(model(&first_run.run), "model-a");
    assert_eq!(model(&second_run.run), "model-b");
    assert_eq!(second_run.run.assistant_message.as_deref(), Some("second"));
    assert_eq!(first.requests.lock().expect("requests").len(), 1);
    assert_eq!(second.requests.lock().expect("requests").len(), 1);
    assert!(
        second
            .compact_requests
            .lock()
            .expect("compactions")
            .is_empty()
    );
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let view = store
        .load_view(first_run.session_id)
        .await
        .expect("strict replay")
        .expect("Session");
    store.close().await.expect("Store shutdown");
    assert_eq!(
        view.runs
            .iter()
            .map(|run| run.objective.as_str())
            .collect::<Vec<_>>(),
        ["First", "Second"],
        "refused and abandoned submissions admit nothing"
    );
}

#[tokio::test]
async fn a_handle_dropped_inside_a_provider_call_frees_the_engine_for_the_same_session() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
    let state_path = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("private state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let (started, mut started_rx) = mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let held = Arc::new(WaitingProvider {
        started,
        dropped: Arc::clone(&dropped),
        requests: Arc::new(Mutex::new(Vec::new())),
        identity: waiting_identity(Duration::from_secs(120)),
    });
    let RunHandle {
        mut events,
        mut result,
        ..
    } = engine
        .submit(
            AdmittedProviders::single(held),
            request(&workspace, "Disconnected"),
            RunControls::default(),
        )
        .expect("idle Engine");
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            outcome = &mut result => panic!("the held call returned: {:?}", outcome.map(|result| result.run.run.status)),
            started = started_rx.recv() => assert_eq!(started, Some(())),
        }
    })
    .await
    .expect("the Provider call starts");
    // A client that disconnects drops its handle while the Provider call is in flight.
    drop(result);
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "the in-flight call is dropped"
    );
    let session_id = match events.next().await {
        Some(arany::Tapped::Committed(envelope)) => envelope.session_id,
        other => panic!("the handle streams the Session start first: {other:?}"),
    };

    let answer = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
        Answer {
            text: "reconnected".into(),
        },
    )]));
    let mut next = request(&workspace, "Reconnected");
    next.session_id = Some(session_id);
    next.title = None;
    let outcome = run_to_end(&engine, &answer, next)
        .await
        .expect("the dropped handle frees the Engine and the Session");
    assert_eq!(outcome.session_id, session_id);
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert_eq!(
        outcome.run.assistant_message.as_deref(),
        Some("reconnected")
    );
    engine.close().await.expect("engine shutdown");
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let view = store
        .load_view(session_id)
        .await
        .expect("strict replay")
        .expect("Session");
    store.close().await.expect("Store shutdown");
    assert_eq!(
        view.runs
            .iter()
            .map(|run| run.objective.as_str())
            .collect::<Vec<_>>(),
        ["Disconnected", "Reconnected"]
    );
    assert_ne!(
        view.runs[0].status,
        RunStatus::Finished,
        "the abandoned Run has no answer"
    );
}

#[test]
fn fork_pins_committed_parent_prefix_and_rejects_drift() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let other_workspace = temp.path().join("other");
        std::fs::create_dir_all(&other_workspace).expect("other workspace");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "first answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "second answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "fork answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "deep answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "bounded".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "bounded".into(),
            }),
        ]);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut first_request = request(&workspace, "First question");
        first_request.goal.finish_after_run = false;
        let first = run_to_end(&engine, &provider, first_request)
            .await
            .expect("source Run");
        let resumed = resume_session(
            StateRoot::admit(&state_path).expect("state readmission"),
            workspace.clone(),
            first.session_id,
        )
        .await
        .expect("exact Session resume");
        assert_eq!(resumed.id, first.session_id);
        assert_eq!(resumed.runs.len(), 1);
        assert!(matches!(
            resume_session(
                StateRoot::admit(&state_path).expect("state readmission"),
                other_workspace.clone(),
                first.session_id,
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            fork_session(
                StateRoot::admit(&state_path).expect("state readmission"),
                other_workspace.clone(),
                first.session_id,
                None,
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            rename_session(
                StateRoot::admit(&state_path).expect("state readmission"),
                other_workspace.clone(),
                first.session_id,
                "Other".into(),
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    first.session_id,
                    other_workspace
                )
                .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        let fork_id = fork_session(
            StateRoot::admit(&state_path).expect("state readmission"),
            workspace.clone(),
            first.session_id,
            Some("Branch".into()),
        )
        .await
        .expect("forked Session");
        assert_ne!(fork_id, first.session_id);
        let continued = continue_session(
            StateRoot::open_existing(&state_path).expect("state readmission"),
            workspace.clone(),
        )
        .await
        .expect("latest fork in Workspace");
        assert_eq!(continued.id, fork_id);
        let listed = list_sessions(
            StateRoot::open_existing(&state_path).expect("state readmission"),
            workspace.clone(),
        )
        .await
        .expect("fork in Session picker");
        assert_eq!(
            listed.iter().map(|item| item.id).collect::<Vec<_>>(),
            [fork_id, first.session_id]
        );
        assert_eq!(listed[0].title, "Branch");
        assert_eq!(listed[0].defaults.model.as_deref(), Some("test-model"));
        let nested_id = engine
            .fork_session(fork_id, workspace.clone(), None)
            .await
            .expect("fork of fork before a new Run");

        let mut parent_request = request(&workspace, "Later source question");
        parent_request.session_id = Some(first.session_id);
        parent_request.title = None;
        run_to_end(&engine, &provider, parent_request)
            .await
            .expect("later source Run");

        let mut child_request = request(&workspace, "Branch question");
        child_request.session_id = Some(fork_id);
        child_request.title = None;
        let child = run_to_end(&engine, &provider, child_request)
            .await
            .expect("fork Run");
        assert_eq!(child.run.status, RunStatus::Finished);
        assert_eq!(
            child
                .run
                .config
                .as_ref()
                .expect("Run config")
                .history_run_ids,
            vec![first.run.id]
        );
        {
            let observed = requests.lock().expect("request lock");
            assert_eq!(observed.len(), 3);
            assert_eq!(observed[2].input.history.len(), 1);
            assert_eq!(observed[2].input.history[0].user, "First question");
            assert_eq!(observed[2].input.history[0].assistant, "first answer");
        }
        let boundary = first.run.finished_sequence.unwrap();
        assert!(matches!(
            engine
                .fork_session_at(
                    first.session_id,
                    workspace.clone(),
                    first.run.accepted_sequence,
                    None
                )
                .await,
            Err(EngineError::ForkNotAtRunBoundary)
        ));
        let earlier_id = engine
            .fork_session_at(first.session_id, workspace.clone(), boundary, None)
            .await
            .unwrap();
        let owner_id = engine
            .fork_session_at(nested_id, workspace.clone(), boundary, None)
            .await
            .unwrap();
        let store = Store::open(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let defaults = arany::SessionDefaults {
            provider: Some("scripted".into()),
            model: Some("test-model".into()),
            policy: CollaborationPolicy::Single,
            ..Default::default()
        };
        arany::set_session_defaults(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
            first.session_id,
            defaults.clone(),
        )
        .await
        .unwrap();
        let source_before = store.load_session(first.session_id).await.unwrap();
        let unsaved = engine
            .rewind_conversation(first.session_id, workspace.clone(), first.run.id)
            .await
            .unwrap();
        assert!(unsaved.session_id.is_none());
        assert_eq!(unsaved.defaults, defaults);
        assert_eq!(unsaved.prompt.as_deref(), Some("First question"));
        assert_eq!(
            source_before,
            store.load_session(first.session_id).await.unwrap()
        );
        let source = store.load_view(first.session_id).await.unwrap().unwrap();
        for head in [
            (source.last_sequence - 1, source.rewinds.prefix_digest()),
            (source.last_sequence, [0; 32]),
        ] {
            assert!(matches!(
                engine
                    .rewind_conversation_at_head(
                        first.session_id,
                        workspace.clone(),
                        source.runs[1].id,
                        head,
                        i64::MAX,
                    )
                    .await,
                Err(EngineError::RewindPreviewExpired)
            ));
            assert_eq!(
                source_before,
                store.load_session(first.session_id).await.unwrap()
            );
        }
        let restored = engine
            .rewind_conversation_at_head(
                first.session_id,
                workspace.clone(),
                source.runs[1].id,
                (source.last_sequence, source.rewinds.prefix_digest()),
                i64::MAX,
            )
            .await
            .unwrap();
        let restored = store
            .load_view(restored.session_id.unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.runs.len(), 1);
        assert!(
            restored.goals.is_empty(),
            "an inherited paused Goal cannot be resumed or duplicate its budget"
        );
        let mut inherited_resume = request(&workspace, "Must not revive inherited Goal");
        inherited_resume.session_id = Some(restored.id);
        inherited_resume.title = None;
        inherited_resume.goal = arany::GoalRequest::resume(source.goals[0].id, true);
        assert!(matches!(
            run_to_end(&engine, &provider, inherited_resume).await,
            Err(EngineError::GoalUnavailable)
        ));
        assert_eq!(
            store
                .load_view(earlier_id)
                .await
                .unwrap()
                .unwrap()
                .runs
                .len(),
            1
        );
        assert_eq!(
            store
                .load_view(owner_id)
                .await
                .unwrap()
                .unwrap()
                .lineage
                .unwrap()
                .source_session_id,
            first.session_id
        );
        let mut deepest = first.session_id;
        for _ in 0..8 {
            deepest = engine
                .fork_session(deepest, workspace.clone(), None)
                .await
                .unwrap();
        }
        let heads_before = list_sessions(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
        )
        .await
        .unwrap();
        assert!(matches!(
            engine.fork_session(deepest, workspace.clone(), None).await,
            Err(EngineError::Store(arany::StoreError::ReplayLimit))
        ));
        assert_eq!(
            heads_before.len(),
            list_sessions(
                StateRoot::open_existing(&state_path).unwrap(),
                workspace.clone()
            )
            .await
            .unwrap()
            .len()
        );
        let mut deep_request = request(&workspace, "Deep question");
        deep_request.session_id = Some(deepest);
        deep_request.title = None;
        let deep = run_to_end(&engine, &provider, deep_request).await.unwrap();
        assert!(matches!(
            engine
                .fork_session_at(
                    deepest,
                    workspace.clone(),
                    deep.run.finished_sequence.unwrap(),
                    None
                )
                .await,
            Err(EngineError::ConversationRewindLimit)
        ));
        let avoided = engine
            .rewind_conversation(deepest, workspace.clone(), deep.run.id)
            .await
            .unwrap();
        assert_eq!(
            store
                .load_view(avoided.session_id.unwrap())
                .await
                .unwrap()
                .unwrap()
                .lineage
                .unwrap()
                .source_session_id,
            first.session_id
        );
        store.close().await.unwrap();
        for bytes in [8 * 1024, 8 * 1024 + 1] {
            let prompt = "x".repeat(bytes);
            let bounded = run_to_end(&engine, &provider, request(&workspace, &prompt))
                .await
                .unwrap();
            let unsaved = engine
                .rewind_conversation(bounded.session_id, workspace.clone(), bounded.run.id)
                .await
                .unwrap();
            assert!(unsaved.session_id.is_none());
            assert_eq!(
                unsaved.prompt.as_deref(),
                (bytes <= 8 * 1024).then_some(prompt.as_str())
            );
        }
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store.load_session(fork_id).await.expect("fork Events");
        assert!(matches!(events[0].event, Event::SessionForked { .. }));
        let view = store
            .load_view(fork_id)
            .await
            .expect("resolved fork")
            .expect("fork exists");
        assert_eq!(view.title, "Branch");
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.runs[0].id, first.run.id);
        assert_eq!(view.runs[1].id, child.run.id);
        let nested = store
            .load_view(nested_id)
            .await
            .expect("resolved nested fork")
            .expect("nested fork exists");
        assert_eq!(nested.runs.len(), 1);
        assert_eq!(nested.runs[0].id, first.run.id);
        store.close().await.expect("store shutdown");

        let listed = list_sessions(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
        )
        .await
        .unwrap();
        let nested_item = listed.iter().find(|item| item.id == nested_id).unwrap();
        assert_eq!(
            nested_item.title, "First question",
            "later source objectives and answers must not enter the fork preview"
        );
        assert_eq!(
            nested.inherited_title.as_deref(),
            Some(nested_item.title.as_str())
        );
        assert_eq!(nested_item.defaults.model.as_deref(), Some("test-model"));
        rename_session(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
            nested_id,
            "New Session".into(),
        )
        .await
        .unwrap();
        let renamed = resume_session(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
            nested_id,
        )
        .await
        .unwrap();
        assert!(renamed.title_is_explicit);
        assert_eq!(
            renamed.conversation_title(),
            "New Session",
            "a manual placeholder-looking name remains explicit after reopen"
        );
        let relisted = list_sessions(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            relisted
                .iter()
                .find(|item| item.id == nested_id)
                .unwrap()
                .title,
            renamed.conversation_title()
        );

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("fault injection connection");
        connection
            .execute(
                "UPDATE events SET payload=?2 WHERE session_id=?1 AND kind='SessionStarted'",
                rusqlite::params![
                    first.session_id.to_string(),
                    Event::SessionStarted {
                        title: "Altered".into(),
                        workspace_identity: resumed.workspace_identity.unwrap(),
                    }
                    .payload()
                    .unwrap()
                ],
            )
            .expect("altered source prefix");
        drop(connection);
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store after drift");
        assert!(matches!(
            store.load_session(fork_id).await,
            Err(arany::StoreError::InvalidHistory)
        ));
        for id in [earlier_id, owner_id] {
            assert!(matches!(
                store.load_view(id).await,
                Err(arany::StoreError::InvalidHistory)
            ));
        }
        store.close().await.expect("store shutdown after drift");

        let provider = ScriptedProvider::new(Vec::new());
        let attempted_calls = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("engine after drift");
        let mut rejected = request(&workspace, "Must not disclose");
        rejected.session_id = Some(fork_id);
        rejected.title = None;
        assert!(matches!(
            run_to_end(&engine, &provider, rejected).await,
            Err(EngineError::Store(arany::StoreError::InvalidHistory))
        ));
        assert!(attempted_calls.lock().expect("request lock").is_empty());
        engine.close().await.expect("engine shutdown after drift");
    });
}

#[test]
fn failed_child_cancels_and_drains_siblings_without_a_final_message() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = GatedProvider {
            started,
            requests: Arc::clone(&requests),
            fail_child: Some("B"),
            identity: gated_identity(3),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 3,
        };
        let mut run_task = tokio::spawn(async move {
            let outcome = run_to_end(&engine, &provider, run_request).await;
            let close = engine.close().await;
            (outcome, close)
        });
        let gates = tokio::time::timeout(Duration::from_secs(5), async {
            let mut gates = HashMap::new();
            for _ in 0..3 {
                let (objective, release) = started_rx.recv().await.expect("child started");
                gates.insert(objective, release);
            }
            gates
        })
        .await;
        let mut gates = match gates {
            Ok(gates) => gates,
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("children did not start within the test deadline");
            }
        };
        gates
            .remove("B")
            .expect("B gate")
            .send(())
            .expect("live B call");
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
        let (outcome, close) = match joined {
            Ok(result) => result.expect("Run task"),
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("failed team Run exceeded its test deadline");
            }
        };
        let outcome = outcome.expect("durable failed Run");
        close.expect("engine shutdown");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(
            outcome
                .run
                .agents
                .iter()
                .map(|agent| agent.status)
                .collect::<Vec<_>>(),
            [
                AgentStatus::Failed,
                AgentStatus::Cancelled,
                AgentStatus::Failed,
                AgentStatus::Cancelled
            ]
        );
        assert!(gates.remove("A").expect("A gate").send(()).is_err());
        assert!(gates.remove("C").expect("C gate").send(()).is_err());
        assert!(
            requests
                .lock()
                .expect("requests lock")
                .iter()
                .all(|request| request.phase != AgentPhase::RootSynthesis)
        );
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(outcome.session_id)
            .await
            .expect("durable Events");
        assert!(events.iter().any(|envelope| matches!(
            &envelope.event,
            Event::ProviderCallRecorded { record, .. }
                if record.disposition == ProviderCallDisposition::Rejected
        )));
        let rejected = events
            .iter()
            .find_map(|envelope| match &envelope.event {
                Event::ProviderCallRecorded { record, .. }
                    if record.disposition == ProviderCallDisposition::Rejected =>
                {
                    Some(record)
                }
                _ => None,
            })
            .expect("failed child's observed call");
        assert_eq!(
            serde_json::to_value(rejected).expect("bounded call metadata")["failure_reason"],
            "account_access",
            "typed HTTP 403 must retain safe guidance through the journal"
        );
        let replay = SessionView::replay(outcome.session_id, &events)
            .expect("strict failure replay")
            .expect("Session");
        assert_eq!(replay.runs[0], outcome.run);

        assert!(
            replay.runs[0]
                .agents
                .iter()
                .filter(|agent| agent.status == AgentStatus::Cancelled)
                .all(|agent| agent
                    .provider_calls
                    .iter()
                    .all(|call| call.failure_reason.is_none()))
        );
        assert!(
            !events
                .iter()
                .any(|envelope| matches!(envelope.event, Event::MessageCommitted { .. }))
        );
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn direct_cancellation_drops_the_call_and_persists_a_cancelled_run() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = WaitingProvider {
            started,
            dropped: Arc::clone(&dropped),
            requests: Arc::clone(&requests),
            identity: waiting_identity(Duration::from_secs(120)),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::admit(&state_path).expect("private state"), EngineConfig::default())
        .expect("engine");
        let cancellation = RunCancellation::new();
        let run_cancellation = cancellation.clone();
        let mut events = engine.subscribe();
        let mut run_task = tokio::spawn(async move {
            let outcome = run_with(&engine, &provider, request(&workspace, "Question"), RunControls { cancellation: run_cancellation, ..Default::default() })
                .await;
            let close = engine.close().await;
            (outcome, close)
        });
        let started = tokio::time::timeout(Duration::from_secs(5), started_rx.recv()).await;
        if started.is_err() {
            run_task.abort();
            let _ = run_task.await;
            panic!("direct Provider call did not start within the test deadline");
        }
        assert_eq!(started.expect("started signal"), Some(()));
        loop {
            let event = next_committed(&mut events, "committed active Run").await;
            if matches!(event.event, Event::AgentSpawned { .. }) { break; }
        }
        assert!(cancellation.cancel());
        assert!(!cancellation.cancel());
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
        let (outcome, close) = match joined {
            Ok(result) => result.expect("Run task"),
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("direct cancellation exceeded the test deadline");
            }
        };
        let outcome = outcome.expect("durable cancelled Run");
        close.expect("engine shutdown");
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        {
            let calls = requests.lock().expect("requests");
            assert_eq!(calls.len(), 1, "exactly one cancelled call");
            assert_test_call(
                &calls[0],
                AgentPhase::RootPlan,
                "Question",
                &["Fact"],
                &[],
                None,
                &[],
            );
            assert_eq!(calls[0].run_id, outcome.run.id);
            assert_eq!(calls[0].agent_run_id, outcome.run.agents[0].id);
        }
        assert_eq!(outcome.run.status, RunStatus::Cancelled);
        assert_eq!(outcome.run.agents[0].status, AgentStatus::Cancelled);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(outcome.run.agents[0].provider_calls.len(), 1);
        assert_eq!(
            outcome.run.agents[0].provider_calls[0].disposition,
            ProviderCallDisposition::Cancelled
        );
        assert_eq!(outcome.run.agents[0].provider_calls[0].response_id, None);
        assert_eq!(outcome.run.agents[0].provider_calls[0].input_tokens, None);
        assert_eq!(outcome.run.agents[0].provider_calls[0].output_tokens, None);
        loop {
            let event = next_committed(&mut events, "committed cancellation").await;
            if matches!(event.event, Event::RunFinished { .. }) {
                assert_eq!(Some(event.sequence), outcome.run.finished_sequence);
                break;
            }
        }

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(outcome.session_id)
            .await
            .expect("durable Events");
        assert!(events.iter().any(|envelope| matches!(
            &envelope.event,
            Event::RunFinished {
                disposition: arany::RunDisposition::Cancelled,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|envelope| matches!(envelope.event, Event::MessageCommitted { .. }))
        );
        let replay = SessionView::replay(outcome.session_id, &events)
            .expect("valid replay")
            .expect("Session");
        assert_eq!(replay.runs[0].status, RunStatus::Cancelled);
        store.close().await.expect("store shutdown");

        for phase in [AgentPhase::RootPlan, AgentPhase::RootSynthesis] {
            for cancel in [true, false] {
                for (case, response, call_disposition, failure_reason) in completion_boundary_cases() {
                    let temp = tempfile::tempdir().expect("completion-time root");
                    let workspace = temp.path().join("workspace");
                    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
                    std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
                    let state_path = temp.path().join("state");
                    let cancellation = RunCancellation::new();
                    let mut provider = ScriptedProvider::new(Vec::new());
                    provider.cancel_at_completion = cancel.then(|| (phase, cancellation.clone()));
                    let mut responses = VecDeque::new();
                    let mut run_request = request(&workspace, "Completion boundary");
                    let expected_phases = if phase == AgentPhase::RootSynthesis {
                        run_request.policy = CollaborationPolicy::Team {
                            max_active_children: 1,
                        };
                        responses.push_back(Ok(ProviderOutcome::Delegate(Delegate {
                            agent: None,
                            children: vec![arany::ChildAssignment { editor: None, objective: "A".into(), profile: None }],
                        })));
                        responses.push_back(Ok(ProviderOutcome::Answer(Answer { text: "Child result".into() })));
                        vec![
                            AgentPhase::RootPlan,
                            AgentPhase::ChildWork,
                            AgentPhase::RootSynthesis,
                        ]
                    } else {
                        vec![AgentPhase::RootPlan]
                    };
                    responses.push_back(response);
                    let mut expected_phases = expected_phases;
                    if case == "invalid answer" && !cancel {
                        for _ in 0..2 {
                            responses.push_back(Ok(ProviderOutcome::Answer(Answer { text: String::new() })));
                            expected_phases.push(phase);
                        }
                    }
                    provider.responses = Mutex::new(responses);
                    let calls = Arc::clone(&provider.requests);
                    let provider = Arc::new(provider);
                    let engine = engine_support::open(StateRoot::admit(&state_path).expect("private state"), EngineConfig::default())
                    .expect("Engine");
                    let mut tapped = engine.subscribe();
                    let outcome = tokio::time::timeout(
                        Duration::from_secs(5),
                        run_with(&engine, &provider, run_request, RunControls { cancellation, ..Default::default() }),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        let phase = format!("{phase:?}");
                        let cancellation = if cancel { "true" } else { "false" };
                        let message = format!(
                            "fixture=direct_cancellation; phase={phase}; case={case}; cancellation={cancellation}; failure=completion_deadline"
                        );
                        process::annotate_native_fixture(&message);
                        panic!("{message}");
                    })
                    .expect("committed Run");
                    engine.close().await.expect("Engine shutdown");
                    let expected_status = if cancel {
                        RunStatus::Cancelled
                    } else if call_disposition == ProviderCallDisposition::Finished {
                        RunStatus::Finished
                    } else {
                        RunStatus::Failed
                    };
                    assert_eq!(
                        outcome.run.status, expected_status,
                        "{phase:?}, {case}, cancellation {cancel}"
                    );
                    let primary = outcome.run.agents.first().expect("primary");
                    assert_eq!(
                        primary.status,
                        match expected_status {
                            RunStatus::Cancelled => AgentStatus::Cancelled,
                            RunStatus::Finished => AgentStatus::Finished,
                            _ => AgentStatus::Failed,
                        }
                    );
                    let record = primary.provider_calls.last().expect("observed call");
                    assert_eq!(record.phase, phase);
                    assert_eq!(record.disposition, call_disposition);
                    assert_eq!(record.failure_reason, failure_reason);
                    assert_eq!(record.response_id, None);
                    assert_eq!(record.input_tokens, None);
                    assert_eq!(
                        record.output_tokens,
                        matches!(case, "invalid answer" | "accepted answer").then_some(8)
                    );
                    assert_eq!(record.wire_provenance, None);
                    assert_eq!(
                        outcome.run.assistant_message.as_deref(),
                        (expected_status == RunStatus::Finished).then_some("Accepted result")
                    );
                    if phase == AgentPhase::RootSynthesis {
                        assert_eq!(outcome.run.agents.len(), 2);
                        assert_eq!(outcome.run.agents[1].status, AgentStatus::Finished);
                    } else {
                        assert_eq!(outcome.run.agents.len(), 1);
                    }
                    assert_eq!(
                        calls
                            .lock()
                            .expect("calls")
                            .iter()
                            .map(|call| call.phase)
                            .collect::<Vec<_>>(),
                        expected_phases,
                        "no retry or extra call"
                    );
                    let terminal = loop {
                        let event = next_committed(&mut tapped, "committed terminal Event").await;
                        if matches!(event.event, Event::RunFinished { .. }) { break event; }
                    };
                    let store = Store::open_read_only(
                        StateRoot::open_existing(&state_path).expect("existing state"),
                    )
                    .expect("read-only Store");
                    let events = store
                        .load_session(outcome.session_id)
                        .await
                        .expect("closed Events");
                    let replay = SessionView::replay(outcome.session_id, &events)
                        .expect("strict replay")
                        .expect("Session");
                    assert_eq!(
                        terminal.sequence,
                        outcome
                            .run
                            .finished_sequence
                            .expect("Run terminal sequence")
                    );
                    // A cancelled Run pauses its Goal: it records no terminal receipt and stays resumable.
                    let expected_goal_status = match outcome.run.status {
                        RunStatus::Finished => Some(arany::GoalStatus::Completed),
                        RunStatus::Failed => Some(arany::GoalStatus::Failed),
                        RunStatus::Cancelled => None,
                        status => panic!("expected a terminal Run, got {status:?}"),
                    };
                    if let Some(expected_goal_status) = expected_goal_status {
                        assert!(events.last().is_some_and(|event| {
                            event.sequence > terminal.sequence
                                && matches!(
                                    event.event,
                                    Event::GoalFinished {
                                        goal_id,
                                        status,
                                        ..
                                    } if goal_id == outcome.goal_id && status == expected_goal_status
                                )
                        }));
                    } else {
                        assert!(
                            !events
                                .iter()
                                .any(|event| matches!(event.event, Event::GoalFinished { .. })),
                            "cancelled Run records no Goal receipt"
                        );
                        assert_eq!(replay.goals[0].status, arany::GoalStatus::Interrupted);
                    }
                    assert_eq!(replay.runs.len(), 1);
                    assert_eq!(replay.runs[0], outcome.run);
                    assert_eq!(
                        events
                            .iter()
                            .filter(|envelope| matches!(
                                envelope.event,
                                Event::MessageCommitted { .. }
                            ))
                            .count(),
                        usize::from(expected_status == RunStatus::Finished)
                    );
                    store.close().await.expect("Store shutdown");
                }
            }
        }
    });
}

#[tokio::test]
async fn streamed_text_before_a_call_is_journaled_as_commentary_and_the_run_continues() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let text = format!("STREAMED_BEFORE_CALL:{}", "x".repeat(3 * 1024));
    let mut provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "A".into(),
                profile: None,
            }],
        }),
        ProviderOutcome::Answer(Answer {
            text: "child result".into(),
        }),
        ProviderOutcome::Answer(Answer { text: text.clone() }),
    ]);
    provider.preview = Some(arany::PreviewText {
        answer: text.clone(),
        tentative: true,
        ..Default::default()
    });
    let gate = Arc::new(tokio::sync::Notify::new());
    provider.preview_gate = Some(gate.clone());
    let provider = Arc::new(provider);
    let engine =
        engine_support::open(StateRoot::admit(&state).unwrap(), EngineConfig::default()).unwrap();
    let mut objective = request(&workspace, "question");
    objective.include_paths.clear();
    objective.policy = CollaborationPolicy::Team {
        max_active_children: 1,
    };
    let hints = RunHints::with_previews();
    let mut previews = hints.previews();
    // The Engine checkpoints a preview before it publishes it as a hint, so the gate opens after the checkpoint.
    let opener = tokio::spawn(async move {
        loop {
            previews.changed().await;
            gate.notify_one();
        }
    });
    let outcome = run_with(
        &engine,
        &provider,
        objective,
        RunControls {
            hints: Some(hints),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    opener.abort();
    engine.close().await.unwrap();
    assert_eq!(outcome.run.status, RunStatus::Finished);
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    let chunks: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::AssistantDisplayRecorded { chunk, .. } => Some(chunk),
            _ => None,
        })
        .collect();
    let call = chunks
        .iter()
        .map(|chunk| chunk.call_index)
        .min()
        .expect("display chunks");
    assert!(
        !chunks
            .iter()
            .any(|chunk| chunk.call_index == call
                && chunk.category == arany::DisplayCategory::Answer)
    );
    assert_eq!(
        chunks
            .iter()
            .filter(|chunk| chunk.call_index == call
                && chunk.category == arany::DisplayCategory::Commentary)
            .map(|chunk| chunk.text.as_str())
            .collect::<String>(),
        text
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn attached_previews_checkpoint_cancel_reopen_and_stay_out_of_context() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let text = format!("INCOMPLETE_CANARY:{}", "x".repeat(2300));
    let provider = PreviewProvider {
        text: arany::PreviewText {
            answer: text.clone(),
            commentary: vec!["PUBLIC_COMMENTARY_CANARY".into()],
            commentary_limited: false,
            tentative: false,
        },
        completion: None,
    };
    let provider = Arc::new(provider);
    let engine =
        engine_support::open(StateRoot::admit(&state).unwrap(), EngineConfig::default()).unwrap();
    let mut objective = request(&workspace, "question");
    objective.include_paths.clear();
    let cancel = RunCancellation::new();
    let mut tapped = engine.subscribe();
    let observer = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let cancellation = cancel.clone();
    let run_hints = RunHints::with_previews();
    let task = tokio::spawn(async move {
        let outcome = run_with(
            &engine,
            &provider,
            objective,
            RunControls {
                cancellation,
                hints: Some(run_hints),
                ..Default::default()
            },
        )
        .await;
        engine.close().await.unwrap();
        outcome.unwrap()
    });
    let session_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = next_committed(&mut tapped, "committed checkpoint").await;
            if matches!(&event.event, Event::AssistantDisplayRecorded { chunk, .. } if chunk.category == arany::DisplayCategory::Answer) {
                let session_id = event.session_id;
                let events = observer.load_session(session_id).await.unwrap();
                assert!(events.iter().any(|event| matches!(&event.event, Event::AssistantDisplayRecorded { chunk, .. } if chunk.text.len() == 2048)));
                break events[0].session_id;
            }
        }
    }).await.unwrap();
    cancel.cancel();
    let outcome = task.await.unwrap();
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    assert!(outcome.run.assistant_message.is_none());
    observer.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(session_id).await.unwrap();
    let view = SessionView::replay(session_id, &events).unwrap().unwrap();
    let run = &view.runs[0];
    assert!(
        run.display
            .iter()
            .filter(|chunk| chunk.category == arany::DisplayCategory::Answer)
            .map(|chunk| chunk.text.as_str())
            .collect::<String>()
            == text,
        "reopened checkpoints must retain the safe answer prefix"
    );
    let mut invalid = events.clone();
    let recorded = invalid
        .iter_mut()
        .find_map(|event| match &mut event.event {
            Event::AssistantDisplayRecorded { chunk, .. } => Some(chunk),
            _ => None,
        })
        .unwrap();
    recorded.offset = 1;
    assert!(SessionView::replay(session_id, &invalid).is_err());
    store.close().await.unwrap();
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "accepted answer".into(),
    })]);
    let provider = provider.with_compactions(vec![Ok(CompactionResponse {
        summary: "safe summary".into(),
        response_id: None,
        usage: arany::Usage {
            input_tokens: None,
            output_tokens: Some(8),
            ..Default::default()
        },
        wire_provenance: None,
    })]);
    let compact_requests = provider.compact_requests.clone();
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::open_existing(&state).unwrap(),
        EngineConfig::default(),
    )
    .unwrap();
    let mut next = request(&workspace, "next question");
    next.include_paths.clear();
    next.session_id = Some(session_id);
    next.title = None;
    let accepted = run_to_end(&engine, &provider, next).await.unwrap();
    assert_eq!(accepted.run.status, RunStatus::Finished);
    let captured = format!("{:?}", requests.lock().unwrap());
    assert!(
        !captured.contains("INCOMPLETE_CANARY"),
        "incomplete display text must stay outside model input"
    );
    assert!(
        !captured.contains("PUBLIC_COMMENTARY_CANARY"),
        "public commentary must stay outside model input"
    );
    assert!(matches!(
        engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                session_id,
                workspace.clone()
            )
            .await
            .unwrap()
            .status,
        CompactionStatus::Succeeded { .. }
    ));
    let compacted = format!("{:?}", compact_requests.lock().unwrap());
    assert!(
        !compacted.contains("INCOMPLETE_CANARY"),
        "incomplete display text must stay outside compaction"
    );
    assert!(
        !compacted.contains("PUBLIC_COMMENTARY_CANARY"),
        "public commentary must stay outside compaction"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn list_sessions_page_rejects_zero_limit_before_workspace_or_store_access() {
    let temp = tempfile::tempdir().expect("temporary root");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).expect("private state");
    let store_before = std::fs::metadata(state_path.join("events.sqlite3"))
        .ok()
        .map(|m| m.len());
    assert!(matches!(
        arany::list_sessions_page(state, temp.path().join("missing-workspace"), None, 0).await,
        Err(EngineError::InvalidRequest)
    ));
    assert_eq!(
        std::fs::metadata(state_path.join("events.sqlite3"))
            .ok()
            .map(|m| m.len()),
        store_before
    );
}

#[tokio::test]
async fn create_session_on_open_engine_commits_session_started_and_submit_appends_to_it() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let engine = Engine::open(StateRoot::admit(&state).unwrap(), EngineConfig::default()).unwrap();
    for title in [String::new(), "x".repeat(129)] {
        assert!(matches!(
            engine.create_session(workspace.clone(), Some(title)).await,
            Err(EngineError::InvalidRequest)
        ));
    }
    let mut tap = engine.subscribe();
    let session = engine
        .create_session(workspace.clone(), Some("Explicit".into()))
        .await
        .unwrap();
    assert!(
        matches!(tokio::time::timeout(Duration::from_secs(10), tap.next()).await.unwrap(), Some(arany::Tapped::Committed(e)) if e.session_id == session && matches!(&e.event, Event::SessionStarted { title,.. } if title == "Explicit"))
    );
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
        Answer {
            text: "Hello".into(),
        },
    )]));
    let mut input = request(&workspace, "Hi");
    input.include_paths.clear();
    input.session_id = Some(session);
    input.title = None;
    let outcome = run_to_end(&engine, &provider, input).await.unwrap();
    assert_eq!(outcome.session_id, session);
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::SessionStarted { .. }))
            .count(),
        1
    );
    assert_eq!(view.title, "Explicit");
    assert_eq!(view.runs[0].id, outcome.run.id);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn write_proposal_refusals_are_durable_bounded_and_spend_goal_calls() {
    use arany::{
        GoalBudget, GoalPlanContract, GoalSuccessCriterion, ProposalRefusalCause, RunFailure,
    };
    use std::os::unix::fs::PermissionsExt;

    for (plan_calls, cause, goal_calls, expected_calls, failure) in [
        (
            None,
            ProposalRefusalCause::PlanInvalid,
            8,
            3,
            Some(RunFailure::RefusalLimit),
        ),
        (
            Some(1),
            ProposalRefusalCause::PlanBudgetSpent,
            8,
            3,
            Some(RunFailure::RefusalLimit),
        ),
        (
            None,
            ProposalRefusalCause::PlanInvalid,
            2,
            2,
            Some(RunFailure::BudgetExhausted),
        ),
    ] {
        let temp = tempfile::tempdir().expect("fixture");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_path = temp.path().join("state");
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig {
                guard_executable: Some(std::env::current_exe().expect("test executable")),
                ..Default::default()
            },
        )
        .expect("engine");
        let config = state_path.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}"#).expect("grant");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("private grant");
        let plan = GoalPlanContract {
            mission: if plan_calls.is_some() {
                "write test.txt containing hello".into()
            } else {
                String::new()
            },
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            paths: vec!["test.txt".into()],
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
                path: "test.txt".into(),
                sha256: Sha256::digest(b"hello")
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            },
            budget: GoalBudget {
                max_provider_calls: plan_calls.or(Some(8)),
                ..GoalBudget::default()
            },
        };
        let provider = Arc::new(ScriptedProvider::new(
            (0..expected_calls)
                .map(|_| arany::test_support::scenario::plan_outcome(plan.clone()))
                .collect(),
        ));
        let mut input = request(&workspace, "write test.txt containing hello");
        input.include_paths.clear();
        input.goal.budget = Some(GoalBudget {
            max_provider_calls: Some(goal_calls),
            ..Default::default()
        });
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .expect("refused Run");
        assert_eq!(
            provider.requests.lock().expect("requests").len(),
            expected_calls,
            "{cause:?}"
        );
        assert!(provider.responses.lock().expect("script").is_empty());
        engine.close().await.expect("close engine");
        let store =
            Store::open_read_only(StateRoot::open_existing(&state_path).expect("existing state"))
                .expect("reopen");
        let events = store.load_session(result.session_id).await.expect("events");
        let view = SessionView::replay(result.session_id, &events)
            .expect("strict replay")
            .expect("Session");
        let run = &view.runs[0];
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.failure, failure);
        assert_eq!(run.agents[0].provider_calls.len(), expected_calls);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
                .count(),
            expected_calls
        );
        {
            let requests = provider.requests.lock().expect("requests");
            assert!(requests[0].input.last_answer_refused.is_none());
            for request in &requests[1..] {
                assert_eq!(
                    request
                        .input
                        .last_answer_refused
                        .as_ref()
                        .expect("repair note")
                        .cause,
                    cause
                );
            }
        }
        let call = &run.agents[0].provider_calls[0];
        assert_eq!(call.disposition, ProviderCallDisposition::InvalidResponse);
        assert_eq!(
            call.failure_reason,
            Some(arany::ProviderFailureReason::ProposalRefused)
        );
        let refusal = call.refusal.as_ref().expect("closed refusal");
        assert_eq!(refusal.operation, "arany_plan");
        assert_eq!(refusal.cause, cause);
        assert_eq!(call.requested_tools, 0);
        assert!(!events.iter().any(|event| matches!(
            event.event,
            Event::ToolStarted { .. } | Event::GoalPlanContractRecorded { .. }
        )));
        assert!(run.assistant_message.is_none());
        assert!(!format!("{events:?}").contains("PRIVATE_ARGUMENT_"));
        assert!(!workspace.join("test.txt").exists());
        store.close().await.expect("close store");
        if plan_calls.is_none() && goal_calls == 8 {
            let crash_path = temp.path().join("crashed-state");
            let crash_store = Store::open(StateRoot::admit(&crash_path).expect("crash state"))
                .expect("crash store");
            for event in &events {
                crash_store
                    .append(result.session_id, event.event.clone())
                    .await
                    .expect("committed prefix");
                if matches!(event.event, Event::ProviderCallRecorded { .. }) {
                    break;
                }
            }
            crash_store.close().await.expect("crash boundary");
            let resumed = resume_session(
                StateRoot::open_existing(&crash_path).expect("crash state"),
                workspace.clone(),
                result.session_id,
            )
            .await
            .expect("resume after refusal");
            assert_eq!(resumed.runs[0].status, RunStatus::Interrupted);
            assert_eq!(
                resumed.runs[0].agents[0].provider_calls[0].refusal,
                call.refusal
            );
            let engine = engine_support::open(
                StateRoot::open_existing(&crash_path).expect("crash state"),
                EngineConfig::default(),
            )
            .expect("resume engine");
            let peer = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
                Answer {
                    text: "No write dispatched".into(),
                },
            )]));
            let mut next = request(&workspace, "What happened?");
            next.include_paths.clear();
            next.session_id = Some(result.session_id);
            next.title = None;
            run_to_end(&engine, &peer, next)
                .await
                .expect("recovery Run");
            engine.close().await.expect("recovery shutdown");
            let requests = peer.requests.lock().expect("recovery requests");
            assert_eq!(requests.len(), 1);
            let history = &requests[0].input.history;
            assert_eq!(history.len(), 1);
            assert!(history[0].assistant.contains("plan_invalid"));
            assert!(history[0].assistant.contains("nothing dispatched"));
        }
    }
}

#[tokio::test]
async fn greeting_commits_one_text_answer_and_resumes_with_the_first_objective_title() {
    let temp = tempfile::tempdir().expect("fixture");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state = temp.path().join("state");
    let answer = "Hola! How can I help?";
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
        Answer {
            text: answer.into(),
        },
    )]));
    let engine = engine_support::open(
        StateRoot::admit(&state).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "hola!");
    input.title = None;
    input.include_paths.clear();
    let result = run_to_end(&engine, &provider, input)
        .await
        .expect("greeting");
    engine.close().await.expect("closed Engine");
    {
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].input.goal_budget.is_none());
    }
    assert!(provider.responses.lock().expect("script").is_empty());
    let store =
        Store::open_read_only(StateRoot::open_existing(&state).expect("state")).expect("store");
    let events = store.load_session(result.session_id).await.expect("Events");
    let view = store
        .load_view(result.session_id)
        .await
        .expect("replay")
        .expect("Session");
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.runs[0].assistant_message.as_deref(), Some(answer));
    assert_eq!(
        view.runs[0]
            .agents
            .first()
            .expect("primary")
            .summary
            .as_deref(),
        Some(answer)
    );
    assert!(view.runs[0].tools.is_empty());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::ProviderCallRecorded { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(&e.event, Event::MessageCommitted { text, .. } if text == answer))
            .count(),
        1
    );
    assert!(!events.iter().any(|e| matches!(
        e.event,
        Event::ToolStarted { .. } | Event::ToolFinished { .. }
    )));
    store.close().await.expect("closed store");
    let resumed = resume_session(
        StateRoot::open_existing(&state).expect("state"),
        workspace,
        result.session_id,
    )
    .await
    .expect("resume");
    assert_eq!(resumed.conversation_title(), "hola!");
}
