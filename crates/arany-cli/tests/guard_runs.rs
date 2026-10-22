use arany::{
    AdmittedProviders, AgentPhase, AgentRunId, AgentStatus, Answer, CollaborationPolicy,
    CompactionFailure, CompactionItem, CompactionRequest, CompactionResponse, CompactionStatus,
    ContextUsage, Delegate, Effort, Engine, EngineConfig, EngineError, Event, ImageOrigin,
    PromptRole, Provider, ProviderCallDisposition, ProviderError, ProviderImage, ProviderOutcome,
    ProviderRequest, ProviderResponse, RunCancellation, RunControls, RunId, RunInput, RunStatus,
    RunSteering, SessionId, SessionView, StateRoot, Store, ToolGrant, continue_session,
    fork_session, list_sessions, rename_session, resume_session,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

#[cfg(unix)]
pub use arany::test_support::process;
use arany::test_support::{engine as engine_support, scenario::*};

use engine_support::{run_to_end, run_with};

#[path = "guard_runs/latency.rs"]
mod latency;

#[test]
#[ignore = "owner-run Linux release latency corpus; never a CI timing gate"]
fn linux_tool_latency_corpus() {
    latency::corpus();
}

#[path = "guard_runs/rewind.rs"]
mod rewind;

#[path = "guard_runs/worker.rs"]
mod worker;

#[path = "guard_runs/permissions.rs"]
mod permissions;

#[path = "guard_runs/unguarded.rs"]
mod unguarded;

#[cfg(target_os = "linux")]
#[path = "guard_runs/git_writes.rs"]
mod git_writes;

fn tool_report(observation: Option<&arany::ToolObservation>) -> String {
    observation.map_or_else(
        || "no observation".to_owned(),
        |observation| {
            let failure = arany::test_support::recorded_tool_failure(observation.intent.id)
                .unwrap_or_else(|| "no recorded failure".to_owned());
            format!(
                "{:?} {:?} [{failure}]",
                observation.disposition, observation.output
            )
        },
    )
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
    if std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").is_none() {
        assert!(root.starts_with(std::fs::canonicalize(std::env::temp_dir()).unwrap()));
    }
}

async fn wait_for_child_finish(store: &Store, session_id: SessionId, objective: &str) {
    loop {
        // The Engine's journal commit can briefly lock this observer's separate connection.
        let events = match store.load_session(session_id).await {
            Err(arany::StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
                if error.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                tokio::task::yield_now().await;
                continue;
            }
            events => events.expect("observable Events"),
        };
        let target = events.iter().find_map(|envelope| match &envelope.event {
            Event::AgentSpawned {
                agent_run_id,
                objective: Some(value),
                ..
            } if value == objective => Some(*agent_run_id),
            _ => None,
        });
        if target.is_some_and(|id| events.iter().any(|envelope|
            matches!(&envelope.event, Event::AgentFinished { agent_run_id, .. } if *agent_run_id == id))) {
            return;
        }
        tokio::task::yield_now().await;
    }
}

fn observe_process(
    command: &mut std::process::Command,
    observation_root: &Path,
    stdout_limit: usize,
) -> std::process::Output {
    use std::io::{Read, Seek, SeekFrom};
    use std::process::{Child, Output, Stdio};

    struct OwnedChild(Option<Child>);

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let mut stdout = tempfile::tempfile_in(observation_root).expect("private stdout");
    let mut stderr = tempfile::tempfile_in(observation_root).expect("private stderr");
    #[cfg(debug_assertions)]
    command.env(
        "ARANY_TEST_ACCOUNT_ROOT",
        arany::test_support::isolate_account_root(),
    );
    let mut child = OwnedChild(Some(
        command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().expect("stdout handle"))
            .stderr(stderr.try_clone().expect("stderr handle"))
            .spawn()
            .expect("observed process"),
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child
            .0
            .as_mut()
            .expect("owned process")
            .try_wait()
            .expect("process status")
        {
            break status;
        }
        assert!(std::time::Instant::now() < deadline, "process deadline");
        let stdout_bytes = stdout.metadata().expect("stdout size").len();
        let stderr_bytes = stderr.metadata().expect("stderr size").len();
        assert!(
            stdout_bytes <= stdout_limit as u64,
            "stdout output bound: {stdout_bytes} bytes exceeds {stdout_limit}"
        );
        assert!(
            stderr_bytes <= 256 * 1024,
            "stderr output bound: {stderr_bytes} bytes exceeds 262144"
        );
        std::thread::yield_now();
    };
    child
        .0
        .take()
        .expect("owned process")
        .wait()
        .expect("process reaped");
    let mut output = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    for (file, bytes, limit) in [
        (&mut stdout, &mut output.stdout, stdout_limit),
        (&mut stderr, &mut output.stderr, 256 * 1024),
    ] {
        file.seek(SeekFrom::Start(0)).expect("observation rewind");
        file.take(limit as u64 + 1)
            .read_to_end(bytes)
            .expect("bounded observation");
        assert!(
            bytes.len() <= limit,
            "process observation bound: {} bytes exceeds {limit}",
            bytes.len()
        );
    }
    output
}

