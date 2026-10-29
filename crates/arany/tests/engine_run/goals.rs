use super::*;
use arany::test_support::goals::*;

#[tokio::test]
async fn objective_plus_includes_overflow_has_a_typed_pre_admission_error() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    let component = "\"".repeat(170);
    let relative = format!("{component}/{component}/{component}");
    let path = workspace.join(&relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "").unwrap();
    let state_path = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state_path).unwrap(),
        EngineConfig::default(),
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut input = request(&workspace, &"\0".repeat(64 * 1024));
    input.include_paths = vec![relative.into(); 16];
    let error = run_to_end(&engine, &provider, input).await.unwrap_err();
    assert!(matches!(error, EngineError::GoalInputTooLarge), "{error:?}");
    assert_eq!(error.to_string(), "objective plus includes too large");
    assert!(provider.requests.lock().unwrap().is_empty());
    engine.close().await.unwrap();
    assert!(
        arany::list_sessions(StateRoot::open_existing(&state_path).unwrap(), workspace)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn goal_lifecycle_bounds_report_and_requires_goal_replay_linkage() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    // Control characters escape to six JSON bytes each, so the serialized report budget shortens this summary.
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "\u{1}".repeat(2000),
    })]);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "Complete a Goal");
    input.include_paths.clear();
    let outcome = run_to_end(&engine, &provider, input)
        .await
        .expect("Goal Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(outcome.session_id)
        .await
        .expect("Events");
    let view = SessionView::replay(outcome.session_id, &events)
        .expect("strict replay")
        .expect("Session");
    let [goal] = view.goals.as_slice() else {
        panic!("one Goal is linked to the accepted task");
    };
    assert_eq!(goal.id, outcome.goal_id);
    assert_eq!(goal.run_ids, [outcome.run.id]);
    assert_eq!(goal.status, arany::GoalStatus::Completed);
    let report = goal.report.as_ref().expect("durable report");
    assert!(report.summary.len() <= 2048);
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("summary was shortened"))
    );
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("token usage is incomplete"))
    );

    let without_goal: Vec<_> = events
        .iter()
        .filter(|event| {
            !matches!(
                &event.event,
                Event::GoalStarted { .. }
                    | Event::GoalRunLinked { .. }
                    | Event::GoalProviderCallReserved { .. }
                    | Event::GoalFinished { .. }
            )
        })
        .cloned()
        .collect();
    assert!(
        SessionView::replay(outcome.session_id, &without_goal).is_err(),
        "current-format replay rejects accepted tasks without Goal linkage"
    );
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn answer_summary_uses_the_first_nonempty_line_and_a_character_boundary() {
    for (summary, expected) in [
        ("ࠀ".repeat(620), "ࠀ".repeat(620)),
        ("🚀".repeat(600), "🚀".repeat(512)),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: format!("\n \n{summary}\naccepted result"),
        })]);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut input = request(&workspace, "Complete a Goal");
        input.include_paths.clear();
        let outcome = run_to_end(&engine, &provider, input)
            .await
            .expect("Goal Run");
        engine.close().await.expect("engine shutdown");

        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only Store");
        let view = store
            .load_view(outcome.session_id)
            .await
            .expect("replayed Session")
            .expect("Session");
        let report = view.goals[0].report.as_ref().expect("durable report");
        assert_eq!(report.summary, expected);
        assert!(
            !report
                .unresolved
                .iter()
                .any(|item| item.contains("summary was shortened"))
        );
        store.close().await.expect("Store shutdown");
    }
}

pub(super) const READS: usize = 80;
// Small model windows shrink the Tool allowance so pressure arrives within a few dozen calls.
// Only a native Anthropic or ChatGPT identity reports one.
pub(super) const MIN_WINDOW_TOKENS: u32 = arany::test_support::MIN_CONTEXT_WINDOW_TOKENS;
// The smallest window where a summary's reservation still fits.
pub(super) const SMALL_WINDOW_TOKENS: u32 = MIN_WINDOW_TOKENS * 3 / 2;
// Pinned prose shrinks the allowance so clearing frees less than its floor; unpinned text still clears.
pub(super) const PROSE_COMPACTION_WINDOW_TOKENS: u32 = MIN_WINDOW_TOKENS * 9 / 4;
// Clearing a page prefix passes its floor while the retained steps plus the next-turn reserve still
// overflow, so a summary follows.
pub(super) const GOAL_COMPACTION_WINDOW_TOKENS: u32 = MIN_WINDOW_TOKENS * 3;

// Two lines at the Read line cut make each page most of its step, so pressure arrives in few calls.
pub(super) fn paged_read_workspace(root: &Path) -> std::path::PathBuf {
    const LINE_BYTES: usize = 2000;
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(
        workspace.join("notes/data.txt"),
        format!("{}\n", "x".repeat(LINE_BYTES - 1)).repeat(2 * READS),
    )
    .expect("bounded fixture file");
    workspace
}

pub(super) fn paged_read_outcomes() -> Vec<ProviderOutcome> {
    let mut outcomes = (0..READS)
        .map(|page| {
            crate::tool_outcome(arany::ToolCall::Read {
                path: "notes/data.txt".into(),
                offset: Some(u32::try_from(page * 2 + 1).expect("bounded line offset")),
                limit: Some(2),
            })
        })
        .collect::<Vec<_>>();
    outcomes.push(ProviderOutcome::Answer(Answer {
        text: "The bounded pages were inspected".into(),
    }));
    outcomes
}

pub(super) const AUTONOMOUS_OBJECTIVE: &str = "Verify the facts file";

pub(super) fn call_facts(request: &arany::ProviderRequest) -> Option<arany::GoalContextFacts> {
    request
        .input
        .tools
        .as_ref()
        .and_then(|tools| tools.steps.last())
        .and_then(|step| step.facts)
        .or(request.input.goal_context_facts)
}

pub(super) fn call_steering_notes(request: &arany::ProviderRequest) -> Vec<String> {
    request
        .input
        .steering_notes
        .iter()
        .cloned()
        .chain(
            request
                .input
                .tools
                .iter()
                .flat_map(|tools| &tools.steps)
                .flat_map(|step| step.steering_notes.iter().cloned()),
        )
        .collect()
}

pub(super) fn facts_digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn assert_successful_in_process_reads(run: &arany::RunView) {
    assert_eq!(run.tools.len(), 2);
    for tool in &run.tools {
        let observed = tool.observation.as_ref().expect("Read receipt");
        assert_eq!(observed.disposition, arany::ToolDisposition::Succeeded);
        assert!(matches!(
            observed.attestation,
            Some(arany::Attestation::InProcess { .. })
        ));
    }
}

pub(super) fn facts_read() -> arany::ToolCall {
    arany::ToolCall::Read {
        path: "notes/facts.txt".into(),
        offset: None,
        limit: None,
    }
}

pub(super) fn facts_plan(sha256: String) -> arany::GoalPlanContract {
    arany::GoalPlanContract {
        mission: AUTONOMOUS_OBJECTIVE.into(),
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
        paths: vec!["notes/facts.txt".into()],
        commands: Vec::new(),
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: arany::GoalSuccessCriterion::WorkspaceFileDigest {
            path: "notes/facts.txt".into(),
            sha256,
        },
        budget: arany::GoalBudget::default(),
    }
}

pub(super) fn write_facts(
    workspace: &Path,
    text: &'static str,
) -> impl Fn() + Send + Sync + 'static {
    let path = workspace.join("notes/facts.txt");
    move || std::fs::write(&path, text).expect("updated facts file")
}

pub(super) fn facts_engine(state: StateRoot, state_path: &Path) -> Engine {
    facts_engine_with_telemetry(state, state_path, arany::Telemetry::disabled())
}

fn facts_engine_with_telemetry(
    state: StateRoot,
    state_path: &Path,
    telemetry: arany::Telemetry,
) -> Engine {
    use std::os::unix::fs::PermissionsExt;
    let engine = engine_support::open(
        state,
        EngineConfig {
            worker_concurrency: None,
            telemetry,
            guard_executable: Some(std::env::current_exe().expect("test executable")),
        },
    )
    .expect("engine");
    let config = state_path.join("tools.json");
    std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("tool configuration");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
        .expect("tool configuration mode");
    engine
}

pub(super) fn autonomous_input(workspace: &Path) -> RunRequest {
    let mut input = request(workspace, AUTONOMOUS_OBJECTIVE);
    input.include_paths.clear();
    input.goal = arany::GoalRequest::autonomous(
        arany::GoalBudget::default(),
        arany::GoalPhase::Implementation,
    );
    input
}

async fn run_autonomous_with_setup_refusal(
    engine: &Engine,
    provider: &Arc<ScriptedProvider>,
    workspace: &Path,
    state: &Path,
) -> arany::RunOutcome {
    let session = engine
        .create_session(workspace.to_owned(), None)
        .await
        .unwrap();
    let (entered, release) = arany::FaultPlan::gate_autonomous_continuation(session);
    let mut input = autonomous_input(workspace);
    input.session_id = Some(session);
    input.title = None;
    let run = run_with(
        engine,
        provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    );
    tokio::pin!(run);
    const CONTINUATION_DEADLINE: Duration = Duration::from_secs(10);
    tokio::time::timeout(CONTINUATION_DEADLINE, async {
        tokio::select! {
            result = &mut run => panic!("autonomous Goal missed its continuation gate: {result:?}"),
            reached = entered => reached.expect("first autonomous Run settled"),
        }
        assert_setup_refused_between_autonomous_runs(state, workspace, session).await;
        assert_eq!(provider.requests.lock().unwrap().len(), 3);
        release.send(()).expect("continue autonomous Goal");
        run.await.expect("autonomous Goal")
    })
    .await
    .expect("autonomous continuation deadline")
}

