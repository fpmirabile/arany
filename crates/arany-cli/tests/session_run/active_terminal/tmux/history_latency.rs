use super::{TmuxServer, capture_pane, decline_initial_consent, redacted_tail};
use crate::loopback::{
    check_profile_with_binary, read_request, send_response, send_response_for, wait_product,
    write_profile,
};
use arany::WorkspaceIdentity;
use arany::{
    AgentDisposition, AgentPhase, AgentRole, AgentRunId, CollaborationPolicy, Event, GoalBudget,
    GoalId, GoalPhase, GoalReport, GoalStatus, ProviderCallDisposition, ProviderCallRecord,
    RunConfig, RunDisposition, RunId, RunStatus, SessionId, StateRoot, Store, resume_session,
};
use rustix::process::Pid;
use std::{
    io::Read,
    net::{TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::Stdio,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const HISTORY_RUNS: usize = 800;
const GOAL_RUNS: usize = 16;
const HISTORY_GOALS: usize = HISTORY_RUNS / GOAL_RUNS;
const EVENTS_PER_RUN: usize = 9;
const EXPECTED_HISTORY_EVENTS: usize = 1 + HISTORY_RUNS * EVENTS_PER_RUN + HISTORY_GOALS * 2;
const MAX_LINEAGE_EVENTS: usize = 10_000;
const MIN_EVENT_HEADROOM: usize = 1_000;
const REPLAY_SAMPLES: usize = 100;
const PREPARE_HISTORY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REQUEST_LIMIT: usize = 1024 * 1024;

fn resumed_tail_layout(screen: &[u8]) -> bool {
    let Ok(screen) = std::str::from_utf8(screen) else {
        return false;
    };
    let rows: Vec<_> = screen.lines().collect();
    rows.iter().all(|row| row.chars().count() <= 80)
        && rows
            .get(rows.len().saturating_sub(4))
            .is_some_and(|row| row.starts_with("─ Ask Arany "))
        && rows
            .get(rows.len().saturating_sub(2))
            .is_some_and(|row| row.contains("Enter sends") && row.contains("Ctrl+O newline"))
        && rows.last().is_some_and(|row| {
            row.trim().starts_with("ready · custom:local/model-1")
                && row.trim_end().ends_with("More history · Ctrl+T history")
        })
}

fn process_memory_kib(pid: Pid) -> (usize, usize) {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid.as_raw_pid()))
        .expect("product process memory");
    let field = |name| {
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix(name)
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .expect("Linux process memory field")
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn wrapper_exit_status(path: &Path) -> String {
    let marker = match std::fs::read_to_string(path) {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return "wrapper has not exited".into();
        }
        Err(_) => return "wrapper exit marker unavailable".into(),
    };
    let mut lines = marker.lines();
    let exit = lines
        .next()
        .and_then(|line| line.parse::<i32>().ok())
        .map_or_else(|| "invalid".to_owned(), |code| code.to_string());
    let before = lines.next().unwrap_or_default();
    let after = lines.next().unwrap_or_default();
    let terminal_restored = !before.is_empty() && before == after;
    format!("wrapper exit={exit}; terminal_restored={terminal_restored}")
}

fn report_replay_samples(samples: &mut [Duration]) {
    samples.sort_unstable();
    let count = samples.len();
    assert_eq!(count, REPLAY_SAMPLES, "bounded replay sample count");
    println!(
        "history_fresh_store_replay samples={count} percentile=nearest_rank p50_us={} p95_us={} max_us={}",
        samples[(count - 1) / 2].as_micros(),
        samples[count * 95 / 100 - 1].as_micros(),
        samples[count - 1].as_micros()
    );
}

fn print_linux_host_metadata() {
    let safe = |value: &str| {
        value
            .chars()
            .filter(|character| {
                character.is_ascii_alphanumeric() || " .,:+=-_/%()".contains(*character)
            })
            .take(128)
            .collect::<String>()
    };
    let mut cpu_info = String::new();
    std::fs::File::open("/proc/cpuinfo")
        .expect("Linux CPU metadata")
        .take(64 * 1024)
        .read_to_string(&mut cpu_info)
        .expect("bounded CPU metadata");
    let cpu = cpu_info
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim() == "model name").then_some(value.trim())
        })
        .unwrap_or("unavailable");
    let kernel =
        std::fs::read_to_string("/proc/sys/kernel/osrelease").expect("Linux kernel version");
    let load = std::fs::read_to_string("/proc/loadavg").expect("Linux load average");
    let cpu_pressure = std::fs::read_to_string("/proc/pressure/cpu")
        .ok()
        .and_then(|value| value.lines().next().map(str::to_owned))
        .unwrap_or_else(|| "unavailable".into());
    println!(
        "history_host os=linux arch={} kernel={} cpu={} parallelism={} load_avg={} cpu_pressure={}",
        std::env::consts::ARCH,
        safe(kernel.trim()),
        safe(cpu),
        std::thread::available_parallelism().map_or(0, usize::from),
        safe(
            &load
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(",")
        ),
        safe(&cpu_pressure)
    );
}