#[test]
fn scripted_runs_snapshot_workspace_and_replay_across_process_exit() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("AGENTS.md"), "Primary instruction").expect("instruction");
        std::fs::write(workspace.join("CLAUDE.md"), "Ignored fallback").expect("fallback");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact one").expect("include");
        let state_path = temp.path().join("state");
        let account_id = Uuid::now_v7();
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer { text: "First result".into() }),
            ProviderOutcome::Answer(Answer { text: "Second result".into() }),
        ])
        .with_profile("openai", "gpt-5.4")
        .with_saved_api_account(account_id)
        .with_effort(Effort::Medium);
        let recorded_requests = Arc::clone(&provider.requests);
        let first_image = image_fixture(64 * 1024);
        let second_image = image_fixture(69);
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::admit(&state_path).expect("private state"), EngineConfig::default())
        .expect("engine");
        let mut first_request = request(&workspace, "First question");
        first_request.title = None;
        first_request.images = vec![first_image.clone()];
        let first = run_to_end(&engine, &provider, first_request)
            .await
            .expect("first Run");
        assert_eq!(first.run.status, RunStatus::Finished);
        assert_eq!(
            first
                .run
                .config
                .as_ref()
                .expect("pinned config")
                .saved_api_account_id,
            Some(account_id)
        );
        assert_eq!(
            first.run.config.as_ref().expect("pinned config").effort,
            Some(Effort::Medium)
        );
        assert_eq!(first.run.assistant_message.as_deref(), Some("First result"));
        let instruction_digest: [u8; 32] = Sha256::digest(b"Primary instruction").into();
        let include_digest: [u8; 32] = Sha256::digest(b"Fact one").into();
        assert_eq!(
            first
                .run
                .config
                .as_ref()
                .expect("pinned config")
                .instruction_sources,
            [arany::InstructionSource {
                directory: String::new(),
                file: Some(arany::InstructionFile::Agents),
                digest: instruction_digest
            }]
        );
        assert_eq!(
            first
                .run
                .config
                .as_ref()
                .expect("pinned config")
                .include_digests,
            vec![include_digest]
        );

        std::fs::write(workspace.join("AGENTS.md"), "Changed instruction")
            .expect("new instruction");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact two").expect("new include");
        let mut second_request = request(&workspace, "Second question");
        second_request.session_id = Some(first.session_id);
        second_request.title = None;
        second_request.images = vec![second_image.clone()];
        let second = run_to_end(&engine, &provider, second_request).await.expect("second Run");
        assert_eq!(second.session_id, first.session_id);
        assert_eq!(second.run.status, RunStatus::Finished);
        assert_eq!(
            second
                .run
                .config
                .as_ref()
                .expect("pinned config")
                .history_run_ids,
            vec![first.run.id]
        );
        {
            let requests = recorded_requests.lock().expect("requests lock");
            assert_eq!(requests.len(), 2);
            for (call, (objective, run)) in requests.iter().zip([
                ("First question", &first.run),
                ("Second question", &second.run),
            ]) {
                assert_eq!(call.phase, AgentPhase::RootPlan);
                assert!(call.model == "gpt-5.4", "selected model");
                assert!(call.input.objective == objective, "exact objective");
                assert!(call.input.context_summary.is_none(), "no derived summary");
                assert!(call.input.child_results.is_empty(), "no child context");
                assert_eq!(call.input.goal_budget, None);
                assert_eq!(call.input.goal_plan_contract, None);
                assert_eq!(call.max_output_tokens, 4096);
                assert_eq!(call.run_id, run.id);
                assert_eq!(call.agent_run_id, run.agents[0].id);
            }
            assert_eq!(
                requests[0].input.workspace_guidance.iter().map(|guidance| guidance.text.as_str()).collect::<Vec<_>>(),
                ["Primary instruction"]
            );
            assert_eq!(requests[0].input.includes, [include_data("notes/facts.txt", "Fact one")]);
            assert_eq!(requests[0].input.images, vec![ProviderImage {
                origin: ImageOrigin::Objective, image: first_image.clone(),
            }]);
            assert!(requests[0].input.history.is_empty());
            assert_eq!(
                requests[1].input.workspace_guidance.iter().map(|guidance| guidance.text.as_str()).collect::<Vec<_>>(),
                ["Changed instruction"]
            );
            assert_eq!(requests[1].input.includes, [include_data("notes/facts.txt", "Fact two")]);
            assert_eq!(requests[1].input.history.len(), 1);
            assert_eq!(requests[1].input.history[0].user, "First question");
            assert_eq!(requests[1].input.history[0].assistant, "First result");
            assert_eq!(requests[1].input.images, vec![
                ProviderImage { origin: ImageOrigin::History { turn_index: 0 }, image: first_image.clone() },
                ProviderImage { origin: ImageOrigin::Objective, image: second_image.clone() },
            ]);
        }
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(first.session_id)
            .await
            .expect("durable journal");
        assert_eq!(events.len(), 23);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalStarted { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalFinished { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
                .count(),
            2
        );
        let view = SessionView::replay(first.session_id, &events)
            .expect("valid replay")
            .expect("Session");
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.runs[0].images, std::slice::from_ref(&first_image));
        assert_eq!(view.runs[1].images, std::slice::from_ref(&second_image));
        assert!(view.runs.iter().all(|run| {
            run.config.as_ref().expect("replayed config").effort == Some(Effort::Medium)
        }));
        assert!(view.runs.iter().all(|run| {
            run.config
                .as_ref()
                .expect("replayed config")
                .saved_api_account_id
                == Some(account_id)
        }));
        assert!(
            view.runs
                .iter()
                .all(|run| run.status == RunStatus::Finished)
        );
        let listed = list_sessions(StateRoot::open_existing(&state_path).unwrap(), workspace.clone()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "First question", "the conversation keeps its first objective as the title");
        assert!(arany_cli::presentation::render_session(&view, &events, arany_cli::presentation::Output::Text).starts_with(&format!("Session: {}\nTitle: {}\n", view.id, listed[0].title)));
        assert_eq!(listed[0].defaults.provider.as_deref(), Some("openai"));
        assert_eq!(listed[0].defaults.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(listed[0].defaults.account_id, Some(account_id));
        assert_eq!(listed[0].defaults.effort, Some(Effort::Medium));
        assert_eq!(listed[0].created_at.len(), 19);
        assert_eq!(listed[0].last_activity_at.len(), 19);
        store.close().await.expect("store shutdown");


        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate { agent: None, children: vec![arany::ChildAssignment { editor: None, objective: "Inspect current image".into(), profile: None }] }),
            ProviderOutcome::Answer(Answer { text: "Child answer".into() }),
            ProviderOutcome::Answer(Answer { text: "Team answer".into() }),
            ProviderOutcome::Answer(Answer { text: "Large image answer".into() }),
            ProviderOutcome::Answer(Answer { text: "Summary answer".into() }),
            ProviderOutcome::Answer(Answer { text: "Fork answer".into() }),
        ]).with_compactions(vec![Ok(CompactionResponse {
            summary: "Image conversation summary".into(), response_id: None,
            usage: arany::Usage {
                input_tokens: Some(50),
                output_tokens: Some(12),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let requests = Arc::clone(&provider.requests);
        let compactions = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::open_existing(&state_path).unwrap(), EngineConfig::default()).unwrap();
        let mut next = request(&workspace, "");
        next.title = None;
        next.session_id = Some(first.session_id);
        next.images = vec![second_image.clone()];
        next.policy = CollaborationPolicy::Team { max_active_children: 1 };
        let third = run_to_end(&engine, &provider, next).await.expect("image-only team after reopen");
        let fork_id = engine.fork_session(first.session_id, workspace.clone(), Some("Image fork".into())).await.unwrap();
        let large_image = image_fixture(arany::MAX_IMAGE_BYTES);
        let maximal_objective = "\0".repeat(64 * 1024);
        // Mixed prose escapes about 1x; control characters would exceed the compaction bound.
        let realistic_objective = "Review the module and keep its contract. ".repeat(1_600);
        let mut next = request(&workspace, &realistic_objective[..64 * 1024]);
        next.title = None;
        next.session_id = Some(first.session_id);
        next.images = vec![large_image.clone()];
        let fourth = run_to_end(&engine, &provider, next).await.expect("whole-history image clipping");
        assert!(fourth.run.config.as_ref().unwrap().history_run_ids.is_empty());
        assert_eq!(fourth.run.config.as_ref().unwrap().excluded_history_runs, 3);
        let compacted = engine.compact_session(&AdmittedProviders::single(provider.clone()), first.session_id, workspace.clone()).await.unwrap();
        assert_eq!(compacted.compiler_version, 3);
        for (id, objective) in [(first.session_id, maximal_objective.as_str()), (fork_id, "After fork")] {
            let mut next = request(&workspace, objective);
            next.session_id = Some(id);
            next.title = None;
            assert_eq!(run_to_end(&engine, &provider, next).await.unwrap().run.status, RunStatus::Finished);
        }
        engine.close().await.unwrap();
        {
            let calls = requests.lock().unwrap();
            assert_eq!(calls.len(), 6, "complete image journey call count");
            let history = vec![
                arany::HistoryTurn { user: "First question".into(), assistant: "First result".into() },
                arany::HistoryTurn { user: "Second question".into(), assistant: "Second result".into() },
            ];
            let historical_images = vec![
                ProviderImage { origin: ImageOrigin::History { turn_index: 0 }, image: first_image.clone() },
                ProviderImage { origin: ImageOrigin::History { turn_index: 1 }, image: second_image.clone() },
            ];
            let current = ProviderImage { origin: ImageOrigin::Objective, image: second_image.clone() };
            let mut all_images = historical_images.clone();
            all_images.push(current.clone());
            for (index, phase, objective, expected_history, expected_images, child_results) in [
                (0, AgentPhase::RootPlan, "", history.clone(), all_images.clone(), Vec::new()),
                (1, AgentPhase::ChildWork, "Inspect current image", Vec::new(), vec![current], Vec::new()),
                (2, AgentPhase::RootSynthesis, "", history.clone(), all_images, vec![arany::ChildResult {
                    objective: "Inspect current image".into(), summary: "Child answer".into(), result: "Child answer".into(),
                }]),
                (3, AgentPhase::RootPlan, &realistic_objective[..64 * 1024], Vec::new(), vec![ProviderImage { origin: ImageOrigin::Objective, image: large_image.clone() }], Vec::new()),
                (4, AgentPhase::RootPlan, maximal_objective.as_str(), Vec::new(), Vec::new(), Vec::new()),
                (5, AgentPhase::RootPlan, "After fork", [history, vec![arany::HistoryTurn { user: "".into(), assistant: "Team answer".into() }]].concat(), [historical_images, vec![ProviderImage { origin: ImageOrigin::History { turn_index: 2 }, image: second_image.clone() }]].concat(), Vec::new()),
            ] {
                let mut actual = calls[index].clone();
                let facts = actual.input.goal_context_facts.take();
                assert_eq!(actual, ProviderRequest { session_id: if index == 5 { fork_id } else { first.session_id }, run_id: calls[index].run_id, agent_run_id: calls[index].agent_run_id, phase, collaboration: if index == 0 || index == 2 { CollaborationPolicy::Team { max_active_children: 1 } } else { CollaborationPolicy::Single }, model: "test-model".into(), max_output_tokens: 4096, input: RunInput { images: expected_images, goal_budget: None, workspace_guidance: vec![arany::WorkspaceGuidance { path: "AGENTS.md".into(), text: "Changed instruction".into() }], includes: vec![include_data("notes/facts.txt", "Fact two")], history: expected_history, context_summary: (index == 4).then(|| "Image conversation summary".into()), child_results, ..RunInput::new(PromptRole::ReadOnly, objective.into()) } }, "image request {index}");
                if phase == AgentPhase::ChildWork {
                    assert!(facts.is_none(), "child calls do not receive primary Goal state");
                } else {
                    assert!(facts.is_some(), "primary calls receive current Goal state");
                }
            }
            assert!(calls[..3].iter().all(|call| call.run_id == third.run.id));
            assert_eq!(calls[0].agent_run_id, third.run.agents[0].id);
            assert_eq!(calls[1].agent_run_id, third.run.agents[1].id);
            assert_eq!(calls[2].agent_run_id, third.run.agents[0].id);
            assert_eq!(calls[3].run_id, fourth.run.id);
            let compactions = compactions.lock().unwrap();
            assert_eq!(compactions.len(), 1);
            let compact = &compactions[0];
            assert_eq!(compact.session_id, first.session_id);
            assert_eq!(compact.covered_run_id, fourth.run.id);
            assert_eq!(compact.model, "test-model");
            assert_eq!(compact.max_output_tokens, 1024);
            assert!(compact.previous_summary.is_none());
            assert_eq!(compact.items.len(), 4);
            let mut source_items = Vec::new();
            for (item, (objective, answer, image)) in compact.items.iter().zip([
                ("First question", "First result", &first_image),
                ("Second question", "Second result", &second_image),
                ("", "Team answer", &second_image),
                (&realistic_objective[..64 * 1024], "Large image answer", &large_image),
            ]) {
                let digest: String = image.digest().iter().map(|byte| format!("{byte:02x}")).collect();
                let user = format!("{objective}\nImage 1: PNG 1x1, {} bytes, SHA-256 {digest}", image.byte_len());
                assert_eq!(item, &CompactionItem::Completed(arany::HistoryTurn { user: user.clone(), assistant: answer.into() }));
                assert!(!user.contains(image.base64()), "compaction excludes image bytes");
                source_items.push(serde_json::json!({"status": "completed", "user": user, "assistant": answer}));
            }
            // The recorded source is the serialized compaction input: no previous summary, then the completed items.
            let source = serde_json::json!({"previous_derived_summary": null, "items": source_items});
            assert_eq!(
                compacted.source_bytes as usize,
                serde_json::to_vec(&source).unwrap().len()
            );
        }
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(first.session_id).await.unwrap();
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
        // Each Provider call reserves its call; each Run settles its active time once in RunFinished.
        assert_eq!(goal_event_counts, [5, 5, 7, 5]);
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
        assert_eq!(non_goal_events, 41);
        assert_eq!(events.len(), 63);
        let view = SessionView::replay(first.session_id, &events).unwrap().unwrap();
        for (run_id, phase) in [
            (first.run.id, AgentPhase::RootPlan),
            (third.run.id, AgentPhase::RootPlan),
            (third.run.id, AgentPhase::ChildWork),
        ] {
            let missing_call: Vec<_> = events.iter().filter(|envelope| {
                !matches!(&envelope.event, Event::ProviderCallRecorded { run_id: id, record, .. } if *id == run_id && record.phase == phase)
            }).cloned().collect();
            assert!(SessionView::replay(first.session_id, &missing_call).is_err(), "missing observed call for {phase:?}");
        }
        assert_eq!(view.runs.len(), 5);
        assert_eq!(view.runs[0].images, std::slice::from_ref(&first_image));
        assert_eq!(view.runs[3].images, [large_image]);
        assert!(view.runs[3].objective == realistic_objective[..64 * 1024], "image objective survives");
        assert!(view.runs[4].objective == maximal_objective, "text-only objective survives escaping");
        assert_eq!(view.runs[2].objective, "", "image-only objective is not invented text");
        assert_eq!(view.compactions[0].record, compacted);
        for output in ["text", "jsonl"] {
            let expected = arany_cli::presentation::render_session(
                &view,
                &events,
                if output == "text" {
                    arany_cli::presentation::Output::Text
                } else {
                    arany_cli::presentation::Output::Jsonl
                },
            );
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arany"));
            command.env_clear().current_dir(temp.path()).args(["show", "--output", output, "--state-dir"])
                .arg(&state_path).arg(first.session_id.to_string());
            let observed = observe_process(&mut command, temp.path(), expected.len());
            assert!(observed.status.success(), "image show {output} exit");
            assert!(observed.stderr.is_empty(), "image show {output} error channel");
            assert!(observed.stdout == expected.as_bytes(), "image show {output} bytes differ");
            if output == "text" {
                let text = String::from_utf8(observed.stdout).unwrap();
                assert!(text.contains("Image 1: PNG 1x1, 192.0 KB"));
                assert!(!text.contains("iVBOR"), "human show leaked encoded image bytes");
            } else {
                let lines = std::str::from_utf8(&observed.stdout).unwrap().lines().map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()).collect::<Vec<_>>();
                assert_eq!(lines.len(), events.len());
                let accepted = lines.iter().filter(|line| line["kind"] == "MessageAccepted").collect::<Vec<_>>();
                assert_eq!(accepted.len(), 5);
                assert_eq!(accepted[0]["event_version"], arany::test_support::payload_version("MessageAccepted"));
                assert_eq!(accepted[0]["payload"]["images"][0]["media_type"], "image/png");
                assert_eq!(accepted[0]["payload"]["images"][0]["data"], view.runs[0].images[0].base64());
                assert_eq!(accepted[2]["payload"]["text"], "");
                assert!(accepted[3]["payload"]["text"].as_str() == Some(&realistic_objective[..64 * 1024]));
                assert_eq!(accepted[4]["event_version"], arany::test_support::payload_version("MessageAccepted"));
                assert!(accepted[4]["payload"]["text"].as_str() == Some(maximal_objective.as_str()));
                assert_eq!(accepted[4]["payload"].to_string().len(), 6 * 64 * 1024 + 36, "maximally escaped text-only input fits its Event");
                assert_eq!(accepted[4]["payload"]["images"], serde_json::json!([]), "text-only input retains the current Message shape");
            }
        }
        let fork = store.load_view(fork_id).await.unwrap().unwrap();
        assert_eq!(fork.runs.len(), 4);
        assert_eq!(fork.runs[0].images, [first_image]);
        assert_eq!(fork.runs[2].images, std::slice::from_ref(&second_image));
        assert!(fork.compactions.is_empty());
        store.close().await.unwrap();

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
        let (sequence, payload): (i64, String) = connection.query_row(
            "SELECT sequence, payload FROM events WHERE session_id=?1 AND kind='MessageAccepted' ORDER BY sequence LIMIT 1",
            [first.session_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        let original: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let message_version = arany::test_support::payload_version("MessageAccepted");
        for fault in ["unsupported version", "unknown version", "missing images", "empty Message", "five images", "aggregate bytes", "non-PNG", "corrupted image", "URL", "extra field", "text overflow", "payload overflow", "agent scope"] {
            let mut invalid = original.clone();
            let mut version = message_version;
            let mut agent_scope = None;
            match fault {
                "unsupported version" => version = message_version - 1,
                "unknown version" => version = message_version + 1,
                "missing images" => { invalid.as_object_mut().unwrap().remove("images"); }
                "empty Message" => { invalid["text"] = "".into(); invalid["images"] = serde_json::json!([]); },
                "five images" => invalid["images"] = serde_json::to_value(vec![second_image.clone(); 5]).unwrap(),
                "aggregate bytes" => invalid["images"] = serde_json::to_value(vec![view.runs[0].images[0].clone(); 4]).unwrap(),
                "non-PNG" => invalid["images"][0]["media_type"] = "image/jpeg".into(),
                "corrupted image" => invalid["images"][0]["data"] = "not PNG".into(),
                "URL" => invalid["images"][0]["data"] = "https://example.invalid/image.png".into(),
                "extra field" => invalid["images"][0]["path"] = "/outside.png".into(),
                "text overflow" => invalid["text"] = "x".repeat(64 * 1024 + 1).into(),
                "payload overflow" => invalid["text"] = "x".repeat(656 * 1024).into(),
                "agent scope" => agent_scope = Some(AgentRunId::new().to_string()),
                _ => unreachable!(),
            }
            connection.execute("UPDATE events SET event_version=?1, payload=?2, agent_run_id=?3 WHERE sequence=?4",
                rusqlite::params![version, invalid.to_string(), agent_scope, sequence]).unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
            assert!(matches!(store.load_view(first.session_id).await, Err(arany::StoreError::InvalidHistory)), "image replay fault: {fault}");
            store.close().await.unwrap();
        }
        connection.execute("UPDATE events SET event_version=?1, payload=?2, agent_run_id=NULL WHERE sequence=?3", rusqlite::params![message_version, payload, sequence]).unwrap();
        drop(connection);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert_eq!(store.load_view(first.session_id).await.unwrap().unwrap(), view);
        assert_eq!(store.load_view(fork_id).await.unwrap().unwrap(), fork);
        store.close().await.unwrap();
        rename_session(StateRoot::admit(&state_path).unwrap(), workspace.clone(), first.session_id, "My conversation".into()).await.unwrap();
        let listed = list_sessions(StateRoot::open_existing(&state_path).unwrap(), workspace.clone()).await.unwrap();
        assert_eq!(listed[0].title, "My conversation");

    });
}

#[test]
fn compaction_is_derived_durable_context_and_failure_keeps_the_prior_summary() {
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
                text: "Answer one".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Answer two".into(),
            }),
        ])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Derived summary".into(),
            response_id: Some("compact-1".into()),
            usage: arany::Usage {
                input_tokens: Some(100),
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
        let first = run_to_end(&engine, &provider, request(&workspace, "Question one"))
            .await
            .expect("first Run");
        let mut second_request = request(&workspace, "Question two");
        second_request.session_id = Some(first.session_id);
        second_request.title = None;
        let second = run_to_end(&engine, &provider, second_request)
            .await
            .expect("second Run");
        let second_config = second.run.config.as_ref().expect("pinned context usage");
        assert_eq!(
            second_config.context_usage,
            Some(ContextUsage {
                used_bytes: ("Question two".len()
                    + include_data("notes/facts.txt", "Fact").len()
                    + 86) as u32,
                budget_bytes: 376 * 1024 - unplanned_goal_metadata_bytes(),
                compactable_bytes: 86,
                tool_history_bytes: 0,
            })
        );
        let mut pressure = second_config.clone();
        for (used, budget, compactable, due) in [
            (79, 100, 10, false),
            (80, 100, 9, false),
            (80, 100, 10, true),
            (100, 100, 10, true),
            // The automatic threshold is 158_080: the input bound less room for two 8 KiB-draft turns.
            (158_079, 376 * 1024, 158_079, false),
            (158_080, 376 * 1024, 158_080, true),
            (158_080, 376 * 1024, 0, false),
        ] {
            pressure.context_usage = Some(ContextUsage {
                used_bytes: used,
                budget_bytes: budget,
                compactable_bytes: compactable,
                tool_history_bytes: 0,
            });
            assert_eq!(
                pressure.auto_compaction_due(),
                due,
                "{used}/{budget}, {compactable} compactable"
            );
        }
        pressure.context_usage = None;
        assert!(!pressure.auto_compaction_due(), "unknown footprint");
        for (tool_history, bytes, due) in
            [(1, 158_079, false), (1, 158_080, true), (0, 158_080, true)]
        {
            pressure.context_usage = Some(ContextUsage {
                used_bytes: bytes,
                budget_bytes: 376 * 1024,
                compactable_bytes: bytes,
                tool_history_bytes: tool_history,
            });
            assert_eq!(
                pressure.auto_compaction_due(),
                due,
                "Tool history shares the compaction threshold"
            );
        }
        pressure.context_usage = None;
        pressure.history_run_ids = vec![RunId::new(); 22];
        assert!(
            !pressure.auto_compaction_due(),
            "below history count threshold"
        );
        pressure.history_run_ids.push(RunId::new());
        assert!(pressure.auto_compaction_due(), "history count threshold");
        let compacted = engine
            .auto_compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
                second.run.id,
            )
            .await
            .expect("durable automatic compaction")
            .expect("new boundary");
        assert_eq!(compacted.covered_run_id, second.run.id);
        assert_eq!(compacted.provider, "scripted");
        assert_eq!(compacted.input_tokens, Some(100));
        assert_eq!(compacted.output_tokens, Some(8));
        assert!(matches!(
            &compacted.status,
            CompactionStatus::Succeeded { summary, .. } if summary == "Derived summary"
        ));
        {
            let requests = compact_requests.lock().expect("compaction request lock");
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].items.len(), 2);
            assert!(matches!(
                &requests[0].items[0],
                CompactionItem::Completed(turn) if turn.user == "Question one"
            ));
            assert!(matches!(
                &requests[0].items[1],
                CompactionItem::Completed(turn) if turn.assistant == "Answer two"
            ));
            assert!(requests[0].previous_summary.is_none());
            assert_eq!(requests[0].max_output_tokens, 1024);
        }
        assert!(matches!(
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    first.session_id,
                    workspace.clone()
                )
                .await,
            Err(EngineError::CompactionInputEmpty)
        ));
        assert!(
            engine
                .auto_compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    first.session_id,
                    workspace.clone(),
                    second.run.id
                )
                .await
                .expect("duplicate automatic boundary")
                .is_none()
        );
        assert_eq!(compact_requests.lock().expect("requests").len(), 1);
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(first.session_id)
            .await
            .expect("durable Events");
        let compact_event_sequence = events.last().expect("compaction Event").sequence;
        assert!(matches!(
            events.last().expect("compaction Event").event,
            Event::ContextCompacted { .. }
        ));
        let shown = observe_process(
            std::process::Command::new(env!("CARGO_BIN_EXE_arany"))
                .env_clear()
                .current_dir(temp.path())
                .args(["show", "--state-dir"])
                .arg(&state_path)
                .args(["--output", "jsonl", &first.session_id.to_string()]),
            temp.path(),
            256 * 1024,
        );
        assert!(shown.status.success());
        assert_eq!(shown.stderr, b"");
        assert!(shown.stdout.ends_with(b"\n"));
        assert_eq!(
            shown.stdout.iter().filter(|byte| **byte == b'\n').count(),
            events.len()
        );
        let lines: Vec<serde_json::Value> = shown
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("JSONL Event"))
            .collect();
        let snapshot_line = lines.last().expect("visible compaction receipt");
        assert_eq!(snapshot_line["kind"], "ContextCompacted");
        assert_eq!(snapshot_line["payload"]["record"]["input_tokens"], 100);
        assert_eq!(snapshot_line["payload"]["record"]["output_tokens"], 8);
        assert_eq!(
            snapshot_line["payload"]["record"]["status"]["summary"],
            "Derived summary"
        );
        let view = store
            .load_view(first.session_id)
            .await
            .expect("durable view")
            .expect("Session exists");
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.compactions.len(), 1);
        assert_eq!(
            view.runs[1]
                .config
                .as_ref()
                .expect("replayed config")
                .context_usage,
            second_config.context_usage
        );
        store.close().await.expect("store shutdown");

        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "Answer three".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Answer four".into(),
            }),
        ])
        .with_profile("changed-provider", "changed-model")
        .with_compactions(vec![Err(ProviderError::RemoteHttp(403))]);
        let observed = Arc::clone(&provider.requests);
        let resumed_compacts = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("resumed engine");
        let mut third_request = request(&workspace, "Question three");
        third_request.session_id = Some(first.session_id);
        third_request.title = None;
        let third = run_to_end(&engine, &provider, third_request)
            .await
            .expect("resumed Run");
        assert_eq!(
            third.run.config.as_ref().expect("config").provider,
            "changed-provider"
        );
        assert_eq!(
            third
                .run
                .config
                .as_ref()
                .expect("config")
                .compaction_event_sequence,
            Some(compact_event_sequence)
        );
        let failed = engine
            .auto_compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
                third.run.id,
            )
            .await
            .expect("durable failed automatic compaction")
            .expect("new boundary");
        assert!(matches!(
            failed.status,
            CompactionStatus::Failed {
                reason: CompactionFailure::ProviderRejected
            }
        ));
        assert!(
            engine
                .auto_compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    first.session_id,
                    workspace.clone(),
                    third.run.id
                )
                .await
                .expect("failed automatic boundary not retried")
                .is_none()
        );
        let mut fourth_request = request(&workspace, "Question four");
        fourth_request.session_id = Some(first.session_id);
        fourth_request.title = None;
        run_to_end(&engine, &provider, fourth_request)
            .await
            .expect("Run after failure");
        assert!(
            engine
                .auto_compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    first.session_id,
                    workspace.clone(),
                    third.run.id
                )
                .await
                .expect("stale automatic boundary")
                .is_none()
        );
        assert_eq!(resumed_compacts.lock().expect("requests").len(), 1);
        {
            let requests = observed.lock().expect("request lock");
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[0].input.context_summary.as_deref(),
                Some("Derived summary")
            );
            assert!(requests[0].input.history.is_empty());
            assert_eq!(
                requests[1].input.context_summary.as_deref(),
                Some("Derived summary")
            );
            assert_eq!(requests[1].input.history.len(), 1);
            assert_eq!(requests[1].input.history[0].user, "Question three");
        }
        engine.close().await.expect("resumed engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("final reopened store");
        let view = store
            .load_view(first.session_id)
            .await
            .expect("final replay")
            .expect("Session exists");
        assert_eq!(view.runs.len(), 4);
        assert_eq!(view.compactions.len(), 2);
        assert_eq!(
            view.runs[0].assistant_message.as_deref(),
            Some("Answer one")
        );
        assert_eq!(
            view.runs[1].assistant_message.as_deref(),
            Some("Answer two")
        );
        store.close().await.expect("final store shutdown");
    });
}