async fn assert_setup_refused_between_autonomous_runs(
    state: &Path,
    workspace: &Path,
    session: arany::SessionId,
) {
    let store = Store::open(StateRoot::open_existing(state).unwrap()).unwrap();
    let before = store.load_session(session).await.unwrap();
    let view = store.load_open_view(session).await.unwrap().unwrap();
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.goals[0].status, arany::GoalStatus::Running);
    assert!(matches!(
        arany::start_provider_setup_edit(
            StateRoot::open_existing(state).unwrap(),
            workspace.to_owned(),
            session,
            arany::ProviderSetupTarget {
                kind: arany::ProviderSetupTargetKind::CustomProfile,
                identity: "example-route".into(),
            },
            None,
        )
        .await,
        Err(EngineError::SessionBusy)
    ));
    assert_eq!(store.load_session(session).await.unwrap(), before);
    store.close().await.unwrap();
}

#[tokio::test]
async fn autonomous_goal_iterates_until_its_success_criterion_is_satisfied() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        crate::tool_outcome(facts_read()),
        finish_with("verified"),
    ])
    .with_before_call(3, write_facts(&workspace, "v2"));
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let outcome =
        run_autonomous_with_setup_refusal(&engine, &provider, &workspace, &state_path).await;
    assert_eq!(outcome.run.status, RunStatus::Finished);
    {
        let calls = requests.lock().expect("Provider requests");
        assert_eq!(
            calls.len(),
            5,
            "one plan call and two Runs with a Read then Answer"
        );
        assert_eq!(
            calls[3].input.objective, AUTONOMOUS_OBJECTIVE,
            "the next Run takes the pinned objective and no new text"
        );
    }
    resume_session(
        StateRoot::open_existing(&state_path).unwrap(),
        workspace.clone(),
        outcome.session_id,
    )
    .await
    .expect("completed autonomous submission releases its Session lease");
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Completed);
    assert_eq!(goal.run_ids.len(), 2);
    assert!(
        !events
            .iter()
            .any(|event| matches!(&event.event, Event::GoalResumed { .. })),
        "autonomous continuations are not explicit resumes"
    );
}

#[tokio::test]
async fn autonomous_goal_pins_permissions_settings_at_submission() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let settings = state_path.join("permissions.json");
    let write_settings = |bytes: &'static [u8]| {
        let path = settings.clone();
        move || {
            std::fs::write(&path, bytes).expect("permissions settings");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("permissions settings mode");
        }
    };
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    write_settings(br#"{"permissions":{"allow":["Read"]}}"#)();
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        crate::tool_outcome(facts_read()),
        finish_with("verified"),
    ])
    .with_before_call(2, {
        let facts = write_facts(&workspace, "v2");
        let settings = write_settings(br#"{ "permissions": { "allow": ["Read"] } }"#);
        move || {
            facts();
            settings();
        }
    });
    let provider = Arc::new(provider);
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("autonomous Goal");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.run_ids.len(), 2);
    let digests: Vec<_> = view
        .runs
        .iter()
        .filter(|run| goal.run_ids.contains(&run.id))
        .map(|run| {
            run.config
                .as_ref()
                .and_then(|config| config.tool_policy.as_ref())
                .and_then(|policy| policy.permissions_digest)
        })
        .collect();
    assert_eq!(digests.len(), 2);
    assert!(digests[0].is_some());
    assert_eq!(
        digests[0], digests[1],
        "the Goal loop pins one settings snapshot"
    );
}

fn collect_trace_bodies(
    listener: std::net::TcpListener,
    done: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<Vec<Vec<u8>>> {
    std::thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("nonblocking Collector");
        let mut bodies = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => bodies.push(read_trace_body(&mut stream)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if done.load(std::sync::atomic::Ordering::Acquire) {
                        return bodies;
                    }
                    std::thread::yield_now();
                }
                Err(error) => panic!("Collector accept failed: {error}"),
            }
        }
    })
}

fn read_trace_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::{Read, Write};
    stream
        .set_nonblocking(false)
        .expect("blocking Collector stream");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("Collector deadline");
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("trace request bytes");
        assert!(count > 0, "trace request ended before its body");
        request.extend_from_slice(&chunk[..count]);
        let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let header_end = end + 4;
        let header = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let body_len = header
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length: ")?
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .expect("trace Content-Length");
        if request.len() >= header_end + body_len {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("Collector acknowledgment");
            return request[header_end..header_end + body_len].to_vec();
        }
    }
}

fn goal_segment_attribute(bodies: &[Vec<u8>], key: &str) -> Vec<String> {
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use prost::Message;
    bodies
        .iter()
        .flat_map(|body| {
            ExportTraceServiceRequest::decode(body.as_slice())
                .expect("OTLP protobuf")
                .resource_spans
        })
        .flat_map(|resource| resource.scope_spans)
        .flat_map(|scope| scope.spans)
        .filter(|span| span.name == "invoke_workflow arany.goal")
        .filter_map(|span| {
            span.attributes
                .into_iter()
                .find(|attribute| attribute.key == key)
                .and_then(|attribute| attribute.value)
                .and_then(|value| match value.value {
                    Some(Value::StringValue(status)) => Some(status),
                    _ => None,
                })
        })
        .collect()
}

#[tokio::test]
async fn exhausted_autonomous_segment_exports_its_failed_status() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_active_ms: Some(60_000),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget;
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("never requested"),
    ])
    // Advanced in the Answer call, after the Read Tool's Guard I/O (a paused clock would auto-advance that I/O's timeouts).
    .with_clock_advance_before_call(2, Duration::from_secs(60));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("Collector listener");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("Collector address")
    );
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let collector = collect_trace_bodies(listener, Arc::clone(&done));
    let telemetry = arany::Telemetry::from_process(Some(&endpoint)).expect("loopback telemetry");
    let provider = Arc::new(provider);
    let engine = facts_engine_with_telemetry(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        telemetry.clone(),
    );
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    let result = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            cancellation: RunCancellation::new(),
            ..Default::default()
        },
    )
    .await;
    assert!(
        matches!(&result, Ok(outcome) if outcome.run.status == RunStatus::Failed),
        "the exhausted Answer fails its Run: {:?}",
        result.as_ref().map(|outcome| outcome.run.status)
    );
    engine.close().await.expect("engine shutdown");
    telemetry.shutdown();
    done.store(true, std::sync::atomic::Ordering::Release);
    let bodies = collector.join().expect("Collector thread");
    assert_eq!(
        goal_segment_attribute(&bodies, "arany.goal.status"),
        vec!["failed".to_owned()],
        "the segment that decided exhaustion carries the Failed terminal status"
    );
}

#[tokio::test]
async fn run_hints_carry_only_display_text_for_the_committed_run() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = Arc::new(crate::PreviewProvider {
        text: arany::PreviewText {
            answer: "x".repeat(2300),
            commentary: Vec::new(),
            commentary_limited: false,
            tentative: false,
        },
        completion: None,
    });
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut hints = RunHints::with_previews();
    let mut previews = hints.previews();
    let cancellation = RunCancellation::new();
    let mut input = request(&workspace, "question");
    input.include_paths.clear();
    let (mut events, update, outcome) = {
        let arany::RunHandle { events, result, .. } = engine
            .submit(
                AdmittedProviders::single(provider),
                input,
                RunControls {
                    cancellation: cancellation.clone(),
                    hints: Some(hints.clone()),
                    ..Default::default()
                },
            )
            .expect("submission");
        let mut result = std::pin::pin!(tokio::time::timeout(Duration::from_secs(10), result));
        let update = tokio::select! {
            update = previews.changed() => update.expect("preview update"),
            _ = &mut result => panic!("the Run ended before its preview"),
        };
        assert!(cancellation.cancel());
        let outcome = result
            .await
            .expect("cancellation deadline")
            .expect("cancelled Run")
            .run;
        (events, update, outcome)
    };
    engine.close().await.expect("engine shutdown");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    assert_eq!(update.run_id, outcome.run.id);
    assert!(!update.text.answer.is_empty());
    assert!(outcome.run.assistant_message.is_none());
    tokio::select! {
        biased;
        _ = hints.waiting_changed() => panic!("no Tool waited for capacity"),
        () = std::future::ready(()) => {}
    }
    let mut tapped = Vec::new();
    while let Some(event) = events.next().await {
        let arany::Tapped::Committed(event) = event else {
            panic!("bounded Run cannot lag");
        };
        tapped.push((*event).clone());
    }
    let (journal, _) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(tapped, journal);
}

#[tokio::test]
async fn live_reduction_of_an_autonomous_goal_equals_the_reloaded_view() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        finish_with("earlier turn"),
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        crate::tool_outcome(facts_read()),
        finish_with("verified"),
    ])
    .with_before_call(4, write_facts(&workspace, "v2"));
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut earlier = request(&workspace, "Earlier");
    earlier.include_paths.clear();
    let earlier = run_to_end(&engine, &provider, earlier)
        .await
        .expect("earlier Run");
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let mut live = store
        .load_view(earlier.session_id)
        .await
        .expect("closed view")
        .expect("Session");
    let mut input = autonomous_input(&workspace);
    input.session_id = Some(earlier.session_id);
    input.title = None;
    let mut events = {
        let arany::RunHandle { events, result, .. } = engine
            .submit(
                AdmittedProviders::single(provider),
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            )
            .expect("submission");
        let outcome = result.await.expect("autonomous Goal").run;
        assert_eq!(outcome.run.status, RunStatus::Finished);
        events
    };
    engine.close().await.expect("engine shutdown");
    while let Some(event) = events.next().await {
        let arany::Tapped::Committed(event) = event else {
            panic!("bounded Run cannot lag");
        };
        arany::reduce_into(&mut live, &event).expect("incremental committed reduction");
    }
    let reloaded = store
        .load_view(earlier.session_id)
        .await
        .expect("reloaded view")
        .expect("Session");
    store.close().await.expect("Store shutdown");
    assert_eq!(live, reloaded);
    assert_eq!(reloaded.goals.last().expect("Goal").run_ids.len(), 2);
}

