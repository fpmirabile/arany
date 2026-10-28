use super::*;
use crate::session::{
    AgentDisposition, AgentRole, AgentRunId, CollaborationPolicy, RunConfig, RunDisposition, RunId,
    RunStatus, WorkspaceIdentity,
};
use crate::store::Store;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

static BEFORE_COMMIT_MARKER: OnceLock<PathBuf> = OnceLock::new();

pub(in crate::store) fn before_commit() {
    if let Some(marker) = BEFORE_COMMIT_MARKER.get() {
        std::fs::write(marker, b"inserted").expect("pre-commit marker");
        loop {
            thread::park();
        }
    }
}

struct CrashChild(Option<Child>);

impl CrashChild {
    fn poll(&mut self) -> Option<std::process::ExitStatus> {
        self.0
            .as_mut()
            .expect("live crash child")
            .try_wait()
            .expect("crash child status")
    }

    fn kill_and_reap(&mut self) {
        let mut child = self.0.take().expect("live crash child");
        child.kill().expect("kill crash child");
        assert!(!child.wait().expect("reap crash child").success());
    }
}

impl Drop for CrashChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("Store test runtime")
}

fn terminal_event(run_id: RunId) -> Event {
    Event::RunFinished {
        run_id,
        disposition: RunDisposition::Failed,
        failure: Some(crate::session::RunFailure::ProviderCall),
        active_ms: 0,
    }
}

fn seed_interrupted_run(state: &Path) -> (SessionId, RunId) {
    let session_id = SessionId::new();
    let run_id = RunId::new();
    let primary_id = AgentRunId::new();
    let store =
        Store::open(StateRoot::admit(state).expect("private State root")).expect("seed Store");
    runtime().block_on(async {
        for event in [
            Event::SessionStarted {
                title: "Crash fixture".into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 1,
                },
            },
            Event::MessageAccepted {
                skill: None,
                run_id,
                text: "Synthetic objective".into(),
                images: Vec::new(),
            },
            Event::RunStarted {
                run_id,
                config: RunConfig {
                    supervisor: None,
                    worker_policy: None,
                    agent_definitions: None,
                    roster: None,
                    providers: None,
                    provider: crate::ProviderRecord::parse("openai").expect("Provider record"),
                    model: "test-model".into(),
                    effort: None,
                    custom_profile_provenance: None,
                    saved_api_account_id: None,
                    chatgpt_provenance: None,
                    output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
                    context_window_tokens: None,
                    policy: CollaborationPolicy::Single,
                    output_token_cap: 4096,
                    provider_concurrency: 1,
                    workspace_device: 1,
                    workspace_inode: 1,
                    user_instruction_digest: None,
                    instruction_sources: Vec::new(),
                    launch_directory: String::new(),
                    include_digests: Vec::new(),
                    history_run_ids: Vec::new(),
                    excluded_history_runs: 0,
                    context_usage: None,
                    compaction_event_sequence: None,
                    compaction_content_digest: None,
                    tool_policy: None,
                },
                active_deadline_ms: None,
            },
            Event::AgentSpawned {
                profile: None,
                assignment: None,
                definition: None,
                run_id,
                agent_run_id: primary_id,
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                grant: None,
            },
            Event::AgentFinished {
                worker: None,
                run_id,
                agent_run_id: primary_id,
                disposition: AgentDisposition::Failed,
                summary: None,
                result: None,
            },
        ] {
            store.append(session_id, event).await.expect("seed Event");
        }
        store.close().await.expect("seed Store close");
    });
    (session_id, run_id)
}

