use arany::test_support::goals::facts_workspace;
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

#[path = "engine_run/compaction.rs"]
mod compaction;

#[path = "engine_run/persistence.rs"]
mod persistence;

#[path = "engine_run/delegation.rs"]
mod delegation;

#[path = "engine_run/tool_steps.rs"]
mod tool_steps;

#[path = "engine_run/performance.rs"]
mod performance;

#[path = "engine_run/goals.rs"]
mod goals;

#[path = "engine_run/budgets.rs"]
mod budgets;

#[path = "engine_run/plan_contract.rs"]
mod plan_contract;

#[path = "engine_run/team.rs"]
mod team;

#[path = "engine_run/steering.rs"]
mod steering;

#[path = "engine_run/recovery.rs"]
mod recovery;

#[path = "engine_run/routing.rs"]
mod routing;

#[path = "engine_run/roster_journeys.rs"]
mod roster_journeys;

#[path = "engine_run/hooks.rs"]
mod hooks;

#[path = "engine_run/permissions.rs"]
mod permissions;

pub use arany::test_support::process;

use arany::test_support::engine as engine_support;

async fn next_committed(events: &mut arany::Subscription, what: &str) -> Arc<arany::EventEnvelope> {
    match tokio::time::timeout(Duration::from_secs(5), events.next()).await {
        Ok(Some(arany::Tapped::Committed(event))) => event,
        _ => panic!("{what}"),
    }
}
use engine_support::{run_to_end, run_with};

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
    #[cfg(debug_assertions)]
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

#[path = "engine_run/workspace_security.rs"]
mod workspace_security;

fn pad_capacity_history(path: &Path, session: SessionId, prefix: i64, branch: bool) {
    let mut connection = rusqlite::Connection::open(path.join("events.sqlite3")).unwrap();
    let transaction = connection.transaction().unwrap();
    let (first_padding_sequence, last_padding_sequence) = if branch {
        let (first, boundary): (i64, i64) = transaction
            .query_row(
                "SELECT MIN(sequence), MAX(CASE WHEN kind = 'RunFinished' THEN sequence END) FROM events WHERE session_id = ?1",
                [session.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let padding = prefix - boundary;
        assert!(padding > 0);
        // Fork capacity counts the selected prefix, so padding must precede its Run boundary.
        transaction
            .execute(
                "UPDATE events SET sequence = sequence + ?1 WHERE session_id = ?2 AND sequence > ?3",
                rusqlite::params![padding, session.to_string(), first],
            )
            .unwrap();
        (first + 1, first + padding)
    } else {
        let first: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (first, prefix)
    };
    for sequence in first_padding_sequence..=last_padding_sequence {
        transaction.execute("INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',?3,'{\"title\":\"x\"}',0)", rusqlite::params![sequence, session.to_string(), arany::test_support::payload_version("SessionRenamed")]).unwrap();
    }
    transaction.commit().unwrap();
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
fn same_session_operation_is_excluded_while_other_sessions_continue() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        for branch in [false, true] {
            let temp = tempfile::tempdir().expect("temporary root");
            let workspace = facts_workspace(temp.path(), "Facts");
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
        let workspace = facts_workspace(temp.path(), "Fact");
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
fn provider_call_timeout_fails_without_a_final_message() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
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
        let workspace = facts_workspace(temp.path(), "Fact");
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
fn user_instructions_lead_the_project_files_and_are_pinned_by_digest() {
    use std::os::unix::fs::PermissionsExt;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("AGENTS.md"), "Root rule").expect("root");
        std::fs::write(workspace.join("notes/AGENTS.md"), "Notes rule").expect("nested");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let state = StateRoot::admit(&state_path).expect("private state");
        let user_file = state_path.join("AGENTS.md");
        std::fs::write(&user_file, "User rule").expect("user instruction");
        std::fs::set_permissions(&user_file, std::fs::Permissions::from_mode(0o600))
            .expect("user instruction mode");
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: "Done".into(),
        })]);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(state, EngineConfig::default()).expect("engine");
        let mut nested = request(&workspace, "Question");
        nested.launch_directory = Some(workspace.join("notes"));
        let outcome = run_to_end(&engine, &provider, nested)
            .await
            .expect("run completes");
        let guidance: Vec<_> = requests.lock().expect("requests lock")[0]
            .input
            .workspace_guidance
            .iter()
            .map(|guidance| (guidance.path.clone(), guidance.text.clone()))
            .collect();
        assert_eq!(
            guidance,
            [
                ("user AGENTS.md".to_owned(), "User rule".to_owned()),
                ("AGENTS.md".to_owned(), "Root rule".to_owned()),
                ("notes/AGENTS.md".to_owned(), "Notes rule".to_owned()),
            ]
        );
        let config = outcome.run.config.expect("pinned config");
        assert_eq!(
            config.user_instruction_digest,
            Some(Sha256::digest(b"User rule").into())
        );
        assert_eq!(config.instruction_sources.len(), 2);
        engine.close().await.expect("engine shutdown");
    });
}