#[tokio::test]
async fn command_argument_refusals_continue_without_dispatch_and_stop_at_the_refusal_limit() {
    for (cwd, attempts, valid) in [
        ("docs/specs", 1, true),
        ("./doc/spec", 1, false),
        ("docs/specs/", 1, false),
        ("./docs/specs", 1, false),
        ("../outside", 1, false),
        ("./doc/spec", 3, false),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "facts");
        std::fs::create_dir_all(workspace.join("docs/specs")).expect("directory");
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let command = arany::ToolCall::Command {
            program: "rg".into(),
            args: vec!["--glob=*.md,spec.md".into(), "x".repeat(543)],
            cwd: cwd.into(),
            resource_class: None,
            timeout_ms: None,
        };
        let provider = Arc::new(ScriptedProvider::new(
            std::iter::once(crate::tool_outcome(facts_read()))
                .chain((0..attempts).map(|_| crate::tool_outcome(command.clone())))
                .chain([finish_with("The proposal was explained")])
                .collect(),
        ));
        let mut input = request(&workspace, "Read, search the docs, and answer");
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
        .expect("known refusal continuation");
        let limited = attempts == 3;
        assert_eq!(
            outcome.run.status,
            if limited {
                RunStatus::Failed
            } else {
                RunStatus::Finished
            }
        );
        engine.close().await.expect("shutdown");
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let run = &view.runs[0];
        assert_eq!(
            run.failure,
            limited.then_some(arany::RunFailure::RefusalLimit)
        );
        assert_eq!(
            run.tools.len(),
            if valid { 2 } else { 1 },
            "invalid arguments create no effect intent"
        );
        assert_eq!(
            run.agents[0].provider_calls[1].disposition,
            if valid {
                ProviderCallDisposition::ToolRequested
            } else {
                ProviderCallDisposition::Rejected
            }
        );
        if !valid {
            assert_eq!(run.agents[0].provider_calls[1].output_tokens, Some(8));
            assert!(
                run.agents[0].provider_calls[1..1 + attempts]
                    .iter()
                    .all(|call| {
                        call.refusal.as_ref().is_some_and(|refusal| {
                            refusal.cause == arany::ProposalRefusalCause::InvalidPath
                        }) && call.failure_reason
                            == Some(arany::ProviderFailureReason::ProposalRefused)
                    })
            );
            assert!(events.iter().all(|event| !matches!(&event.event, Event::ToolStarted { intent, .. } if intent.call == command)));
            let requests = provider.requests.lock().expect("requests");
            let step = &requests[2].input.tools.as_ref().expect("Tools").steps[1];
            assert_eq!(
                step.refusal.as_ref().expect("refused Tool result").cause,
                arany::ProposalRefusalCause::InvalidPath
            );
            assert!(step.observations.is_empty());
            assert!(
                requests
                    .iter()
                    .all(|request| !request.input.outcome_correction),
                "argument refusal does not spend the wire correction"
            );
            assert_eq!(requests.len(), if limited { 4 } else { 3 });
        }
        if limited {
            let engine = engine_support::open(
                StateRoot::open_existing(&state_path).expect("state"),
                EngineConfig::default(),
            )
            .expect("reopen");
            let mut next = request(&workspace, "What was the error?");
            next.session_id = Some(outcome.session_id);
            next.title = None;
            next.include_paths.clear();
            let recovered = run_to_end(&engine, &provider, next)
                .await
                .expect("new user turn");
            assert_eq!(recovered.run.status, RunStatus::Finished);
            engine.close().await.expect("shutdown");
            replayed(&state_path, outcome.session_id).await;
            let requests = provider.requests.lock().expect("requests");
            let failure = requests[4]
                .input
                .previous_run_failure
                .expect("committed failure cause");
            assert_eq!(
                failure.refusal_cause,
                Some(arany::ProposalRefusalCause::InvalidPath)
            );
        }
        assert!(provider.responses.lock().expect("responses").is_empty());
    }
}

#[tokio::test]
async fn outcome_correction_notice_is_spent_after_the_corrective_tool_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "facts");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let provider = ScriptedProvider::new(vec![
        finish_with("rejected placeholder"),
        crate::tool_outcome(facts_read()),
        finish_with("The answer after correction"),
    ]);
    *provider
        .responses
        .lock()
        .expect("responses")
        .front_mut()
        .expect("first") = Err(ProviderError::InvalidOutcomeContract);
    let provider = Arc::new(provider);
    let mut input = request(&workspace, "Read the facts and answer");
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
    .expect("corrective Tool continuation");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("shutdown");
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(view.runs[0].tools.len(), 1);
    let requests = provider.requests.lock().expect("requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.input.outcome_correction)
            .collect::<Vec<_>>(),
        [false, true, false]
    );
    assert!(provider.responses.lock().expect("responses").is_empty());
}

#[tokio::test]
async fn malformed_outcomes_get_one_journaled_correction_and_keep_the_failure_cause() {
    for failures in [1, 2] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "facts");
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let mut script = vec![crate::tool_outcome(facts_read())];
        script.extend((0..failures).map(|_| finish_with("rejected placeholder")));
        script.push(finish_with("The answer from the valid response"));
        let provider = ScriptedProvider::new(script);
        for response in provider
            .responses
            .lock()
            .expect("responses")
            .iter_mut()
            .skip(1)
            .take(failures)
        {
            *response = Err(ProviderError::InvalidOutcomeContract);
        }
        let provider = Arc::new(provider);
        let mut input = request(&workspace, "Read the facts and answer");
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
        .expect("bounded outcome correction");
        engine.close().await.expect("close before replay");
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let run = &view.runs[0];
        assert_eq!(run.tools.len(), 1, "the read is never redispatched");
        assert_eq!(run.agents[0].provider_calls.len(), 3);
        assert_eq!(
            run.failure,
            (failures == 2).then_some(arany::RunFailure::ProviderCall)
        );
        assert_eq!(
            run.assistant_message.as_deref(),
            (failures == 1).then_some("The answer from the valid response")
        );
        for end in 1..=events.len() {
            SessionView::replay(outcome.session_id, &events[..end])
                .expect("every committed prefix replays");
        }
        let calls = &run.agents[0].provider_calls;
        assert_eq!(
            calls[1].failure_reason,
            Some(arany::ProviderFailureReason::OutcomeContract)
        );
        assert_eq!(
            calls[2].failure_reason,
            (failures == 2).then_some(arany::ProviderFailureReason::OutcomeContract)
        );
        {
            let requests = provider.requests.lock().expect("requests");
            assert_eq!(requests.len(), 3);
            assert!(!requests[1].input.outcome_correction);
            assert!(requests[2].input.outcome_correction);
            assert_eq!(
                requests[1].input.tools.as_ref().expect("Tools").steps[0].observations,
                requests[2].input.tools.as_ref().expect("Tools").steps[0].observations,
                "settled observations survive correction"
            );
        }
        if failures == 2 {
            let refusal = events.iter().position(|event| matches!(event.event, Event::ProviderCallRecorded { ref record, .. } if record.failure_reason == Some(arany::ProviderFailureReason::OutcomeContract)))
                .expect("first malformed call");
            let second = events.iter().enumerate().skip(refusal + 1).find_map(|(index, event)| matches!(event.event, Event::ProviderCallRecorded { ref record, .. } if record.failure_reason == Some(arany::ProviderFailureReason::OutcomeContract)).then_some(index))
                .expect("second malformed call");
            let mut forbidden = events[..=second].to_vec();
            let mut extra = forbidden
                .iter()
                .rev()
                .find(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
                .expect("prior admitted reservation")
                .clone();
            extra.sequence = forbidden.last().expect("last Event").sequence + 1;
            forbidden.push(extra);
            assert!(
                SessionView::replay(outcome.session_id, &forbidden).is_err(),
                "replay refuses a third attempt"
            );
            let engine = engine_support::open(
                StateRoot::open_existing(&state_path).expect("state"),
                EngineConfig::default(),
            )
            .expect("reopen");
            let mut next = request(&workspace, "What was the error?");
            next.session_id = Some(outcome.session_id);
            next.title = None;
            next.include_paths.clear();
            let follow_up = run_to_end(&engine, &provider, next)
                .await
                .expect("new turn");
            engine.close().await.expect("shutdown");
            replayed(&state_path, follow_up.session_id).await;
            let requests = provider.requests.lock().expect("requests");
            let feedback = requests[3]
                .input
                .previous_run_failure
                .expect("prior failure after Tools");
            assert_eq!(
                feedback.stop,
                arany::RunStop::Failed(arany::RunFailure::ProviderCall)
            );
            assert_eq!(
                feedback.provider_failure,
                Some(arany::ProviderFailureReason::OutcomeContract)
            );
            assert!(
                !requests[3].input.outcome_correction,
                "a new Run has its own correction bound"
            );
        }
        assert!(provider.responses.lock().expect("responses").is_empty());
    }
}

#[tokio::test]
async fn pending_tool_refusal_does_not_clear_or_compact_settled_results() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "original");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    std::fs::write(state_path.join("tools.json"),
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#
    ).expect("write grant");
    let script = (0..4)
        .map(|_| crate::tool_outcome(facts_read()))
        .chain([crate::tool_outcome(arany::ToolCall::Write {
            path: "notes/facts.txt".into(),
            expected_digest: Some(facts_digest("original")),
            content: "x".repeat(64 * 1024),
        })])
        .collect();
    let provider = Arc::new(
        ScriptedProvider::new(script)
            .with_chatgpt_account(Uuid::now_v7(), [7; 32])
            .with_context_window(MIN_WINDOW_TOKENS)
            .with_max_output_tokens(4096)
            .with_input_tokens_at(4, 30_000),
    );
    let mut input = request(&workspace, "Read four times, then request the write");
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
    .expect("pending request ends with a typed context refusal");
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.failure, Some(arany::RunFailure::ToolContextFull));
    assert_eq!(run.tools.len(), 4);
    assert!(
        run.tools
            .iter()
            .all(|tool| matches!(tool.intent.call, arany::ToolCall::Read { .. }))
    );
    assert!(events.iter().all(|event| !matches!(
        event.event,
        Event::ToolResultsCleared { .. }
            | Event::GoalCompactionCallReserved { .. }
            | Event::GoalContextCompacted { .. }
    )));
    assert!(
        provider
            .compact_requests
            .lock()
            .expect("compactions")
            .is_empty()
    );
    assert_eq!(provider.requests.lock().expect("requests").len(), 5);
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("original file"),
        "original"
    );
}