async fn seed_history(state: &std::path::Path, workspace: &std::path::Path) -> (SessionId, usize) {
    let metadata = std::fs::metadata(workspace).expect("Workspace identity");
    let session_id = SessionId::new();
    let store = Store::open(StateRoot::admit(state).expect("private State")).expect("Store");
    store
        .append(
            session_id,
            Event::SessionStarted {
                title: "Near-cap fixture".into(),
                workspace_identity: WorkspaceIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
            },
        )
        .await
        .expect("Session start");
    let objective_body = "0123456789abcdef".repeat(511);
    let answer_body = "0123456789abcdef".repeat(2047);
    let mut raw_text_bytes = 0;
    for goal_index in 0..HISTORY_GOALS {
        let goal_id = GoalId::new();
        let first_run = goal_index * GOAL_RUNS;
        let goal_objective = format!("objective {first_run:04} {objective_body}");
        store
            .append(
                session_id,
                Event::GoalStarted {
                    goal_id,
                    objective: goal_objective.clone(),
                    workspace_identity: WorkspaceIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    },
                    budget: GoalBudget {
                        max_provider_calls: Some(GOAL_RUNS as u32),
                        max_runs: GOAL_RUNS as u8,
                        ..GoalBudget::default()
                    },
                    phase: GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: arany::ToolResourceSettings::default(),
                    continues: None,
                },
            )
            .await
            .expect("Goal start");
        for index in first_run..first_run + GOAL_RUNS {
            let run_id = RunId::new();
            let agent_run_id = AgentRunId::new();
            let objective = if index == first_run {
                goal_objective.clone()
            } else {
                format!("objective {index:04} {objective_body}")
            };
            let answer = format!("answer {index:04} {answer_body}");
            assert_eq!(objective.len(), 8_191, "bounded objective text");
            assert_eq!(answer.len(), 32_764, "bounded answer text");
            raw_text_bytes += objective.len() + answer.len();
            for event in [
                Event::GoalRunLinked {
                    goal_id,
                    run_id,
                    phase: GoalPhase::Implementation,
                },
                Event::MessageAccepted {
                    run_id,
                    text: objective,
                    images: Vec::new(),
                },
                Event::RunStarted {
                    run_id,
                    config: RunConfig {
                        agent_definitions: None,
                        roster: None,
                        providers: None,
                        provider: "scripted".into(),
                        model: "test-model".into(),
                        effort: None,
                        custom_profile_provenance: None,
                        saved_api_account_id: None,
                        chatgpt_provenance: None,
                        output_token_bound: arany::OutputTokenBound::ProviderEnforced,
                        policy: CollaborationPolicy::Single,
                        output_token_cap: 4096,
                        provider_concurrency: 1,
                        workspace_device: metadata.dev(),
                        workspace_inode: metadata.ino(),
                        instruction_digest: None,
                        include_digests: Vec::new(),
                        history_run_ids: Vec::new(),
                        excluded_history_runs: 0,
                        context_usage: None,
                        compaction_event_sequence: None,
                        compaction_content_digest: None,
                        tool_policy: None,
                    },
                    active_deadline_ms: Some(arany::MAX_GOAL_ACTIVE_MS),
                },
                Event::AgentSpawned {
                    profile: None,
                    definition: None,
                    run_id,
                    agent_run_id,
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                    grant: None,
                },
                Event::GoalProviderCallReserved {
                    goal_id,
                    run_id,
                    agent_run_id,
                    active_ceiling_ms: 0,
                },
                Event::ProviderCallRecorded {
                    run_id,
                    agent_run_id,
                    record: ProviderCallRecord {
                        phase: AgentPhase::RootPlan,
                        disposition: ProviderCallDisposition::Finished,
                        response_id: None,
                        input_tokens: None,
                        output_tokens: None,
                        wire_provenance: None,
                        failure_reason: None,
                        requested_tools: 0,
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                    },
                },
                Event::AgentFinished {
                    run_id,
                    agent_run_id,
                    disposition: AgentDisposition::Finished,
                    summary: Some("done".into()),
                    result: Some(answer.clone()),
                },
                Event::MessageCommitted {
                    run_id,
                    text: answer,
                },
                Event::RunFinished {
                    run_id,
                    disposition: RunDisposition::Finished,
                    active_ms: 0,
                },
            ] {
                store.append(session_id, event).await.expect("valid Event");
            }
        }
        store
            .append(
                session_id,
                Event::GoalFinished {
                    goal_id,
                    status: GoalStatus::Completed,
                    active_ms: 0,
                    report: GoalReport {
                        summary: "fixture completed".into(),
                        changes: Vec::new(),
                        evidence: Vec::new(),
                        unresolved: Vec::new(),
                        cached_input_tokens: 0,
                    },
                    exhausted: false,
                },
            )
            .await
            .expect("Goal finish");
    }
    store.close().await.expect("closed Store");
    (session_id, raw_text_bytes)
}