#[test]
fn team_children_finish_in_reverse_order_but_synthesis_receives_assignment_order() {
    use std::process::Command;

    const SCENARIO: &str =
        "team_children_finish_in_reverse_order_but_synthesis_receives_assignment_order";
    const SEED_ROOT: &str = "ARANY_TEST_SESSION_JOURNEY_ROOT";

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");

    if let Some(root) = std::env::var_os(SEED_ROOT) {
        let root = std::path::PathBuf::from(root);
        runtime.block_on(async {
            let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
                text: "Seed answer".into(),
            })])
            .with_compactions(vec![Ok(CompactionResponse {
                summary: "Derived summary".into(),
                response_id: Some("compact-seed".into()),
                usage: arany::Usage {
                    input_tokens: Some(20),
                    output_tokens: Some(8),
                    ..Default::default()
                },
                wire_provenance: None,
            })]);
            let requests = Arc::clone(&provider.requests);
            let compacts = Arc::clone(&provider.compact_requests);
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&root.join("state")).expect("private seed State"),
                EngineConfig::default(),
            )
            .expect("seed Engine");
            let outcome = run_to_end(
                &engine,
                &provider,
                request(&root.join("workspace"), "Seed question"),
            )
            .await
            .expect("seed single-agent Run");
            assert_eq!(outcome.run.status, RunStatus::Finished);
            assert_eq!(outcome.run.agents.len(), 1);
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    outcome.session_id,
                    root.join("workspace"),
                )
                .await
                .expect("seed derived summary");
            engine.close().await.expect("seed Engine shutdown");
            let requests = requests.lock().expect("seed requests");
            assert_eq!(requests.len(), 1);
            assert_test_call(
                &requests[0],
                AgentPhase::RootPlan,
                "Seed question",
                &["Fact"],
                &[],
                None,
                &[],
            );
            assert_eq!(requests[0].run_id, outcome.run.id);
            assert_eq!(requests[0].agent_run_id, outcome.run.agents[0].id);
            let compacts = compacts.lock().expect("seed compactions");
            assert_eq!(compacts.len(), 1);
            assert_eq!(compacts[0].session_id, outcome.session_id);
            assert_eq!(compacts[0].covered_run_id, outcome.run.id);
            assert!(compacts[0].model == "test-model", "seed compaction model");
            assert!(
                compacts[0].previous_summary.is_none(),
                "seed compaction summary"
            );
            assert!(
                compacts[0].items
                    == [CompactionItem::Completed(arany::HistoryTurn {
                        user: "Seed question".into(),
                        assistant: "Seed answer".into(),
                    })],
                "seed compaction source"
            );
            assert_eq!(compacts[0].max_output_tokens, 1024);
        });
        return;
    }

    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let seeded = observe_process(
            Command::new(std::env::current_exe().expect("absolute test executable"))
                .env_clear()
                .env(SEED_ROOT, temp.path())
                .current_dir(temp.path())
                .args(["--exact", SCENARIO, "--nocapture"]),
            temp.path(),
            256 * 1024,
        );
        assert!(seeded.status.success(), "seed process exit");
        assert!(
            seeded
                .stdout
                .windows(b"running 1 test\n".len())
                .any(|part| part == b"running 1 test\n")
        );
        assert_eq!(seeded.stderr, b"");
        process::assert_single_test_completed(&seeded.stdout, "seed");
        let selected = continue_session(
            StateRoot::open_existing(&state_path).expect("seed State"),
            workspace.clone(),
        )
        .await
        .expect("discover committed seed Session");
        let resumed = resume_session(
            StateRoot::open_existing(&state_path).expect("seed State"),
            workspace.clone(),
            selected.id,
        )
        .await
        .expect("explicit fresh-process resume");
        assert_eq!(selected, resumed);
        assert_eq!(resumed.runs.len(), 1);
        assert_eq!(resumed.runs[0].config.as_ref().expect("seed config").policy, CollaborationPolicy::Single);
        assert_eq!(resumed.runs[0].status, RunStatus::Finished);
        assert_eq!(
            resumed.runs[0].assistant_message.as_deref(),
            Some("Seed answer")
        );
        let session_id = resumed.id;
        let seed_run_id = resumed.runs[0].id;
        let summary_sequence = resumed.compactions[0].event_sequence;

        let (started, mut started_rx) = mpsc::unbounded_channel();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = GatedProvider {
            started,
            requests: Arc::clone(&requests),
            fail_child: None,
            identity: gated_identity(3),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::admit(&state_path).expect("state readmission"), EngineConfig::default())
        .expect("engine");
        let observer = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("observer");
        let mut run_request = request(&workspace, "Question");
        run_request.session_id = Some(session_id);
        run_request.title = None;
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 3,
        };
        let mut tapped = engine.subscribe();
        let mut live = resumed.clone();
        let mut run_task = tokio::spawn(async move {
            let outcome = run_with(&engine, &provider, run_request, RunControls { cancellation: RunCancellation::new(), ..Default::default() })
                .await;
            let close = engine.close().await;
            (outcome, close)
        });
        let controlled = tokio::time::timeout(Duration::from_secs(5), async {
            let mut gates = HashMap::new();
            for _ in 0..3 {
                let (objective, release) = started_rx.recv().await.expect("child started");
                gates.insert(objective, release);
            }
            let committed = observer.load_session(session_id).await.expect("committed activity");
            let sequence = committed.last().expect("last Event").sequence;
            while live.last_sequence < sequence {
                let Some(arany::Tapped::Committed(event)) = tapped.next().await else { panic!("committed activity"); };
                arany::reduce_into(&mut live, &event).expect("valid activity");
            }
            let active = live.runs.last().unwrap();
            assert_eq!(active.status, RunStatus::Active);
            assert_eq!(active.agents.len(), 4);
            assert!(active.agents.iter().all(|agent| agent.status == AgentStatus::Active));
            for objective in ["C", "B", "A"] {
                gates
                    .remove(objective)
                    .expect("release gate")
                    .send(())
                    .expect("live child");
                wait_for_child_finish(&observer, session_id, objective).await;
                loop {
                    let Some(arany::Tapped::Committed(event)) = tapped.next().await else { panic!("committed child completion"); };
                    arany::reduce_into(&mut live, &event).expect("valid child completion");
                    let child = live.runs.last().unwrap()
                        .agents
                        .iter()
                        .find(|agent| agent.objective.as_deref() == Some(objective))
                        .expect("admitted child");
                    if child.status == AgentStatus::Finished {
                        break;
                    }
                }
            }
        })
        .await;
        match controlled {
            Ok(()) => (),
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("team child coordination exceeded its test deadline");
            }
        };
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
        let (outcome, close) = match joined {
            Ok(result) => result.expect("Run task"),
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("team Run exceeded its test deadline");
            }
        };
        let outcome = outcome.expect("successful team Run");
        close.expect("engine shutdown");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(outcome.run.agents.len(), 4);
        let team_config = outcome.run.config.as_ref().expect("resumed team config");
        assert_eq!(team_config.provider, "gated");
        assert_ne!(team_config.provider, resumed.runs[0].config.as_ref().expect("seed config").provider);
        assert_eq!(team_config.compaction_event_sequence, Some(summary_sequence));
        while live.runs.last().unwrap().status != RunStatus::Finished {
            let Some(arany::Tapped::Committed(event)) = tapped.next().await else { panic!("committed Run completion"); };
            arany::reduce_into(&mut live, &event).expect("valid Run completion");
        }
        assert_eq!(live.runs.last().unwrap(), &outcome.run);
        assert_eq!(Some(live.last_sequence), outcome.run.finished_sequence);
        {
            let requests = requests.lock().expect("requests lock");
            assert_eq!(requests.len(), 5);
            assert_test_call(
                &requests[0], AgentPhase::RootPlan, "Question", &["Fact"], &[], Some("Derived summary"), &[],
            );
            let mut children = requests[1..4].iter().collect::<Vec<_>>();
            children.sort_by(|left, right| left.input.objective.cmp(&right.input.objective));
            for (call, objective) in children.into_iter().zip(["A", "B", "C"]) {
                assert_test_call(call, AgentPhase::ChildWork, objective, &["Fact"], &[], None, &[]);
                let agent = outcome.run.agents.iter().find(|agent| agent.objective.as_deref() == Some(objective)).expect("assigned child");
                assert_eq!(call.agent_run_id, agent.id);
            }
            assert_test_call(
                &requests[4], AgentPhase::RootSynthesis, "Question", &["Fact"], &[], Some("Derived summary"),
                &["A", "B", "C"].map(|objective| arany::ChildResult {
                    objective: objective.into(), summary: format!("result:{objective}"), result: format!("result:{objective}"),
                }),
            );
            assert!(requests.iter().all(|call| call.run_id == outcome.run.id));
            assert_eq!(requests[0].agent_run_id, outcome.run.agents[0].id);
            assert_eq!(requests[4].agent_run_id, requests[0].agent_run_id);
        }
        let events = observer
            .load_session(session_id)
            .await
            .expect("durable Events");
        let child_ids: HashMap<AgentRunId, &str> = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                Event::AgentSpawned {
                    agent_run_id,
                    objective: Some(value),
                    ..
                } => Some((*agent_run_id, value.as_str())),
                _ => None,
            })
            .collect();
        let finish_order: Vec<_> = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                Event::AgentFinished { agent_run_id, .. } => child_ids.get(agent_run_id).copied(),
                _ => None,
            })
            .collect();
        assert_eq!(finish_order, ["C", "B", "A"]);
        let call_order: Vec<_> = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                Event::ProviderCallRecorded {
                    agent_run_id,
                    record,
                    ..
                } if record.phase == AgentPhase::ChildWork => {
                    assert_eq!(record.disposition, ProviderCallDisposition::Finished);
                    child_ids.get(agent_run_id).copied()
                }
                _ => None,
            })
            .collect();
        assert_eq!(call_order, ["C", "B", "A"]);
        observer.close().await.expect("observer shutdown");

        let fork_id = fork_session(
            StateRoot::open_existing(&state_path).expect("fork State"),
            workspace.clone(),
            session_id,
            Some("Branch".into()),
        )
        .await
        .expect("fork team boundary");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer { text: "Later parent answer".into() }),
            ProviderOutcome::Answer(Answer { text: "Branch answer".into() }),
        ])
        .with_profile("gated", "test-model")
        .with_compactions(vec![Err(ProviderError::RemoteHttp(403))]);
        let continued_requests = Arc::clone(&provider.requests);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::open_existing(&state_path).expect("continued State"), EngineConfig::default())
        .expect("continued Engine");
        let failed = engine.compact_session(&AdmittedProviders::single(provider.clone()), session_id, workspace.clone()).await.expect("recorded compaction failure");
        assert_eq!(failed.covered_run_id, outcome.run.id);
        assert!(matches!(failed.status, CompactionStatus::Failed { reason: CompactionFailure::ProviderRejected }));
        let mut continued_request = request(&workspace, "Continue parent");
        continued_request.session_id = Some(session_id);
        continued_request.title = None;
        let continued = run_to_end(&engine, &provider, continued_request).await.expect("parent Run after failed compaction");
        let mut branch_request = request(&workspace, "Branch question");
        branch_request.session_id = Some(fork_id);
        branch_request.title = None;
        let branch = run_to_end(&engine, &provider, branch_request).await.expect("fork Run after parent extension");
        engine.close().await.expect("continued Engine shutdown");
        for run in [&continued.run, &branch.run] {
            assert_eq!(run.status, RunStatus::Finished);
            let config = run.config.as_ref().expect("continued config");
            assert_eq!(config.compaction_event_sequence, Some(summary_sequence));
            assert_eq!(config.history_run_ids, [outcome.run.id]);
        }
        {
            let requests = continued_requests.lock().expect("continued requests");
            assert_eq!(requests.len(), 2);
            for (call, (objective, run)) in requests.iter().zip([("Continue parent", &continued.run), ("Branch question", &branch.run)]) {
                assert_test_call(
                    call, AgentPhase::RootPlan, objective, &["Fact"],
                    &[arany::HistoryTurn { user: "Question".into(), assistant: "A B C".into() }],
                    Some("Derived summary"), &[],
                );
                assert_eq!(call.run_id, run.id);
                assert_eq!(call.agent_run_id, run.agents[0].id);
            }
            let compacts = compact_requests.lock().expect("compaction requests");
            assert_eq!(compacts.len(), 1);
            assert_eq!(compacts[0].session_id, session_id);
            assert_eq!(compacts[0].covered_run_id, outcome.run.id);
            assert!(compacts[0].model == "test-model", "continued compaction model");
            assert_eq!(compacts[0].max_output_tokens, 1024);
            assert_eq!(compacts[0].previous_summary.as_deref(), Some("Derived summary"));
            assert_eq!(compacts[0].items.len(), 1);
            assert!(matches!(&compacts[0].items[0], CompactionItem::Completed(turn) if turn.user == "Question" && turn.assistant == "A B C"));
        }

        let (started, mut started_rx) = mpsc::unbounded_channel();
        let cancelled_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(GatedProvider { started, requests: Arc::clone(&cancelled_requests), fail_child: None, identity: gated_identity(3), });
        let engine = engine_support::open(StateRoot::open_existing(&state_path).expect("cancellation State"), EngineConfig::default())
        .expect("cancellation Engine");
        let cancellation = RunCancellation::new();
        let run_cancellation = cancellation.clone();
        let mut cancel_request = request(&workspace, "Cancel team");
        cancel_request.session_id = Some(session_id);
        cancel_request.title = None;
        cancel_request.policy = CollaborationPolicy::Team { max_active_children: 3 };
        let mut cancel_task = tokio::spawn(async move {
            let result = run_with(&engine, &provider, cancel_request, RunControls { cancellation: run_cancellation, ..Default::default() }).await;
            let close = engine.close().await;
            (result, close)
        });
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            let mut gates = Vec::new();
            for _ in 0..3 {
                gates.push(started_rx.recv().await.expect("cancellation child started"));
            }
            gates
        })
        .await;
        let gates = match ready {
            Ok(gates) => gates,
            Err(_) => {
                cancel_task.abort();
                let _ = cancel_task.await;
                panic!("resumed cancellation admission deadline");
            }
        };
        assert!(cancellation.cancel());
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut cancel_task).await;
        let (cancelled, close) = match joined {
            Ok(result) => result.expect("cancellation task"),
            Err(_) => {
                cancel_task.abort();
                let _ = cancel_task.await;
                panic!("resumed cancellation cleanup deadline");
            }
        };
        let cancelled = cancelled.expect("committed cancellation");
        close.expect("cancellation Engine shutdown");
        assert_eq!(cancelled.run.status, RunStatus::Cancelled);
        assert_eq!(cancelled.run.agents.len(), 4);
        assert!(cancelled.run.agents.iter().all(|agent| agent.status == AgentStatus::Cancelled));
        assert!(cancelled.run.assistant_message.is_none());
        for (_, release) in gates {
            assert!(release.send(()).is_err(), "cancelled child call remains live");
        }
        assert!(matches!(started_rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)));
        {
            let requests = cancelled_requests.lock().expect("cancelled requests");
            assert_eq!(requests.len(), 4);
            assert!(!requests.iter().any(|request| request.phase == AgentPhase::RootSynthesis));
            let primary = requests.iter().find(|request| request.phase == AgentPhase::RootPlan).expect("cancelled primary");
            assert_eq!(primary.input.context_summary.as_deref(), Some("Derived summary"));
            assert_eq!(primary.input.history.iter().map(|turn| turn.user.as_str()).collect::<Vec<_>>(), ["Question", "Continue parent"]);
        }

        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("final State")).expect("closed journal reopen");
        let parent_events = store.load_session(session_id).await.expect("parent Events");
        let parent = SessionView::replay(session_id, &parent_events).expect("strict parent replay").expect("parent Session");
        assert_eq!(&parent_events[..events.len()], events.as_slice(), "immutable pre-fork prefix");
        assert_eq!(parent.runs.iter().map(|run| run.id).collect::<Vec<_>>(), [seed_run_id, outcome.run.id, continued.run.id, cancelled.run.id]);
        assert_eq!(parent.runs.iter().map(|run| run.status).collect::<Vec<_>>(), [RunStatus::Finished, RunStatus::Finished, RunStatus::Finished, RunStatus::Cancelled]);
        assert_eq!(parent.runs[1], outcome.run);
        assert_eq!(parent.runs[2], continued.run);
        assert_eq!(parent.runs[3], cancelled.run);
        assert_eq!(parent.compactions.len(), 2);
        assert_eq!(parent.compactions[0], resumed.compactions[0]);
        assert_eq!(parent.compactions[1].record, failed);
        let fork_events = store.load_session(fork_id).await.expect("fork Events");
        let fork = store.load_view(fork_id).await.expect("strict fork lineage replay").expect("fork Session");
        let lineage = fork.lineage.expect("fork lineage");
        assert_eq!(lineage.source_session_id, session_id);
        assert_eq!(lineage.source_run_id, outcome.run.id);
        assert_eq!(lineage.source_sequence, outcome.run.finished_sequence.expect("team boundary"));
        assert_eq!(fork.runs, [resumed.runs[0].clone(), outcome.run.clone(), branch.run]);
        assert_eq!(fork.compactions, resumed.compactions);
        store.close().await.expect("final Store shutdown");

        let journal_before = std::fs::read(state_path.join("events.sqlite3")).expect("journal before inspection");
        for (view, direct_events) in [(&parent, &parent_events), (&fork, &fork_events)] {
            for (mode, output) in [("text", arany_cli::presentation::Output::Text), ("jsonl", arany_cli::presentation::Output::Jsonl)] {
                let shown = observe_process(
                    Command::new(env!("CARGO_BIN_EXE_arany"))
                        .env_clear()
                        .current_dir(temp.path())
                        .args(["show", "--state-dir"])
                        .arg(&state_path)
                        .args(["--output", mode, &view.id.to_string()]),
                    temp.path(),
                    256 * 1024,
                );
                assert!(shown.status.success(), "{mode} inspection exit");
                assert_eq!(shown.stderr, b"", "{mode} inspection stderr");
                assert_eq!(shown.stdout, arany_cli::presentation::render_session(view, direct_events, output).as_bytes(), "{mode} inspection bytes");
                assert!(!shown.stdout.contains(&0x1b) && !shown.stdout.contains(&b'\r'));
                if mode == "jsonl" {
                    let lines = shown.stdout.strip_suffix(b"\n").expect("JSONL final newline").split(|byte| *byte == b'\n').collect::<Vec<_>>();
                    assert_eq!(lines.len(), direct_events.len());
                    for (line, envelope) in lines.into_iter().zip(direct_events) {
                        let value: serde_json::Value = serde_json::from_slice(line).expect("one JSONL Event");
                        assert_eq!(value["sequence"], envelope.sequence);
                        assert_eq!(value["session_id"], envelope.session_id.to_string());
                        assert_eq!(value["run_id"], serde_json::json!(envelope.run_id.map(|id| id.to_string())));
                        assert_eq!(value["agent_run_id"], serde_json::json!(envelope.agent_run_id.map(|id| id.to_string())));
                        assert_eq!(value["created_at_ms"], envelope.created_at_ms);
                        match &envelope.event {
                            Event::MessageCommitted { text, .. } => {
                                assert_eq!(value["kind"], "MessageCommitted");
                                assert_eq!(value["payload"]["text"], *text);
                            }
                            Event::RunFinished { disposition, .. } => {
                                assert_eq!(value["kind"], "RunFinished");
                                assert_eq!(value["payload"]["disposition"], serde_json::to_value(disposition).expect("Run disposition"));
                            }
                            Event::ContextCompacted { record, active_ms } => {
                                assert_eq!(value["kind"], "ContextCompacted");
                                assert_eq!(value["payload"]["record"], serde_json::to_value(record).expect("compaction fact"));
                                assert_eq!(value["payload"]["active_ms"], serde_json::json!(active_ms));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        assert!(std::fs::read(state_path.join("events.sqlite3")).expect("journal after inspection") == journal_before, "inspection changed canonical State");
    });
}

#[derive(Clone, Copy)]
struct StepSetup {
    write: bool,
    approvals: Option<arany::ApprovalMode>,
}

const READ_ONLY: StepSetup = StepSetup {
    write: false,
    approvals: None,
};

async fn tool_step_events(
    policy: CollaborationPolicy,
    outcomes: Vec<ProviderOutcome>,
    setup: StepSetup,
) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("fact");
    let state_path = temp.path().join("state");
    let provider = Arc::new(ScriptedProvider::new(outcomes));
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .expect("engine");
    let config = state_path.join("tools.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"version":1,"workspace_paths":["notes"],"write":{},"commands":[],"skills":[],"mcp":[]}}"#,
            setup.write
        ),
    )
    .expect("Tool configuration");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
        .expect("private Tool configuration");
    let (approvals, mut inbox) = setup.approvals.map(arany::ToolApprovals::new).unzip();
    let controls = RunControls {
        tools: ToolGrant::Configured,
        approvals: approvals.clone(),
        ..Default::default()
    };
    let mut input = request(&workspace, "Inspect the notes");
    input.policy = policy;
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: None,
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
    });
    let outcome = match (setup.approvals, inbox.as_mut()) {
        (Some(arany::ApprovalMode::Request), Some(inbox)) => {
            let decide = async {
                inbox.next().await.expect("approval requested").decide(true);
            };
            tokio::join!(run_with(&engine, &provider, input, controls), decide).0
        }
        _ => run_with(&engine, &provider, input, controls).await,
    }
    .expect("Tool Run");
    if setup.write && outcome.run.status == RunStatus::Finished {
        assert_eq!(
            std::fs::read(workspace.join("notes/new.txt")).expect("unplanned Write published"),
            b"New"
        );
    }
    assert!(
        !outcome.run.agents[0]
            .provider_calls
            .iter()
            .any(|call| call.disposition == arany::ProviderCallDisposition::PlanRecorded)
    );
    engine.close().await.expect("engine shutdown");
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(outcome.session_id)
        .await
        .expect("Events");
    store.close().await.expect("store shutdown");
    events
        .iter()
        .map(|envelope| match &envelope.event {
            Event::ToolStarted { intent, .. } => format!(
                "ToolStarted result_bytes={} runtime_ms={} cache_bytes={} cache_transfer_ms={}",
                intent.limits.result_bytes,
                intent.limits.runtime_ms,
                intent.limits.cache_bytes,
                intent.limits.cache_transfer_ms,
            ),
            Event::ToolFinished { observation, .. } => {
                format!("ToolFinished {:?}", observation.disposition)
            }
            Event::ProviderCallRecorded { record, .. } => {
                format!("ProviderCallRecorded {:?}", record.phase)
            }
            event => format!("{event:?}")
                .split([' ', '{', '('])
                .next()
                .expect("variant name")
                .to_owned(),
        })
        .collect()
}

