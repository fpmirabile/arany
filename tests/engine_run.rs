use arany::{
    AgentPhase, AgentRunId, AgentStatus, ChatGptAdmission, ChatGptProvenance, CollaborationPolicy,
    CompactionFailure, CompactionItem, CompactionRequest, CompactionResponse, CompactionStatus,
    ContextUsage, Delegate, Effort, Engine, EngineError, Event, Finish, ImageAttachment,
    ImageOrigin, OutputTokenBound, Provider, ProviderCallDisposition, ProviderError, ProviderImage,
    ProviderOutcome, ProviderRequest, ProviderResponse, RunCancellation, RunId, RunProgress,
    RunRequest, RunStatus, SessionId, SessionView, StateRoot, Store, UnansweredStatus,
    continue_session, fork_session, list_sessions, rename_session, resume_session,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

#[path = "engine_run/performance.rs"]
mod performance;

#[cfg(unix)]
#[path = "common/process.rs"]
pub mod process;

#[cfg(unix)]
#[path = "engine_run/workspace_security.rs"]
mod workspace_security;

struct ScriptedProvider {
    profile: String,
    model: String,
    effort: Option<Effort>,
    saved_api_account_id: Option<Uuid>,
    chatgpt_provenance: Option<ChatGptProvenance>,
    output_token_bound: OutputTokenBound,
    capacity: u8,
    responses: Mutex<VecDeque<Result<ProviderOutcome, ProviderError>>>,
    cancel_at_completion: Option<(AgentPhase, RunCancellation)>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    compact_responses: Mutex<VecDeque<Result<CompactionResponse, ProviderError>>>,
    compact_requests: Arc<Mutex<Vec<CompactionRequest>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ProviderOutcome>) -> Self {
        Self {
            profile: "scripted".into(),
            model: "test-model".into(),
            effort: None,
            saved_api_account_id: None,
            chatgpt_provenance: None,
            output_token_bound: OutputTokenBound::ProviderEnforced,
            capacity: 3,
            responses: Mutex::new(responses.into_iter().map(Ok).collect()),
            cancel_at_completion: None,
            requests: Arc::new(Mutex::new(Vec::new())),
            compact_responses: Mutex::new(VecDeque::new()),
            compact_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_compactions(
        mut self,
        responses: Vec<Result<CompactionResponse, ProviderError>>,
    ) -> Self {
        self.compact_responses = Mutex::new(responses.into());
        self
    }

    fn with_profile(mut self, profile: &str, model: &str) -> Self {
        self.profile = profile.into();
        self.model = model.into();
        self
    }

    fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }

    fn with_saved_api_account(mut self, id: Uuid) -> Self {
        self.saved_api_account_id = Some(id);
        self
    }

    fn with_chatgpt_account(mut self, id: Uuid, fingerprint: [u8; 32]) -> Self {
        self.profile = "chatgpt".into();
        self.model = "model-a".into();
        self.effort = Some(Effort::High);
        self.chatgpt_provenance = Some(ChatGptProvenance {
            account_id: id,
            evidence_fingerprint: fingerprint,
            admission: ChatGptAdmission::AccountConsent,
        });
        self.output_token_bound = OutputTokenBound::LocalAcceptanceOnly;
        self.capacity = 1;
        self
    }
}

impl Provider for ScriptedProvider {
    fn profile_name(&self) -> &str {
        &self.profile
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn reasoning_effort(&self) -> Option<Effort> {
        self.effort
    }

    fn max_concurrent_calls(&self) -> u8 {
        self.capacity
    }

    fn saved_api_account_id(&self) -> Option<Uuid> {
        self.saved_api_account_id
    }

    fn chatgpt_provenance(&self) -> Option<ChatGptProvenance> {
        self.chatgpt_provenance.clone()
    }

    fn output_token_bound(&self) -> OutputTokenBound {
        self.output_token_bound
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let phase = request.phase;
        self.requests.lock().expect("requests lock").push(request);
        let outcome = self
            .responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .expect("scripted response");
        if let Some((target, cancellation)) = &self.cancel_at_completion
            && phase == *target
        {
            assert!(cancellation.cancel(), "first completion-time cancellation");
        }
        Ok(ProviderResponse {
            outcome: outcome?,
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
            wire_provenance: None,
        })
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        self.compact_requests
            .lock()
            .expect("compaction requests lock")
            .push(request);
        self.compact_responses
            .lock()
            .expect("compaction responses lock")
            .pop_front()
            .expect("scripted compaction response")
    }
}

struct GatedProvider {
    started: mpsc::UnboundedSender<(String, oneshot::Sender<()>)>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    fail_child: Option<&'static str>,
    capacity: u8,
}

impl Provider for GatedProvider {
    fn profile_name(&self) -> &str {
        "gated"
    }

    fn model_name(&self) -> &str {
        "test-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        self.capacity
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        let outcome = match request.phase {
            AgentPhase::RootPlan => ProviderOutcome::Delegate(Delegate {
                children: vec!["A".into(), "B".into(), "C".into()],
            }),
            AgentPhase::ChildWork => {
                let (release, gate) = oneshot::channel();
                self.started
                    .send((request.objective.clone(), release))
                    .map_err(|_| ProviderError::Unavailable)?;
                gate.await.map_err(|_| ProviderError::Unavailable)?;
                if self.fail_child == Some(request.objective.as_str()) {
                    return Err(ProviderError::RemoteHttp(403));
                }
                ProviderOutcome::Finish(Finish {
                    summary: "done".into(),
                    result: format!("result:{}", request.objective),
                })
            }
            AgentPhase::RootSynthesis => ProviderOutcome::Finish(Finish {
                summary: "synthesized".into(),
                result: "A B C".into(),
            }),
        };
        Ok(ProviderResponse {
            outcome,
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
            wire_provenance: None,
        })
    }
}

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct WaitingProvider {
    started: mpsc::UnboundedSender<()>,
    dropped: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

struct WaitingCompactionProvider {
    started: mpsc::UnboundedSender<()>,
    dropped: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    compact_requests: Arc<Mutex<Vec<CompactionRequest>>>,
}

impl Provider for WaitingCompactionProvider {
    fn profile_name(&self) -> &str {
        "waiting-compaction"
    }

    fn model_name(&self) -> &str {
        "test-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().expect("requests").push(request);
        Ok(ProviderResponse {
            outcome: ProviderOutcome::Finish(Finish {
                summary: "seed".into(),
                result: "Seed answer".into(),
            }),
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
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

impl Provider for WaitingProvider {
    fn profile_name(&self) -> &str {
        "waiting"
    }

    fn model_name(&self) -> &str {
        "test-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().expect("requests").push(request);
        let _drop_counter = DropCounter(Arc::clone(&self.dropped));
        self.started
            .send(())
            .map_err(|_| ProviderError::Unavailable)?;
        std::future::pending().await
    }
}

async fn wait_for_child_finish(store: &Store, session_id: SessionId, objective: &str) {
    loop {
        let events = store
            .load_session(session_id)
            .await
            .expect("observable Events");
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

fn request(workspace: &Path, objective: &str) -> RunRequest {
    RunRequest {
        session_id: None,
        title: Some("Research".into()),
        objective: objective.into(),
        images: Vec::new(),
        workspace: workspace.to_path_buf(),
        include_paths: vec!["notes/facts.txt".into()],
        policy: CollaborationPolicy::Single,
    }
}

fn image_fixture(bytes: usize) -> ImageAttachment {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let mut png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC").unwrap();
    if bytes > png.len() {
        let data_len = bytes
            .checked_sub(png.len() + 12)
            .expect("PNG padding length");
        let mut chunk = (data_len as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(b"arNy");
        chunk.resize(8 + data_len, 0);
        let mut crc = !0_u32;
        for &byte in &chunk[4..] {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
            }
        }
        chunk.extend_from_slice(&(!crc).to_be_bytes());
        png.splice(png.len() - 12..png.len() - 12, chunk);
    }
    assert_eq!(png.len(), bytes);
    ImageAttachment::from_png(&png).expect("synthetic PNG container")
}

fn assert_test_call(
    actual: &ProviderRequest,
    phase: AgentPhase,
    objective: &str,
    includes: &[&str],
    history: &[arany::HistoryTurn],
    summary: Option<&str>,
    children: &[arany::ChildResult],
) {
    assert_eq!(actual.phase, phase, "call phase");
    assert!(actual.model == "test-model", "call model");
    assert!(actual.objective == objective, "call objective");
    assert!(actual.images.is_empty(), "call images");
    assert!(actual.instructions.is_none(), "call instructions");
    assert!(
        actual
            .includes
            .iter()
            .map(String::as_str)
            .eq(includes.iter().copied()),
        "call includes"
    );
    assert!(actual.history == history, "call history");
    assert!(actual.context_summary.as_deref() == summary, "call summary");
    assert!(actual.child_results == children, "call child results");
    assert_eq!(actual.max_output_tokens, 4096, "call output cap");
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
        assert!(
            stdout.metadata().expect("stdout size").len() <= stdout_limit as u64
                && stderr.metadata().expect("stderr size").len() <= 256 * 1024,
            "process output bound"
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
        assert!(bytes.len() <= limit, "process observation bound");
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
            ProviderOutcome::Finish(Finish {
                summary: "First summary".into(),
                result: "First result".into(),
            }),
            ProviderOutcome::Finish(Finish {
                summary: "Second summary".into(),
                result: "Second result".into(),
            }),
        ])
        .with_profile("openai", "gpt-5.4")
        .with_saved_api_account(account_id)
        .with_effort(Effort::Medium);
        let recorded_requests = Arc::clone(&provider.requests);
        let first_image = image_fixture(64 * 1024);
        let second_image = image_fixture(69);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let mut first_request = request(&workspace, "First question");
        first_request.images = vec![first_image.clone()];
        let first = engine
            .run(first_request)
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
                .instruction_digest,
            Some(instruction_digest)
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
        let second = engine.run(second_request).await.expect("second Run");
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
                assert!(call.objective == objective, "exact objective");
                assert!(call.context_summary.is_none(), "no derived summary");
                assert!(call.child_results.is_empty(), "no child context");
                assert_eq!(call.max_output_tokens, 4096);
                assert_eq!(call.run_id, run.id);
                assert_eq!(call.agent_run_id, run.agents[0].id);
            }
            assert_eq!(
                requests[0].instructions.as_deref(),
                Some("Primary instruction")
            );
            assert_eq!(requests[0].includes, ["Fact one"]);
            assert_eq!(requests[0].images, vec![ProviderImage {
                origin: ImageOrigin::Objective, image: first_image.clone(),
            }]);
            assert!(requests[0].history.is_empty());
            assert_eq!(
                requests[1].instructions.as_deref(),
                Some("Changed instruction")
            );
            assert_eq!(requests[1].includes, ["Fact two"]);
            assert_eq!(requests[1].history.len(), 1);
            assert_eq!(requests[1].history[0].user, "First question");
            assert_eq!(requests[1].history[0].assistant, "First result");
            assert_eq!(requests[1].images, vec![
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
        assert_eq!(events.len(), 15);
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
        store.close().await.expect("store shutdown");

        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate { children: vec!["Inspect current image".into()] }),
            ProviderOutcome::Finish(Finish { summary: "inspected".into(), result: "Child answer".into() }),
            ProviderOutcome::Finish(Finish { summary: "combined".into(), result: "Team answer".into() }),
            ProviderOutcome::Finish(Finish { summary: "large".into(), result: "Large image answer".into() }),
            ProviderOutcome::Finish(Finish { summary: "after summary".into(), result: "Summary answer".into() }),
            ProviderOutcome::Finish(Finish { summary: "fork".into(), result: "Fork answer".into() }),
        ]).with_compactions(vec![Ok(CompactionResponse {
            summary: "Image conversation summary".into(), response_id: None,
            input_tokens: Some(50), output_tokens: Some(12), wire_provenance: None,
        })]);
        let requests = Arc::clone(&provider.requests);
        let compactions = Arc::clone(&provider.compact_requests);
        let mut engine = Engine::open(StateRoot::open_existing(&state_path).unwrap(), provider).unwrap();
        let mut next = request(&workspace, "");
        next.title = None;
        next.session_id = Some(first.session_id);
        next.images = vec![second_image.clone()];
        next.policy = CollaborationPolicy::Team { max_active_children: 1 };
        let third = engine.run(next).await.expect("image-only team after reopen");
        let fork_id = engine.fork_session(first.session_id, workspace.clone(), Some("Image fork".into())).await.unwrap();
        let large_image = image_fixture(arany::MAX_IMAGE_BYTES);
        let maximal_objective = "\0".repeat(8 * 1024);
        let mut next = request(&workspace, &maximal_objective);
        next.title = None;
        next.session_id = Some(first.session_id);
        next.images = vec![large_image.clone()];
        let fourth = engine.run(next).await.expect("whole-history image clipping");
        assert!(fourth.run.config.as_ref().unwrap().history_run_ids.is_empty());
        assert_eq!(fourth.run.config.as_ref().unwrap().excluded_history_runs, 3);
        let compacted = engine.compact_session(first.session_id, workspace.clone()).await.unwrap();
        assert_eq!(compacted.compiler_version, 2);
        for (id, objective) in [(first.session_id, maximal_objective.as_str()), (fork_id, "After fork")] {
            let mut next = request(&workspace, objective);
            next.session_id = Some(id);
            next.title = None;
            assert_eq!(engine.run(next).await.unwrap().run.status, RunStatus::Finished);
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
                    objective: "Inspect current image".into(), summary: "inspected".into(), result: "Child answer".into(),
                }]),
                (3, AgentPhase::RootPlan, maximal_objective.as_str(), Vec::new(), vec![ProviderImage { origin: ImageOrigin::Objective, image: large_image.clone() }], Vec::new()),
                (4, AgentPhase::RootPlan, maximal_objective.as_str(), Vec::new(), Vec::new(), Vec::new()),
                (5, AgentPhase::RootPlan, "After fork", [history, vec![arany::HistoryTurn { user: "".into(), assistant: "Team answer".into() }]].concat(), [historical_images, vec![ProviderImage { origin: ImageOrigin::History { turn_index: 2 }, image: second_image.clone() }]].concat(), Vec::new()),
            ] {
                assert_eq!(calls[index], ProviderRequest {
                    run_id: calls[index].run_id, agent_run_id: calls[index].agent_run_id,
                    phase, model: "test-model".into(), instructions: Some("Changed instruction".into()),
                    collaboration: if index == 0 || index == 2 { CollaborationPolicy::Team { max_active_children: 1 } } else { CollaborationPolicy::Single },
                    objective: objective.into(), images: expected_images, includes: vec!["Fact two".into()],
                    history: expected_history,
                    context_summary: (index == 4).then(|| "Image conversation summary".into()),
                    child_results, max_output_tokens: 4096, tools: None,
                }, "image request {index}");
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
            let mut source_bytes = 0;
            for (item, (objective, answer, image)) in compact.items.iter().zip([
                ("First question", "First result", &first_image),
                ("Second question", "Second result", &second_image),
                ("", "Team answer", &second_image),
                (maximal_objective.as_str(), "Large image answer", &large_image),
            ]) {
                let digest: String = image.digest().iter().map(|byte| format!("{byte:02x}")).collect();
                let user = format!("{objective}\nImage 1: PNG 1x1, {} bytes, SHA-256 {digest}", image.byte_len());
                assert_eq!(item, &CompactionItem::Completed(arany::HistoryTurn { user: user.clone(), assistant: answer.into() }));
                assert!(!user.contains(image.base64()), "compaction excludes image bytes");
                source_bytes += user.len() + answer.len() + 64;
            }
            assert_eq!(compacted.source_bytes as usize, source_bytes);
        }
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(first.session_id).await.unwrap();
        assert_eq!(events.len(), 41);
        let view = SessionView::replay(first.session_id, &events).unwrap().unwrap();
        assert_eq!(view.runs.len(), 5);
        assert_eq!(view.runs[0].images, std::slice::from_ref(&first_image));
        assert_eq!(view.runs[3].images, [large_image]);
        assert!(view.runs[3].objective == maximal_objective, "image objective survives escaping");
        assert!(view.runs[4].objective == maximal_objective, "text-only objective survives escaping");
        assert_eq!(view.runs[2].objective, "", "image-only objective is not invented text");
        assert_eq!(view.compactions[0].record, compacted);
        for output in ["text", "jsonl"] {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arany"));
            command.env_clear().current_dir(temp.path()).args(["show", "--output", output, "--state-dir"])
                .arg(&state_path).arg(first.session_id.to_string());
            let observed = observe_process(&mut command, temp.path(), 512 * 1024);
            assert!(observed.status.success(), "image show {output} exit");
            assert!(observed.stderr.is_empty(), "image show {output} error channel");
            let expected = arany::render_session(&view, &events, if output == "text" { arany::Output::Text } else { arany::Output::Jsonl });
            assert!(observed.stdout == expected.as_bytes(), "image show {output} bytes differ");
            if output == "text" {
                let text = String::from_utf8(observed.stdout).unwrap();
                assert!(text.contains("Image 1: PNG 1x1, 196608 bytes"));
                assert!(!text.contains("iVBOR"), "human show leaked encoded image bytes");
            } else {
                let lines = std::str::from_utf8(&observed.stdout).unwrap().lines().map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()).collect::<Vec<_>>();
                assert_eq!(lines.len(), events.len());
                let accepted = lines.iter().filter(|line| line["kind"] == "MessageAccepted").collect::<Vec<_>>();
                assert_eq!(accepted.len(), 5);
                assert_eq!(accepted[0]["event_version"], 2);
                assert_eq!(accepted[0]["payload"]["images"][0]["media_type"], "image/png");
                assert_eq!(accepted[0]["payload"]["images"][0]["data"], view.runs[0].images[0].base64());
                assert_eq!(accepted[2]["payload"]["text"], "");
                assert!(accepted[3]["payload"]["text"].as_str() == Some(maximal_objective.as_str()));
                let image_payload_bytes = accepted[3]["payload"].to_string().len();
                assert!((300 * 1024..=320 * 1024).contains(&image_payload_bytes), "maximally escaped image input fits its Event");
                assert_eq!(accepted[4]["event_version"], 1);
                assert!(accepted[4]["payload"]["text"].as_str() == Some(maximal_objective.as_str()));
                assert_eq!(accepted[4]["payload"].to_string().len(), 6 * 8 * 1024 + 11, "maximally escaped text-only input fits its Event");
                assert!(accepted[4]["payload"].get("images").is_none(), "text-only v1 changed encoding");
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
        for fault in ["legacy version", "unknown version", "missing images", "empty images", "five images", "aggregate bytes", "non-PNG", "corrupted image", "URL", "extra field", "text overflow", "payload overflow", "agent scope"] {
            let mut invalid = original.clone();
            let mut version = 2;
            let mut agent_scope = None;
            match fault {
                "legacy version" => version = 1,
                "unknown version" => version = 3,
                "missing images" => { invalid.as_object_mut().unwrap().remove("images"); }
                "empty images" => invalid["images"] = serde_json::json!([]),
                "five images" => invalid["images"] = serde_json::to_value(vec![second_image.clone(); 5]).unwrap(),
                "aggregate bytes" => invalid["images"] = serde_json::to_value(vec![view.runs[0].images[0].clone(); 4]).unwrap(),
                "non-PNG" => invalid["images"][0]["media_type"] = "image/jpeg".into(),
                "corrupted image" => invalid["images"][0]["data"] = "not PNG".into(),
                "URL" => invalid["images"][0]["data"] = "https://example.invalid/image.png".into(),
                "extra field" => invalid["images"][0]["path"] = "/outside.png".into(),
                "text overflow" => invalid["text"] = "x".repeat(8 * 1024 + 1).into(),
                "payload overflow" => invalid["text"] = "x".repeat(320 * 1024).into(),
                "agent scope" => agent_scope = Some(AgentRunId::new().to_string()),
                _ => unreachable!(),
            }
            connection.execute("UPDATE events SET event_version=?1, payload=?2, agent_run_id=?3 WHERE sequence=?4",
                rusqlite::params![version, invalid.to_string(), agent_scope, sequence]).unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
            assert!(matches!(store.load_view(first.session_id).await, Err(arany::StoreError::InvalidHistory)), "image replay fault: {fault}");
            store.close().await.unwrap();
        }
        connection.execute("UPDATE events SET event_version=2, payload=?1, agent_run_id=NULL WHERE sequence=?2", rusqlite::params![payload, sequence]).unwrap();
        drop(connection);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert_eq!(store.load_view(first.session_id).await.unwrap().unwrap(), view);
        assert_eq!(store.load_view(fork_id).await.unwrap().unwrap(), fork);
        store.close().await.unwrap();
    });
}

#[test]
fn subscription_account_and_local_bound_survive_run_and_compaction_replay() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        for admission in [
            ChatGptAdmission::Conformance,
            ChatGptAdmission::AccountConsent,
        ] {
            let temp = tempfile::tempdir().expect("temporary root");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
            std::fs::write(workspace.join("notes/facts.txt"), "Synthetic fact").expect("include");
            let state_path = temp.path().join("state");
            let account_id = Uuid::now_v7();
            let fingerprint = [7; 32];
            let mut provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                summary: "Synthetic summary".into(),
                result: "Synthetic answer".into(),
            })])
            .with_chatgpt_account(account_id, fingerprint)
            .with_compactions(vec![Ok(CompactionResponse {
                summary: "Compacted synthetic history".into(),
                response_id: Some("resp_compact".into()),
                input_tokens: Some(20),
                output_tokens: Some(8),
                wire_provenance: Some(
                    arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested,
                ),
            })]);
            provider.chatgpt_provenance.as_mut().unwrap().admission = admission;
            let mut engine = Engine::open(
                StateRoot::admit(&state_path).expect("private state"),
                provider,
            )
            .expect("engine");
            let outcome = engine
                .run(request(&workspace, "Synthetic objective"))
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
                    admission,
                })
            );
            let compaction = engine
                .compact_session(outcome.session_id, workspace)
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
                Event::ContextCompacted { record }
                    if record.chatgpt_provenance == config.chatgpt_provenance
                        && record.output_token_bound == OutputTokenBound::LocalAcceptanceOnly
            )));
            store.close().await.expect("store shutdown");
        }
    });
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
            ProviderOutcome::Finish(Finish {
                summary: "first".into(),
                result: "first answer".into(),
            }),
            ProviderOutcome::Finish(Finish {
                summary: "second".into(),
                result: "second answer".into(),
            }),
            ProviderOutcome::Finish(Finish {
                summary: "fork".into(),
                result: "fork answer".into(),
            }),
        ]);
        let requests = Arc::clone(&provider.requests);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let first = engine
            .run(request(&workspace, "First question"))
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
            .await
            , Err(EngineError::WorkspaceMismatch)
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
                .compact_session(first.session_id, other_workspace)
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
        assert_eq!(listed.iter().map(|item| item.id).collect::<Vec<_>>(), [fork_id, first.session_id]);
        assert_eq!(listed[0].title, "Branch");
        let nested_id = engine
            .fork_session(fork_id, workspace.clone(), Some("Nested branch".into()))
            .await
            .expect("fork of fork before a new Run");

        let mut parent_request = request(&workspace, "Later source question");
        parent_request.session_id = Some(first.session_id);
        parent_request.title = None;
        engine.run(parent_request).await.expect("later source Run");

        let mut child_request = request(&workspace, "Branch question");
        child_request.session_id = Some(fork_id);
        child_request.title = None;
        let child = engine.run(child_request).await.expect("fork Run");
        assert_eq!(child.run.status, RunStatus::Finished);
        assert_eq!(
            child.run.config.as_ref().expect("Run config").history_run_ids,
            vec![first.run.id]
        );
        {
            let observed = requests.lock().expect("request lock");
            assert_eq!(observed.len(), 3);
            assert_eq!(observed[2].history.len(), 1);
            assert_eq!(observed[2].history[0].user, "First question");
            assert_eq!(observed[2].history[0].assistant, "first answer");
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

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("fault injection connection");
        connection
            .execute(
                r#"UPDATE events SET payload='{"title":"Altered"}' WHERE session_id=?1 AND kind='SessionStarted'"#,
                [first.session_id.to_string()],
            )
            .expect("altered source prefix");
        drop(connection);
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store after drift");
        assert!(matches!(
            store.load_session(fork_id).await,
            Err(arany::StoreError::InvalidHistory)
        ));
        store.close().await.expect("store shutdown after drift");

        let provider = ScriptedProvider::new(Vec::new());
        let attempted_calls = Arc::clone(&provider.requests);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            provider,
        )
        .expect("engine after drift");
        let mut rejected = request(&workspace, "Must not disclose");
        rejected.session_id = Some(fork_id);
        rejected.title = None;
        assert!(matches!(
            engine.run(rejected).await,
            Err(EngineError::Store(arany::StoreError::InvalidHistory))
        ));
        assert!(attempted_calls.lock().expect("request lock").is_empty());
        engine.close().await.expect("engine shutdown after drift");
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
            ProviderOutcome::Finish(Finish {
                summary: "one".into(),
                result: "Answer one".into(),
            }),
            ProviderOutcome::Finish(Finish {
                summary: "two".into(),
                result: "Answer two".into(),
            }),
        ])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Derived summary".into(),
            response_id: Some("compact-1".into()),
            input_tokens: Some(100),
            output_tokens: Some(8),
            wire_provenance: None,
        })]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let first = engine
            .run(request(&workspace, "Question one"))
            .await
            .expect("first Run");
        let mut second_request = request(&workspace, "Question two");
        second_request.session_id = Some(first.session_id);
        second_request.title = None;
        let second = engine.run(second_request).await.expect("second Run");
        let second_config = second.run.config.as_ref().expect("pinned context usage");
        assert_eq!(
            second_config.context_usage,
            Some(ContextUsage {
                used_bytes: 102,
                budget_bytes: 376 * 1024,
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
            (179_071, 376 * 1024, 179_071, false),
            (179_072, 376 * 1024, 179_072, true),
            (179_072, 376 * 1024, 0, false),
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
        assert!(!pressure.auto_compaction_due(), "legacy footprint");
        for (tool_history, bytes, due) in
            [(1, 43_903, false), (1, 43_904, true), (0, 43_904, false)]
        {
            pressure.context_usage = Some(ContextUsage {
                used_bytes: bytes,
                budget_bytes: 376 * 1024,
                compactable_bytes: bytes,
                tool_history_bytes: tool_history,
            });
            assert_eq!(pressure.auto_compaction_due(), due, "Tool source headroom");
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
            .auto_compact_session(first.session_id, workspace.clone(), second.run.id)
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
                .compact_session(first.session_id, workspace.clone())
                .await,
            Err(EngineError::CompactionInputEmpty)
        ));
        assert!(
            engine
                .auto_compact_session(first.session_id, workspace.clone(), second.run.id)
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
        assert_eq!(snapshot_line["payload"]["input_tokens"], 100);
        assert_eq!(snapshot_line["payload"]["output_tokens"], 8);
        assert_eq!(
            snapshot_line["payload"]["status"]["summary"],
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
            ProviderOutcome::Finish(Finish {
                summary: "three".into(),
                result: "Answer three".into(),
            }),
            ProviderOutcome::Finish(Finish {
                summary: "four".into(),
                result: "Answer four".into(),
            }),
        ])
        .with_profile("changed-provider", "changed-model")
        .with_compactions(vec![Err(ProviderError::RemoteHttp(403))]);
        let observed = Arc::clone(&provider.requests);
        let resumed_compacts = Arc::clone(&provider.compact_requests);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            provider,
        )
        .expect("resumed engine");
        let mut third_request = request(&workspace, "Question three");
        third_request.session_id = Some(first.session_id);
        third_request.title = None;
        let third = engine.run(third_request).await.expect("resumed Run");
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
            .auto_compact_session(first.session_id, workspace.clone(), third.run.id)
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
                .auto_compact_session(first.session_id, workspace.clone(), third.run.id)
                .await
                .expect("failed automatic boundary not retried")
                .is_none()
        );
        let mut fourth_request = request(&workspace, "Question four");
        fourth_request.session_id = Some(first.session_id);
        fourth_request.title = None;
        engine.run(fourth_request).await.expect("Run after failure");
        assert!(
            engine
                .auto_compact_session(first.session_id, workspace.clone(), third.run.id)
                .await
                .expect("stale automatic boundary")
                .is_none()
        );
        assert_eq!(resumed_compacts.lock().expect("requests").len(), 1);
        {
            let requests = observed.lock().expect("request lock");
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[0].context_summary.as_deref(),
                Some("Derived summary")
            );
            assert!(requests[0].history.is_empty());
            assert_eq!(
                requests[1].context_summary.as_deref(),
                Some("Derived summary")
            );
            assert_eq!(requests[1].history.len(), 1);
            assert_eq!(requests[1].history[0].user, "Question three");
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
            ProviderOutcome::Finish(Finish {
                summary: "source".into(),
                result: "Source answer".into(),
            }),
            ProviderOutcome::Finish(Finish {
                summary: "child".into(),
                result: "Child answer".into(),
            }),
        ])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Source-only derived summary".into(),
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
            wire_provenance: None,
        })]);
        let observed = Arc::clone(&provider.requests);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let first = engine
            .run(request(&workspace, "Source question"))
            .await
            .expect("source Run");
        engine
            .compact_session(first.session_id, workspace.clone())
            .await
            .expect("source compaction");
        let fork_id = engine
            .fork_session(first.session_id, workspace.clone(), None)
            .await
            .expect("fork at committed Run boundary");
        let mut branch_request = request(&workspace, "Branch question");
        branch_request.session_id = Some(fork_id);
        branch_request.title = None;
        let branch = engine.run(branch_request).await.expect("branch Run");
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
            assert!(requests[1].context_summary.is_none());
            assert_eq!(requests[1].history.len(), 1);
            assert_eq!(requests[1].history[0].assistant, "Source answer");
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
            let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                summary: "source".into(),
                result: "Source answer".into(),
            })])
            .with_compactions(vec![Ok(CompactionResponse {
                summary: "Derived summary".into(),
                response_id: None,
                input_tokens: Some(12),
                output_tokens: Some(8),
                wire_provenance: None,
            })]);
            let mut engine = Engine::open(
                StateRoot::admit(&state_path).expect("private state"),
                provider,
            )
            .expect("engine");
            let first = engine
                .run(request(&workspace, "Source question"))
                .await
                .expect("source Run");
            engine
                .compact_session(first.session_id, workspace.clone())
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
            let mut document: serde_json::Value =
                serde_json::from_str(&payload).expect("valid snapshot JSON");
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
                    [document.to_string(), first.session_id.to_string()],
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
            let mut engine = Engine::open(
                StateRoot::admit(&state_path).expect("state readmission"),
                provider,
            )
            .expect("engine after fault");
            let mut rejected = request(&workspace, "Must not disclose");
            rejected.session_id = Some(first.session_id);
            rejected.title = None;
            assert!(
                matches!(
                    engine.run(rejected).await,
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
            ProviderOutcome::Finish(Finish {
                summary: "done".into(),
                result: "x".repeat(32 * 1024),
            });
            7
        ]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let mut session_id = None;
        for _ in 0..7 {
            let mut next = request(&workspace, &"Q".repeat(8 * 1024));
            next.session_id = session_id;
            if session_id.is_some() {
                next.title = None;
            }
            session_id = Some(engine.run(next).await.expect("bounded Run").session_id);
        }
        let session_id = session_id.expect("Session exists");
        assert!(matches!(
            engine.compact_session(session_id, workspace.clone()).await,
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
        assert_eq!(events.len(), 50);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
                .count(),
            7
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::ContextCompacted { .. }))
        );
        store.close().await.expect("store shutdown");

        let maintained_path = temp.path().join("maintained-state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Finish(Finish {
                summary: "done".into(),
                result: "x".repeat(32 * 1024),
            });
            16
        ])
        .with_compactions(
            (0..2)
                .map(|_| {
                    Ok(CompactionResponse {
                        summary: "s".repeat(8 * 1024),
                        response_id: None,
                        input_tokens: Some(100),
                        output_tokens: Some(1024),
                        wire_provenance: None,
                    })
                })
                .collect(),
        );
        let requests = Arc::clone(&provider.requests);
        let compactions = Arc::clone(&provider.compact_requests);
        let mut engine = Engine::open(StateRoot::admit(&maintained_path).unwrap(), provider)
            .expect("maintained Engine");
        let mut session_id = None;
        let mut maintenance_sequences = Vec::new();
        for index in 0..16 {
            let objective = format!("{index:02}{}", "Q".repeat(8 * 1024 - 2));
            let mut next = request(&workspace, &objective);
            next.session_id = session_id;
            if session_id.is_some() {
                next.title = None;
            }
            let outcome = engine.run(next).await.expect("maintained bounded Run");
            session_id = Some(outcome.session_id);
            let config = outcome.run.config.as_ref().expect("context footprint");
            assert_eq!(
                config.auto_compaction_due(),
                matches!(index, 5 | 11),
                "maintenance must precede an oversized prefix at turn {}",
                index + 1
            );
            if matches!(index, 6 | 12) {
                assert_eq!(
                    config.compaction_event_sequence,
                    maintenance_sequences.last().copied()
                );
                let requests = requests.lock().expect("observed requests");
                let request = requests.last().expect("post-maintenance request");
                assert_eq!(
                    request.context_summary.as_deref(),
                    Some("s".repeat(8 * 1024).as_str())
                );
                assert!(
                    request.history.is_empty(),
                    "summary replaces covered context, not canonical history"
                );
            }
            if config.auto_compaction_due() {
                let compacted = engine
                    .auto_compact_session(outcome.session_id, workspace.clone(), outcome.run.id)
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
        assert_eq!(maintenance_sequences.len(), 2);
        {
            let requests = requests.lock().expect("observed Run requests");
            assert_eq!(requests.len(), 16);
            for (index, request) in requests.iter().enumerate() {
                assert_eq!(request.phase, AgentPhase::RootPlan);
                assert_eq!(request.model, "test-model");
                assert_eq!(request.max_output_tokens, 4096);
                assert_eq!(
                    request.objective,
                    format!("{index:02}{}", "Q".repeat(8 * 1024 - 2))
                );
                assert_eq!(request.includes, ["Fact"]);
                assert!(request.instructions.is_none());
                assert!(request.child_results.is_empty());
            }
            let compactions = compactions.lock().expect("observed compactions");
            assert_eq!(compactions.len(), 2);
            for (cycle, request) in compactions.iter().enumerate() {
                assert_eq!(request.model, "test-model");
                assert_eq!(request.max_output_tokens, 1024);
                assert_eq!(request.previous_summary.is_some(), cycle > 0);
                assert_eq!(request.items.len(), 6);
                for (offset, item) in request.items.iter().enumerate() {
                    let CompactionItem::Completed(turn) = item else {
                        panic!("completed source turn");
                    };
                    assert_eq!(
                        turn.user,
                        format!("{:02}{}", cycle * 6 + offset, "Q".repeat(8 * 1024 - 2))
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
        assert_eq!(view.compactions.len(), 2);
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
            ProviderOutcome::Finish(Finish {
                summary: "source".into(),
                result: "Source answer".into(),
            }),
            ProviderOutcome::Finish(Finish {
                summary: "later".into(),
                result: "Later answer".into(),
            }),
        ])
        .with_compactions(vec![
            Ok(CompactionResponse {
                summary: "z".repeat(8 * 1024 + 1),
                response_id: None,
                input_tokens: Some(12),
                output_tokens: Some(1024),
                wire_provenance: None,
            }),
            Ok(CompactionResponse {
                summary: "safe summary".into(),
                response_id: Some("bad\nidentifier".into()),
                input_tokens: Some(12),
                output_tokens: Some(8),
                wire_provenance: None,
            }),
        ]);
        let requests = Arc::clone(&provider.requests);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let first = engine
            .run(request(&workspace, "Source question"))
            .await
            .expect("source Run");
        let failed = engine
            .compact_session(first.session_id, workspace.clone())
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
        engine.run(later_request).await.expect("later Run");
        let hostile_id = engine
            .compact_session(first.session_id, workspace.clone())
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
            assert!(observed[1].context_summary.is_none());
            assert_eq!(observed[1].history.len(), 1);
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
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Delegate(Delegate {
            children: vec!["not allowed".into()],
        })])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Question remains unresolved".into(),
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
            wire_provenance: None,
        })]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let failed = engine
            .run(request(&workspace, "Unanswered question"))
            .await
            .expect("durable failed Run");
        assert_eq!(failed.run.status, RunStatus::Failed);
        let record = engine
            .compact_session(failed.session_id, workspace)
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
        let session_id = SessionId::new();
        let store =
            Store::open(StateRoot::admit(&state_path).expect("private state")).expect("store");
        store
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Interrupted Session".into(),
                    workspace_identity: None,
                },
            )
            .await
            .expect("Session start");
        store
            .append(
                session_id,
                Event::MessageAccepted {
                    run_id: arany::RunId::new(),
                    text: "Unfinished question".into(),
                    images: Vec::new(),
                },
            )
            .await
            .expect("accepted objective");
        store.close().await.expect("store shutdown");

        let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
            summary: "finished".into(),
            result: "Finished answer".into(),
        })])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Unfinished then finished".into(),
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
            wire_provenance: None,
        })]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            provider,
        )
        .expect("engine");
        let mut resumed = request(&workspace, "Finished question");
        resumed.session_id = Some(session_id);
        resumed.title = None;
        engine.run(resumed).await.expect("completed later Run");
        engine
            .compact_session(session_id, workspace.clone())
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
            .fork_session(session_id, workspace, None)
            .await
            .expect("fork after interruption");
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
        for (index, (prefix, policy, admitted, tools, branch)) in [
            (9995, CollaborationPolicy::Single, false, false, false),
            (9993, CollaborationPolicy::Single, true, false, false),
            (9990, CollaborationPolicy::Team { max_active_children: 1 }, false, false, false),
            (9989, CollaborationPolicy::Team { max_active_children: 1 }, true, false, false),
            (9950, CollaborationPolicy::Single, false, true, false),
            (9993, CollaborationPolicy::Single, false, false, true),
            (9992, CollaborationPolicy::Single, true, false, true),
        ].into_iter().enumerate() {
            let path = temp.path().join(format!("state-{index}"));
            let finish = || ProviderOutcome::Finish(Finish { summary: "done".into(), result: "answer".into() });
            let mut engine = Engine::open(StateRoot::admit(&path).unwrap(), ScriptedProvider::new(vec![finish()])).unwrap();
            let id = engine.run(request(&workspace, "Initial")).await.unwrap().session_id;
            engine.close().await.unwrap();
            let mut connection = rusqlite::Connection::open(path.join("events.sqlite3")).unwrap();
            let transaction = connection.transaction().unwrap();
            for sequence in 9..=prefix {
                transaction.execute("INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',1,'{\"title\":\"x\"}',0)", rusqlite::params![sequence, id.to_string()]).unwrap();
            }
            transaction.commit().unwrap();
            drop(connection);
            let responses = if matches!(policy, CollaborationPolicy::Team { .. }) {
                vec![ProviderOutcome::Delegate(Delegate { children: vec!["Child".into()] }), finish(), finish()]
            } else { vec![finish()] };
            let provider = ScriptedProvider::new(responses);
            let requests = provider.requests.clone();
            let mut engine = Engine::open(StateRoot::admit(&path).unwrap(), provider).unwrap();
            let id = if branch { engine.fork_session(id, workspace.clone(), None).await.unwrap() } else { id };
            if tools {
                use std::os::unix::fs::PermissionsExt;
                let config = path.join("tools.json");
                std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
                std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
                engine.enable_tools(env!("CARGO_BIN_EXE_arany").into()).unwrap();
            }
            let mut next = request(&workspace, "Next");
            next.session_id = Some(id);
            next.title = None;
            next.policy = policy;
            let result = engine.run(next).await;
            if admitted {
                assert_eq!(result.unwrap().run.status, RunStatus::Finished, "case {index}");
            } else {
                assert!(matches!(result, Err(EngineError::Store(arany::StoreError::ReplayLimit))), "case {index}");
            }
            engine.close().await.unwrap();
            let phases: Vec<_> = requests.lock().unwrap().iter().map(|request| request.phase).collect();
            assert_eq!(phases, if !admitted { vec![] } else if matches!(policy, CollaborationPolicy::Single) { vec![AgentPhase::RootPlan] } else { vec![AgentPhase::RootPlan, AgentPhase::ChildWork, AgentPhase::RootSynthesis] }, "case {index}");
            let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
            assert_eq!(store.load_session(id).await.unwrap().len(), if branch { if admitted { 8 } else { 1 } } else if admitted { 10_000 } else { prefix as usize });
            let view = store.load_view(id).await.unwrap().unwrap();
            assert_eq!(view.runs.len(), if admitted { 2 } else { 1 });
            assert!(view.runs.iter().all(|run| run.status == RunStatus::Finished));
            store.close().await.unwrap();
        }
        let path = temp.path().join("compactions");
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish { summary: "source".into(), result: "answer".into() }), ProviderOutcome::Finish(Finish { summary: "later".into(), result: "later answer".into() })])
            .with_compactions((0..65).map(|_| Err(ProviderError::RemoteHttp(403))).collect());
        let calls = provider.compact_requests.clone();
        let mut engine = Engine::open(StateRoot::admit(&path).unwrap(), provider).unwrap();
        let id = engine.run(request(&workspace, "Source")).await.unwrap().session_id;
        for _ in 0..64 {
            assert!(matches!(engine.compact_session(id, workspace.clone()).await.unwrap().status, CompactionStatus::Failed { .. }));
        }
        assert!(matches!(engine.compact_session(id, workspace.clone()).await, Err(EngineError::CompactionLimit)));
        assert_eq!(calls.lock().unwrap().len(), 64, "quota rejection must precede inference");
        let mut later = request(&workspace, "Later");
        later.session_id = Some(id);
        later.title = None;
        engine.run(later).await.unwrap();
        let branch = engine.fork_session(id, workspace.clone(), None).await.unwrap();
        assert!(matches!(engine.compact_session(branch, workspace.clone()).await.unwrap().status, CompactionStatus::Failed { .. }), "inherited compactions do not consume the fork's direct quota");
        assert_eq!(calls.lock().unwrap().len(), 65);
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
        assert_eq!(store.load_view(id).await.unwrap().unwrap().compactions.len(), 64);
        assert_eq!(store.load_view(branch).await.unwrap().unwrap().compactions.len(), 65);
        store.close().await.unwrap();
    });
}