fn read_bounded_request(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("request timeout");
    let mut request = Vec::new();
    let mut expected = None;
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("request bytes");
        assert!(count > 0 && request.len() + count <= REQUEST_LIMIT);
        request.extend_from_slice(&chunk[..count]);
        if expected.is_none()
            && let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n")
        {
            let header_end = end + 4;
            let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
            assert!(header.starts_with("POST /v1/responses HTTP/1.1\r\n"));
            let body_len = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("Content-Length");
            assert!(header_end + body_len <= REQUEST_LIMIT);
            expected = Some(header_end + body_len);
        }
        if expected.is_some_and(|length| request.len() >= length) {
            assert!(
                request
                    .windows(b"latency-probe".len())
                    .any(|part| part == b"latency-probe")
            );
            return;
        }
    }
}

#[test]
#[ignore = "native Linux near-cap active keyboard measurement; requires /usr/bin/tmux"]
fn active_keyboard_stays_visible_with_near_cap_history() {
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("history fixture runtime");
    runtime.block_on(run_history_fixture());
}

async fn run_history_fixture() {
    let executable = super::test_product_executable();
    assert!(
        std::env::current_exe()
            .expect("history test executable")
            .components()
            .any(|component| component.as_os_str() == "release"),
        "named-host history measurement requires a release test binary"
    );
    print_linux_host_metadata();
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let exit_path = temp.path().join("exit");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    write_profile(
        &state,
        listener.local_addr().expect("Provider address").port(),
    );
    let (request_sender, request_ready) = mpsc::channel();
    let (release_sender, release_ready) = mpsc::channel();
    let (runtime_ready_sender, runtime_ready_receiver) = mpsc::channel();
    let provider = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..5 {
            if index == 3 {
                runtime_ready_receiver
                    .recv_timeout(PREPARE_HISTORY_TIMEOUT)
                    .expect("bounded history preparation before runtime requests");
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "synthetic request missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("synthetic accept: {error}"),
                }
            };
            if index < 3 {
                read_request(&mut stream);
                let text = match index {
                    0 => {
                        serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}})
                    }
                    1 => {
                        serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"child","profile":null}]}})
                    }
                    _ => serde_json::json!({"summary":"summary"}),
                };
                send_response(&mut stream, index, text);
            } else if index == 3 {
                read_bounded_request(&mut stream);
                request_sender.send(()).expect("active Run request");
                release_ready
                    .recv_timeout(Duration::from_secs(60))
                    .expect("release primary response");
                send_response_for(
                    &mut stream,
                    index,
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"child","profile":null}]}}),
                    true,
                );
            } else {
                read_request(&mut stream);
                request_sender.send(()).expect("child Run request");
                release_ready
                    .recv_timeout(Duration::from_secs(60))
                    .expect("release child response");
            }
        }
    });
    check_profile_with_binary(&workspace, &state, &executable);
    let (session_id, run_text_bytes) = seed_history(&state, &workspace).await;
    assert_eq!(
        run_text_bytes, 32_764_000,
        "declared objective/answer bytes"
    );
    let event_headroom = MAX_LINEAGE_EVENTS
        .checked_sub(EXPECTED_HISTORY_EVENTS)
        .expect("seed below lineage Event cap");
    assert!(
        event_headroom >= MIN_EVENT_HEADROOM,
        "fixture leaves declared lineage Event headroom"
    );
    let database = state.join("events.sqlite3");
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .expect("read-only fixture database");
    let (event_count, payload_bytes): (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(CAST(payload AS BLOB))), 0) \
             FROM events WHERE session_id = ?1",
            [session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("serialized fixture payload size");
    assert_eq!(
        usize::try_from(event_count).expect("nonnegative Event count"),
        EXPECTED_HISTORY_EVENTS,
        "seed Event count"
    );
    assert!(
        payload_bytes > i64::try_from(run_text_bytes).expect("fixture text byte bound"),
        "serialized payload includes the declared objective and answer text"
    );
    let database_bytes = std::fs::metadata(&database)
        .expect("fixture database metadata")
        .len();
    assert!(
        database_bytes >= u64::try_from(payload_bytes).expect("nonnegative payload bytes"),
        "SQLite file includes its serialized Event payloads"
    );
    drop(connection);

    let mut replay_samples = Vec::with_capacity(REPLAY_SAMPLES);
    for _ in 0..REPLAY_SAMPLES {
        let store = Store::open_read_only(
            StateRoot::open_existing(&state).expect("fresh read-only StateRoot"),
        )
        .expect("fresh read-only Store");
        let started = Instant::now();
        let view = store
            .load_view(session_id)
            .await
            .expect("fresh Store replay")
            .expect("seed Session");
        replay_samples.push(started.elapsed());
        assert_eq!(
            view.last_sequence,
            u64::try_from(EXPECTED_HISTORY_EVENTS).expect("fixture sequence bound")
        );
        assert_eq!(view.runs.len(), HISTORY_RUNS);
        assert_eq!(view.goals.len(), HISTORY_GOALS);
        assert!(
            view.runs
                .iter()
                .all(|run| run.status == RunStatus::Finished)
        );
        assert!(
            view.goals
                .iter()
                .all(|goal| goal.status == GoalStatus::Completed)
        );
        store.close().await.expect("close fresh read-only Store");
    }
    report_replay_samples(&mut replay_samples);
    println!(
        "history_fixture runs={HISTORY_RUNS} goals={HISTORY_GOALS} seed_events={event_count} lineage_event_headroom={event_headroom} min_event_headroom={MIN_EVENT_HEADROOM} run_objective_answer_bytes={run_text_bytes} event_payload_bytes={payload_bytes} sqlite_bytes={database_bytes}"
    );

    let wrapper = temp.path().join("tmux-pane.sh");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nbefore=$(stty -g)\n\"$ARANY_TEST_EXE\" --classic --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider custom:local --model model-1 --no-color\nresult=$?\nafter=$(stty -g)\nprintf '%s\\n%s\\n%s\\n' \"$result\" \"$before\" \"$after\" > \"$ARANY_TEST_EXIT\"\nexit \"$result\"\n",
    )
    .expect("tmux pane wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))
        .expect("private executable wrapper");
    let server = TmuxServer {
        socket: temp.path().join("tmux.sock"),
    };
    let mut start =
        crate::isolation::account_isolated_command(temp.path(), "/usr/bin/tmux", &executable);
    start
        .env_clear()
        .env("TERM", "xterm")
        .env("SHELL", "/bin/sh")
        .env("PATH", "/usr/bin:/bin")
        .env("ARANY_TEST_EXE", &executable)
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", session_id.to_string())
        .env("ARANY_TEST_EXIT", &exit_path)
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .arg("-S")
        .arg(&server.socket)
        .args([
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-x",
            "80",
            "-y",
            "24",
            "-s",
            "arany-test",
        ])
        .arg(&wrapper)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let resume_start = Instant::now();
    assert!(
        wait_product(start.spawn().expect("tmux server start"))
            .status
            .success()
    );
    let mut declined = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(screen) = capture_pane(&server) {
            decline_initial_consent(&server, &screen, &mut declined);
            if resumed_tail_layout(&screen) {
                assert!(
                    !exit_path.exists(),
                    "ready composer frame remained after product exit; {}",
                    wrapper_exit_status(&exit_path)
                );
                break;
            }
        }
        assert!(
            Instant::now() < deadline && !exit_path.exists(),
            "near-cap resumed composer missing: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    let resume_to_ready = resume_start.elapsed();
    println!(
        "history_process_resume_ready_ms={}",
        resume_to_ready.as_millis()
    );
    let pane_pid = server.run(&[
        "display-message",
        "-p",
        "-t",
        "arany-test:0.0",
        "#{pane_pid}",
    ]);
    assert!(pane_pid.status.success(), "test-owned pane PID");
    let pane_pid = String::from_utf8(pane_pid.stdout)
        .expect("pane PID UTF-8")
        .trim()
        .parse::<u32>()
        .expect("pane PID");
    let product_pid =
        super::try_product_child_of_executable(pane_pid, &executable).unwrap_or_else(|| {
            panic!(
                "active product process not found under pane; {}",
                wrapper_exit_status(&exit_path)
            )
        });
    let product_guard = super::ProductGuard::for_executable(product_pid, state.clone(), executable);
    assert!(
        product_guard.is_owned_and_running(),
        "composer belongs to the live product process; {}",
        wrapper_exit_status(&exit_path)
    );
    let screen = capture_pane(&server).expect("resumed Classic pane");
    assert!(
        String::from_utf8_lossy(&screen).contains("More history · Ctrl+T history"),
        "bounded Classic projection advertises full history access"
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-f"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server)
            .is_some_and(|screen| String::from_utf8_lossy(&screen).contains("Find:"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "full-history find did not open");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "objective 0000",])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server)
            .is_some_and(|screen| String::from_utf8_lossy(&screen).contains("objective 0000"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "full-history find missed omitted Run"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Escape"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| resumed_tail_layout(&screen)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Classic composer did not return from find"
        );
        thread::yield_now();
    }
    assert!(
        product_guard.is_owned_and_running(),
        "same product process remains live after closing History; {}",
        wrapper_exit_status(&exit_path)
    );
    let native_history = server.run(&["capture-pane", "-p", "-S", "-", "-t", "arany-test:0.0"]);
    assert!(native_history.status.success(), "native scrollback capture");
    assert!(
        native_history.stdout.len() <= 64 * 1024,
        "bounded projected scrollback"
    );
    let native_history = String::from_utf8_lossy(&native_history.stdout);
    assert!(
        native_history.contains("objective 0799"),
        "recent Run is projected"
    );
    assert!(
        native_history.contains("answer 0799"),
        "recent answer is projected"
    );
    assert!(
        !native_history.contains("objective 0000"),
        "old prefix is omitted"
    );
    let (idle_rss_kib, idle_hwm_kib) = process_memory_kib(product_pid);
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "latency-probe"])
            .status
            .success()
    );
    runtime_ready_sender
        .send(())
        .expect("arm runtime Provider request deadline");
    let submit_start = Instant::now();
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success()
    );
    request_ready
        .recv_timeout(Duration::from_secs(30))
        .expect("active Provider request");
    let admission = submit_start.elapsed();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server)
            .is_some_and(|screen| String::from_utf8_lossy(&screen).contains("working"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "active Run not visible");
        thread::yield_now();
    }

    let mut samples = Vec::new();
    let draft = "QWERTYUIOP";
    for (index, character) in draft.char_indices() {
        if index == 5 {
            assert!(
                server
                    .run(&["send-keys", "-t", "arany-test:0.0", "PageUp"])
                    .status
                    .success()
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if capture_pane(&server).is_some_and(|screen| {
                    String::from_utf8_lossy(&screen).contains("New text · Ctrl+L live")
                }) {
                    break;
                }
                assert!(Instant::now() < deadline, "history navigation not visible");
                thread::yield_now();
            }
            release_sender.send(()).expect("release primary delegation");
            request_ready
                .recv_timeout(Duration::from_secs(20))
                .expect("child Provider request");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if capture_pane(&server).is_some_and(|screen| {
                    let text = String::from_utf8_lossy(&screen);
                    text.contains("New text · Ctrl+L live")
                        && text.contains("QWERT")
                        && text.contains("child 1 · active")
                }) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "delegation lost draft or history mode"
                );
                thread::yield_now();
            }
        }
        let start = Instant::now();
        assert!(
            server
                .run(&[
                    "send-keys",
                    "-l",
                    "-t",
                    "arany-test:0.0",
                    &character.to_string()
                ])
                .status
                .success()
        );
        let expected = &draft[..index + character.len_utf8()];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if capture_pane(&server)
                .is_some_and(|screen| String::from_utf8_lossy(&screen).contains(expected))
            {
                samples.push(start.elapsed());
                break;
            }
            assert!(Instant::now() < deadline, "active draft key not visible");
            thread::yield_now();
        }
    }
    samples.sort();
    println!(
        "history_active_tmux runs={HISTORY_RUNS} run_objective_answer_bytes={run_text_bytes} admission={admission:?} key_samples={} key_p50={:?} key_max={:?}",
        samples.len(),
        samples[(samples.len() - 1) / 2],
        samples[samples.len() - 1]
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-l"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains(draft) && !text.contains("History")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "live tail did not retain draft: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-c"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains(draft) && text.contains("Draft retained:") && !text.contains("History")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancelled Run did not retain draft: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    release_sender
        .send(())
        .expect("release synthetic connection");
    provider.join().expect("synthetic Provider server");
    let (final_rss_kib, final_hwm_kib) = process_memory_kib(product_pid);
    println!(
        "history_process_memory idle_rss_kib={idle_rss_kib} idle_hwm_kib={idle_hwm_kib} final_rss_kib={final_rss_kib} final_hwm_kib={final_hwm_kib}"
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-c"])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "/exit", "Enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(marker) = std::fs::read_to_string(&exit_path) {
            let lines: Vec<_> = marker.lines().collect();
            if lines.len() == 3 {
                assert_eq!(lines[0], "0", "product exit success");
                assert_eq!(lines[1], lines[2], "terminal settings restored");
                break;
            }
        }
        assert!(Instant::now() < deadline, "product did not exit");
        thread::yield_now();
    }
    let view = resume_session(
        StateRoot::open_existing(&state).expect("read-only State"),
        workspace,
        session_id,
    )
    .await
    .expect("strict Session replay");
    assert_eq!(view.runs.len(), HISTORY_RUNS + 1, "draft did not submit");
    let last = view.runs.last().expect("active Run");
    assert_eq!(last.objective, "latency-probe");
    assert_eq!(last.status, RunStatus::Cancelled);
    assert_eq!(last.agents.len(), 2, "committed child delegation");
    assert!(last.assistant_message.is_none());
    assert!(!product_guard.is_owned_and_running(), "product exited");
}