#[tokio::test]
async fn near_full_context_refuses_write_before_dispatch_and_preserves_the_file() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "original");
    std::fs::write(workspace.join("AGENTS.md"), "a".repeat(32 * 1024)).expect("instructions");
    for name in ["big1.txt", "big2.txt"] {
        std::fs::write(workspace.join(name), "b".repeat(127 * 1024)).expect("include");
    }
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    std::fs::write(state_path.join("tools.json"),
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#
    ).expect("write grant");
    let provider = Arc::new(ScriptedProvider::new(vec![crate::tool_outcome(
        arany::ToolCall::Write {
            path: "notes/facts.txt".into(),
            expected_digest: Some(facts_digest("original")),
            content: "published only with a complete receipt".into(),
        },
    )]));
    let mut input = request(&workspace, &"x".repeat(60 * 1024));
    input.include_paths = vec!["big1.txt".into(), "big2.txt".into()];
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
    .expect("context refusal");
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    let allowance = run
        .config
        .as_ref()
        .expect("config")
        .tool_policy
        .as_ref()
        .expect("Tools")
        .max_context_bytes;
    assert!((16 * 1024..64 * 1024).contains(&allowance));
    assert_eq!(run.failure, Some(arany::RunFailure::ToolContextFull));
    assert!(run.tools.is_empty(), "no effect intent was dispatched");
    assert!(events.iter().all(|event| !matches!(
        event.event,
        Event::ToolStarted { .. } | Event::ToolFinished { .. }
    )));
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("unchanged file"),
        "original"
    );
    assert_eq!(provider.requests.lock().expect("requests").len(), 1);
}

#[tokio::test]
async fn tool_context_overflow_fails_with_the_context_cause() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    // 71 bytes of the 36,744 admitted are the rendered AGENTS.md heading and wrapper.
    std::fs::write(workspace.join("AGENTS.md"), "a".repeat(36_673)).expect("instructions");
    std::fs::write(workspace.join("notes/facts.txt"), "f".repeat(40 * 1024)).expect("facts");
    std::fs::write(workspace.join("big1.txt"), "b".repeat(127 * 1024)).expect("include");
    std::fs::write(workspace.join("big2.txt"), "c".repeat(127 * 1024)).expect("include");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_provider_calls: Some(30),
        ..arany::GoalBudget::default()
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(arany::ToolCall::Write {
            path: "notes/facts.txt".into(),
            expected_digest: Some(facts_digest(&"f".repeat(40 * 1024))),
            content: "x".repeat(64 * 1024),
        }),
        finish_with("Unexpected continuation after context refusal"),
    ]));
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    std::fs::write(state_path.join("tools.json"),
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#
    ).expect("write grant");
    let mut input = request(&workspace, &"x".repeat(60 * 1024));
    input.include_paths = vec!["big1.txt".into(), "big2.txt".into()];
    input.goal = arany::GoalRequest::new(budget, arany::GoalPhase::Implementation);
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
    .expect("context-full Run");
    let recovery = Arc::new(ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(arany::ToolCall::Search {
            path: "notes/facts.txt".into(),
            query: "missing".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: arany::MAX_SEARCH_ROWS,
        }),
        finish_with("The next message works"),
    ]));
    let mut next = request(&workspace, "Inspect the facts and answer this new message");
    next.session_id = Some(outcome.session_id);
    next.title = None;
    next.include_paths.clear();
    let recovered = run_with(
        &engine,
        &recovery,
        next,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("the failed Session accepts a new message");
    assert_eq!(recovered.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.failure, Some(arany::RunFailure::ToolContextFull));
    assert!(run.assistant_message.is_none());
    assert_eq!(run.tools.len(), 1, "only the bounded Read was dispatched");
    let read = run.tools[0].observation.as_ref().expect("Read receipt");
    assert_eq!(read.disposition, arany::ToolDisposition::Succeeded);
    let output: serde_json::Value = serde_json::from_str(&read.output).expect("Read result");
    assert_eq!(output["truncated"], true);
    assert!(
        run.config
            .as_ref()
            .expect("config")
            .tool_policy
            .as_ref()
            .expect("Tools")
            .max_context_bytes
            < 64 * 1024
    );
    assert!(events.iter().all(|event| !matches!(&event.event,
        Event::ToolStarted { intent, .. } if matches!(intent.call, arany::ToolCall::Write { .. })
    )));
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("original facts"),
        "f".repeat(40 * 1024)
    );
    assert_eq!(provider.requests.lock().expect("requests").len(), 2);
    assert_eq!(
        provider.responses.lock().expect("responses").len(),
        1,
        "the healthy refusal sentinel was never called"
    );
    assert!(
        provider
            .compact_requests
            .lock()
            .expect("compactions")
            .is_empty()
    );
    assert!(recovery.responses.lock().expect("responses").is_empty());
    assert_eq!(view.runs.len(), 2);
    assert_eq!(
        view.runs[1].assistant_message.as_deref(),
        Some("The next message works")
    );
    assert_successful_in_process_reads(&view.runs[1]);
}

#[tokio::test]
async fn cancelled_autonomous_segment_exports_its_run_outcome() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let cancellation = RunCancellation::new();
    let cancel = cancellation.clone();
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("cancelled pass"),
    ])
    .with_before_call(3, move || assert!(cancel.cancel(), "first cancellation"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("Collector listener");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("Collector address")
    );
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let collector = collect_trace_bodies(listener, Arc::clone(&done));
    let telemetry = arany::Telemetry::from_process(Some(&endpoint)).expect("loopback telemetry");
    let provider = Arc::new(provider);
    let engine = facts_engine_with_telemetry(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        telemetry.clone(),
    );
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            cancellation,
            ..Default::default()
        },
    )
    .await
    .expect("cancelled Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    telemetry.shutdown();
    done.store(true, std::sync::atomic::Ordering::Release);
    let bodies = collector.join().expect("Collector thread");
    assert_eq!(
        goal_segment_attribute(&bodies, "arany.outcome"),
        vec!["finished".to_owned(), "cancelled".to_owned()],
        "a cancelled segment of an open Goal exports the Run's cancelled outcome"
    );
    assert_eq!(
        goal_segment_attribute(&bodies, "arany.goal.status"),
        vec!["running".to_owned(), "running".to_owned()],
        "the open Goal keeps its Running status"
    );
}

#[tokio::test]
async fn tool_step_journals_requested_tools_one_and_replays() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(facts_read()),
        finish_with("done"),
    ]);
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read twice");
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
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let requested: Vec<u8> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ProviderCallRecorded { record, .. } => Some(record.requested_tools),
            _ => None,
        })
        .collect();
    assert_eq!(requested, [1, 1, 0]);
    assert_eq!(
        view.runs[0], outcome.run,
        "cold replay equals the live view"
    );
    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 3);
    for (index, call) in calls.iter().enumerate() {
        let steps = &call.input.tools.as_ref().expect("Tool context").steps;
        assert_eq!(steps.len(), index);
        assert!(steps.iter().all(|step| step.requests.len() == 1
            && step.observations.len() == 1
            && step.observations[0].intent.call == step.requests[0].call));
    }
}

#[tokio::test]
async fn previews_chunk_a_finishing_call_within_its_bound() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let answer = "x".repeat(5000);
    let provider = crate::PreviewProvider {
        text: arany::PreviewText {
            answer: answer.clone(),
            commentary: vec!["commentary".into()],
            commentary_limited: false,
            tentative: false,
        },
        completion: Some(ProviderOutcome::Answer(Answer { text: answer })),
    };
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "question");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            cancellation: RunCancellation::new(),
            hints: Some(RunHints::with_previews()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");
    let (events, _) = replayed(&state_path, outcome.session_id).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, Event::AssistantDisplayRecorded { .. }))
    );
    assert_action_bounds(&events, true, false);
}

#[tokio::test]
async fn between_run_goal_compaction_stays_within_its_bound() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![finish_with("first")]).with_compactions(vec![Ok(
        CompactionResponse {
            summary: "The completed Run remains in the canonical history.".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(12),
                output_tokens: Some(4),
                ..Default::default()
            },
            wire_provenance: None,
        },
    )]);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut first = request(&workspace, "Use one Provider call");
    first.include_paths.clear();
    first.goal.finish_after_run = false;
    let initial = run_to_end(&engine, &provider, first)
        .await
        .expect("Goal Run");
    engine
        .compact_session(
            &AdmittedProviders::single(provider.clone()),
            initial.session_id,
            workspace.clone(),
        )
        .await
        .expect("in-Goal compaction between Runs");
    engine.close().await.expect("engine shutdown");
    let (events, _) = replayed(&state_path, initial.session_id).await;
    assert_action_bounds(&events, false, false);
}

