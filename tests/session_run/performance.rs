use super::loopback::{check_profile, read_request, send_response, wait_product, write_profile};
use arany::{
    CollaborationPolicy, Output, RunId, RunStatus, SessionId, SessionView, StateRoot, Store,
    create_session, rename_session, render_session,
};
use std::{
    collections::HashSet,
    net::TcpListener,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
#[path = "performance/memory.rs"]
mod memory;
#[path = "performance/startup.rs"]
mod startup;

const SAMPLES: usize = 1_000;
const WARMUPS: usize = 20;
const TEAM_SAMPLES: usize = 100;
const TEAM_WARMUPS: usize = 10;

fn show_command(workspace: &Path, state: &Path, id: SessionId) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .current_dir(workspace)
        .args(["show", "--output", "text", "--state-dir"])
        .arg(state)
        .arg(id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn measure_show(workspace: &Path, state: &Path, id: SessionId) -> Duration {
    let mut command = show_command(workspace, state, id);
    let start = Instant::now();
    let result = wait_product(command.spawn().expect("release show process"));
    assert!(result.status.success(), "release show succeeded");
    start.elapsed()
}

fn report(label: &str, samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    let count = samples.len();
    let p50 = samples[count / 2 - 1];
    let p95 = samples[count * 95 / 100 - 1];
    let max = samples[count - 1];
    println!(
        "{label}: n={count} p50={}us p95={}us max={}us",
        p50.as_micros(),
        p95.as_micros(),
        max.as_micros()
    );
    p95
}

#[test]
#[ignore = "run only with cargo test --release --test session_run show_replay_latency -- --ignored --nocapture"]
fn show_replay_latency_on_named_host() {
    let test_binary = std::env::current_exe().expect("test binary path");
    assert!(
        test_binary
            .components()
            .any(|part| part.as_os_str() == "release"),
        "release profile required for latency measurement"
    );
    let temp = tempfile::tempdir().expect("private benchmark root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("benchmark Workspace");
    let state = temp.path().join("state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("benchmark runtime");
    let baseline_id = runtime
        .block_on(create_session(
            StateRoot::admit(&state).expect("private state"),
            workspace.clone(),
            Some("Baseline".into()),
        ))
        .expect("one-Event Session");
    let measured_id = runtime
        .block_on(create_session(
            StateRoot::open_existing(&state).expect("existing state"),
            workspace.clone(),
            Some("Measured".into()),
        ))
        .expect("100-Event Session");
    for index in 0..99 {
        runtime
            .block_on(rename_session(
                StateRoot::open_existing(&state).expect("existing state"),
                workspace.clone(),
                measured_id,
                format!("Measured {index}"),
            ))
            .expect("benchmark Event");
    }
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("existing state"))
        .expect("read-only Store");
    let events = runtime
        .block_on(store.load_session(measured_id))
        .expect("100 Events");
    assert_eq!(events.len(), 100);
    let view = SessionView::replay(measured_id, &events)
        .expect("strict replay")
        .expect("measured Session");
    let expected = render_session(&view, &events, Output::Text);
    runtime.block_on(store.close()).expect("close Store");
    let mut first = show_command(&workspace, &state, measured_id);
    first.stdout(Stdio::piped()).stderr(Stdio::piped());
    let first = wait_product(first.spawn().expect("verify shipped show"));
    assert!(
        first.status.success(),
        "shipped show failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first.stdout, expected.as_bytes());
    assert_eq!(first.stderr, b"");

    println!(
        "host_os={} host_arch={} parallelism={} profile=release",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, usize::from)
    );
    for _ in 0..WARMUPS {
        measure_show(&workspace, &state, baseline_id);
        measure_show(&workspace, &state, measured_id);
    }
    let mut baseline = Vec::with_capacity(SAMPLES);
    let mut measured = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        if index % 2 == 0 {
            baseline.push(measure_show(&workspace, &state, baseline_id));
            measured.push(measure_show(&workspace, &state, measured_id));
        } else {
            measured.push(measure_show(&workspace, &state, measured_id));
            baseline.push(measure_show(&workspace, &state, baseline_id));
        }
    }
    report("show_1_event", &mut baseline);
    let measured_p95 = report("show_100_events", &mut measured);
    assert!(
        measured_p95 <= Duration::from_millis(25),
        "100-Event show p95 exceeds named-host 25 ms release gate"
    );
}

#[test]
#[ignore = "run only with cargo test --release --test session_run team_two_process_latency -- --ignored --nocapture"]
fn team_two_process_latency_on_named_host() {
    let test_binary = std::env::current_exe().expect("test binary path");
    assert!(
        test_binary
            .components()
            .any(|part| part.as_os_str() == "release"),
        "release profile required for latency measurement"
    );
    let temp = tempfile::tempdir().expect("private benchmark root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("benchmark Workspace");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    listener
        .set_nonblocking(true)
        .expect("bounded Provider accept");
    write_profile(
        &state,
        listener.local_addr().expect("Provider address").port(),
    );
    let server = thread::spawn(move || {
        for index in 0..3 + (TEAM_WARMUPS + TEAM_SAMPLES) * 4 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "synthetic request missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            };
            let body = read_request(&mut stream);
            let wire: serde_json::Value =
                serde_json::from_slice(&body).expect("synthetic Responses request");
            assert_eq!(wire["model"], "model-1");
            let input: serde_json::Value =
                serde_json::from_str(wire["input"].as_str().expect("semantic input"))
                    .expect("semantic request");
            let text = if index < 3 {
                match index {
                    0 => {
                        serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}})
                    }
                    1 => {
                        serde_json::json!({"outcome":{"type":"delegate","children":["synthetic child"]}})
                    }
                    _ => serde_json::json!({"summary":"synthetic summary"}),
                }
            } else {
                let phase = (index - 3) % 4;
                assert_eq!(
                    input["phase"],
                    ["root_plan", "child_work", "child_work", "root_synthesis"][phase]
                );
                assert!(input["workspace_guidance"].is_null());
                assert_eq!(input["includes"], serde_json::json!([]));
                match phase {
                    0 => {
                        assert_eq!(input["objective"], "Synthetic team question");
                        serde_json::json!({"outcome":{"type":"delegate","children":["A","B"]}})
                    }
                    1 | 2 => {
                        assert_eq!(input["objective"], if phase == 1 { "A" } else { "B" });
                        serde_json::json!({"outcome":{"type":"finish","summary":"child","result":"done"}})
                    }
                    _ => {
                        assert_eq!(input["objective"], "Synthetic team question");
                        assert_eq!(input["child_results"][0]["objective"], "A");
                        assert_eq!(input["child_results"][1]["objective"], "B");
                        serde_json::json!({"outcome":{"type":"finish","summary":"synthesized","result":"team answer"}})
                    }
                }
            };
            send_response(&mut stream, index, text);
        }
    });
    check_profile(&workspace, &state);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("benchmark runtime");
    let observer = Store::open_read_only(StateRoot::open_existing(&state).expect("state"))
        .expect("read-only observer");
    let mut expected_sequence = None;
    let mut session_ids = HashSet::with_capacity(TEAM_WARMUPS + TEAM_SAMPLES);
    let mut samples = Vec::with_capacity(TEAM_SAMPLES);
    for index in 0..TEAM_WARMUPS + TEAM_SAMPLES {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
            .current_dir(&workspace)
            .args(["exec", "--state-dir"])
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .args([
                "--provider",
                "custom:local",
                "--model",
                "model-1",
                "--collaboration",
                "team",
                "--max-active-children",
                "2",
                "Synthetic team question",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let result = wait_product(command.spawn().expect("release team process"));
        let elapsed = started.elapsed();
        if index >= TEAM_WARMUPS {
            samples.push(elapsed);
        }
        assert!(result.status.success(), "team process succeeded");
        assert_eq!(result.stdout, b"Answer:\n  team answer\n");
        let receipt = std::str::from_utf8(&result.stderr).expect("team receipt");
        let lines = receipt.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 4);
        let session_id = lines[0]
            .strip_prefix("Session: ")
            .expect("Session receipt")
            .parse::<SessionId>()
            .expect("Session ID");
        assert!(session_ids.insert(session_id), "fresh Session per Run");
        let run_id = lines[1]
            .strip_prefix("Run: ")
            .expect("Run receipt")
            .parse::<RunId>()
            .expect("Run ID");
        assert_eq!(lines[2], "Status: finished");
        assert_eq!(lines[3], "Provider: custom verified");
        let events = runtime
            .block_on(observer.load_session(session_id))
            .expect("committed Events");
        let view = SessionView::replay(session_id, &events)
            .expect("strict replay")
            .expect("team Session");
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].id, run_id);
        assert_eq!(view.runs[0].objective, "Synthetic team question");
        assert_eq!(
            view.runs[0].config.as_ref().expect("pinned Run").policy,
            CollaborationPolicy::Team {
                max_active_children: 2
            }
        );
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        assert_eq!(view.runs[0].agents.len(), 3);
        assert_eq!(
            view.runs[0].assistant_message.as_deref(),
            Some("team answer")
        );
        let sequence = events
            .iter()
            .map(|event| std::mem::discriminant(&event.event))
            .collect::<Vec<_>>();
        if let Some(expected) = &expected_sequence {
            assert_eq!(&sequence, expected, "semantic Event sequence {index}");
        } else {
            expected_sequence = Some(sequence);
        }
    }
    server.join().expect("synthetic Provider completed");
    runtime.block_on(observer.close()).expect("close observer");
    let p95 = report("team2_process", &mut samples);
    assert!(
        p95 <= Duration::from_millis(100),
        "two-child process p95 exceeds named-host 100 ms release gate"
    );
}