#[test]
#[ignore = "supervised Linux Store transaction crash helper"]
fn crash_append_child() {
    let state = PathBuf::from(std::env::var_os("ARANY_TEST_CRASH_STATE").expect("State path"));
    let marker = PathBuf::from(std::env::var_os("ARANY_TEST_CRASH_MARKER").expect("marker path"));
    let stage = std::env::var("ARANY_TEST_CRASH_STAGE").expect("crash stage");
    let session_id =
        SessionId::from_str(&std::env::var("ARANY_TEST_CRASH_SESSION").expect("Session ID"))
            .expect("typed Session ID");
    let run_id = RunId::from_str(&std::env::var("ARANY_TEST_CRASH_RUN").expect("Run ID"))
        .expect("typed Run ID");
    match stage.as_str() {
        "before" => BEFORE_COMMIT_MARKER
            .set(marker.clone())
            .expect("one pre-commit gate"),
        "after" => {}
        _ => panic!("invalid crash stage"),
    }
    let store = Store::open(StateRoot::open_existing(&state).expect("existing State"))
        .expect("crash child Store");
    runtime().block_on(async {
        if let Some(path) = std::env::var_os("ARANY_TEST_CRASH_EVENT") {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            if let Some(id) = value.get("check_id") {
                let id = serde_json::from_value(id.clone()).unwrap();
                let op = if let Some(digest) = value.get("evidence_digest") {
                    crate::journal::check::CheckOp::PublishChatgpt {
                        id,
                        digest: serde_json::from_value(digest.clone()).unwrap(),
                    }
                } else if let Some(record) = value.get("record") {
                    crate::journal::check::CheckOp::PublishProvider {
                        id,
                        record: serde_json::from_value(record.clone()).unwrap(),
                    }
                } else {
                    crate::journal::check::CheckOp::Append {
                        id,
                        event: serde_json::from_value(value["event"].clone()).unwrap(),
                    }
                };
                store.check(op).await.expect("acknowledged check Event");
                return;
            }
        }
        store
            .append(
                session_id,
                if let Some(path) = std::env::var_os("ARANY_TEST_CRASH_EVENT") {
                    let (kind, version, payload, run, actor): (
                        String,
                        i64,
                        String,
                        Option<RunId>,
                        Option<AgentRunId>,
                    ) = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                    Event::decode(&kind, version, &payload, run, actor).unwrap()
                } else {
                    terminal_event(run_id)
                },
            )
            .await
            .expect("acknowledged crash Event");
    });
    if stage == "after" {
        std::fs::write(marker, b"acknowledged").expect("post-commit marker");
        loop {
            thread::park();
        }
    }
    unreachable!("pre-commit gate must hold the Store thread");
}

#[test]
#[ignore = "native Linux Store transaction crash release gate"]
fn process_death_preserves_only_acknowledged_terminal_events() {
    for (stage, expected_len, expected_status) in [
        ("before", 5, RunStatus::Interrupted),
        ("after", 6, RunStatus::Failed),
    ] {
        let temp = tempfile::tempdir().expect("private crash test root");
        let state = temp.path().join("state");
        let marker = temp.path().join("crash-stage");
        let (session_id, run_id) = seed_interrupted_run(&state);
        kill_at_append(&state, session_id, run_id, &marker, stage, None);

        let store = Store::open(StateRoot::open_existing(&state).expect("recovered State"))
            .expect("recovery Store");
        runtime().block_on(async {
            let events = store
                .load_session(session_id)
                .await
                .expect("recovered Events");
            assert_eq!(events.len(), expected_len, "{stage} Event prefix");
            for (index, event) in events.iter().enumerate() {
                assert_eq!(event.sequence, index as u64 + 1, "{stage} sequence");
            }
            let view = SessionView::replay(session_id, &events)
                .expect("strict recovered replay")
                .expect("recovered Session");
            assert_eq!(view.runs.len(), 1);
            assert_eq!(view.runs[0].status, expected_status, "{stage} Run status");
            assert!(view.runs[0].assistant_message.is_none());
            let connection =
                Connection::open(state.join(DATABASE_FILE)).expect("integrity connection");
            let integrity: String = connection
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .expect("SQLite integrity check");
            assert_eq!(integrity, "ok", "{stage} SQLite integrity");
            drop(connection);
            if stage == "before" {
                let committed = store
                    .append(session_id, terminal_event(run_id))
                    .await
                    .expect("post-crash terminal append");
                assert_eq!(committed.sequence, 6);
                let view = store
                    .load_view(session_id)
                    .await
                    .expect("recovered terminal replay")
                    .expect("recovered Session");
                assert_eq!(view.runs[0].status, RunStatus::Failed);
            }
            store.close().await.expect("recovery Store close");
        });
    }
}