// A paused clock keeps rendered active-time digits equal across both Runs; they set each Read's result cap.
#[tokio::test(start_paused = true)]
async fn supervisor_shadow_journals_loops_without_changing_provider_input_or_outcome() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = facts_workspace(temp.path(), "stable");
    let mut observations = Vec::new();
    let normalized_run = arany::RunId::new();
    let normalized_actor = arany::AgentRunId::new();
    for enabled in [true, false] {
        let state_path = temp.path().join(if enabled { "shadow" } else { "off" });
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let session = engine
            .create_session(workspace.clone(), Some("Supervisor".into()))
            .await
            .unwrap();
        if !enabled {
            arany::FaultPlan::supervisor_off(session);
        }
        let mut outcomes: Vec<_> = (0..4).map(|_| crate::tool_outcome(facts_read())).collect();
        outcomes.push(finish_with("done"));
        let provider = Arc::new(ScriptedProvider::new(outcomes));
        let mut input = request(&workspace, "Inspect repeatedly");
        input.include_paths.clear();
        input.session_id = Some(session);
        input.title = None;
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
        engine.close().await.unwrap();
        let calls = provider.requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 5);
        let (events, view) = replayed(&state_path, session).await;
        assert_action_bounds(&events, false, false);
        let verdicts: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.event {
                Event::SupervisorVerdictRecorded { verdict, .. } => Some((e.sequence, verdict)),
                _ => None,
            })
            .collect();
        assert_eq!(verdicts.len(), if enabled { 2 } else { 0 });
        for (sequence, verdict) in verdicts {
            assert!(
                verdict
                    .signals
                    .iter()
                    .any(|signal| signal.signal == arany::supervisor::SignalKind::Loop)
            );
            assert_eq!(sequence, verdict.subject_sequence + 1);
        }
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        let inputs: Vec<_> = calls
            .iter()
            .map(|c| {
                let mut input = c.input.clone();
                // Elapsed and reserved time vary across live Runs; compare every other Goal fact.
                let normalize_facts = |facts: &mut arany::GoalContextFacts| {
                    facts.active_ms = 0;
                    facts.reserved_active_ms = 0;
                };
                if let Some(facts) = &mut input.goal_context_facts {
                    normalize_facts(facts);
                }
                let tools = input.tools.as_mut().unwrap();
                // The catalog embeds the wall clock at each Run's start.
                let mut catalog: serde_json::Value = serde_json::from_str(tools.catalog()).unwrap();
                catalog["host_clock"]["unix_ms"] = 0.into();
                tools.set_catalog(&catalog);
                for step in &mut tools.steps {
                    if let Some(facts) = &mut step.facts {
                        normalize_facts(facts);
                    }
                    for observation in &mut step.observations {
                        // Normalize the per-run intent identities, which never enter model input.
                        observation.intent.id = Uuid::nil();
                        observation.intent.run_id = normalized_run;
                        observation.intent.agent_run_id = normalized_actor;
                    }
                }
                input
            })
            .collect();
        observations.push((
            result.run.assistant_message.clone(),
            result.run.agents[0].summary.clone(),
            inputs,
        ));
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            5,
            "strict replay never calls the Provider"
        );
    }
    assert_eq!(observations[0], observations[1]);
}

struct WindowRun {
    _temp: tempfile::TempDir,
    workspace: std::path::PathBuf,
    result: arany::RunOutcome,
    events: Vec<arany::EventEnvelope>,
    view: arany::SessionView,
    compactions: usize,
}

async fn window_run(window: Option<u32>, reads: usize, reports: &[(usize, u32)]) -> WindowRun {
    window_run_with(window, arany::ContextWindow::default(), reads, reports).await
}

async fn window_run_with(
    window: Option<u32>,
    setting: arany::ContextWindow,
    reads: usize,
    reports: &[(usize, u32)],
) -> WindowRun {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), &format!("{}\n", "x".repeat(99)).repeat(150));
    let state_path = temp.path().join("state");
    let script = (0..reads)
        .map(|_| crate::tool_outcome(facts_read()))
        .chain([finish_with("Inspected the pages")])
        .collect();
    let mut provider =
        ScriptedProvider::new(script).with_compactions(vec![Err(ProviderError::Unavailable)]);
    for &(index, tokens) in reports {
        provider = provider.with_input_tokens_at(index, tokens);
    }
    if let Some(window) = window {
        provider = provider
            .with_chatgpt_account(Uuid::now_v7(), [7; 32])
            .with_context_window(window);
    }
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let mut input = request(&workspace, "Inspect the pages");
    input.include_paths.clear();
    input.goal.budget = Some(arany::GoalBudget {
        max_runs: 2,
        ..Default::default()
    });
    input.goal.context_window = setting;
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
    .expect("windowed Run");
    assert_eq!(result.run.status, RunStatus::Finished);
    assert_eq!(provider.requests.lock().unwrap().len(), reads + 1);
    engine.close().await.unwrap();
    let (events, view) = replayed(&state_path, result.session_id).await;
    WindowRun {
        _temp: temp,
        workspace,
        result,
        events,
        view,
        compactions: provider.compact_requests.lock().unwrap().len(),
    }
}

fn window_pin(view: &arany::SessionView, run: usize) -> (Option<u32>, u32, Option<u32>) {
    let config = view.runs[run].config.clone().expect("pinned config");
    (
        config.context_window_tokens,
        config.context_usage.expect("usage").budget_bytes,
        config.tool_policy.map(|policy| policy.max_context_bytes),
    )
}

fn clearings(events: &[arany::EventEnvelope]) -> Vec<(usize, u32)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event.event {
            Event::ToolResultsCleared {
                covered_tool_count, ..
            } => Some((index, covered_tool_count)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn anthropic_evidence_refreshes_between_runs_and_replays_each_window() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "facts\n");
    let state_path = temp.path().join("state");
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            finish_with("Checked the facts"),
            finish_with("Checked the facts again"),
        ])
        .with_profile("anthropic", "claude-sonnet-5")
        .with_context_window_snapshots(vec![Some(100_000), None]),
    );
    let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let mut first_input = request(&workspace, "Check the facts");
    first_input.include_paths.clear();
    let first = run_with(&engine, &provider, first_input, RunControls::default())
        .await
        .expect("Anthropic context window admitted");
    assert_eq!(first.run.status, RunStatus::Finished);
    let mut second_input = request(&workspace, "Check the facts again");
    second_input.include_paths.clear();
    let second = run_with(&engine, &provider, second_input, RunControls::default())
        .await
        .expect("Anthropic context window revalidated");
    assert_eq!(second.run.status, RunStatus::Finished);
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[0].input.context_window_tokens, Some(100_000));
        assert_eq!(requests[1].input.context_window_tokens, None);
    }
    engine.close().await.expect("engine shutdown");

    let (first_events, first_view) = replayed(&state_path, first.session_id).await;
    let (second_events, second_view) = replayed(&state_path, second.session_id).await;
    assert_eq!(
        first_view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .context_window_tokens,
        Some(100_000)
    );
    assert_eq!(
        second_view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .context_window_tokens,
        None
    );
    assert!(first_events.iter().any(|event| matches!(
        &event.event,
        Event::RunStarted { config, .. } if config.context_window_tokens == Some(100_000)
    )));
    assert!(second_events.iter().any(|event| matches!(
        &event.event,
        Event::RunStarted { config, .. } if config.context_window_tokens.is_none()
    )));
}

#[tokio::test]
async fn catalog_window_resizes_the_allowance_and_reported_pressure_clears_before_the_next_call() {
    let default = window_run(None, 8, &[(7, 244_800)]).await;
    let pressure = window_run(Some(272_000), 8, &[(7, 244_800)]).await;
    let clearing = window_run(Some(272_000), 8, &[(7, 190_400)]).await;
    let calm = window_run(Some(272_000), 8, &[(7, 190_399)]).await;

    let (window, default_budget, default_tools) = window_pin(&default.view, 0);
    assert_eq!(
        window, None,
        "a route without catalog metadata keeps the defaults"
    );
    let (window, budget, tools) = window_pin(&pressure.view, 0);
    assert_eq!(window, Some(272_000));
    // 0.9 x 272,000 tokens x 3 bytes replaces the 376 KiB content ceiling.
    assert_eq!(budget - default_budget, 734_400 - 376 * 1024);
    assert_eq!(
        tools.unwrap() - default_tools.unwrap(),
        734_400 - 376 * 1024
    );
    assert_eq!(window_pin(&calm.view, 0), (Some(272_000), budget, tools));

    assert!(clearings(&default.events).is_empty());
    assert!(
        clearings(&calm.events).is_empty(),
        "69.99% does not trigger"
    );
    for run in [&default, &pressure, &clearing, &calm] {
        assert_eq!(run.compactions, 0);
    }
    let [(_, cleared)] = clearings(&clearing.events)[..] else {
        panic!("one clearing after the 70% report");
    };
    assert_eq!(cleared, 5);
    let [(index, covered)] = clearings(&pressure.events)[..] else {
        panic!("one clearing after the 90% report");
    };
    assert_eq!(covered, 5, "the three newest results stay");
    assert!(matches!(
        pressure.events[index + 1].event,
        Event::GoalProviderCallReserved { .. }
    ));
    assert_eq!(
        pressure.events[index + 1..]
            .iter()
            .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
            .count(),
        1,
        "clearing precedes the call after the report"
    );

    // An interruption after the clearing boundary resumes with identical, still-windowed requests.
    let mut resumed = Vec::new();
    for copy in ["first", "second"] {
        let temp = tempfile::tempdir().expect("crash root");
        let crash_path = temp.path().join(copy);
        let store = Store::open(StateRoot::admit(&crash_path).unwrap()).unwrap();
        for event in &pressure.events[..=index] {
            store
                .append(pressure.result.session_id, event.event.clone())
                .await
                .unwrap();
        }
        store.close().await.unwrap();
        let provider = Arc::new(
            ScriptedProvider::new(vec![finish_with("Checked the saved facts")])
                .with_chatgpt_account(
                    pressure.view.runs[0]
                        .config
                        .as_ref()
                        .unwrap()
                        .chatgpt_provenance
                        .as_ref()
                        .unwrap()
                        .account_id,
                    [7; 32],
                )
                .with_context_window(272_000),
        );
        let engine = facts_engine(StateRoot::open_existing(&crash_path).unwrap(), &crash_path);
        let mut input = request(&pressure.workspace, "Continue the inspection");
        input.session_id = Some(pressure.result.session_id);
        input.title = None;
        input.include_paths.clear();
        input.goal = arany::GoalRequest::resume(pressure.result.goal_id, true);
        run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .expect("fresh Run after the clearing boundary");
        engine.close().await.unwrap();
        let (_, view) = replayed(&crash_path, pressure.result.session_id).await;
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        assert_eq!(view.runs[0].tool_results_cleared_count, covered);
        assert_eq!(window_pin(&view, 1).0, Some(272_000));
        assert_eq!(window_pin(&view, 1).1, budget);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let history = &requests[0].input.history;
        let records: serde_json::Value =
            serde_json::from_str(&history.last().unwrap().assistant).unwrap();
        assert_eq!(
            records["tool_records"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|record| record["cleared"] == true)
                .count(),
            covered as usize
        );
        resumed.push(history.clone());
    }
    assert_eq!(resumed[0], resumed[1], "the resumed request is byte-stable");
}