// macOS has a different command contract, so only Linux pins exact result sizes.
fn platform_events(events: &str) -> String {
    if !cfg!(target_os = "macos") {
        return events.to_owned();
    }
    events
        .lines()
        .map(|line| match line.split_once(" result_bytes=") {
            Some((head, rest)) => {
                let tail = rest.split_once(' ').map_or("", |(_, tail)| tail);
                format!("{head} result_bytes=_ {tail}")
            }
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_plan() -> arany::GoalPlanContract {
    arany::GoalPlanContract {
        mission: "Record a new fact".into(),
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
        paths: vec!["notes/new.txt".into()],
        commands: Vec::new(),
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
            path: "notes/new.txt".into(),
            content: "New".into(),
        },
        budget: arany::GoalBudget {
            max_provider_calls: None,
            max_tokens: None,
            max_active_ms: Some(30 * 60 * 1000),
            max_runs: 1,
        },
    }
}

#[tokio::test]
async fn primary_tool_step_events_match_the_snapshot() {
    let finish = |result: &str| {
        ProviderOutcome::Answer(Answer {
            text: result.into(),
        })
    };
    let read = arany::ToolCall::Read {
        path: "notes/facts.txt".into(),
        offset: None,
        limit: Some(32),
    };
    let git = arany::ToolCall::GitStatus {
        path: "notes".into(),
    };
    let write = || {
        tool_outcome(arany::ToolCall::Write {
            path: "notes/new.txt".into(),
            expected_digest: None,
            content: "New".into(),
        })
    };
    let granted = |approvals| StepSetup {
        write: true,
        approvals,
    };
    // The read-only grant denies Git and the first Write before any Guard launch; the other Writes
    // run in the Guard, after an AI review or a human approval where the mode asks for one.
    for (case, outcomes, setup, expected) in [
        (
            "in-process read",
            vec![tool_outcome(read), finish("answer")],
            READ_ONLY,
            PRIMARY_READ,
        ),
        (
            "guard read",
            vec![tool_outcome(git), finish("answer")],
            READ_ONLY,
            PRIMARY_GIT,
        ),
        (
            "denied mutation",
            vec![write(), finish("answer")],
            READ_ONLY,
            PRIMARY_DENIED_WRITE,
        ),
        (
            "repaired mutation",
            vec![
                tool_outcome(arany::ToolCall::Write {
                    path: "../outside".into(),
                    expected_digest: None,
                    content: "New".into(),
                }),
                write(),
                finish("answer"),
            ],
            granted(None),
            PRIMARY_REPAIRED_WRITE,
        ),
        (
            "guard mutation",
            vec![write(), finish("answer")],
            granted(None),
            PRIMARY_WRITE,
        ),
        (
            "reviewed mutation",
            vec![
                write(),
                ProviderOutcome::Review(arany::ReviewVerdict::Approve),
                finish("answer"),
            ],
            granted(Some(arany::ApprovalMode::Auto)),
            PRIMARY_REVIEWED_WRITE,
        ),
        (
            "approved mutation",
            vec![write(), finish("answer")],
            granted(Some(arany::ApprovalMode::Request)),
            PRIMARY_APPROVED_WRITE,
        ),
    ] {
        let events = tool_step_events(CollaborationPolicy::Single, outcomes, setup).await;
        assert_eq!(
            platform_events(&events.join("\n")),
            platform_events(expected),
            "primary {case} step"
        );
    }
}

#[tokio::test]
async fn primary_and_researcher_reads_receive_the_general_rendered_ceiling() {
    let read = || arany::ToolCall::Read {
        path: "notes/facts.txt".into(),
        offset: None,
        limit: Some(32),
    };
    let finish = || {
        ProviderOutcome::Answer(Answer {
            text: "answer".into(),
        })
    };
    let result_bytes = |events: &[String]| {
        events
            .iter()
            .find_map(|line| line.strip_prefix("ToolStarted result_bytes="))
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|value| value.parse::<usize>().ok())
            .expect("ToolStarted result bound")
    };
    let child = tool_step_events(
        CollaborationPolicy::Team {
            max_active_children: 1,
        },
        vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect the fact".into(),
                    profile: None,
                }],
            }),
            tool_outcome(read()),
            finish(),
            finish(),
        ],
        READ_ONLY,
    )
    .await;
    let primary = tool_step_events(
        CollaborationPolicy::Single,
        vec![tool_outcome(read()), finish()],
        READ_ONLY,
    )
    .await;
    assert_eq!(result_bytes(&child), 64 * 1024);
    assert_eq!(result_bytes(&primary), 64 * 1024);
}

