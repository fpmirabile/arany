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

pub(super) fn before_commit() {
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
        .build()
        .expect("Store test runtime")
}

fn terminal_event(run_id: RunId) -> Event {
    Event::RunFinished {
        run_id,
        disposition: RunDisposition::Failed,
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
                    provider: "scripted".into(),
                    model: "test-model".into(),
                    effort: None,
                    custom_profile_provenance: None,
                    saved_api_account_id: None,
                    chatgpt_provenance: None,
                    output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
                    policy: CollaborationPolicy::Single,
                    output_token_cap: 4096,
                    provider_concurrency: 1,
                    workspace_device: 1,
                    workspace_inode: 1,
                    instruction_digest: None,
                    include_digests: Vec::new(),
                    history_run_ids: Vec::new(),
                    excluded_history_runs: 0,
                    context_usage: None,
                    compaction_event_sequence: None,
                    compaction_content_digest: None,
                    tool_policy: None,
                },
                active_deadline_ms: Some(crate::session::MAX_GOAL_ACTIVE_MS),
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
            } | Event::WorkerDataChunk { .. }
                | Event::WorkerDataSealed { .. }
                | Event::WorkerHandoverRecorded { .. }
                | Event::WorkerIntegrationRecorded { .. }
                | Event::WorkerLeaseReleased { .. }
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