#[tokio::test]
async fn context_window_setting_pins_the_effective_window_for_budgets_and_thresholds() {
    let full = window_run(Some(272_000), 8, &[]).await;
    // 70% of the effective 136,000 tokens clears, though it is only 35% of the model window.
    let half = window_run_with(
        Some(272_000),
        arany::ContextWindow::Percent(50),
        8,
        &[(7, 95_200)],
    )
    .await;
    let calm = window_run_with(
        Some(272_000),
        arany::ContextWindow::Tokens(136_000),
        8,
        &[(7, 95_199)],
    )
    .await;
    let clamped =
        window_run_with(Some(272_000), arany::ContextWindow::Tokens(500_000), 8, &[]).await;
    let unknown = window_run_with(None, arany::ContextWindow::Tokens(136_000), 8, &[]).await;
    let effective = |run: &WindowRun| {
        let config = run.view.runs[0].config.as_ref().expect("pinned config");
        (
            config.context_window_tokens,
            config.effective_context_window_tokens,
        )
    };
    assert_eq!(effective(&full), (Some(272_000), Some(272_000)));
    assert_eq!(effective(&half), (Some(272_000), Some(136_000)));
    assert_eq!(effective(&calm), (Some(272_000), Some(136_000)));
    assert_eq!(effective(&clamped), (Some(272_000), Some(272_000)));
    assert_eq!(effective(&unknown), (None, None));
    let (_, full_budget, _) = window_pin(&full.view, 0);
    let (_, half_budget, _) = window_pin(&half.view, 0);
    // 0.9 x 136,000 fewer tokens at 3 bytes each.
    assert_eq!(full_budget - half_budget, 734_400 - 367_200);
    assert_eq!(window_pin(&clamped.view, 0).1, full_budget);
    assert!(clearings(&full.events).is_empty());
    assert!(clearings(&calm.events).is_empty());
    assert!(clearings(&clamped.events).is_empty());
    assert_eq!(clearings(&half.events).len(), 1);
    // Each primary call commits the content bytes its reported tokens calibrate against.
    let calls = &half.view.runs[0].agents[0].provider_calls;
    assert_eq!(calls.len(), 9);
    assert!(calls.iter().all(|call| call.context_bytes.is_some()));
}

#[tokio::test]
async fn each_pressured_report_maintains_context_once() {
    let repeated = window_run(Some(272_000), 9, &[(7, 244_800), (8, 244_800)]).await;
    let clearings = clearings(&repeated.events);
    assert_eq!(
        clearings
            .iter()
            .map(|(_, covered)| *covered)
            .collect::<Vec<_>>(),
        [5],
        "clearing the newly old result alone would free less than the floor"
    );
    // The second report skips clearing for one charged summary, then the Run ends.
    assert_eq!(repeated.compactions, 1);
    let summaries: Vec<_> = repeated
        .events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.event, Event::GoalContextCompacted { .. }))
        .map(|(index, _)| index)
        .collect();
    let [last_maintenance] = summaries[..] else {
        panic!("one summary after the second report");
    };
    assert!(last_maintenance > clearings[0].0);
    assert_eq!(
        repeated.events[last_maintenance..]
            .iter()
            .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
            .count(),
        1,
        "no further maintenance after the final call"
    );
}

#[tokio::test]
async fn catalog_windows_leave_continuation_room_in_a_crowded_session() {
    for (window, line_repeats) in [
        (None, 96),
        (Some(MIN_WINDOW_TOKENS), 96),
        (Some(272_000), 96),
        (Some(MIN_WINDOW_TOKENS), 24),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(
            temp.path(),
            &format!("{}\n", "fact ".repeat(line_repeats)).repeat(83),
        );
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let seed = Arc::new(ScriptedProvider::new(vec![
            finish_with("Historical answer");
            6
        ]));
        let mut session = None;
        for _ in 0..6 {
            let mut input = request(&workspace, &"x".repeat(60 * 1024));
            input.session_id = session;
            input.title = None;
            input.include_paths.clear();
            session = Some(
                run_with(&engine, &seed, input, RunControls::default())
                    .await
                    .expect("historical Run")
                    .session_id,
            );
        }
        let mut provider = ScriptedProvider::new(vec![
            crate::tool_outcome(facts_read()),
            crate::tool_outcome(arany::ToolCall::Search {
                path: "notes/facts.txt".into(),
                query: "missing".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: arany::MAX_SEARCH_ROWS,
            }),
            finish_with("Read 83 lines and searched for a missing fact"),
        ])
        .with_chatgpt_account(Uuid::now_v7(), [7; 32])
        .with_profile("chatgpt", "gpt-6-luna")
        .with_effort(arany::Effort::Medium)
        .with_max_output_tokens(4096);
        if let Some(window) = window {
            provider = provider.with_context_window(window);
        }
        let provider = Arc::new(provider);
        let mut input = request(&workspace, "Inspect the facts and answer");
        input.session_id = session;
        input.title = None;
        input.include_paths.clear();
        if line_repeats == 24 {
            input.goal = arany::GoalRequest::new(
                arany::GoalBudget {
                    max_provider_calls: Some(3),
                    ..Default::default()
                },
                arany::GoalPhase::Implementation,
            );
        }
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
        .expect("bounded continuation");
        assert_eq!(outcome.run.status, RunStatus::Finished, "window={window:?}");
        engine.close().await.expect("shutdown");
        let (_, view) = replayed(&state_path, outcome.session_id).await;
        let run = &view.runs[6];
        let config = run.config.as_ref().expect("pinned context");
        assert_eq!(config.context_window_tokens, window);
        let allowance = config
            .tool_policy
            .as_ref()
            .expect("Tool budget")
            .max_context_bytes;
        assert!(
            allowance
                >= if window == Some(MIN_WINDOW_TOKENS) {
                    72 * 1024
                } else {
                    192 * 1024
                },
            "optional history cannot consume continuation room: {allowance}"
        );
        assert_successful_in_process_reads(run);
        assert_eq!(provider.requests.lock().expect("requests").len(), 3);
        assert!(
            provider
                .compact_requests
                .lock()
                .expect("compactions")
                .is_empty(),
            "a read below two-thirds of the allowance leaves the worst-case reserve to admission"
        );
        if window != Some(272_000) {
            assert!(
                config.excluded_history_runs > 0,
                "bounded history selection is explicit"
            );
        }
        assert_eq!(
            config.history_run_ids.len() + config.excluded_history_runs as usize,
            6
        );
        assert_eq!(view.runs.len(), 7, "canonical history is preserved");
    }
}

#[tokio::test]
async fn primary_fallback_chain_pins_its_smallest_window_or_none() {
    let mut pins = Vec::new();
    for fallback in [Some(200_000), None] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "facts\n");
        let state_path = temp.path().join("state");
        let account = Uuid::now_v7();
        let primary = Arc::new(
            ScriptedProvider::new(vec![finish_with("Answered")])
                .with_chatgpt_account(account, [7; 32])
                .with_context_window(272_000),
        );
        let mut backup = ScriptedProvider::new(Vec::new())
            .with_chatgpt_account(account, [8; 32])
            .with_profile("chatgpt", "fallback-model");
        if let Some(window) = fallback {
            backup = backup.with_context_window(window);
        }
        let backup = Arc::new(backup);
        let bindings = arany::AdmittedProviders::from_roles(arany::Providers::all(vec![
            primary.clone() as Arc<dyn arany::Provider>,
            backup.clone() as Arc<dyn arany::Provider>,
        ]));
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut input = request(&workspace, "Answer directly");
        input.include_paths.clear();
        let outcome =
            super::engine_support::run_routed(&engine, bindings, input, RunControls::default())
                .await
                .expect("routed Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert!(backup.requests.lock().unwrap().is_empty());
        engine.close().await.expect("shutdown");
        let (_, view) = replayed(&state_path, outcome.session_id).await;
        pins.push(window_pin(&view, 0));
    }
    assert_eq!(pins[0].0, Some(200_000), "the smallest window in the chain");
    assert_eq!(
        pins[1].0, None,
        "a fallback without metadata keeps the defaults"
    );
    // 0.9 x 200,000 tokens x 3 bytes against the 376 KiB default.
    assert_eq!(pins[0].1 - pins[1].1, 540_000 - 376 * 1024);
}

#[tokio::test]
async fn worst_case_reserve_alone_neither_clears_nor_summarizes_before_a_command_step() {
    let temp = tempfile::tempdir().expect("temporary root");
    let page = format!("{}\n", "x".repeat(99)).repeat(125);
    let workspace = facts_workspace(temp.path(), &page);
    for name in ["big1.txt", "big2.txt"] {
        std::fs::write(workspace.join(name), "b".repeat(47 * 1024)).expect("include");
    }
    let state_path = temp.path().join("state");
    let command = arany::ToolCall::Command {
        program: "unconfigured".into(),
        args: Vec::new(),
        cwd: "notes".into(),
        resource_class: None,
        timeout_ms: None,
    };
    let provider = Arc::new(ScriptedProvider::new(
        (0..4)
            .map(|_| crate::tool_outcome(facts_read()))
            .chain([
                crate::tool_outcome(command.clone()),
                finish_with("Checked the Command refusal"),
            ])
            .collect(),
    ));
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Inspect four pages then request a Command");
    input.include_paths = vec!["big1.txt".into(), "big2.txt".into()];
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
    .expect("Run with room for the complete Command step");
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.failure, None);
    let allowance = run
        .config
        .as_ref()
        .unwrap()
        .tool_policy
        .as_ref()
        .unwrap()
        .max_context_bytes;
    assert!((270 * 1024..=274 * 1024).contains(&allowance));
    assert_eq!(run.tools.len(), 5);
    assert_eq!(run.tools[4].intent.call, command);
    assert_eq!(run.tools[4].intent.limits.result_bytes, 64 * 1024);
    assert_eq!(
        run.tools[4].observation.as_ref().unwrap().disposition,
        arany::ToolDisposition::Denied
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().all(|request| {
        !request.input.outcome_correction
            && request.input.previous_run_failure.is_none()
            && request.input.last_answer_refused.is_none()
    }));
    assert!(provider.compact_requests.lock().unwrap().is_empty());
    assert!(provider.responses.lock().unwrap().is_empty());
    let context = requests[4].input.tools.as_ref().unwrap();
    assert_eq!(context.observation_count(), 4);
    assert!(context.steps.iter().all(|step| !step.cleared));
    let used = arany::test_support::tool_context_bytes(context);
    assert!((48 * 1024..=60 * 1024).contains(&used));
    let reserve = arany::test_support::tool_continuation_reserve(requests[4].max_output_tokens, 0);
    assert!(
        used + reserve > allowance as usize,
        "the worst-case next turn no longer triggers soft maintenance"
    );
    assert!(used * 3 < allowance as usize * 2);
    assert!(events.iter().all(|event| !matches!(
        event.event,
        Event::ToolResultsCleared { .. }
            | Event::GoalCompactionCallReserved { .. }
            | Event::GoalContextCompacted { .. }
    )));
}