#[tokio::test]
async fn researcher_tool_step_events_match_the_snapshot() {
    let finish = || {
        ProviderOutcome::Answer(Answer {
            text: "answer".into(),
        })
    };
    for (case, call, expected) in [
        (
            "in-process read",
            arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: Some(32),
            },
            RESEARCHER_READ,
        ),
        (
            "guard read",
            arany::ToolCall::Skill {
                name: "missing".into(),
                resource: None,
            },
            RESEARCHER_SKILL,
        ),
    ] {
        let outcomes = vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect the fact".into(),
                    profile: None,
                }],
            }),
            tool_outcome(call),
            finish(),
            finish(),
        ];
        let events = tool_step_events(
            CollaborationPolicy::Team {
                max_active_children: 1,
            },
            outcomes,
            READ_ONLY,
        )
        .await;
        assert_eq!(
            platform_events(&events.join("\n")),
            platform_events(expected),
            "researcher {case} step"
        );
    }
}

const PRIMARY_READ: &str = "\
SessionStarted
GoalStarted
GoalRunLinked
MessageAccepted
RunStarted
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded RootPlan
ToolStarted result_bytes=65536 runtime_ms=60000 cache_bytes=67108864 cache_transfer_ms=0
ToolFinished Succeeded
GoalProviderCallReserved
ProviderCallRecorded RootPlan
AgentFinished
MessageCommitted
RunFinished
GoalFinished";

const PRIMARY_GIT: &str = "\
SessionStarted
GoalStarted
GoalRunLinked
MessageAccepted
RunStarted
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded RootPlan
ToolStarted result_bytes=65536 runtime_ms=60000 cache_bytes=67108864 cache_transfer_ms=0
ToolFinished Denied
GoalProviderCallReserved
ProviderCallRecorded RootPlan
AgentFinished
MessageCommitted
RunFinished
GoalFinished";

const PRIMARY_DENIED_WRITE: &str = "\
SessionStarted
GoalStarted
GoalRunLinked
MessageAccepted
RunStarted
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded RootPlan
ToolStarted result_bytes=65536 runtime_ms=60000 cache_bytes=67108864 cache_transfer_ms=0
ToolFinished Denied
GoalProviderCallReserved
ProviderCallRecorded RootPlan
AgentFinished
MessageCommitted
RunFinished
GoalFinished";

const PRIMARY_WRITE: &str = "\
SessionStarted
GoalStarted
GoalRunLinked
MessageAccepted
RunStarted
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded RootPlan
ToolStarted result_bytes=65536 runtime_ms=60000 cache_bytes=67108864 cache_transfer_ms=0
ToolFinished Succeeded
GoalProviderCallReserved
ProviderCallRecorded RootPlan
AgentFinished
MessageCommitted
RunFinished
GoalFinished";

const PRIMARY_REPAIRED_WRITE: &str = "\
SessionStarted
GoalStarted
GoalRunLinked
MessageAccepted
RunStarted
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded RootPlan
GoalProviderCallReserved
ProviderCallRecorded RootPlan
ToolStarted result_bytes=65536 runtime_ms=60000 cache_bytes=67108864 cache_transfer_ms=0
ToolFinished Succeeded
GoalProviderCallReserved
ProviderCallRecorded RootPlan
AgentFinished
MessageCommitted
RunFinished
GoalFinished";

const PRIMARY_REVIEWED_WRITE: &str = "\
SessionStarted
GoalStarted
GoalRunLinked
MessageAccepted
RunStarted
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded RootPlan
ToolStarted result_bytes=65536 runtime_ms=60000 cache_bytes=67108864 cache_transfer_ms=0
GoalProviderCallReserved
ProviderCallRecorded ToolReview
ToolFinished Succeeded
GoalProviderCallReserved
ProviderCallRecorded RootPlan
AgentFinished
MessageCommitted
RunFinished
GoalFinished";

const PRIMARY_APPROVED_WRITE: &str = "\
SessionStarted
GoalStarted
GoalRunLinked
MessageAccepted
RunStarted
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded RootPlan
ToolStarted result_bytes=65536 runtime_ms=60000 cache_bytes=67108864 cache_transfer_ms=0
ToolFinished Succeeded
GoalProviderCallReserved
ProviderCallRecorded RootPlan
AgentFinished
MessageCommitted
RunFinished
GoalFinished";

const RESEARCHER_READ: &str = "\
SessionStarted
GoalStarted
GoalRunLinked
MessageAccepted
RunStarted
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded RootPlan
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded ChildWork
ToolStarted result_bytes=65536 runtime_ms=60000 cache_bytes=67108864 cache_transfer_ms=0
ToolFinished Succeeded
GoalProviderCallReserved
ProviderCallRecorded ChildWork
AgentFinished
GoalProviderCallReserved
ProviderCallRecorded RootSynthesis
AgentFinished
MessageCommitted
RunFinished
GoalFinished";

const RESEARCHER_SKILL: &str = "\
SessionStarted
GoalStarted
GoalRunLinked
MessageAccepted
RunStarted
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded RootPlan
AgentSpawned
GoalProviderCallReserved
ProviderCallRecorded ChildWork
ToolStarted result_bytes=65536 runtime_ms=60000 cache_bytes=67108864 cache_transfer_ms=0
ToolFinished Denied
GoalProviderCallReserved
ProviderCallRecorded ChildWork
AgentFinished
GoalProviderCallReserved
ProviderCallRecorded RootSynthesis
AgentFinished
MessageCommitted
RunFinished
GoalFinished";

mod goals {
    use super::*;
    use arany::test_support::goals::*;
    struct ExactCapToolReview;

    #[async_trait::async_trait]
    impl Provider for ExactCapToolReview {
        fn identity(&self) -> &arany::ProviderIdentity {
            static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
                std::sync::LazyLock::new(|| {
                    arany::ProviderIdentity::new(
                        "scripted",
                        "exact-cap-model",
                        arany::Provenance::native(),
                    )
                });
            &IDENTITY
        }

        async fn invoke(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            let (outcome, input_tokens, output_tokens) = match request.phase {
                AgentPhase::RootPlan => (
                    crate::tool_outcome(arany::ToolCall::Write {
                        path: "notes/a.txt".into(),
                        expected_digest: None,
                        content: "x".into(),
                    }),
                    3,
                    2,
                ),
                AgentPhase::ToolReview => {
                    (ProviderOutcome::Review(arany::ReviewVerdict::Approve), 3, 2)
                }
                _ => return Err(ProviderError::InvalidOutcome),
            };
            Ok(ProviderResponse {
                turn: arany::ProviderTurn::scripted(),
                outcome,
                response_id: None,
                usage: arany::Usage {
                    input_tokens: Some(input_tokens),
                    output_tokens: Some(output_tokens),
                    ..Default::default()
                },
                wire_provenance: None,
            })
        }