fn kill_at_append(
    state: &Path,
    session_id: SessionId,
    run_id: RunId,
    marker: &Path,
    stage: &str,
    event_path: Option<&Path>,
) {
    let marker_bytes = if stage == "before" {
        b"inserted".as_slice()
    } else {
        b"acknowledged".as_slice()
    };
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command.env_clear();
    if let Some(path) = event_path {
        command.env("ARANY_TEST_CRASH_EVENT", path);
    }
    let child = command
        .env("ARANY_TEST_CRASH_STATE", state)
        .env("ARANY_TEST_CRASH_MARKER", marker)
        .env("ARANY_TEST_CRASH_STAGE", stage)
        .env("ARANY_TEST_CRASH_SESSION", session_id.to_string())
        .env("ARANY_TEST_CRASH_RUN", run_id.to_string())
        .args([
            "--exact",
            "store::journal::crash_tests::crash_append_child",
            "--ignored",
            "--nocapture",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn crash child");
    let mut child = CrashChild(Some(child));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let marker_ready = if marker.exists() {
            let observed = std::fs::read(marker).expect("stage marker");
            assert!(marker_bytes.starts_with(&observed), "invalid stage marker");
            observed == marker_bytes
        } else {
            false
        };
        if marker_ready {
            assert!(child.poll().is_none(), "crash child exited before kill");
            break;
        }
        assert!(child.poll().is_none(), "crash child exited before stage");
        assert!(Instant::now() < deadline, "crash stage deadline exceeded");
        thread::yield_now();
    }
    child.kill_and_reap();
}

#[test]
#[ignore = "native Linux Store worker transaction crash release gate"]
fn worker_process_death_preserves_exact_recovery_prefixes() {
    let f = crate::session::worker_protocol_fixture();
    for (index, event) in f.events.iter().enumerate().filter(|(_, e)| {
        matches!(
            e,
            Event::AgentSpawned {
                assignment: Some(_),
                ..
            } | Event::WorkerLeaseReleased { .. }
                | Event::ToolStarted { .. }
                | Event::ToolFinished { .. }
                | Event::AgentFinished {
                    worker: Some(_),
                    ..
                }
                | Event::ProviderCallRecorded {
                    record: crate::session::ProviderCallRecord {
                        disposition: crate::session::ProviderCallDisposition::Finished,
                        ..
                    },
                    ..
                }
        )
    }) {
        for stage in ["before", "after"] {
            let temp = tempfile::tempdir().unwrap();
            let state = temp.path().join("state");
            let event_path = temp.path().join("event.json");
            let marker = temp.path().join("stage");
            let store = Store::open(StateRoot::admit(&state).unwrap()).unwrap();
            runtime().block_on(async {
                for event in &f.events[..index] {
                    store.append(f.session, event.clone()).await.unwrap();
                }
                store.close().await.unwrap();
            });
            let (run, actor) = event.scope();
            std::fs::write(
                &event_path,
                serde_json::to_vec(&(
                    event.kind(),
                    event.version(),
                    event.payload().unwrap(),
                    run,
                    actor,
                ))
                .unwrap(),
            )
            .unwrap();
            kill_at_append(&state, f.session, f.run, &marker, stage, Some(&event_path));
            let expected_len = index + usize::from(stage == "after");
            let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
            runtime().block_on(async {
                let events = store.load_session(f.session).await.unwrap();
                assert_eq!(
                    events.iter().map(|e| &e.event).collect::<Vec<_>>(),
                    f.events[..expected_len].iter().collect::<Vec<_>>(),
                    "{} {stage}",
                    event.kind()
                );
                let actual = store.load_view(f.session).await.unwrap().unwrap();
                let expected = SessionView::replay(f.session, &f.envelopes()[..expected_len])
                    .unwrap()
                    .unwrap();
                assert_eq!(actual, expected, "{} {stage}", event.kind());
                assert_eq!(
                    actual.runs[0].worker_history(),
                    expected.runs[0].worker_history()
                );
                store.close().await.unwrap();
            });
        }
    }
}

#[test]
#[ignore = "native Linux Store rewind transaction crash release gate"]
fn rewind_process_death_preserves_exact_recovery_prefixes() {
    for finish in [false, true] {
        for stage in ["before", "after"] {
            let f = crate::session::worker_protocol_fixture();
            let temp = tempfile::tempdir().unwrap();
            let state = temp.path().join("state");
            let event_path = temp.path().join("event.json");
            let marker = temp.path().join("stage");
            let store = Store::open(StateRoot::admit(&state).unwrap()).unwrap();
            let (event, prefix) = runtime().block_on(async {
                for event in &f.events {
                    store.append(f.session, event.clone()).await.unwrap();
                }
                let view = store.load_view(f.session).await.unwrap().unwrap();
                let begin = crate::session::rewind_started_fixture(
                    &view,
                    view.rewinds.checkpoints[0].clone(),
                );
                let event = if finish {
                    store.append(f.session, begin).await.unwrap();
                    let view = store.load_view(f.session).await.unwrap().unwrap();
                    crate::session::rewind_finished_fixture(
                        &view,
                        crate::session::rewind::RewindOutcome::Restored,
                    )
                } else {
                    begin
                };
                let prefix = store.load_session(f.session).await.unwrap();
                store.close().await.unwrap();
                (event, prefix)
            });
            let (run, actor) = event.scope();
            std::fs::write(
                &event_path,
                serde_json::to_vec(&(
                    event.kind(),
                    event.version(),
                    event.payload().unwrap(),
                    run,
                    actor,
                ))
                .unwrap(),
            )
            .unwrap();
            kill_at_append(&state, f.session, f.run, &marker, stage, Some(&event_path));
            let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
            runtime().block_on(async {
                let events = store.load_session(f.session).await.unwrap();
                assert_eq!(&events[..prefix.len()], prefix.as_slice());
                assert_eq!(events.len(), prefix.len() + usize::from(stage == "after"));
                if stage == "after" {
                    assert_eq!(events.last().unwrap().event, event);
                }
                let view = store.load_view(f.session).await.unwrap().unwrap();
                assert_eq!(
                    view,
                    SessionView::replay(f.session, &events).unwrap().unwrap()
                );
                if finish || stage == "after" {
                    let rewind = view.rewinds.operations.last().unwrap();
                    let restored = finish && stage == "after";
                    assert_eq!(
                        rewind.outcome() == crate::session::rewind::RewindOutcome::Restored,
                        restored
                    );
                    assert_eq!(rewind.finished.is_some(), restored);
                    let resume = store
                        .append(f.session, Event::GoalResumed { goal_id: f.goal })
                        .await;
                    assert_eq!(resume.is_ok(), restored);
                }
                store.close().await.unwrap();
            });
        }
    }
}

#[tokio::test]
#[ignore = "native Linux Store plan-observation transaction crash release gate"]
async fn plan_observation_process_death_preserves_only_committed_attempt_samples() {
    observation_crash_prefix(false).await;
}

#[test]
#[cfg_attr(not(arany_ci), ignore)]
fn api_observation_process_death_preserves_only_committed_attempt_samples() {
    let _gate = crate::test_process::NATIVE_GATE.lock().unwrap();
    runtime().block_on(observation_crash_prefix(true));
}

async fn observation_crash_prefix(api: bool) {
    use crate::test_support::{engine::run_to_end, routing::*, scenario::ScriptedProvider};
    let source = tempfile::tempdir().unwrap();
    let workspace = source.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = source.path().join("state");
    let mut provider = ScriptedProvider::new(vec![finish("done")]);
    let samples = if api {
        [Some(api_sample(1000)), Some(api_sample(1001))]
    } else {
        provider = provider.with_chatgpt_account(uuid::Uuid::now_v7(), [1; 32]);
        [Some(plan_sample(1000)), Some(plan_sample(1001))]
    };
    provider
        .responses
        .get_mut()
        .unwrap()
        .push_front(Err(crate::ProviderError::RateLimited(None)));
    provider.rate_limits.get_mut().unwrap().extend(samples);
    let provider = route(provider);
    let engine = open(&state);
    let session = engine
        .create_session(workspace.clone(), None)
        .await
        .unwrap();
    crate::FaultPlan::skip_retry_backoff(session);
    let mut request = input(&workspace);
    request.session_id = Some(session);
    request.title = None;
    let outcome = run_to_end(&engine, &provider, request).await.unwrap();
    engine.close().await.unwrap();
    let (events, _) = reopen(&state, session).await;
    for (index, envelope) in events.iter().enumerate().filter(|(_, envelope)| {
        matches!(
            envelope.event,
            Event::ProviderCallRecorded { .. } | Event::ProviderRetryScheduled { .. }
        )
    }) {
        for stage in ["before", "after"] {
            let temp = tempfile::tempdir().unwrap();
            let state = temp.path().join("state");
            let event_path = temp.path().join("event.json");
            let marker = temp.path().join("stage");
            let store = Store::open(StateRoot::admit(&state).unwrap()).unwrap();
            for prefix in &events[..index] {
                store.append(session, prefix.event.clone()).await.unwrap();
            }
            store.close().await.unwrap();
            let event = &envelope.event;
            let (run, actor) = event.scope();
            std::fs::write(
                &event_path,
                serde_json::to_vec(&(
                    event.kind(),
                    event.version(),
                    event.payload().unwrap(),
                    run,
                    actor,
                ))
                .unwrap(),
            )
            .unwrap();
            kill_at_append(
                &state,
                session,
                outcome.run.id,
                &marker,
                stage,
                Some(&event_path),
            );
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let actual = store.load_session(session).await.unwrap();
            let expected_len = index + usize::from(stage == "after");
            assert_eq!(
                actual
                    .iter()
                    .map(|e| e.event.payload().unwrap())
                    .collect::<Vec<_>>(),
                events[..expected_len]
                    .iter()
                    .map(|e| e.event.payload().unwrap())
                    .collect::<Vec<_>>()
            );
            let view = SessionView::replay(session, &actual).unwrap().unwrap();
            let expected = SessionView::replay(session, &events[..expected_len])
                .unwrap()
                .unwrap();
            assert_eq!(view.usage, expected.usage);
            if api {
                let receipt = match (&envelope.event, stage) {
                    (Event::ProviderRetryScheduled { .. }, "before") => None,
                    (Event::ProviderCallRecorded { .. }, "after") => Some(1001),
                    _ => Some(1000),
                };
                assert_eq!(view.usage.api_limits.len(), usize::from(receipt.is_some()));
                if let Some(receipt) = receipt {
                    assert_eq!(
                        view.usage.api_limits[0].rows,
                        api_sample(receipt).api.unwrap().rows
                    );
                }
            }
            assert_eq!(
                provider.requests.lock().unwrap().len(),
                2,
                "replay cannot dispatch"
            );
            store.close().await.unwrap();
        }
    }
}

#[test]
#[cfg_attr(not(arany_ci), ignore)]
fn provider_check_process_death_never_completes_unconfirmed_publication() {
    use crate::journal::check::{
        CheckDisposition, CheckEvent, CheckJournal, CheckOp,
        tests::{fixture, start_fixture},
    };
    use crate::provider::evidence::ProviderEvidenceRecord;
    for index in [2, 3, 8, 9] {
        for stage in ["before", "after"] {
            let temp = tempfile::tempdir().unwrap();
            let state = temp.path().join("state");
            let marker = temp.path().join("stage");
            let event_path = temp.path().join("check.json");
            let (id, mut events) = fixture();
            let now = crate::journal::check::now_ms().unwrap();
            let record = ProviderEvidenceRecord {
                name: "local".into(),
                digest: [1; 32],
                addresses: vec!["127.0.0.1:9321".parse().unwrap()],
                checked_at_ms: now,
                expires_at_ms: now + 60_000,
            };
            let digest = crate::store::check::provider_digest(&record).unwrap();
            events[8] = CheckEvent::ProviderCheckEvidenceStarted {
                digest,
                snapshot: record.digest,
            };
            events[9] = CheckEvent::ProviderCheckFinished {
                disposition: CheckDisposition::Succeeded,
                evidence: Some(digest),
            };
            let store = Store::open(StateRoot::admit(&state).unwrap()).unwrap();
            runtime().block_on(async {
                store
                    .check(CheckOp::Start {
                        retained: None,
                        id,
                        start: start_fixture(),
                    })
                    .await
                    .unwrap();
                for event in &events[2..index] {
                    store
                        .check(CheckOp::Append {
                            id,
                            event: event.clone(),
                        })
                        .await
                        .unwrap();
                }
                store.close().await.unwrap();
            });
            let packet = if index == 9 {
                serde_json::json!({"check_id":id,"record":record})
            } else {
                serde_json::json!({"check_id":id,"event":events[index]})
            };
            std::fs::write(&event_path, serde_json::to_vec(&packet).unwrap()).unwrap();
            kill_at_append(
                &state,
                SessionId::new(),
                RunId::new(),
                &marker,
                stage,
                Some(&event_path),
            );
            let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
            runtime().block_on(async {
                let view = store.load_check(id).await.unwrap().unwrap();
                let success = index == 9 && stage == "after";
                assert_eq!(view.events, index + usize::from(stage == "after"));
                assert_eq!(
                    view.replay_disposition(),
                    if success {
                        CheckDisposition::Succeeded
                    } else {
                        CheckDisposition::Uncertain
                    }
                );
                assert_eq!(
                    store.load_provider_evidence("local".into()).await.unwrap(),
                    success.then_some(record)
                );
                store.close().await.unwrap();
                let next =
                    CheckJournal::start(StateRoot::open_existing(&state).unwrap(), start_fixture())
                        .await
                        .unwrap();
                assert_eq!(next.view().completed_calls, 0);
                next.close().await.unwrap();
            });
        }
    }
}

#[test]
#[cfg_attr(not(arany_ci), ignore)]
fn chatgpt_check_process_death_never_acknowledges_an_unfinished_external_publication() {
    use crate::journal::check::{
        CheckDisposition, CheckEvent, CheckJournal, CheckOp, tests::chatgpt_fixture,
    };
    for index in [1, 2, 3, 4, 10, 11] {
        for stage in ["before", "after"] {
            let temp = tempfile::tempdir().unwrap();
            let state = temp.path().join("state");
            let marker = temp.path().join("stage");
            let packet_path = temp.path().join("check.json");
            let (id, events) = chatgpt_fixture();
            let CheckEvent::ProviderCheckStarted { start } = events[0].clone() else {
                unreachable!()
            };
            let store = Store::open(StateRoot::admit(&state).unwrap()).unwrap();
            runtime().block_on(async {
                store
                    .check(CheckOp::Start {
                        id,
                        start: start.clone(),
                        retained: Some(Vec::new()),
                    })
                    .await
                    .unwrap();
                for event in &events[1..index] {
                    store
                        .check(CheckOp::Append {
                            id,
                            event: event.clone(),
                        })
                        .await
                        .unwrap();
                }
                store.close().await.unwrap();
            });
            let packet = if index == events.len() - 1 {
                serde_json::json!({"check_id":id,"evidence_digest":vec![3; 32]})
            } else {
                serde_json::json!({"check_id":id,"event":events[index]})
            };
            std::fs::write(&packet_path, serde_json::to_vec(&packet).unwrap()).unwrap();
            kill_at_append(
                &state,
                SessionId::new(),
                RunId::new(),
                &marker,
                stage,
                Some(&packet_path),
            );
            let root = StateRoot::open_existing(&state).unwrap();
            let success = index == events.len() - 1 && stage == "after";
            assert_eq!(
                root.check_evidence_valid(id, [1; 32], [3; 32]).unwrap(),
                success
            );
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            runtime().block_on(async {
                let view = store.load_check(id).await.unwrap().unwrap();
                assert_eq!(view.events, index + usize::from(stage == "after"));
                assert_eq!(
                    view.replay_disposition(),
                    if success {
                        CheckDisposition::Succeeded
                    } else {
                        CheckDisposition::Uncertain
                    }
                );
                store.close().await.unwrap();
                let next = CheckJournal::start(StateRoot::open_existing(&state).unwrap(), start)
                    .await
                    .unwrap();
                assert_eq!(next.view().completed_calls, 0);
                next.close().await.unwrap();
            });
            assert!(!root.check_evidence_valid(id, [1; 32], [3; 32]).unwrap());
        }
    }
}

#[test]
fn host_verdict_process_death_reopens_without_outcome_or_retry() {
    let f = crate::session::host_protocol_fixture();
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let event_path = temp.path().join("verdict.json");
    let marker = temp.path().join("committed");
    let store = Store::open(StateRoot::admit(&state).unwrap()).unwrap();
    runtime().block_on(async {
        for event in &f.events[..f.events.len() - 1] {
            store.append(f.session, event.clone()).await.unwrap();
        }
        store.close().await.unwrap();
    });
    let verdict = f.events.last().unwrap();
    let (run, actor) = verdict.scope();
    std::fs::write(
        &event_path,
        serde_json::to_vec(&(
            verdict.kind(),
            verdict.version(),
            verdict.payload().unwrap(),
            run,
            actor,
        ))
        .unwrap(),
    )
    .unwrap();
    kill_at_append(
        &state,
        f.session,
        f.run,
        &marker,
        "after",
        Some(&event_path),
    );
    for _ in 0..2 {
        let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
        runtime().block_on(async {
            let events = store.load_session(f.session).await.unwrap();
            assert_eq!(
                events.iter().map(|e| &e.event).collect::<Vec<_>>(),
                f.events.iter().collect::<Vec<_>>()
            );
            let view = store.load_view(f.session).await.unwrap().unwrap();
            assert_eq!(view.runs[0].status, RunStatus::Interrupted);
            let tool = view.runs[0]
                .tools
                .iter()
                .find(|t| t.intent.call.is_host())
                .unwrap();
            assert!(tool.ran_host() && tool.observation.is_none());
            assert!(
                crate::session::rewind::plan(&view, &view.rewinds.checkpoints[0])
                    .unwrap()
                    .non_rewindable
                    .iter()
                    .any(|(_, effect)| *effect == crate::session::rewind::NonRewindable::Host)
            );
            store.close().await.unwrap();
        });
    }
}