#[tokio::test]
async fn near_full_context_refuses_command_before_dispatch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "original");
    std::fs::write(workspace.join("AGENTS.md"), "a".repeat(32 * 1024)).expect("instructions");
    for name in ["big1.txt", "big2.txt"] {
        std::fs::write(workspace.join(name), "b".repeat(127 * 1024)).expect("include");
    }
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    std::fs::write(
        state_path.join("tools.json"),
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("write grant");
    let command = arany::ToolCall::Command {
        program: "unconfigured".into(),
        args: Vec::new(),
        cwd: "".into(),
        resource_class: None,
        timeout_ms: None,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![crate::tool_outcome(command)]));
    let mut input = request(&workspace, &"x".repeat(60 * 1024));
    input.include_paths = vec!["big1.txt".into(), "big2.txt".into()];
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
    .expect("context refusal");
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.failure, Some(arany::RunFailure::ToolContextFull));
    assert!(run.tools.is_empty(), "no command intent was dispatched");
    assert!(events.iter().all(|event| !matches!(
        event.event,
        Event::ToolStarted { .. } | Event::ToolFinished { .. }
    )));
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("unchanged file"),
        "original"
    );
    assert_eq!(provider.requests.lock().expect("requests").len(), 1);
}

#[tokio::test]
async fn clearing_below_the_floor_summarizes_and_uses_remaining_capacity_before_context_full() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "");
    let state_path = temp.path().join("state");
    let provider = Arc::new(
        ScriptedProvider::new(
            (0..2000)
                .map(|_| crate::tool_outcome(facts_read()))
                .collect(),
        )
        .with_compactions(
            (0..2000)
                .map(|_| {
                    Ok(CompactionResponse {
                        summary: String::new(),
                        response_id: None,
                        usage: arany::Usage::default(),
                        wire_provenance: None,
                    })
                })
                .collect(),
        )
        .with_profile("anthropic", "test-model")
        .with_context_window(MIN_WINDOW_TOKENS),
    );
    let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let mut input = request(
        &workspace,
        "Inspect bounded empty pages until context is full",
    );
    input.include_paths.clear();
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: None,
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
    });
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
    .expect("honest context-full outcome");
    engine.close().await.unwrap();
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(
        view.runs[0].failure,
        Some(arany::RunFailure::ToolContextFull)
    );
    assert!(
        view.runs[0]
            .tools
            .iter()
            .any(|tool| tool.intent.limits.result_bytes < 64 * 1024)
    );
    // Empty pages free too little to clear, so maintenance summarizes instead of breaking the cache.
    assert!(events.iter().all(|event| !matches!(
        event.event,
        Event::ToolResultsCleared { cleared_bytes, .. } if cleared_bytes < 64 * 1024
    )));
    let summary = events
        .iter()
        .position(|event| matches!(event.event, Event::GoalCompactionCallReserved { .. }))
        .unwrap();
    assert!(
        events[summary + 1..]
            .iter()
            .any(|event| matches!(event.event, Event::GoalContextCompacted { .. }))
    );
    assert!(!provider.compact_requests.lock().unwrap().is_empty());
    let goal = &view.goals[0];
    assert_eq!(
        goal.provider_calls as usize,
        provider.requests.lock().unwrap().len() + provider.compact_requests.lock().unwrap().len()
    );
}

#[tokio::test]
async fn skill_reads_continue_under_context_pressure_without_a_guard_executable() {
    use std::os::unix::fs::PermissionsExt;
    for compact in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), &"Fact ".repeat(20 * 1024));
        let mut bodies = Vec::new();
        for index in 0..5 {
            let name = format!("skill-{index}");
            let directory = workspace.join(".agents/skills").join(&name);
            std::fs::create_dir_all(&directory).expect("Skill directory");
            let body = format!(
                "---\nname: {name}\ndescription: Guidance\n---\n{}",
                "instruction ".repeat(680)
            );
            std::fs::write(directory.join("SKILL.md"), &body).expect("Skill body");
            bodies.push(body);
        }
        std::fs::write(workspace.join("AGENTS.md"), "Guidance. ".repeat(1200)).expect("guidance");
        let state_path = temp.path().join("state");
        let state = StateRoot::admit(&state_path).expect("state");
        let config = state_path.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#).expect("config");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("private config");
        let engine = engine_support::open(
            state,
            EngineConfig {
                guard_executable: Some(temp.path().join("missing-guard")),
                ..Default::default()
            },
        )
        .expect("engine");
        let provider = ScriptedProvider::new(
            (1..5)
                .map(|index| {
                    tool_outcome(arany::ToolCall::Skill {
                        name: format!("skill-{index}"),
                        resource: None,
                    })
                })
                .chain([finish_with("Skills read")])
                .collect(),
        )
        .with_max_output_tokens(if compact { 4096 } else { 16384 })
        .with_compactions(
            (0..8)
                .map(|_| {
                    if compact {
                        Ok(CompactionResponse {
                            summary: "Earlier Skills were read; no scripts ran.".into(),
                            response_id: None,
                            usage: arany::Usage {
                                input_tokens: Some(32),
                                output_tokens: Some(12),
                                ..Default::default()
                            },
                            wire_provenance: None,
                        })
                    } else {
                        Err(ProviderError::Rejected)
                    }
                })
                .collect(),
        );
        // The summary follows reported pressure: clearing one Skill body alone would free too little.
        let provider = Arc::new(if compact {
            provider
                .with_chatgpt_account(Uuid::now_v7(), [7; 32])
                .with_context_window(272_000)
                .with_input_tokens_at(3, 244_800)
        } else {
            provider
        });
        let mut input = request(&workspace, "Apply the selected Skill");
        input.skill = Some("skill-0".into());
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
        .expect("Skill Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(
            outcome.run.assistant_message.as_deref(),
            Some("Skills read")
        );
        engine.close().await.expect("close");
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let run = view.runs.last().expect("Run");
        assert_eq!(run.tools.len(), 5);
        for (tool, body) in run.tools.iter().zip(&bodies) {
            let observation = tool.observation.as_ref().expect("observation");
            assert_eq!(&observation.output, body);
            assert!(matches!(
                observation.attestation,
                Some(arany::Attestation::InProcess { .. })
            ));
        }
        assert_eq!(
            view.goals.last().expect("Goal").status,
            arany::GoalStatus::Completed
        );
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 5);
        let initial = requests[0].input.tools.as_ref().expect("Tools");
        assert_eq!(
            initial.invoked_skill.as_ref().expect("invoked").output,
            bodies[0]
        );
        assert!(!initial.catalog().contains("instruction instruction"));
        assert!(initial.steps.is_empty());
        assert!(provider.responses.lock().expect("responses").is_empty());
        if compact {
            let allowance = run
                .config
                .as_ref()
                .expect("config")
                .tool_policy
                .as_ref()
                .expect("Tool policy")
                .max_context_bytes;
            let reserve =
                arany::test_support::tool_continuation_reserve(requests[0].max_output_tokens, 0);
            assert!(
                reserve <= allowance as usize,
                "the compaction fixture must avoid the half-allowance fallback"
            );
            assert!(
                !provider
                    .compact_requests
                    .lock()
                    .expect("compactions")
                    .is_empty()
            );
            assert!(events.iter().any(|event| matches!(&event.event,
                Event::GoalContextCompacted { record, .. }
                    if matches!(record.status, CompactionStatus::Succeeded { .. }))));
        }
        assert_action_bounds(&events, false, false);
    }
}