        async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
            Err(ProviderError::InvalidOutcome)
        }
    }

    #[tokio::test]
    async fn exact_token_cap_after_tool_review_settles_the_goal_as_failed() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        let state_path = temp.path().join("state");
        let provider = Arc::new(ExactCapToolReview);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .expect("engine");
        {
            use std::os::unix::fs::PermissionsExt;
            let config = state_path.join("tools.json");
            std::fs::write(
            &config,
            br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
        )
        .expect("tool config");
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
                .expect("tool config mode");
        }
        let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
        let mut input = request(&workspace, "Write one note at the exact token cap");
        input.include_paths.clear();
        input.goal = arany::GoalRequest::new(
            arany::GoalBudget {
                max_tokens: Some(10),
                ..arany::GoalBudget::default()
            },
            arany::GoalPhase::Implementation,
        );
        let outcome = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: Some(approvals.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("exhaustion ends the Run as a durable failure");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(
            !workspace.join("notes/a.txt").exists(),
            "the exact-cap Tool was not dispatched"
        );
        engine.close().await.expect("engine shutdown");

        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only Store");
        let view = store
            .load_view(outcome.session_id)
            .await
            .expect("replayed Session")
            .expect("Session");
        assert_eq!(view.goals[0].status, arany::GoalStatus::Failed);
        assert!(view.goals[0].report.is_some());
        let events = store
            .load_session(outcome.session_id)
            .await
            .expect("Events");
        // The exhausted-budget stop closes inside the bound of the action that found the budget spent.
        assert_action_bounds(&events, false, false);
        store.close().await.expect("Store shutdown");
    }

    #[tokio::test]
    async fn approval_wait_is_not_charged_to_the_runs_active_time() {
        use std::os::unix::fs::PermissionsExt;
        const APPROVAL_WAIT: Duration = Duration::from_secs(20);
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(arany::GoalPlanContract {
                mission: "Change the facts".into(),
                resource_class: arany::ToolResourceClass::Default,
                command_timeout_ms: None,
                paths: vec!["notes".into()],
                commands: Vec::new(),
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                    path: "notes/facts.txt".into(),
                    content: "changed".into(),
                },
                budget: arany::GoalBudget::default(),
            }),
            crate::tool_outcome(arany::ToolCall::Write {
                path: "notes/facts.txt".into(),
                expected_digest: None,
                content: "changed".into(),
            }),
        ]);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .expect("engine");
        let config = state_path.join("tools.json");
        std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("tool configuration");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("tool configuration mode");
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let mut input = request(&workspace, "Change the facts");
        input.include_paths.clear();

        // The user answers after a long wait; only the clock inside the wait advances.
        let decide = async {
            let approval = inbox.next().await.expect("approval requested");
            tokio::time::pause();
            tokio::time::advance(APPROVAL_WAIT).await;
            tokio::time::resume();
            approval.decide(false);
        };
        let (run, ()) = tokio::join!(
            run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    approvals: Some(approvals.clone()),
                    ..Default::default()
                }
            ),
            decide
        );
        let outcome = run.expect("denied Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        engine.close().await.expect("engine shutdown");

        let (events, _) = replayed(&state_path, outcome.session_id).await;
        let run_active_ms = events
            .iter()
            .find_map(|envelope| match &envelope.event {
                Event::RunFinished { active_ms, .. } => Some(*active_ms),
                _ => None,
            })
            .expect("the Run settles its active time");
        assert!(
            u128::from(run_active_ms) < APPROVAL_WAIT.as_millis() / 10,
            "{run_active_ms} ms charged across a {APPROVAL_WAIT:?} approval wait"
        );
    }

    #[tokio::test]
    async fn pause_during_approval_wait_dispatches_no_tool() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let steering = RunSteering::new();
        let pause = steering.clone();
        let provider = ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(arany::GoalPlanContract {
                mission: "Change the facts".into(),
                resource_class: arany::ToolResourceClass::Default,
                command_timeout_ms: None,
                paths: vec!["notes".into()],
                commands: Vec::new(),
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                    path: "notes/facts.txt".into(),
                    content: "changed".into(),
                },
                budget: arany::GoalBudget::default(),
            }),
            crate::tool_outcome(arany::ToolCall::Write {
                path: "notes/facts.txt".into(),
                expected_digest: None,
                content: "changed".into(),
            }),
            finish_with("never reached"),
        ]);
        let requests = provider.requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .expect("engine");
        let config = state_path.join("tools.json");
        std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("tool configuration");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("tool configuration mode");
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let mut input = request(&workspace, "Change the facts");
        input.include_paths.clear();

        // The approval wait is the only point where the caller can pause before dispatch.
        let decide = async {
            let approval = inbox.next().await.expect("approval requested");
            assert!(pause.pause(), "first pause");
            approval.decide(true);
        };
        let (run, ()) = tokio::join!(
            run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    approvals: Some(approvals.clone()),
                    steering: Some(steering.clone()),
                    ..Default::default()
                }
            ),
            decide
        );
        let outcome = run.expect("paused Run");
        assert_eq!(outcome.run.status, RunStatus::Cancelled);
        engine.close().await.expect("engine shutdown");

        assert_eq!(
            std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
            "v1",
            "a Tool approved during a pause is not dispatched"
        );
        assert_eq!(
            requests.lock().expect("requests").len(),
            2,
            "the plan call and the Write call; nothing after the pause"
        );
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        assert_action_bounds(&events, false, false);
        let observed: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.event {
                Event::ToolFinished { observation, .. } => Some(observation.disposition),
                _ => None,
            })
            .collect();
        assert_eq!(observed, [arany::ToolDisposition::Denied]);
        assert_eq!(
            view.goal(outcome.goal_id).expect("Goal").status,
            arany::GoalStatus::Interrupted
        );
    }

    #[tokio::test]
    async fn pause_during_tool_review_skips_the_prompt_and_dispatches_no_tool() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let steering = RunSteering::new();
        let pause = steering.clone();
        let provider = ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(arany::GoalPlanContract {
                mission: "Change the facts".into(),
                resource_class: arany::ToolResourceClass::Default,
                command_timeout_ms: None,
                paths: vec!["notes".into()],
                commands: Vec::new(),
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                    path: "notes/facts.txt".into(),
                    content: "changed".into(),
                },
                budget: arany::GoalBudget::default(),
            }),
            crate::tool_outcome(arany::ToolCall::Write {
                path: "notes/facts.txt".into(),
                expected_digest: None,
                content: "changed".into(),
            }),
            ProviderOutcome::Review(arany::ReviewVerdict::Ask),
            finish_with("never reached"),
        ])
        // Zero-based call 2 is the Tool review that follows the proposed Tool.
        .with_before_call(2, move || assert!(pause.pause(), "first pause"));
        let requests = provider.requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .expect("engine");
        let config = state_path.join("tools.json");
        std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("tool configuration");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("tool configuration mode");
        let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
        let mut input = request(&workspace, "Change the facts");
        input.include_paths.clear();
        let outcome = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: Some(approvals.clone()),
                steering: Some(steering.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("paused Run");
        assert_eq!(outcome.run.status, RunStatus::Cancelled);
        engine.close().await.expect("engine shutdown");

        assert_eq!(
            std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
            "v1",
            "a Tool reviewed during a pause is not dispatched"
        );
        assert_eq!(requests.lock().expect("requests").len(), 3);
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let observed: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.event {
                Event::ToolFinished { observation, .. } => Some(observation.disposition),
                _ => None,
            })
            .collect();
        assert_eq!(observed, [arany::ToolDisposition::Denied]);
        assert_eq!(
            view.goal(outcome.goal_id).expect("Goal").status,
            arany::GoalStatus::Interrupted
        );
    }

    #[tokio::test]
    async fn reviewed_write_stays_within_each_action_bound() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(arany::GoalPlanContract {
                mission: "Change the facts".into(),
                resource_class: arany::ToolResourceClass::Default,
                command_timeout_ms: None,
                paths: vec!["notes".into()],
                commands: Vec::new(),
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                    path: "notes/a.txt".into(),
                    content: "x".into(),
                },
                budget: arany::GoalBudget::default(),
            }),
            crate::tool_outcome(arany::ToolCall::Write {
                path: "notes/a.txt".into(),
                expected_digest: None,
                content: "x".into(),
            }),
            ProviderOutcome::Review(arany::ReviewVerdict::Approve),
            finish_with("x"),
        ]);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .expect("engine");
        let config = state_path.join("tools.json");
        std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("tool configuration");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("tool configuration mode");
        let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
        let mut input = request(&workspace, "Change the facts");
        input.include_paths.clear();
        let outcome = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: Some(approvals.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        engine.close().await.expect("engine shutdown");
        let (events, _) = replayed(&state_path, outcome.session_id).await;
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::ProviderCallRecorded { record, .. } if record.phase == AgentPhase::ToolReview
        )));
        assert_action_bounds(&events, false, false);
    }
    #[tokio::test]
    async fn a_stalled_checkpoint_capture_is_skipped_and_the_edit_still_publishes() {
        use arany::test_support::goals::*;
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "original");
        let state_path = temp.path().join("state");
        let engine = engine_support::open(
            StateRoot::admit(&state_path).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        let config = state_path.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut plan = write_plan();
        plan.paths = vec!["notes/facts.txt".into()];
        plan.budget = arany::GoalBudget::default();
        plan.success_criterion = arany::GoalSuccessCriterion::WorkspaceFileContent {
            path: "notes/facts.txt".into(),
            content: "one".into(),
        };
        let provider = Arc::new(ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(plan),
            crate::tool_outcome(arany::ToolCall::Write {
                path: "notes/facts.txt".into(),
                expected_digest: Some(
                    Sha256::digest(b"original")
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect(),
                ),
                content: "one".into(),
            }),
            finish_with("one"),
        ]));
        arany::FaultPlan::stall_checkpoint(session);
        let mut input = request(&workspace, "Change the facts");
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        let outcome = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.run.status, RunStatus::Finished);
        let tool = &outcome.run.tools[0];
        assert_eq!(
            tool.observation.as_ref().unwrap().disposition,
            arany::ToolDisposition::Succeeded
        );
        let checkpoint = tool.checkpoint.as_ref().unwrap();
        assert_eq!(checkpoint.status, arany::CheckpointStatus::Skipped);
        assert_eq!(
            checkpoint.reason,
            Some(arany::CheckpointReason::StoreUnavailable)
        );
        assert_eq!(
            std::fs::read(workspace.join("notes/facts.txt")).unwrap(),
            b"one"
        );
        engine.close().await.unwrap();
    }

    #[tokio::test]
    async fn checkpoints_commit_before_go_skip_repeats_capture_gaps_and_preserve_replay() {
        use arany::test_support::goals::*;
        use std::os::unix::fs::PermissionsExt;
        for unavailable in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = facts_workspace(temp.path(), "original");
            let state_path = temp.path().join("state");
            let engine = engine_support::open(
                StateRoot::admit(&state_path).unwrap(),
                EngineConfig {
                    guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let session = engine
                .create_session(workspace.clone(), None)
                .await
                .unwrap();
            let config = state_path.join("tools.json");
            std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
            if unavailable {
                std::fs::write(state_path.join("checkpoints.sqlite"), b"malformed").unwrap();
            }
            let write = |before: &str, after: &str| {
                crate::tool_outcome(arany::ToolCall::Write {
                    path: "notes/facts.txt".into(),
                    expected_digest: Some(
                        Sha256::digest(before.as_bytes())
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect(),
                    ),
                    content: after.into(),
                })
            };
            let mut plan = write_plan();
            plan.paths = vec!["notes/facts.txt".into()];
            plan.budget = arany::GoalBudget::default();
            plan.success_criterion = arany::GoalSuccessCriterion::WorkspaceFileContent {
                path: "notes/facts.txt".into(),
                content: "three".into(),
            };
            let path = workspace.join("notes/facts.txt");
            let external = path.clone();
            let provider = Arc::new(
                ScriptedProvider::new(vec![
                    arany::test_support::scenario::plan_outcome(plan),
                    write("original", "one"),
                    write("one", "two"),
                    write("external", "three"),
                    finish_with("three"),
                ])
                .with_before_call(3, move || std::fs::write(&external, b"external").unwrap()),
            );
            let mut input = request(&workspace, "Change the facts");
            input.session_id = Some(session);
            input.title = None;
            input.include_paths.clear();
            let outcome = {
                let gate = (!unavailable).then(|| arany::FaultPlan::gate_checkpoint(session));
                let execution = run_with(
                    &engine,
                    &provider,
                    input,
                    RunControls {
                        tools: ToolGrant::Configured,
                        ..Default::default()
                    },
                );
                tokio::pin!(execution);
                if let Some((entered, release)) = gate {
                    tokio::select! {
                        result = &mut execution => panic!("Run completed before the pre-GO checkpoint gate: {result:?}"),
                        result = tokio::time::timeout(Duration::from_secs(120), entered) => result.unwrap().unwrap(),
                    }
                    assert_eq!(
                        std::fs::read(&path).unwrap(),
                        b"original",
                        "GO has not published yet"
                    );
                    let _ = release.send(());
                }
                execution.await.unwrap()
            };
            assert_eq!(outcome.run.status, RunStatus::Finished);
            let statuses: Vec<_> = outcome
                .run
                .tools
                .iter()
                .map(|t| t.checkpoint.as_ref().unwrap().status)
                .collect();
            let captured = if unavailable {
                arany::CheckpointStatus::Skipped
            } else {
                arany::CheckpointStatus::Captured
            };
            assert_eq!(
                statuses,
                [captured, arany::CheckpointStatus::NotNeeded, captured]
            );
            assert_eq!(provider.requests.lock().unwrap().len(), 5);
            assert_eq!(std::fs::read(&path).unwrap(), b"three");
            engine.close().await.unwrap();
            let (events, view) = replayed(&state_path, session).await;
            assert_action_bounds(&events, false, false);
            if unavailable {
                assert!(
                    view.goals[0]
                        .report
                        .as_ref()
                        .unwrap()
                        .unresolved
                        .iter()
                        .any(|fact| fact.contains("cannot be rewound"))
                );
            } else {
                let store =
                    Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
                for bytes in [b"original".as_slice(), b"external".as_slice()] {
                    assert_eq!(
                        store
                            .checkpoint_blob(Sha256::digest(bytes).into())
                            .await
                            .unwrap()
                            .as_deref(),
                        Some(bytes)
                    );
                }
                store.close().await.unwrap();
                let checkpoints = rusqlite::Connection::open_with_flags(
                    state_path.join("checkpoints.sqlite"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .unwrap();
                let sequence: i64 = checkpoints
                    .query_row(
                        "SELECT tool_sequence FROM refs WHERE session_id=?1 AND digest=?2",
                        rusqlite::params![
                            session.to_string(),
                            Sha256::digest(b"original").as_slice()
                        ],
                        |row| row.get(0),
                    )
                    .unwrap();
                let sequence = u64::try_from(sequence).unwrap();
                let Event::ToolStarted { intent, .. } = &events
                    .iter()
                    .find(|event| event.sequence == sequence)
                    .unwrap()
                    .event
                else {
                    panic!("checkpoint ref must name ToolStarted");
                };
                let finished = events
                    .iter()
                    .find(|event| matches!(&event.event, Event::ToolFinished { observation, .. } if observation.intent.id == intent.id))
                    .unwrap();
                assert!(sequence < finished.sequence);
            }
            std::fs::write(&path, b"after exit").unwrap();
            assert_eq!(
                SessionView::replay(session, &events).unwrap().unwrap(),
                view
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"after exit",
                "strict replay never re-executes"
            );
        }
    }
}

mod routing {
    use super::engine_support::run_routed;
    use super::*;
    use arany::ProviderRole;
    use arany::test_support::routing::*;

    #[tokio::test]
    async fn review_role_fallback_does_not_dispatch_the_proposed_tool_twice() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
        let state = temp.path().join("state");
        let write = arany::ToolCall::Write {
            path: "notes/result.txt".into(),
            expected_digest: None,
            content: "done".into(),
        };
        let plan = arany::GoalPlanContract {
            mission: "Write the result".into(),
            paths: vec!["notes".into()],
            commands: vec![],
            skills: vec![],
            mcp_tools: vec![],
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            budget: arany::GoalBudget::default(),
            success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                path: "notes/result.txt".into(),
                content: "done".into(),
            },
        };
        let primary = route(
            ScriptedProvider::new(vec![
                arany::test_support::scenario::plan_outcome(plan),
                crate::tool_outcome(write),
                finish("done"),
            ])
            .with_profile("scripted", "primary"),
        );
        // The clock pauses only for the review's backoff; the Guard runs after it resumes.
        let mut review = ScriptedProvider::new(vec![])
            .with_profile("scripted", "review")
            .with_before_call(0, tokio::time::pause);
        review.responses = Mutex::new(
            (0..=arany::MAX_PROVIDER_RETRIES)
                .map(|_| Err(ProviderError::RemoteHttp(503)))
                .collect(),
        );
        let review = route(review);
        let fallback = route(
            ScriptedProvider::new(vec![ProviderOutcome::Review(arany::ReviewVerdict::Approve)])
                .with_profile("scripted", "review-fallback")
                .with_before_call(0, tokio::time::resume),
        );
        let engine = engine_support::open(
            StateRoot::admit(&state).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..EngineConfig::default()
            },
        )
        .unwrap();
        let config = state.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
        let mut bindings = routes(&primary);
        bindings.roles.review = vec![review.clone(), fallback.clone()];
        let result = run_routed(
            &engine,
            bindings,
            input(&workspace),
            RunControls {
                approvals: Some(approvals.clone()),
                ..configured()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.run.status, RunStatus::Finished);
        engine.close().await.unwrap();
        let (events, view) = reopen(&state, result.session_id).await;
        assert_eq!(
            view.runs[0].provider_fallbacks[0].1.role,
            ProviderRole::Review
        );
        assert!(
            view.runs[0]
                .provider_retries
                .iter()
                .map(|(_, retry)| (retry.role, retry.attempt))
                .eq([1, 2, 3].map(|attempt| (ProviderRole::Review, attempt)))
        );
        assert_eq!(review.requests.lock().unwrap()[0].model, "review");
        assert_eq!(
            fallback.requests.lock().unwrap()[0].model,
            "review-fallback"
        );
        assert!(
            primary
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| request.phase != AgentPhase::ToolReview)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::ToolStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::ToolFinished { .. }))
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("notes/result.txt")).unwrap(),
            "done"
        );
    }

    #[tokio::test]
    async fn selected_profile_fallback_in_the_guard_keeps_one_read_only_tool_effect() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
        let state = temp.path().join("state");
        let primary = route(
            ScriptedProvider::new(vec![
                ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        editor: None,
                        objective: "Read the facts".into(),
                        profile: Some("spare".into()),
                    }],
                }),
                finish("done"),
            ])
            .with_profile("scripted", "primary"),
        );
        // The clock pauses after the Guard Read, only for the backoff before the switch.
        let mut first = ScriptedProvider::new(vec![tool_outcome(arany::ToolCall::Read {
            path: "notes/facts.txt".into(),
            offset: None,
            limit: None,
        })])
        .with_profile("scripted", "spare-first")
        .with_before_call(1, tokio::time::pause);
        first
            .responses
            .get_mut()
            .unwrap()
            .extend((0..=arany::MAX_PROVIDER_RETRIES).map(|_| Err(ProviderError::RemoteHttp(503))));
        let first = route(first);
        let fallback = route(
            ScriptedProvider::new(vec![finish("read facts")])
                .with_profile("scripted", "spare-fallback")
                .with_before_call(0, tokio::time::resume),
        );
        let engine = engine_support::open(
            StateRoot::admit(&state).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..EngineConfig::default()
            },
        )
        .unwrap();
        let config = state.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut bindings = routes(&primary);
        pin_roster(&mut bindings, Some(vec![first.clone(), fallback.clone()]));
        let mut request = input(&workspace);
        request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let result = run_routed(&engine, bindings, request, configured())
            .await
            .unwrap();
        assert_eq!(result.run.status, RunStatus::Finished);
        engine.close().await.unwrap();
        let (events, view) = reopen(&state, result.session_id).await;
        let child = &view.runs[0].agents[1];
        assert_eq!(child.profile.as_deref(), Some("spare"));
        assert_eq!(
            child.grant,
            Some(arany::ChildGrant::ReadOnly {
                unguarded: arany::UnguardedGrant::Denied
            })
        );
        assert_eq!(child.tools.len(), 1);
        assert_eq!(view.runs[0].provider_fallbacks.len(), 1);
        assert_eq!(view.runs[0].provider_retries.len(), 3);
        assert_eq!(first.requests.lock().unwrap().len(), 5);
        let requests = fallback.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let step = &requests[0].input.tools.as_ref().unwrap().steps[0];
        assert_eq!((step.requests.len(), step.observations.len()), (1, 1));
        for started in [true, false] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| match event.event {
                        Event::ToolStarted { .. } => started,
                        Event::ToolFinished { .. } => !started,
                        _ => false,
                    })
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn compaction_fallback_in_a_fork_runs_without_the_inherited_goal() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
        let state = temp.path().join("state");
        let primary =
            route(ScriptedProvider::new(vec![finish("done")]).with_profile("scripted", "primary"));
        let first = route(
            ScriptedProvider::new(vec![])
                .with_profile("scripted", "compaction")
                .with_compactions(
                    (0..=arany::MAX_PROVIDER_RETRIES)
                        .map(|_| Err(ProviderError::RemoteHttp(503)))
                        .collect(),
                ),
        );
        let second = route(
            ScriptedProvider::new(vec![])
                .with_profile("scripted", "compaction-fallback")
                .with_compactions(vec![Ok(CompactionResponse {
                    summary: "Fork summary".into(),
                    response_id: None,
                    usage: arany::Usage {
                        input_tokens: Some(20),
                        output_tokens: Some(8),
                        ..Default::default()
                    },
                    wire_provenance: None,
                })]),
        );
        let engine = engine_support::open(
            StateRoot::admit(&state).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..EngineConfig::default()
            },
        )
        .unwrap();
        let config = state.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut bindings = routes(&primary);
        bindings.roles.compaction = vec![first.clone(), second.clone()];
        let outcome = run_routed(&engine, bindings.clone(), input(&workspace), configured())
            .await
            .unwrap();
        assert_eq!(outcome.run.status, RunStatus::Finished);
        let fork = engine
            .fork_session(outcome.session_id, workspace.clone(), None)
            .await
            .unwrap();
        // The journaled retry delays stay; only the waits end at once.
        arany::FaultPlan::skip_retry_backoff(fork);
        let record = engine
            .compact_session(&bindings, fork, workspace.clone())
            .await
            .unwrap();
        assert!(matches!(record.status, CompactionStatus::Succeeded { .. }));
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let view = store.load_view(fork).await.unwrap().unwrap();
        store.close().await.unwrap();
        assert_eq!(view.runs.last().unwrap().provider_fallbacks.len(), 1);
        assert_eq!(
            view.runs.last().unwrap().provider_fallbacks[0].1.role,
            ProviderRole::Compaction
        );
        assert_eq!(second.compact_requests.lock().unwrap().len(), 1);
        let shown = std::process::Command::new(env!("CARGO_BIN_EXE_arany"))
            .args(["show", "--state-dir"])
            .arg(&state)
            .args(["--output", "text", &fork.to_string()])
            .output()
            .unwrap();
        assert!(
            shown.status.success(),
            "show on a fork after a compaction switch: {}",
            String::from_utf8_lossy(&shown.stderr)
        );
    }

    #[tokio::test]
    async fn fork_inherited_runs_ignore_later_parent_compaction_switches() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
        let state = temp.path().join("state");
        let primary =
            route(ScriptedProvider::new(vec![finish("done")]).with_profile("scripted", "primary"));
        let first = route(
            ScriptedProvider::new(vec![])
                .with_profile("scripted", "compaction")
                .with_compactions(
                    (0..=arany::MAX_PROVIDER_RETRIES)
                        .map(|_| Err(ProviderError::RemoteHttp(503)))
                        .collect(),
                ),
        );
        let second = route(
            ScriptedProvider::new(vec![])
                .with_profile("scripted", "compaction-fallback")
                .with_compactions(vec![Ok(CompactionResponse {
                    summary: "Parent summary".into(),
                    response_id: None,
                    usage: arany::Usage {
                        input_tokens: Some(20),
                        output_tokens: Some(8),
                        ..Default::default()
                    },
                    wire_provenance: None,
                })]),
        );
        let engine = engine_support::open(
            StateRoot::admit(&state).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..EngineConfig::default()
            },
        )
        .unwrap();
        let config = state.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut bindings = routes(&primary);
        bindings.roles.compaction = vec![first.clone(), second.clone()];
        let outcome = run_routed(&engine, bindings.clone(), input(&workspace), configured())
            .await
            .unwrap();
        assert_eq!(outcome.run.status, RunStatus::Finished);
        let fork = engine
            .fork_session(outcome.session_id, workspace.clone(), None)
            .await
            .unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let before = store.load_view(fork).await.unwrap().unwrap();
        store.close().await.unwrap();
        arany::FaultPlan::skip_retry_backoff(outcome.session_id);
        engine
            .compact_session(&bindings, outcome.session_id, workspace.clone())
            .await
            .unwrap();
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let after = store.load_view(fork).await.unwrap().unwrap();
        let parent = store.load_view(outcome.session_id).await.unwrap().unwrap();
        store.close().await.unwrap();
        assert_eq!(parent.runs.last().unwrap().provider_fallbacks.len(), 1);
        assert_eq!(after.runs, before.runs);
        assert!(after.runs.last().unwrap().provider_fallbacks.is_empty());
    }
}