#[test]
#[cfg(unix)]
#[ignore = "file-backed 256 MiB database-capacity admission and recovery"]
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
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).unwrap(),
            ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                summary: "source".into(),
                result: "answer".into(),
            })]),
        )
        .unwrap();
        let id = engine.run(request(&workspace, "Source")).await.unwrap().session_id;
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
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
            summary: "done".into(),
            result: "r".repeat(32 * 1024),
        })]).with_compactions(vec![Ok(CompactionResponse {
            summary: "source and recovered answer".into(),
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
            wire_provenance: None,
        })]);
        let calls = provider.requests.clone();
        let compactions = provider.compact_requests.clone();
        let mut engine = Engine::open(StateRoot::admit(&state_path).unwrap(), provider).unwrap();
        let next = || {
            let mut value = request(&workspace, "Recovered");
            value.session_id = Some(id);
            value.title = None;
            value
        };
        for policy in [CollaborationPolicy::Single, CollaborationPolicy::Team { max_active_children: 8 }] {
            let mut attempt = next();
            attempt.policy = policy;
            assert!(matches!(engine.run(attempt).await, Err(EngineError::Store(arany::StoreError::StorageFull))));
            assert!(calls.lock().unwrap().is_empty(), "database capacity rejection must precede inference");
        }
        {
            use std::os::unix::fs::PermissionsExt;
            let config = state_path.join("tools.json");
            std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        engine.enable_tools(env!("CARGO_BIN_EXE_arany").into()).unwrap();
        assert!(matches!(engine.run(next()).await, Err(EngineError::Store(arany::StoreError::StorageFull))));
        assert!(matches!(engine.compact_session(id, workspace.clone()).await, Err(EngineError::Store(arany::StoreError::StorageFull))));
        assert!(calls.lock().unwrap().is_empty());
        assert!(compactions.lock().unwrap().is_empty());
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert_eq!(store.load_session(id).await.unwrap().len(), 8);
        let view = store.load_view(id).await.unwrap().unwrap();
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        assert!(view.compactions.is_empty());
        store.close().await.unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("DROP TABLE capacity_padding; VACUUM;").unwrap();
        drop(connection);
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
            summary: "done".into(),
            result: "recovered answer".into(),
        })]).with_compactions(vec![Ok(CompactionResponse {
            summary: "source and recovered answer".into(),
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
            wire_provenance: None,
        })]);
        let calls = provider.requests.clone();
        let compactions = provider.compact_requests.clone();
        let mut engine = Engine::open(StateRoot::admit(&state_path).unwrap(), provider).unwrap();
        let recovered = engine.run(next()).await.unwrap();
        assert_eq!(recovered.run.status, RunStatus::Finished);
        engine.compact_session(id, workspace.clone()).await.unwrap();
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
            assert_eq!(request.objective, "Recovered");
            assert!(request.images.is_empty());
            assert!(request.instructions.is_none());
            assert_eq!(request.includes, vec!["Fact".to_owned()]);
            assert_eq!(request.history, vec![arany::HistoryTurn { user: "Source".into(), assistant: "answer".into() }]);
            assert!(request.context_summary.is_none());
            assert!(request.child_results.is_empty());
            assert_eq!(request.max_output_tokens, 4096);
            assert!(request.tools.is_none());
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
        assert_eq!(store.load_session(id).await.unwrap().len(), 16);
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
fn bounded_finish_output_must_fit_canonical_json() {
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
            (10_000, false, true),
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
                    children: vec!["Child".into()],
                }));
            }
            responses.push(ProviderOutcome::Finish(Finish {
                summary: "done".into(),
                result: result.clone(),
            }));
            let provider = ScriptedProvider::new(responses);
            let requests = provider.requests.clone();
            let mut engine = Engine::open(StateRoot::admit(&path).unwrap(), provider).unwrap();
            let mut next = request(&workspace, "Question");
            if child {
                next.policy = CollaborationPolicy::Team {
                    max_active_children: 1,
                };
            }
            let outcome = engine
                .run(next)
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
                assert_eq!(calls.len(), if child { 2 } else { 1 });
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
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Delegate(Delegate {
            children: vec!["unauthorized child".into()],
        })]);
        let requests = provider.requests.clone();
        let state_path = temp.path().join("state");
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let outcome = engine
            .run(request(&workspace, "Question"))
            .await
            .expect("durable failed Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(outcome.run.agents.len(), 1);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                1,
                "invalid outcome must not trigger a second call or fake exhaustion"
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
        assert_eq!(outcome.run.agents[0].provider_calls.len(), 1);
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
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
            summary: "summary".into(),
            result: "unearned result".into(),
        })]);
        let requests = provider.requests.clone();
        let state_path = temp.path().join("state");
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let outcome = engine.run(run_request).await.expect("durable failed Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                1,
                "invalid outcome must not trigger a second call or fake exhaustion"
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
        assert_eq!(outcome.run.agents[0].provider_calls.len(), 1);
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
                children: vec!["Check fact".into()],
            }),
            ProviderOutcome::Finish(Finish {
                summary: "checked".into(),
                result: "Fact is true".into(),
            }),
            ProviderOutcome::Finish(Finish {
                summary: "synthesized".into(),
                result: "Final answer".into(),
            }),
        ]);
        let recorded_requests = Arc::clone(&provider.requests);
        let state_path = temp.path().join("state");
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let outcome = engine.run(run_request).await.expect("successful team Run");
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
                used_bytes: 12,
                budget_bytes: 376 * 1024 - (20 * 1024 + 64),
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
            assert_eq!(requests[1].objective, "Check fact");
            assert!(requests[1].history.is_empty());
            assert_eq!(requests[2].phase, AgentPhase::RootSynthesis);
            assert_eq!(requests[2].child_results.len(), 1);
            assert_eq!(requests[2].child_results[0].result, "Fact is true");
        }
        engine.close().await.expect("engine shutdown");
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(outcome.session_id)
            .await
            .expect("durable Events");
        assert_eq!(events.len(), 12);
        let interrupted = SessionView::replay(outcome.session_id, &events[..5])
            .expect("valid post-call prefix")
            .expect("Session");
        assert_eq!(interrupted.runs[0].status, RunStatus::Interrupted);
        assert_eq!(interrupted.runs[0].agents[0].provider_calls.len(), 1);
        assert_eq!(
            interrupted.runs[0].agents[0].provider_calls[0].disposition,
            ProviderCallDisposition::Delegated
        );
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
        if let Event::ProviderCallRecorded { record, .. } = &mut invalid_phase[4].event {
            record.phase = AgentPhase::RootSynthesis;
        } else {
            panic!("expected primary planning call");
        }
        assert!(SessionView::replay(outcome.session_id, &invalid_phase).is_err());
        let mut duplicate_call = events.clone();
        duplicate_call[5].event = events[4].event.clone();
        duplicate_call[5].agent_run_id = events[4].agent_run_id;
        assert!(SessionView::replay(outcome.session_id, &duplicate_call).is_err());
        let legacy_events = events
            .iter()
            .filter(|event| !matches!(event.event, Event::ProviderCallRecorded { .. }))
            .cloned()
            .collect::<Vec<_>>();
        let legacy = SessionView::replay(outcome.session_id, &legacy_events)
            .expect("legacy journal without call records")
            .expect("Session");
        assert!(
            legacy.runs[0]
                .agents
                .iter()
                .all(|agent| agent.provider_calls.is_empty())
        );
        assert!(matches!(
            events[7].event,
            arany::Event::AgentFinished { .. }
        ));
        assert!(matches!(
            events[9].event,
            arany::Event::AgentFinished { .. }
        ));
        store.close().await.expect("store shutdown");
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
            let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                summary: "seed".into(),
                result: "Seed answer".into(),
            })])
            .with_compactions(vec![Ok(CompactionResponse {
                summary: "Derived summary".into(),
                response_id: Some("compact-seed".into()),
                input_tokens: Some(20),
                output_tokens: Some(8),
                wire_provenance: None,
            })]);
            let requests = Arc::clone(&provider.requests);
            let compacts = Arc::clone(&provider.compact_requests);
            let mut engine = Engine::open(
                StateRoot::admit(&root.join("state")).expect("private seed State"),
                provider,
            )
            .expect("seed Engine");
            let outcome = engine
                .run(request(&root.join("workspace"), "Seed question"))
                .await
                .expect("seed single-agent Run");
            assert_eq!(outcome.run.status, RunStatus::Finished);
            assert_eq!(outcome.run.agents.len(), 1);
            engine
                .compact_session(outcome.session_id, root.join("workspace"))
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
        assert!(
            seeded.stdout.windows(b"test result: ok. 1 passed; 0 failed; 0 ignored;".len())
                .any(|part| part == b"test result: ok. 1 passed; 0 failed; 0 ignored;"),
            "exactly one completed seed test"
        );
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
            capacity: 3,
        };
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            provider,
        )
        .expect("engine");
        let observer = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("observer");
        let mut run_request = request(&workspace, "Question");
        run_request.session_id = Some(session_id);
        run_request.title = None;
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 3,
        };
        let mut progress = RunProgress::new();
        let run_progress = progress.clone();
        let mut run_task = tokio::spawn(async move {
            let outcome = engine
                .run_with_progress(run_request, RunCancellation::new(), run_progress)
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
            let mut update = progress.changed().await;
            assert_eq!(update.run.status, RunStatus::Active);
            assert_eq!(update.run.agents.len(), 4);
            assert!(
                update
                    .run
                    .agents
                    .iter()
                    .all(|agent| agent.status == AgentStatus::Active)
            );
            let committed = observer
                .load_session(session_id)
                .await
                .expect("committed progress");
            assert_eq!(
                committed.last().expect("last Event").sequence,
                update.sequence
            );
            for objective in ["C", "B", "A"] {
                gates
                    .remove(objective)
                    .expect("release gate")
                    .send(())
                    .expect("live child");
                wait_for_child_finish(&observer, session_id, objective).await;
                loop {
                    update = progress.changed().await;
                    let child = update
                        .run
                        .agents
                        .iter()
                        .find(|agent| agent.objective.as_deref() == Some(objective))
                        .expect("admitted child");
                    if child.status == AgentStatus::Finished {
                        break;
                    }
                }
            }
            update
        })
        .await;
        let observed_update = match controlled {
            Ok(update) => update,
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
        let final_update = if observed_update.run.status == RunStatus::Finished {
            observed_update
        } else {
            tokio::time::timeout(Duration::from_secs(5), progress.changed())
                .await
                .expect("final progress deadline")
        };
        assert_eq!(final_update.run, outcome.run);
        assert_eq!(
            final_update.sequence,
            outcome.run.finished_sequence.expect("terminal Event")
        );
        {
            let requests = requests.lock().expect("requests lock");
            assert_eq!(requests.len(), 5);
            assert_test_call(
                &requests[0], AgentPhase::RootPlan, "Question", &["Fact"], &[], Some("Derived summary"), &[],
            );
            let mut children = requests[1..4].iter().collect::<Vec<_>>();
            children.sort_by(|left, right| left.objective.cmp(&right.objective));
            for (call, objective) in children.into_iter().zip(["A", "B", "C"]) {
                assert_test_call(call, AgentPhase::ChildWork, objective, &["Fact"], &[], None, &[]);
                let agent = outcome.run.agents.iter().find(|agent| agent.objective.as_deref() == Some(objective)).expect("assigned child");
                assert_eq!(call.agent_run_id, agent.id);
            }
            assert_test_call(
                &requests[4], AgentPhase::RootSynthesis, "Question", &["Fact"], &[], Some("Derived summary"),
                &["A", "B", "C"].map(|objective| arany::ChildResult {
                    objective: objective.into(), summary: "done".into(), result: format!("result:{objective}"),
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
            ProviderOutcome::Finish(Finish { summary: "parent".into(), result: "Later parent answer".into() }),
            ProviderOutcome::Finish(Finish { summary: "branch".into(), result: "Branch answer".into() }),
        ])
        .with_profile("gated", "test-model")
        .with_compactions(vec![Err(ProviderError::RemoteHttp(403))]);
        let continued_requests = Arc::clone(&provider.requests);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let mut engine = Engine::open(
            StateRoot::open_existing(&state_path).expect("continued State"),
            provider,
        )
        .expect("continued Engine");
        let failed = engine.compact_session(session_id, workspace.clone()).await.expect("recorded compaction failure");
        assert_eq!(failed.covered_run_id, outcome.run.id);
        assert!(matches!(failed.status, CompactionStatus::Failed { reason: CompactionFailure::ProviderRejected }));
        let mut continued_request = request(&workspace, "Continue parent");
        continued_request.session_id = Some(session_id);
        continued_request.title = None;
        let continued = engine.run(continued_request).await.expect("parent Run after failed compaction");
        let mut branch_request = request(&workspace, "Branch question");
        branch_request.session_id = Some(fork_id);
        branch_request.title = None;
        let branch = engine.run(branch_request).await.expect("fork Run after parent extension");
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
        let mut engine = Engine::open(
            StateRoot::open_existing(&state_path).expect("cancellation State"),
            GatedProvider { started, requests: Arc::clone(&cancelled_requests), fail_child: None, capacity: 3 },
        )
        .expect("cancellation Engine");
        let cancellation = RunCancellation::new();
        let run_cancellation = cancellation.clone();
        let mut cancel_request = request(&workspace, "Cancel team");
        cancel_request.session_id = Some(session_id);
        cancel_request.title = None;
        cancel_request.policy = CollaborationPolicy::Team { max_active_children: 3 };
        let mut cancel_task = tokio::spawn(async move {
            let result = engine.run_with_cancel(cancel_request, run_cancellation).await;
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
            assert_eq!(primary.context_summary.as_deref(), Some("Derived summary"));
            assert_eq!(primary.history.iter().map(|turn| turn.user.as_str()).collect::<Vec<_>>(), ["Question", "Continue parent"]);
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
            for (mode, output) in [("text", arany::Output::Text), ("jsonl", arany::Output::Jsonl)] {
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
                assert_eq!(shown.stdout, arany::render_session(view, direct_events, output).as_bytes(), "{mode} inspection bytes");
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
                            Event::ContextCompacted { record } => {
                                assert_eq!(value["kind"], "ContextCompacted");
                                assert_eq!(value["payload"], serde_json::to_value(record).expect("compaction fact"));
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
            capacity: 3,
        };
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 3,
        };
        let mut run_task = tokio::spawn(async move {
            let outcome = engine.run(run_request).await;
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
            arany::render_run_feedback(&replay.runs[0])
                .unwrap()
                .contains("Provider denied account access.")
        );
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
            capacity: 1,
        };
        let mut engine = Engine::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            provider,
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 3,
        };
        let mut run_task = tokio::spawn(async move {
            let outcome = engine.run(run_request).await;
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
        };
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let cancellation = RunCancellation::new();
        let run_cancellation = cancellation.clone();
        let mut progress = RunProgress::new();
        let run_progress = progress.clone();
        let mut run_task = tokio::spawn(async move {
            let outcome = engine
                .run_with_progress(
                    request(&workspace, "Question"),
                    run_cancellation,
                    run_progress,
                )
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
        let active = progress.changed().await;
        assert_eq!(active.run.status, RunStatus::Active);
        assert_eq!(active.run.agents.len(), 1);
        assert!(active.run.assistant_message.is_none());
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
        let terminal = progress.changed().await;
        assert_eq!(terminal.run, outcome.run);

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
                for (case, response, call_disposition, failure_reason) in [
                    (
                        "rejected call",
                        Err(ProviderError::RemoteHttp(403)),
                        ProviderCallDisposition::Rejected,
                        Some(arany::ProviderFailureReason::AccountAccess),
                    ),
                    (
                        "usage limit",
                        Err(ProviderError::RemoteHttp(429)),
                        ProviderCallDisposition::Rejected,
                        Some(arany::ProviderFailureReason::UsageLimit),
                    ),
                    (
                        "service unavailable",
                        Err(ProviderError::RemoteHttp(503)),
                        ProviderCallDisposition::Unavailable,
                        Some(arany::ProviderFailureReason::ServiceUnavailable),
                    ),
                    (
                        "usage temporarily unavailable",
                        Err(ProviderError::RemoteStreamCode(
                            "subscription_sharing_usage_unavailable".into(),
                        )),
                        ProviderCallDisposition::Unavailable,
                        Some(arany::ProviderFailureReason::UsageTemporarilyUnavailable),
                    ),
                    (
                        "unknown stream code",
                        Err(ProviderError::RemoteStreamCode(
                            "OMITTED_ERROR_CANARY".into(),
                        )),
                        ProviderCallDisposition::Rejected,
                        None,
                    ),
                    ("malformed stream", Err(ProviderError::InvalidStream), ProviderCallDisposition::InvalidResponse, Some(arany::ProviderFailureReason::StreamProtocol)),
                    ("response contract", Err(ProviderError::InvalidResponseContract), ProviderCallDisposition::InvalidResponse, Some(arany::ProviderFailureReason::ResponseContract)),
                    ("outcome contract", Err(ProviderError::InvalidOutcomeContract), ProviderCallDisposition::InvalidResponse, Some(arany::ProviderFailureReason::OutcomeContract)),
                    ("local output limit", Err(ProviderError::LocalOutputLimit), ProviderCallDisposition::InvalidResponse, Some(arany::ProviderFailureReason::LocalOutputLimit)),
                    (
                        "invalid finish",
                        Ok(ProviderOutcome::Finish(Finish {
                            summary: String::new(),
                            result: "Unaccepted result".into(),
                        })),
                        ProviderCallDisposition::InvalidResponse,
                        None,
                    ),
                    (
                        "accepted finish",
                        Ok(ProviderOutcome::Finish(Finish {
                            summary: "done".into(),
                            result: "Accepted result".into(),
                        })),
                        ProviderCallDisposition::Finished,
                        None,
                    ),
                ] {
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
                            children: vec!["A".into()],
                        })));
                        responses.push_back(Ok(ProviderOutcome::Finish(Finish {
                            summary: "child done".into(),
                            result: "Child result".into(),
                        })));
                        vec![
                            AgentPhase::RootPlan,
                            AgentPhase::ChildWork,
                            AgentPhase::RootSynthesis,
                        ]
                    } else {
                        vec![AgentPhase::RootPlan]
                    };
                    responses.push_back(response);
                    provider.responses = Mutex::new(responses);
                    let calls = Arc::clone(&provider.requests);
                    let mut engine = Engine::open(
                        StateRoot::admit(&state_path).expect("private state"),
                        provider,
                    )
                    .expect("Engine");
                    let mut progress = RunProgress::new();
                    let outcome = tokio::time::timeout(
                        Duration::from_secs(5),
                        engine.run_with_progress(run_request, cancellation, progress.clone()),
                    )
                    .await
                    .expect("completion deadline")
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
                    if expected_status == RunStatus::Failed {
                        let feedback = arany::render_run_feedback(&outcome.run).expect("failure guidance");
                        for (reason, text) in [
                            (arany::ProviderFailureReason::StreamProtocol, "Provider stream was incomplete or malformed."),
                            (arany::ProviderFailureReason::ResponseContract, "Provider response did not match the selected model, completed-message, or usage contract."),
                            (arany::ProviderFailureReason::OutcomeContract, "Provider output did not match Arany's structured outcome contract."),
                            (arany::ProviderFailureReason::LocalOutputLimit, "Provider reported output above Arany's local token limit."),
                        ] {
                            if failure_reason == Some(reason) {
                                assert!(feedback.contains(text), "{case}: {feedback}");
                                assert!(feedback.contains("No answer was accepted"));
                            }
                        }
                    }
                    assert_eq!(record.response_id, None);
                    assert_eq!(record.input_tokens, None);
                    assert_eq!(
                        record.output_tokens,
                        matches!(case, "invalid finish" | "accepted finish").then_some(8)
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
                    let terminal = tokio::time::timeout(Duration::from_secs(5), progress.changed())
                        .await
                        .expect("terminal progress deadline");
                    assert_eq!(terminal.run, outcome.run);
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
                        events.last().expect("terminal Event").sequence
                    );
                    assert_eq!(replay.runs.len(), 1);
                    assert_eq!(replay.runs[0], outcome.run);
                    let jsonl = arany::render_session(&replay, &events, arany::Output::Jsonl);
                    assert!(!jsonl.contains("OMITTED_ERROR_CANARY"));
                    for (line, envelope) in jsonl.lines().zip(&events) {
                        if let Event::ProviderCallRecorded { record, .. } = &envelope.event {
                            let document: serde_json::Value =
                                serde_json::from_str(line).expect("canonical JSONL");
                            assert_eq!(
                                document["event_version"],
                                if record.failure_reason.is_some() {
                                    2
                                } else {
                                    1
                                }
                            );
                            assert_eq!(document["payload"], serde_json::to_value(record).unwrap());
                        }
                    }
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
            let mut initial_engine = Engine::open(
                StateRoot::admit(&state_path).expect("private state"),
                ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                    summary: "initial".into(),
                    result: "initial answer".into(),
                })]),
            )
            .expect("initial engine");
            let initial = initial_engine
                .run(request(&workspace, "Initial"))
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
            let mut held_engine = Engine::open(
                StateRoot::admit(&state_path).expect("state readmission"),
                WaitingProvider {
                    started,
                    dropped: Arc::clone(&dropped),
                    requests: Arc::clone(&held_calls),
                },
            )
            .expect("held engine");
            let cancellation = RunCancellation::new();
            let held_cancellation = cancellation.clone();
            let mut held_request = request(&workspace, "Held");
            held_request.session_id = Some(held_session_id);
            held_request.title = None;
            let mut held_task = tokio::spawn(async move {
                let outcome = held_engine
                    .run_with_cancel(held_request, held_cancellation)
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

            let contender_provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                summary: "after lock".into(),
                result: "later answer".into(),
            })]);
            let contender_calls = Arc::clone(&contender_provider.requests);
            let mut contender = Engine::open(
                StateRoot::admit(&state_path).expect("contender state"),
                contender_provider,
            )
            .expect("contender engine");
            let mut blocked_request = request(&workspace, "Blocked");
            blocked_request.session_id = Some(held_session_id);
            blocked_request.title = None;
            blocked_request.include_paths = vec!["missing-include.txt".into()];
            let blocked =
                tokio::time::timeout(Duration::from_secs(5), contender.run(blocked_request))
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
                    .compact_session(held_session_id, workspace.clone())
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
                    contender.run(parent_request).await,
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

            let mut independent = Engine::open(
                StateRoot::admit(&state_path).expect("independent state"),
                ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                    summary: "independent".into(),
                    result: "independent answer".into(),
                })]),
            )
            .expect("independent engine");
            let independent_outcome = independent
                .run(request(&workspace, "Independent"))
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
            let resumed = contender.run(retry).await.expect("Run after release");
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
        };
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let cancellation = RunCancellation::new();
        assert!(cancellation.cancel());
        assert!(matches!(
            engine
                .run_with_cancel(request(&workspace, "Question"), cancellation)
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
                capacity,
            };
            let mut engine = Engine::open(
                StateRoot::admit(&state_path).expect("private state"),
                provider,
            )
            .expect("engine");
            let cancellation = RunCancellation::new();
            let run_cancellation = cancellation.clone();
            let mut run_request = request(&workspace, "Question");
            run_request.policy = CollaborationPolicy::Team {
                max_active_children: 3,
            };
            let mut run_task = tokio::spawn(async move {
                let outcome = engine.run_with_cancel(run_request, run_cancellation).await;
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
        };
        let mut engine = Engine::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            provider,
        )
        .expect("engine");
        let mut run_task = tokio::spawn(async move {
            let outcome = engine.run(request(&workspace, "Question")).await;
            let close = engine.close().await;
            (outcome, close)
        });
        let started = tokio::time::timeout(Duration::from_secs(5), started_rx.recv()).await;
        assert_eq!(started.expect("Provider start deadline"), Some(()));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(121)).await;
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
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        let session_id = engine
            .run(request(&workspace, "Seed question"))
            .await
            .expect("seed Run")
            .session_id;
        let mut compact_task = tokio::spawn(async move {
            let outcome = engine.compact_session(session_id, workspace).await;
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
            children: objectives.clone(),
        })];
        responses.extend((0..8).map(|_| {
            ProviderOutcome::Finish(Finish {
                summary: "done".into(),
                result: "result".into(),
            })
        }));
        responses.push(ProviderOutcome::Finish(Finish {
            summary: "synthesized".into(),
            result: "final".into(),
        }));
        let provider = ScriptedProvider::new(responses);
        let requests = Arc::clone(&provider.requests);
        let mut engine = Engine::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            provider,
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 8,
        };
        let outcome = engine.run(run_request).await.expect("eight-child Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(outcome.run.agents.len(), 9);
        {
            let requests = requests.lock().expect("requests lock");
            assert_eq!(requests.len(), 10);
            let synthesis = requests.last().expect("synthesis call");
            assert_eq!(synthesis.phase, AgentPhase::RootSynthesis);
            assert_eq!(
                synthesis
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
            let children = (0..count).map(|child| format!("Child {child}")).collect();
            let provider =
                ScriptedProvider::new(vec![ProviderOutcome::Delegate(Delegate { children })]);
            let requests = provider.requests.clone();
            let mut engine = Engine::open(
                StateRoot::admit(&temp.path().join(format!("state-{index}")))
                    .expect("private state"),
                provider,
            )
            .expect("engine");
            let mut run_request = request(&workspace, "Question");
            run_request.policy = policy;
            let outcome = engine.run(run_request).await.expect("durable failed Run");
            assert_eq!(outcome.run.status, RunStatus::Failed, "case {index}");
            assert_eq!(outcome.run.agents.len(), 1, "case {index}");
            assert!(outcome.run.assistant_message.is_none(), "case {index}");
            {
                let requests = requests.lock().unwrap();
                assert_eq!(
                    requests.len(),
                    1,
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
            assert_eq!(outcome.run.agents[0].provider_calls.len(), 1);
            assert_eq!(
                outcome.run.agents[0].provider_calls[0].disposition,
                ProviderCallDisposition::InvalidResponse
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
            let mut engine = Engine::open(
                StateRoot::admit(&temp.path().join(format!("invalid-{index}")))
                    .expect("private state"),
                provider,
            )
            .expect("engine");
            let mut run_request = request(&workspace, "Question");
            run_request.policy = policy;
            assert!(
                matches!(
                    engine.run(run_request).await,
                    Err(EngineError::InvalidRequest)
                ),
                "invalid policy case {index}"
            );
            assert!(requests.lock().expect("requests lock").is_empty());
            engine.close().await.expect("engine shutdown");
        }
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
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
            summary: "Unexpected call".into(),
            result: "Unexpected result".into(),
        })]);
        let requests = Arc::clone(&provider.requests);
        let state_path = temp.path().join("state");
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private state"),
            provider,
        )
        .expect("engine");
        for (objective, images) in [
            (String::new(), Vec::new()),
            (String::new(), vec![image_fixture(69); 5]),
            (String::new(), vec![image_fixture(64 * 1024); 4]),
            ("x".repeat(8 * 1024 + 1), Vec::new()),
            (
                "x".repeat(8 * 1024 + 1),
                vec![image_fixture(arany::MAX_IMAGE_BYTES)],
            ),
        ] {
            let mut invalid = request(&workspace, &objective);
            invalid.images = images;
            assert!(
                matches!(engine.run(invalid).await, Err(EngineError::InvalidRequest)),
                "invalid objective/images reject before hostile Workspace"
            );
        }
        assert!(matches!(
            engine.run(request(&workspace, "Question")).await,
            Err(EngineError::WorkspaceUnavailable)
        ));
        std::fs::remove_file(workspace.join("AGENTS.md")).expect("remove test symlink");
        let mut traversal = request(&workspace, "Question");
        traversal.include_paths = vec!["../outside".into()];
        assert!(matches!(
            engine.run(traversal).await,
            Err(EngineError::InvalidWorkspacePath)
        ));
        assert!(
            requests.lock().expect("requests lock").is_empty(),
            "rejected admission makes no Provider call"
        );
        engine.close().await.expect("engine shutdown");

        let mut forged_custom = Engine::open(
            StateRoot::open_existing(&state_path).expect("existing state"),
            ScriptedProvider::new(vec![]).with_profile("custom:local", "test-model"),
        )
        .expect("unproven Provider opened");
        let mut invalid_workspace = request(&workspace, "Question");
        invalid_workspace.include_paths = vec!["../outside".into()];
        assert!(matches!(
            forged_custom.run(invalid_workspace).await,
            Err(EngineError::InvalidRequest)
        ));
        forged_custom.close().await.expect("engine shutdown");

        let mut forged_chatgpt = Engine::open(
            StateRoot::open_existing(&state_path).expect("existing state"),
            ScriptedProvider::new(vec![]).with_profile("chatgpt", "test-model"),
        )
        .expect("unproven Provider opened");
        let mut invalid_workspace = request(&workspace, "Question");
        invalid_workspace.include_paths = vec!["../outside".into()];
        assert!(matches!(
            forged_chatgpt.run(invalid_workspace).await,
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
        let mut engine = Engine::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            provider,
        )
        .expect("engine");

        std::fs::write(workspace.join("AGENTS.md"), vec![b'a'; 64 * 1024 + 1])
            .expect("oversized instruction");
        assert!(matches!(
            engine.run(request(&workspace, "Question")).await,
            Err(EngineError::InputTooLarge)
        ));
        std::fs::remove_file(workspace.join("AGENTS.md")).expect("clear test instruction");

        std::fs::write(
            workspace.join("notes/facts.txt"),
            vec![b'f'; 128 * 1024 + 1],
        )
        .expect("oversized include");
        assert!(matches!(
            engine.run(request(&workspace, "Question")).await,
            Err(EngineError::InputTooLarge)
        ));
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("restore include");

        let mut too_many = request(&workspace, "Question");
        too_many.include_paths = vec!["notes/facts.txt".into(); 17];
        assert!(matches!(
            engine.run(too_many).await,
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
            engine.run(too_much_context).await,
            Err(EngineError::ContextTooLarge)
        ));
        assert!(recorded_requests.lock().expect("requests lock").is_empty());
        engine.close().await.expect("engine shutdown");

        let provider = ScriptedProvider::new(vec![]);
        let state_inside_workspace = workspace.join("state");
        let mut engine = Engine::open(
            StateRoot::admit(&state_inside_workspace).expect("private state"),
            provider,
        )
        .expect("engine");
        assert!(matches!(
            engine.run(request(&workspace, "Question")).await,
            Err(EngineError::StateOverlap)
        ));
        engine.close().await.expect("engine shutdown");
    });
}