#[test]
fn prompt_overrides_are_pinned_rendered_and_unsafe_ones_refuse_admission() {
    use arany::{PromptFile, PromptOverrideError, PromptSource};
    use std::os::unix::fs::PermissionsExt;

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
        let state = StateRoot::admit(&state_path).expect("private state");
        let prompts = state_path.join("prompts");
        std::fs::create_dir(&prompts).expect("prompt directory");
        std::fs::set_permissions(&prompts, std::fs::Permissions::from_mode(0o700))
            .expect("prompt directory mode");
        let primary = prompts.join("primary.md");
        std::fs::write(&primary, "Work tersely.").expect("override");
        std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o600))
            .expect("override mode");
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: "Done".into(),
        })]);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(state, EngineConfig::default()).expect("engine");
        let outcome = run_to_end(&engine, &provider, request(&workspace, "Question"))
            .await
            .expect("run completes");
        assert_eq!(
            requests.lock().expect("requests lock")[0]
                .input
                .prompts
                .text(PromptFile::Primary),
            "Work tersely."
        );
        let config = outcome.run.config.expect("pinned config");
        let mut expected = arany::EffectivePrompts::default().pins();
        expected[0].source = PromptSource::Override;
        expected[0].digest = Sha256::digest(b"Work tersely.").into();
        assert_eq!(expected[0].file, PromptFile::Primary);
        assert_eq!(config.prompts, expected);

        std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o644))
            .expect("unsafe mode");
        let error = run_to_end(&engine, &provider, request(&workspace, "Again"))
            .await
            .expect_err("unsafe override refuses admission");
        assert!(matches!(
            error,
            EngineError::PromptOverride(PromptOverrideError::NotPrivate { .. })
        ));
        assert!(error.to_string().contains(&primary.display().to_string()));
        assert_eq!(requests.lock().expect("requests lock").len(), 1);
        engine.close().await.expect("engine shutdown");
    });
}

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
        symlink(
            temp.path().join("outside"),
            workspace.join("notes/AGENTS.md"),
        )
        .expect("symlinked nested instruction");
        let mut nested = request(&workspace, "Question");
        nested.launch_directory = Some(workspace.join("notes"));
        assert!(matches!(
            run_to_end(&engine, &provider, nested).await,
            Err(EngineError::WorkspaceUnavailable)
        ));
        std::fs::remove_file(workspace.join("notes/AGENTS.md")).expect("remove nested symlink");
        // 32 sources with sixfold-escaped names overflow RunStarted; nothing may commit first.
        let mut launch = workspace.join("long");
        std::fs::create_dir(&launch).expect("long tree");
        for level in 0..30 {
            std::fs::write(launch.join("AGENTS.md"), "x").expect("nested instruction");
            launch.push(if level < 2 {
                "\u{1}".repeat(255)
            } else {
                "s".into()
            });
            std::fs::create_dir(&launch).expect("long tree level");
        }
        std::fs::write(launch.join("AGENTS.md"), "x").expect("deepest instruction");
        assert!(
            launch.join("AGENTS.md").as_os_str().len() < 1024,
            "within macOS PATH_MAX"
        );
        std::fs::write(workspace.join("AGENTS.md"), "x").expect("root instruction");
        let mut long = request(&workspace, "Question");
        long.launch_directory = Some(launch);
        assert!(matches!(
            run_to_end(&engine, &provider, long).await,
            Err(EngineError::InputTooLarge)
        ));
        std::fs::remove_dir_all(workspace.join("long")).expect("remove long tree");
        std::fs::remove_file(workspace.join("AGENTS.md")).expect("remove root instruction");
        let mut traversal = request(&workspace, "Question");
        traversal.include_paths = vec!["../outside".into()];
        assert!(matches!(
            run_to_end(&engine, &provider, traversal).await,
            Err(EngineError::InputPath { source, .. })
                if matches!(source.as_ref(), EngineError::Tool(arany::ToolError::OutsideWorkspace))
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

#[test]
fn workspace_limits_and_state_overlap_fail_before_provider_disclosure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
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
        // Nested files share one cap; the second file overflowing it rejects the turn.
        std::fs::write(workspace.join("AGENTS.md"), vec![b'a'; 32 * 1024]).expect("root");
        std::fs::write(workspace.join("notes/AGENTS.md"), vec![b'n'; 32 * 1024 + 1])
            .expect("nested instruction");
        let mut nested = request(&workspace, "Question");
        nested.launch_directory = Some(workspace.join("notes"));
        assert!(matches!(
            run_to_end(&engine, &provider, nested).await,
            Err(EngineError::InputTooLarge)
        ));
        std::fs::remove_file(workspace.join("AGENTS.md")).expect("clear test instruction");
        std::fs::remove_file(workspace.join("notes/AGENTS.md")).expect("clear nested");
        // The user file has its own cap, privacy and encoding checks; none truncates or falls back.
        use std::os::unix::fs::PermissionsExt;
        let user_file = temp.path().join("state/AGENTS.md");
        for (bytes, mode) in [
            (vec![b'u'; 64 * 1024 + 1], 0o600),
            (b"rule".to_vec(), 0o644),
            (vec![0xff], 0o600),
        ] {
            std::fs::write(&user_file, &bytes).expect("user instruction");
            std::fs::set_permissions(&user_file, std::fs::Permissions::from_mode(mode))
                .expect("user instruction mode");
            let result = run_to_end(&engine, &provider, request(&workspace, "Question")).await;
            if bytes == [0xff] {
                assert!(matches!(result, Err(EngineError::InputNotUtf8)));
            } else {
                assert!(matches!(result, Err(EngineError::Store(_))), "{mode:o}");
            }
        }
        std::fs::remove_file(&user_file).expect("clear user instruction");

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
        let mut too_much_context = request(&workspace, &"large paste ".repeat(800));
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

#[tokio::test]
async fn one_engine_refuses_a_concurrent_submission_and_takes_a_provider_per_submission() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "Fact");
    let state_path = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("private state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let answer = |text: &str| ProviderOutcome::Answer(Answer { text: text.into() });
    let first =
        Arc::new(ScriptedProvider::new(vec![answer("first")]).with_profile("openai", "model-a"));
    let second =
        Arc::new(ScriptedProvider::new(vec![answer("second")]).with_profile("openai", "model-b"));

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
    let workspace = facts_workspace(temp.path(), "Fact");
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
fn direct_cancellation_drops_the_call_and_persists_a_cancelled_run() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
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
                    let workspace = facts_workspace(temp.path(), "Fact");
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
                        responses.push_back(Ok(delegate(&[("A", None)])));
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
                    // An uncancelled 5xx retries the same route three times before it settles.
                    let retried = case == "service unavailable" && !cancel;
                    if retried {
                        for _ in 0..arany::MAX_PROVIDER_RETRIES {
                            responses.push_back(Err(ProviderError::RemoteHttp(503)));
                            expected_phases.push(phase);
                        }
                    }
                    if failure_reason == Some(arany::ProviderFailureReason::OutcomeContract) && !cancel {
                        responses.push_back(Err(ProviderError::InvalidOutcomeContract));
                        expected_phases.push(phase);
                    }
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
                    let run = run_with(&engine, &provider, run_request, RunControls { cancellation, ..Default::default() });
                    // Paused time skips the backoff; a paused watchdog would fire while the Store thread works.
                    let outcome = if retried {
                        tokio::time::pause();
                        let outcome = run.await;
                        tokio::time::resume();
                        Ok(outcome)
                    } else {
                        tokio::time::timeout(Duration::from_secs(5), run).await
                    }
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
                    if failure_reason == Some(arany::ProviderFailureReason::ProposalRefused) {
                        assert_eq!(record.refusal.as_ref().expect("semantic refusal").cause,
                            arany::ProposalRefusalCause::AnswerInvalid);
                    }
                    assert!(provider.responses.lock().expect("responses").is_empty(),
                        "{phase:?}, {case}, cancellation {cancel}");
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
                        "only the scripted calls and refusal retries"
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
        delegate(&[("A", None)]),
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