#[tokio::test]
async fn native_private_guard_target_keeps_edits_disposable_and_reads_assignment_local() {
    use std::os::unix::fs::PermissionsExt;
    arany::test_support::isolate_account_root();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir_all(workspace.join("owned")).unwrap();
    let mut original = b"original".to_vec();
    original.extend(vec![b'x'; 96 * 1024]);
    std::fs::write(workspace.join("owned/a"), original).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let config = state.path().join("tools.json");
    std::fs::write(&config, br#"{"version":1,"workspace_paths":["owned"],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    arany::test_support::exercise_private_guard_target(
        &state,
        &workspace,
        env!("CARGO_BIN_EXE_arany").into(),
    )
    .await
    .unwrap();
}

#[test]
fn native_private_guard_target_commands_fit_a_one_and_a_half_mebibyte_stack() {
    std::thread::Builder::new()
        .name("worker-command-stack-canary".into())
        .stack_size(1536 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Box::pin(private_guard_command_cases()))
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn private_guard_command_cases() {
    use std::os::unix::fs::PermissionsExt;
    arany::test_support::isolate_account_root();
    for case in ["success", "outside", "nonzero", "link", "lost", "cancelled"] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(workspace.join("owned")).unwrap();
        std::fs::write(workspace.join("owned/a"), b"original").unwrap();
        std::fs::create_dir(workspace.join(".git")).unwrap();
        let hostile = format!(
            "[core]\nfsmonitor = touch {}\n",
            temp.path().join("git-executed").display()
        );
        std::fs::write(workspace.join(".git/config"), &hostile).unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        let config = serde_json::json!({"version":1,"workspace_paths":["owned"],"write":true,
            "commands":[{"name":"sh","interpreter":false,"executable":shell,"sha256":Sha256::digest(std::fs::read(&shell).unwrap()).iter().map(|b| format!("{b:02x}")).collect::<String>(),"inputs":[]}],"skills":[],"mcp":[]});
        let config_path = state.path().join("tools.json");
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        arany::test_support::exercise_private_guard_command(
            &state,
            &workspace,
            env!("CARGO_BIN_EXE_arany").into(),
            case,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join(".git/config")).unwrap(),
            hostile
        );
        assert!(!temp.path().join("git-executed").exists());
        assert!(!workspace.join("outside").exists());
    }
}