#[tokio::test]
async fn empty_tool_steps_fail_as_provider_calls_without_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = facts_workspace(temp.path(), "bounded facts");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Tools(
        Vec::new(),
    )]));
    let mut input = request(&workspace, "Read the facts");
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
    .expect("a malformed Tool response has a durable failed Run");
    engine.close().await.unwrap();
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(*run, outcome.run);
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.failure, Some(arany::RunFailure::ProviderCall));
    assert!(run.tools.is_empty());
    assert!(run.assistant_message.is_none());
    let calls = &run.agents[0].provider_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].disposition,
        ProviderCallDisposition::InvalidResponse
    );
    assert!(calls[0].refusal.is_none());
    assert_eq!(calls[0].requested_tools, 0);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn multi_call_steps_commit_all_intents_and_refuse_whole_invalid_batches() {
    for (case, count, invalid, duplicate, window) in [
        ("four reads", 4, false, false, None),
        (
            "over cap",
            arany::test_support::MAX_STEP_TOOL_CALLS + 1,
            false,
            false,
            None,
        ),
        ("invalid third call", 4, true, false, None),
        ("duplicate id", 4, false, true, None),
        ("context full", 4, false, false, Some(MIN_WINDOW_TOKENS)),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "bounded facts");
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let requests = (0..count)
            .map(|index| arany::ToolRequest {
                id: arany::ToolUseId::new(format!("call_{}", if duplicate { 0 } else { index }))
                    .unwrap(),
                call: if invalid && index == 2 {
                    arany::ToolCall::Read {
                        path: "../outside".into(),
                        offset: None,
                        limit: None,
                    }
                } else {
                    facts_read()
                },
            })
            .collect();
        let mut provider = ScriptedProvider::new(vec![
            ProviderOutcome::Tools(requests),
            finish_with("Read step settled"),
        ])
        .with_context_window_snapshots(vec![window]);
        if window.is_some() {
            provider = provider.with_chatgpt_account(Uuid::now_v7(), [7; 32]);
        }
        let provider = Arc::new(provider);
        let mut input = request(&workspace, "Read the facts in one step");
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
        .expect(case);
        engine.close().await.unwrap();
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        assert_eq!(view.runs[0], outcome.run, "{case}: durable result");
        let starts: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolStarted { .. }))
            .map(|event| event.sequence)
            .collect();
        let finishes: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolFinished { .. }))
            .map(|event| event.sequence)
            .collect();
        if case == "four reads" {
            assert_eq!(starts.len(), 4);
            assert_eq!(finishes.len(), 4);
            let proposals: Vec<_> = events
                .iter()
                .filter(|event| matches!(event.event, Event::ToolProposed { .. }))
                .map(|event| event.sequence)
                .collect();
            assert_eq!(proposals.len(), 4);
            let reservations: Vec<_> = events
                .iter()
                .filter(|event| matches!(event.event, Event::ToolActiveTimeReserved { .. }))
                .collect();
            assert_eq!(reservations.len(), 4);
            assert!(
                arany::test_support::admissions::recorded(outcome.session_id)
                    .iter()
                    .all(|(count, _, _, _)| {
                        *count < proposals[0] as usize || *count >= proposals[3] as usize
                    }),
                "initial intents commit without a second admission boundary"
            );
            let oversized_slots: Vec<_> =
                arany::test_support::admissions::recorded(outcome.session_id)
                    .into_iter()
                    .filter(|(count, _, _, _)| *count + 1 == proposals[0] as usize)
                    .map(|(_, _, _, observations)| observations)
                    .collect();
            let per_call = oversized_slots.iter().min().expect("per-call admission");
            let batch = oversized_slots.iter().max().expect("batch admission");
            assert!(
                *batch >= count * per_call,
                "batch admission must retain every call's oversized payload allowance"
            );
            for (index, reservation) in reservations.iter().enumerate() {
                let intent_id = outcome.run.tools[index].intent.id;
                let finish = events
                    .iter()
                    .find(|event| matches!(&event.event,
                        Event::ToolFinished { observation, .. } if observation.intent.id == intent_id))
                    .unwrap();
                assert!(
                    proposals[3] < reservation.sequence && reservation.sequence < starts[index]
                );
                assert!(starts[index] < finish.sequence, "call {index}");
            }

            assert!(
                proposals[3] < starts[0],
                "every intent commits before dispatch"
            );
            assert_eq!(outcome.run.status, RunStatus::Finished);
            assert_eq!(outcome.run.agents[0].provider_calls[0].requested_tools, 4);
            let calls = provider.requests.lock().unwrap();
            let step = &calls[1].input.tools.as_ref().unwrap().steps[0];
            assert_eq!(step.requests.len(), 4);
            assert_eq!(step.observations.len(), 4);
            assert!(
                step.observations
                    .iter()
                    .all(|observed| observed.disposition == arany::ToolDisposition::Succeeded)
            );
        } else {
            assert!(
                starts.is_empty() && finishes.is_empty(),
                "{case}: nothing dispatched"
            );
            if window.is_some() {
                assert_eq!(
                    outcome.run.failure,
                    Some(arany::RunFailure::ToolContextFull)
                );
            } else {
                assert_eq!(outcome.run.status, RunStatus::Finished);
                let calls = provider.requests.lock().unwrap();
                let step = &calls[1].input.tools.as_ref().unwrap().steps[0];
                assert!(step.refusal.is_some());
                assert_eq!(step.refusal_index, invalid.then_some(2));
            }
        }
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            if window.is_some() { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn concurrent_reads_commit_in_completion_order_and_settle_before_effects() {
    for case in [
        "four reads",
        "read before effect",
        "cancel reads",
        "limit reads",
    ] {
        let _deadline = engine_support::deadline("concurrent read step", Duration::from_secs(180));
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "first\nsecond\nthird\nfourth\n");
        let state = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state).unwrap(), &state);
        std::fs::write(state.join("tools.json"),
            br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
        let count = if case == "four reads" { 4 } else { 2 };
        let reads: Vec<_> = (1..=count)
            .map(|offset| arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: Some(offset),
                limit: Some(1),
            })
            .collect();
        let mut calls = reads.clone();
        if count < 4 {
            calls.insert(
                0,
                arany::ToolCall::Mkdir {
                    path: "notes/created".into(),
                },
            );
            calls.push(arany::ToolCall::Mkdir {
                path: "notes/final".into(),
            });
        }
        let requests = calls
            .iter()
            .enumerate()
            .map(|(index, call)| arany::ToolRequest {
                id: arany::ToolUseId::new(format!("call_{index}")).unwrap(),
                call: call.clone(),
            })
            .collect();
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Tools(requests),
            finish_with("Step settled"),
        ]);
        let recorded = Arc::clone(&provider.requests);
        let (send, receive) = tokio::sync::oneshot::channel();
        let send = Mutex::new(Some(send));
        let provider = Arc::new(provider.with_before_call(0, move || {
            let run = recorded.lock().unwrap()[0].run_id;
            let gates: Vec<_> = reads
                .iter()
                .map(|call| arany::FaultPlan::gate_in_process_read(run, call.clone()))
                .collect();
            send.lock().unwrap().take().unwrap().send(gates).unwrap();
        }));
        let cancellation = RunCancellation::new();
        let mut events = engine.subscribe();
        let mut input = request(&workspace, "Read the facts concurrently");
        input.include_paths.clear();
        let drive = async {
            let gates = receive.await.unwrap();
            let mut releases = Vec::new();
            for (entered, release) in gates {
                entered.await.unwrap();
                releases.push(Some(release));
            }
            assert!(
                !workspace.join("notes/created").exists(),
                "effect waits for every read"
            );
            if case == "cancel reads" {
                cancellation.cancel();
                return releases;
            }
            if case == "limit reads" {
                let mut started = 0;
                let mut runtime_ms = 0;
                while started < calls.len() {
                    let event = next_committed(&mut events, "read deadlines").await;
                    if let Event::ToolProposed { intent, .. } = &event.event {
                        started += 1;
                        runtime_ms = runtime_ms.max(intent.limits.runtime_ms);
                    }
                }
                tokio::time::pause();
                tokio::time::advance(Duration::from_millis(u64::from(runtime_ms) + 1)).await;
                tokio::time::resume();
                return releases;
            }
            let order = if count == 4 {
                vec![2, 0, 3, 1]
            } else {
                vec![1, 0]
            };
            for index in order {
                assert!(!workspace.join("notes/created").exists());
                releases[index].take().unwrap().send(()).unwrap();
                loop {
                    let event = next_committed(&mut events, "read completion").await;
                    if let Event::ToolFinished { observation, .. } = &event.event {
                        assert!(matches!(&observation.intent.call,
                            arany::ToolCall::Read { offset: Some(offset), .. } if *offset as usize == index + 1));
                        assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
                        break;
                    }
                }
            }
            releases
        };
        let (outcome, _held_releases) = tokio::join!(
            run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    cancellation: cancellation.clone(),
                    ..Default::default()
                }
            ),
            drive
        );
        let outcome = outcome.unwrap();
        engine.close().await.unwrap();
        let (events, view) = replayed(&state, outcome.session_id).await;
        assert_eq!(view.runs[0], outcome.run, "{case}: closed replay");
        assert_eq!(outcome.run.tools.len(), 4);
        assert!(
            outcome
                .run
                .tools
                .iter()
                .all(|tool| tool.observation.is_some())
        );
        if matches!(case, "cancel reads" | "limit reads") {
            assert_eq!(
                outcome.run.status,
                if case == "cancel reads" {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                }
            );
            assert_eq!(provider.requests.lock().unwrap().len(), 1);
            for tool in &outcome.run.tools {
                let observation = tool.observation.as_ref().unwrap();
                if matches!(tool.intent.call, arany::ToolCall::Mkdir { .. }) {
                    assert!(observation.output.starts_with("not_dispatched:"));
                } else {
                    assert_eq!(
                        observation.disposition,
                        if case == "cancel reads" {
                            arany::ToolDisposition::Cancelled
                        } else {
                            arany::ToolDisposition::Limit
                        }
                    );
                }
            }
            assert!(!workspace.join("notes/created").exists());
            assert!(!workspace.join("notes/final").exists());
        } else {
            assert_eq!(outcome.run.status, RunStatus::Finished);
            let recorded = provider.requests.lock().unwrap();
            let step = &recorded[1].input.tools.as_ref().unwrap().steps[0];
            assert_eq!(
                step.observations
                    .iter()
                    .map(|observation| &observation.intent.call)
                    .collect::<Vec<_>>(),
                calls.iter().collect::<Vec<_>>()
            );
            if count < 4 {
                assert!(workspace.join("notes/created").is_dir());
                assert!(workspace.join("notes/final").is_dir());
                let last_read = events.iter().filter(|event| matches!(&event.event,
                    Event::ToolFinished { observation, .. } if matches!(observation.intent.call, arany::ToolCall::Read { .. }))).map(|event| event.sequence).max().unwrap();
                assert!(events.iter().filter(|event| matches!(&event.event,
                    Event::ToolFinished { observation, .. } if matches!(observation.intent.call, arany::ToolCall::Mkdir { .. }))).all(|event| event.sequence > last_read));
            }
        }
    }
}