#[tokio::test]
async fn attached_shape_write_without_a_plan_dispatches_after_ask_approval() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("fixture");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state).expect("state"),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .expect("engine");
    let config = state.join("tools.json");
    std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("grant");
    std::fs::set_permissions(config, std::fs::Permissions::from_mode(0o600))
        .expect("private grant");
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_outcome(arany::ToolCall::Write {
            path: "test.txt".into(),
            expected_digest: None,
            content: "hello".into(),
        }),
        ProviderOutcome::Answer(Answer {
            text: "Wrote test.txt".into(),
        }),
    ]));
    let mut input = request(&workspace, "write test.txt containing hello");
    input.policy = CollaborationPolicy::Single;
    input.include_paths.clear();
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let controls = RunControls {
        tools: ToolGrant::Configured,
        approvals: Some(approvals),
        ..Default::default()
    };
    let execution = async {
        tokio::join!(run_with(&engine, &provider, input, controls), async {
            let approval = inbox.next().await.expect("Ask approval");
            assert!(
                !workspace.join("test.txt").exists(),
                "approval precedes dispatch"
            );
            approval.decide(true);
        })
        .0
    };
    let result = tokio::time::timeout(Duration::from_secs(30), execution)
        .await
        .expect("bounded journey")
        .expect("Run");
    engine.close().await.expect("shutdown");
    let (events, view) = arany::test_support::routing::reopen(&state, result.session_id).await;
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert!(!view.goals[0].autonomous);
    assert!(view.goals[0].plan_contract.is_none());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::GoalPlanContractRecorded { .. }))
    );
    assert_eq!(
        std::fs::read(workspace.join("test.txt")).expect("published Write"),
        b"hello"
    );
    let observation = view.runs[0].tools[0].observation.as_ref().expect("receipt");
    assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
    assert!(observation.attestation.is_some());
    assert_eq!(provider.requests.lock().expect("requests").len(), 2);
    assert!(provider.responses.lock().expect("script").is_empty());
}

#[test]
#[cfg_attr(not(arany_ci), ignore = "native Skill Guard read on Linux and macOS")]
fn invoked_skill_is_pinned_journaled_and_read_before_provider_egress() {
    let _gate = process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use std::os::unix::fs::PermissionsExt;
            for (mismatch, objective) in [
                (false, "  do X \nKeep spacing  "),
                (false, "/ask-matt"),
                (true, "do X"),
            ] {
                let temp = tempfile::tempdir().unwrap();
                let workspace = temp.path().join("workspace");
                std::fs::create_dir_all(workspace.join("notes")).unwrap();
                std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
                let skill_dir = workspace.join(".agents/skills/ask-matt");
                std::fs::create_dir_all(&skill_dir).unwrap();
                let body = b"---\nname: ask-matt\ndescription: Guide the task\ndisable-model-invocation: true\n---\nPinned untrusted Skill guidance.\n";
                std::fs::write(skill_dir.join("SKILL.md"), body).unwrap();
                let state_path = temp.path().join("state");
                let engine = engine_support::open(
                    StateRoot::admit(&state_path).unwrap(),
                    EngineConfig {
                        guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                        ..Default::default()
                    },
                )
                .unwrap();
                let hash = if mismatch {
                    "0".repeat(64)
                } else {
                    Sha256::digest(body)
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                };
                let config = serde_json::json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"mcp":[],"skills":[{
                    "name":"ask-matt","description":"Reviewed guidance","directory":skill_dir,"files":{"SKILL.md":hash},"disable_model_invocation":true
                }]});
                let config_path = state_path.join("tools.json");
                std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
                std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
                let provider = Arc::new(ScriptedProvider::new(vec![
                    tool_outcome(arany::ToolCall::Skill {
                        name: "ask-matt".into(),
                        resource: None,
                    }),
                    tool_outcome(arany::ToolCall::Read {
                        path: "notes/facts.txt".into(),
                        offset: None,
                        limit: Some(32),
                    }),
                    ProviderOutcome::Answer(Answer {
                        text: "Applied the Skill.".into(),
                    }),
                ]));
                let mut composer = arany_cli::terminal::Composer::default();
                composer.set_runtime_skills(
                    &arany::runtime_skill_names(
                        &StateRoot::open_existing(&state_path).unwrap(),
                        &workspace,
                    )
                    .unwrap(),
                );
                for character in "/ask".chars() {
                    composer.apply(arany_cli::terminal::TerminalInput::Character(character));
                }
                composer.apply(if objective == "/ask-matt" {
                    arany_cli::terminal::TerminalInput::Submit
                } else {
                    arany_cli::terminal::TerminalInput::Tab
            });
            if objective != "/ask-matt" {
                composer.insert_paste(objective).unwrap();
            }
            let arany_cli::terminal::Submission::Skill { name, .. } = composer.submission().unwrap()
            else {
                panic!("completion must submit the admitted Skill");
            };
            let submitted = composer.text().to_owned();
            let mut input = request(&workspace, &submitted);
            input.skill = Some(name.to_owned());
            let outcome = run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let requests = provider.requests.lock().unwrap().clone();
            if mismatch {
                assert_eq!(outcome.run.status, RunStatus::Failed);
                assert_eq!(outcome.run.failure, Some(arany::RunFailure::ToolStopped));
                assert!(
                    requests.is_empty(),
                    "digest mismatch must fail before egress"
                );
            } else {
                assert_eq!(
                    outcome.run.status,
                    RunStatus::Finished,
                    "{:?}",
                    outcome.run.failure
                );
                assert_eq!(
                    requests.len(),
                    3,
                    "the model-originated Skill load is refused"
                );
                let tools = requests[0].input.tools.as_ref().unwrap();
                assert_eq!(
                    tools.invoked_skill.as_ref().unwrap().output.as_bytes(),
                    body
                );
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(tools.catalog()).unwrap()["skills"],
                    serde_json::json!([])
                );
            }
            engine.close().await.unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
            let events = store.load_session(outcome.session_id).await.unwrap();
            let view = store.load_view(outcome.session_id).await.unwrap().unwrap();
            assert_eq!(view.runs[0].skill.as_deref(), Some("ask-matt"));
            assert_eq!(view.runs[0].objective, submitted);
            assert!(events.iter().any(|event| matches!(
                &event.event,
                Event::MessageAccepted { text, skill: Some(skill), .. }
                    if text == &submitted && skill == "ask-matt"
            )));
            let started = events
                .iter()
                .position(|event| matches!(event.event, Event::ToolStarted { .. }))
                .unwrap();
            let observed = events
                .iter()
                .position(|event| matches!(event.event, Event::ToolFinished { .. }))
                .unwrap();
            assert!(started < observed);
            if let Some(egress) = events
                .iter()
                .position(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
            {
                assert!(!mismatch);
                assert!(observed < egress);
            } else {
                assert!(mismatch);
            }
            store.close().await.unwrap();
        }
        });
}

#[test]
#[cfg_attr(not(arany_ci), ignore = "native Skill Guard read on Linux and macOS")]
fn invoked_skill_crash_window_resumes_without_rereading_or_rerunning() {
    let _gate = process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
    let skill_dir = workspace.join(".agents/skills/ask-matt");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let body = b"---\nname: ask-matt\ndescription: Guide the task\n---\nPinned guidance.\n";
    std::fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    let state_path = temp.path().join("state");
    let config = serde_json::json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"mcp":[],"skills":[{
        "name":"ask-matt","description":"Reviewed guidance","directory":skill_dir,
        "files":{"SKILL.md":Sha256::digest(body).iter().map(|byte| format!("{byte:02x}")).collect::<String>()}
    }]});
    let open = || {
        engine_support::open(
            StateRoot::admit(&state_path).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap()
    };
    let engine = open();
    let config_path = state_path.join("tools.json");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "Applied.".into(),
    })]));
    let mut input = request(&workspace, "do X");
    input.skill = Some("ask-matt".into());
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
    .unwrap();
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let events = store.load_session(first.session_id).await.unwrap();
    store.close().await.unwrap();
    let finished = events
        .iter()
        .find(|event| matches!(event.event, Event::ToolFinished { .. }))
        .unwrap()
        .sequence;
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute("DELETE FROM events WHERE sequence > ?1", [i64::try_from(finished).unwrap()])
        .unwrap();
    drop(connection);

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let prefix = store.load_session(first.session_id).await.unwrap();
    assert_eq!(prefix.last().unwrap().sequence, finished);
    assert!(store.load_view(first.session_id).await.unwrap().is_some(), "the crash window prefix replays strictly");
    store.close().await.unwrap();

    let engine = open();
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "Next.".into(),
    })]));
    let mut next = request(&workspace, "next turn");
    next.session_id = Some(first.session_id);
    next.title = None;
    let second = run_with(
        &engine,
        &provider,
        next,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(second.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let events = store.load_session(first.session_id).await.unwrap();
    let view = store.load_view(first.session_id).await.unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolStarted { .. }))
            .count(),
        1,
        "resume must not re-read or re-run the Skill"
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(
        provider.requests.lock().unwrap()[0]
            .input
            .tools
            .as_ref()
            .is_none_or(|tools| tools.invoked_skill.is_none())
    );
    store.close().await.unwrap();
    });
}

#[test]
#[cfg_attr(not(arany_ci), ignore = "native Skill Guard read on Linux and macOS")]
fn researcher_children_cannot_load_a_model_hidden_skill() {
    let _gate = process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
    let skill_dir = workspace.join(".agents/skills/ask-matt");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let body = b"---\nname: ask-matt\ndescription: Guide\n---\nGuidance.\n";
    std::fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    let state_path = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state_path).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let config = serde_json::json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"mcp":[],"skills":[{
        "name":"ask-matt","description":"Reviewed guidance","directory":skill_dir,"disable_model_invocation":true,
        "files":{"SKILL.md":Sha256::digest(body).iter().map(|byte| format!("{byte:02x}")).collect::<String>()}
    }]});
    let config_path = state_path.join("tools.json");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect the fact".into(),
                profile: None,
            }],
        }),
        tool_outcome(arany::ToolCall::Skill {
            name: "ask-matt".into(),
            resource: None,
        }),
        ProviderOutcome::Answer(Answer { text: "Child".into() }),
        ProviderOutcome::Answer(Answer { text: "Team".into() }),
        ProviderOutcome::Answer(Answer { text: "Team".into() }),
    ]));
    let mut input = request(&workspace, "Inspect");
    input.policy = CollaborationPolicy::Team {
        max_active_children: 1,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await;
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let session = outcome.unwrap().session_id;
    let events = store.load_session(session).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::ToolStarted { .. })),
        "a researcher must not dispatch a model-hidden Skill"
    );
    store.close().await.unwrap();
    });
}

#[tokio::test]
async fn missing_file_targets_fail_with_guard_attestation_and_recover_explicitly() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    let state = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let config = state.join("tools.json");
    std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
    std::fs::set_permissions(config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let write = || arany::ToolCall::Write {
        path: "notes/new/file.txt".into(),
        expected_digest: None,
        content: "New".into(),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_outcome(write()),
        tool_outcome(arany::ToolCall::Mkdir {
            path: "notes/new/deeper".into(),
        }),
        tool_outcome(arany::ToolCall::Edit {
            path: "notes/missing.txt".into(),
            expected_digest: Sha256::digest(b"old")
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            old: "old".into(),
            new: "new".into(),
        }),
        tool_outcome(arany::ToolCall::Mkdir {
            path: "notes/new".into(),
        }),
        tool_outcome(write()),
        arany::test_support::goals::finish_with("Created the parent and wrote the file"),
    ]));
    let mut input = request(&workspace, "Write a note after creating its parent");
    input.include_paths.clear();
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
    .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    assert!(provider.responses.lock().unwrap().is_empty());
    assert_eq!(
        std::fs::read(workspace.join("notes/new/file.txt")).unwrap(),
        b"New"
    );
    assert!(!workspace.join("notes/new/deeper").exists());
    assert!(!workspace.join("notes/missing.txt").exists());
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(result.session_id).await.unwrap();
    let view = SessionView::replay(result.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(view.runs[0], result.run);
    assert_eq!(view.runs[0].tools.len(), 5);
    for (index, text) in [
        (0, "Parent directory not found; create it with Mkdir first"),
        (1, "Parent directory not found; create it with Mkdir first"),
        (
            2,
            "File or directory not found; List the parent directory to check the path",
        ),
    ] {
        let observation = view.runs[0].tools[index].observation.as_ref().unwrap();
        assert_eq!(observation.disposition, arany::ToolDisposition::Failed);
        assert_eq!(observation.output, text);
        assert!(matches!(
            observation.attestation,
            Some(arany::Attestation::Guard(_))
        ));
    }
    for step in &view.runs[0].tools[3..] {
        assert_eq!(
            step.observation.as_ref().unwrap().disposition,
            arany::ToolDisposition::Succeeded
        );
    }
    store.close().await.unwrap();
}
