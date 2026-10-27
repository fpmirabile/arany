use super::*;
use arany::test_support::latency::{
    CORPUS_ROWS, JournalIntervals, LatencyError, MAX_SAMPLES, MEASURED, Operation, Sample, WARMUPS,
    percentiles,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const FIXTURE: &str = "fn main() {}\n";
const PROVISIONAL_P95_US: u64 = 100_000;
const CORPUS_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const REPORT_CAP: u64 = 1024 * 1024;
const SAMPLE_CAP: u64 = 8 * 1024 * 1024;
const EVIDENCE_CAP: u64 = 256 * 1024 * 1024;
const SHOW_BYTES_PER_READ: usize = 64 * 1024;

fn private_file(path: &Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn noop_pin(root: &Path) -> serde_json::Value {
    use std::os::unix::fs::MetadataExt;
    let executable = ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .find_map(|path| std::fs::canonicalize(path).ok())
        .expect("native no-op executable prerequisite");
    let pin = serde_json::json!({
        "name":"noop", "executable":executable, "sha256":executable_digest(&executable),
        "interpreter":false, "inputs":[],
    });
    let metadata = std::fs::metadata(&executable).unwrap();
    private_file(
        &root.join("prerequisites.json"),
        &serde_json::to_vec(
            &serde_json::json!({"pin":pin,"device":metadata.dev(),"inode":metadata.ino()}),
        )
        .unwrap(),
    );
    pin
}

fn fixture_call(operation: Operation, path: &str) -> arany::ToolCall {
    match operation {
        Operation::Read => arany::ToolCall::Read {
            path: path.into(),
            offset: None,
            limit: None,
        },
        Operation::Edit => arany::ToolCall::Edit {
            path: path.into(),
            expected_digest: digest_hex(fixture_content(operation).as_bytes()),
            old: "fn pick".into(),
            new: "fn done".into(),
        },
        Operation::Command => arany::ToolCall::Command {
            program: "noop".into(),
            args: Vec::new(),
            cwd: "src".into(),
        },
        Operation::Other | Operation::InvalidProposal => panic!("unsupported latency operation"),
    }
}

fn fixture_for(
    root: &Path,
    path: &str,
    operation: Operation,
    calls: usize,
) -> (Engine, Arc<ScriptedProvider>, arany::RunRequest) {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(
        &workspace.join("src/main.rs"),
        fixture_content(operation).as_bytes(),
    );
    assert_eq!(
        std::fs::read(workspace.join("src/main.rs")).unwrap().len(),
        FIXTURE.len() * 4096
    );
    let state = root.join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let permissions = if operation == Operation::Command {
        br#"{"permissions":{"defaultMode":"default","allow":["Bash(noop)"]}}"#.as_slice()
    } else {
        br#"{"permissions":{"defaultMode":"acceptEdits"}}"#.as_slice()
    };
    private_file(&state.join("permissions.json"), permissions);
    let commands = if operation == Operation::Command {
        vec![noop_pin(root)]
    } else {
        Vec::new()
    };
    private_file(&state.join("tools.json"), &serde_json::to_vec(&serde_json::json!({"version":1,"workspace_paths":["src"],"write":operation == Operation::Edit,"commands":commands,"skills":[],"mcp":[]})).unwrap());
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            ProviderOutcome::Tools(
                (0..calls)
                    .map(|index| arany::ToolRequest {
                        id: arany::ToolUseId::new(format!("call_{index}")).unwrap(),
                        call: fixture_call(operation, path),
                    })
                    .collect(),
            ),
            arany::test_support::goals::finish_with(if operation == Operation::Command {
                "Command completed"
            } else {
                "Read completed"
            }),
        ])
        .with_known_input_tokens(),
    );
    let mut input = request(
        &workspace,
        if operation == Operation::Command {
            "Run the admitted native no-op command"
        } else {
            "Read the first page of src/main.rs"
        },
    );
    input.include_paths = if operation == Operation::Edit {
        vec!["src/main.rs".into()]
    } else {
        Vec::new()
    };
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: Some(4),
        max_tokens: Some(32_768),
        max_active_ms: Some(120_000),
        max_runs: 1,
    });
    (engine, provider, input)
}

fn fixture_content(operation: Operation) -> String {
    if operation == Operation::Edit {
        format!("{}fn pick() {{}}\n", FIXTURE.repeat(4095))
    } else {
        FIXTURE.repeat(4096)
    }
}

fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn controls() -> RunControls {
    RunControls {
        tools: ToolGrant::Configured,
        ..Default::default()
    }
}

fn assert_primary_run(view: &SessionView, calls: usize) -> &arany::RunView {
    assert_eq!(view.runs.len(), 1);
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.tools.len(), calls);
    assert!(
        run.agents
            .iter()
            .all(|agent| agent.role == arany::AgentRole::Primary)
    );
    assert!(view.compactions.is_empty());
    assert!(run.provider_fallbacks.is_empty());
    assert!(run.provider_retries.is_empty());
    run
}

fn assert_read(view: &SessionView, reads: usize) {
    let run = assert_primary_run(view, reads);
    assert_eq!(run.assistant_message.as_deref(), Some("Read completed"));
    for tool in &run.tools {
        let observation = tool.observation.as_ref().unwrap();
        assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
        assert!(matches!(
            observation.attestation,
            Some(arany::Attestation::InProcess { .. })
        ));
        let page: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
        let digest = digest_hex(FIXTURE.repeat(4096).as_bytes());
        assert_eq!(
            page,
            serde_json::json!({
                "sha256":digest, "total_lines":4096, "start_line":1, "end_line":2000,
                "next_line":2001, "truncated":true, "text":(1..=2000).map(|line| format!("{line}\t{FIXTURE}")).collect::<String>(),
            })
        );
    }
}

fn assert_operation(view: &SessionView, operation: Operation, calls: usize) {
    let run = assert_primary_run(view, calls);
    if operation == Operation::Read {
        assert_read(view, calls);
        assert!(run.tools.iter().all(|tool| tool.checkpoint.is_none()));
        return;
    }
    let tool = &run.tools[0];
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
    assert!(tool.permission_verdict.as_ref().unwrap().approved);
    if operation == Operation::Edit {
        assert!(matches!(&observation.attestation,
            Some(arany::Attestation::InProcess { receipt: Some(receipt), .. })
            if receipt.intent_digest == tool.permission_verdict.as_ref().unwrap().intent_digest
                && receipt.before == Some(Sha256::digest(fixture_content(operation).as_bytes()).into())
                && receipt.after == Some(Sha256::digest(fixture_content(operation).replace("fn pick", "fn done").as_bytes()).into())));
        assert!(tool.checkpoint.is_some());
    } else {
        assert_eq!(operation, Operation::Command);
        assert!(matches!(&observation.attestation,
            Some(arany::Attestation::Guard(receipt))
            if receipt.contract_version == 1
                && receipt.intent_digest == tool.permission_verdict.as_ref().unwrap().intent_digest
                && receipt.enforcement_digest == observation.intent.enforcement_digest
                && receipt.limits == observation.intent.limits));
        let result: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "");
        assert_eq!(result["stderr"], "");
        assert_eq!(result["stdout_total"]["bytes"], 0);
        assert_eq!(result["stderr_total"]["bytes"], 0);
        assert_eq!(result["changeset"]["files"], serde_json::json!([]));
        assert!(tool.checkpoint.is_none());
        assert_eq!(
            tool.permission_verdict.as_ref().unwrap().source,
            arany::permissions::PermissionSource::Rule
        );
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditedSample {
    ordinal: usize,
    warmup: bool,
    sample: Sample,
    journal_interval_ms: JournalIntervals,
    effective_bounds: serde_json::Value,
}

async fn measure(root: &Path, ordinal: usize, operation: Operation, calls: usize) {
    assert!(CORPUS_ROWS.contains(&(operation, calls)));
    let (engine, provider, input) = fixture_for(root, "src/main.rs", operation, calls);
    let recorder = engine_support::record_tool_latency(&engine);
    let result = run_with(&engine, &provider, input, controls()).await;
    private_file(
        &root.join("capture.json"),
        &serde_json::to_vec(&recorder.samples()).unwrap(),
    );
    engine.close().await.unwrap();
    let outcome = result.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert!(provider.responses.lock().unwrap().is_empty());
    assert!(provider.compact_requests.lock().unwrap().is_empty());
    let (events, view) =
        arany::test_support::goals::replayed(&root.join("state"), outcome.session_id).await;
    assert_eq!(view.runs[0], outcome.run);
    assert_operation(&view, operation, calls);
    private_file(
        &root.join("expected-show.jsonl"),
        arany_cli::presentation::render_session(
            &view,
            &events,
            arany_cli::presentation::Output::Jsonl,
        )
        .as_bytes(),
    );
    let samples = recorder.samples().unwrap();
    assert_eq!(samples.len(), 1);
    assert!(!recorder.has_pending());
    assert!(samples[0].successful());
    let audit = samples[0].audit(&events);
    if let Err(error) = &audit {
        private_file(
            &root.join("capture.json"),
            &serde_json::to_vec(&serde_json::json!({"samples":samples,"audit_error":error}))
                .unwrap(),
        );
    }
    let audited = AuditedSample {
        ordinal,
        warmup: ordinal < WARMUPS,
        journal_interval_ms: audit.unwrap(),
        sample: samples[0].clone(),
        effective_bounds: effective_bounds(root, &view),
    };
    assert_eq!(
        std::fs::read(root.join("workspace/src/main.rs")).unwrap(),
        fixture_content(operation)
            .replace("fn pick", "fn done")
            .as_bytes()
    );
    private_file(
        &root.join("sample.json"),
        &serde_json::to_vec(&audited).unwrap(),
    );
}

fn effective_bounds(root: &Path, view: &SessionView) -> serde_json::Value {
    let grant: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("state/tools.json")).unwrap()).unwrap();
    let permissions: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("state/permissions.json")).unwrap())
            .unwrap();
    let executable_identity = load_failed_capture(&root.join("prerequisites.json"));
    assert_eq!(
        executable_identity.is_some(),
        !grant["commands"].as_array().unwrap().is_empty()
    );
    serde_json::json!({"goal_budget":view.goals[0].budget,
        "resource_settings":view.goals[0].resource_settings,
        "tool_limits":view.runs[0].tools[0].intent.limits,"grant":grant,
        "permission_mode":permissions["permissions"]["defaultMode"],
        "checkpoint":view.runs[0].tools[0].checkpoint,"executable_identity":executable_identity})
}

fn audit_show(root: &Path, sample: &Sample, remaining: Duration) {
    let mut command = isolated_command(env!("CARGO_BIN_EXE_arany"), root);
    command
        .args(["show", "--state-dir"])
        .arg(root.join("state"))
        .args([&sample.session_id.to_string(), "--output", "jsonl"]);
    use process::BoundedOutput;
    let show_limit = SHOW_BYTES_PER_READ * sample.requested_tools.max(1);
    let output = command
        .bounded_output_for(remaining.min(Duration::from_secs(10)), show_limit)
        .unwrap();
    private_file(&root.join("show-stdout.jsonl"), &output.stdout);
    private_file(&root.join("show-stderr.txt"), &output.stderr);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let shown: Vec<serde_json::Value> = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let expected_path = root.join("expected-show.jsonl");
    assert!(expected_path.metadata().unwrap().len() <= u64::try_from(show_limit).unwrap());
    let expected_bytes = std::fs::read(expected_path).unwrap();
    let expected: Vec<serde_json::Value> = expected_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(output.stdout, expected_bytes);
    assert_eq!(shown, expected);
}

fn load_failed_capture(path: &Path) -> Option<serde_json::Value> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= REPORT_CAP)
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(REPORT_CAP + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > REPORT_CAP {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

#[test]
fn failed_capture_preserves_partial_files_without_losing_report_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("capture.json");
    assert!(load_failed_capture(&path).is_none());
    for (bytes, expected) in [
        (
            br#"{"Err":"overflow"}"#.as_slice(),
            Some(serde_json::json!({"Err":"overflow"})),
        ),
        (br#"{"Err":"over"#.as_slice(), None),
        (&vec![b' '; REPORT_CAP as usize + 1], None),
    ] {
        private_file(&path, bytes);
        assert_eq!(load_failed_capture(&path), expected);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

fn executable_digest(path: &Path) -> String {
    use std::io::Read;
    let mut file = std::fs::File::open(path).unwrap();
    assert!(file.metadata().unwrap().len() <= 512 * 1024 * 1024);
    let mut digest = Sha256::new();
    let mut bytes = [0; 8192];
    loop {
        let count = file.read(&mut bytes).unwrap();
        if count == 0 {
            break;
        }
        digest.update(&bytes[..count]);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn isolated_command(program: impl AsRef<std::ffi::OsStr>, root: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command
        .current_dir(root)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("user-state"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    command
}

fn bytes_under(root: &Path) -> u64 {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = entry.path().symlink_metadata().unwrap();
            assert!(!metadata.file_type().is_symlink(), "no evidence aliases");
            if metadata.is_dir() {
                bytes_under(&entry.path())
            } else {
                metadata.len()
            }
        })
        .sum()
}

fn require_release_linux() {
    if cfg!(debug_assertions) {
        panic!("invalid latency corpus: attempted=0; release required for Tool latency");
    }
    if !cfg!(target_os = "linux") {
        panic!("invalid latency corpus: attempted=0; Linux required; macOS latency is unverified");
    }
}

#[test]
#[ignore = "private helper for linux_tool_latency_corpus"]
fn linux_tool_latency_sample() {
    require_release_linux();
    let root =
        PathBuf::from(std::env::var_os("ARANY_LATENCY_SAMPLE").expect("private sample root"));
    let ordinal: usize = std::env::var("ARANY_LATENCY_ORDINAL")
        .unwrap()
        .parse()
        .unwrap();
    assert!(root.is_absolute() && ordinal < WARMUPS + MEASURED);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(measure(
            &root,
            ordinal,
            match std::env::var("ARANY_LATENCY_OPERATION").unwrap().as_str() {
                "Read" => Operation::Read,
                "Edit" => Operation::Edit,
                "Command" => Operation::Command,
                _ => panic!("unsupported latency operation"),
            },
            std::env::var("ARANY_LATENCY_CALLS")
                .unwrap()
                .parse()
                .unwrap(),
        ));
}

pub(super) fn corpus() {
    require_release_linux();
    let _gate = process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let deadline = std::time::Instant::now() + CORPUS_TIMEOUT;
    for (operation, calls) in CORPUS_ROWS {
        corpus_row(operation, calls, deadline);
    }
}

fn corpus_row(operation: Operation, calls: usize, deadline: std::time::Instant) {
    let parent = PathBuf::from(
        std::env::var_os("ARANY_LATENCY_ROOT")
            .expect("set ARANY_LATENCY_ROOT to an existing local-disk evidence directory"),
    );
    assert!(parent.is_absolute());
    let metadata_path = parent.join("host.json");
    assert!(metadata_path.metadata().unwrap().len() <= 16 * 1024);
    let host: serde_json::Value =
        serde_json::from_slice(&std::fs::read(metadata_path).unwrap()).unwrap();
    let fields = [
        "date",
        "revision",
        "toolchain",
        "host",
        "cpu",
        "ram",
        "kernel",
        "architecture",
        "filesystem",
        "device",
        "power",
        "load",
        "native_prerequisites",
    ];
    assert_eq!(
        host.as_object().unwrap().len(),
        fields.len(),
        "only reviewed host metadata fields"
    );
    for field in fields {
        assert!(
            host[field]
                .as_str()
                .is_some_and(|value| !value.is_empty() && value.len() <= 1024),
            "missing host metadata: {field}"
        );
    }
    assert!(
        host["revision"]
            .as_str()
            .is_some_and(|revision| revision.len() == 40
                && revision.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "exact revision required"
    );
    let binary_sha256 = executable_digest(Path::new(env!("CARGO_BIN_EXE_arany")));
    let test_binary_sha256 = executable_digest(&std::env::current_exe().unwrap());
    let root = tempfile::Builder::new()
        .prefix(&format!("arany-tool-latency-{operation:?}-{calls}-"))
        .tempdir_in(&parent)
        .unwrap()
        .keep();
    #[cfg(target_os = "linux")]
    {
        let directory = std::fs::File::open(&root).unwrap();
        assert_ne!(
            rustix::fs::fstatfs(&directory).unwrap().f_type,
            0x0102_1994,
            "local disk required; tmpfs is not this workload"
        );
    }
    let mut samples = Vec::with_capacity(WARMUPS + MEASURED);
    let mut failure = None;
    let mut attempted = 0;
    for ordinal in 0..WARMUPS + MEASURED {
        if std::time::Instant::now() >= deadline
            || bytes_under(&root) + SAMPLE_CAP + REPORT_CAP
                > EVIDENCE_CAP / CORPUS_ROWS.len() as u64
        {
            failure = Some("invocation deadline or evidence capacity");
            break;
        }
        attempted += 1;
        let sample_root = root.join(format!("sample-{ordinal:03}"));
        std::fs::create_dir(&sample_root).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sample_root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = isolated_command(std::env::current_exe().unwrap(), &sample_root);
        command
            .args([
                "latency::linux_tool_latency_sample",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("ARANY_LATENCY_SAMPLE", &sample_root)
            .env("ARANY_LATENCY_ORDINAL", ordinal.to_string())
            .env("ARANY_LATENCY_OPERATION", format!("{operation:?}"))
            .env("ARANY_LATENCY_CALLS", calls.to_string());
        use process::BoundedOutput;
        let sample_deadline = deadline.min(std::time::Instant::now() + Duration::from_secs(120));
        let remaining = sample_deadline.saturating_duration_since(std::time::Instant::now());
        let captured = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let output = command.bounded_output_for(remaining, 64 * 1024)?;
            if output.status.success() {
                process::assert_single_test_completed(&output.stdout, "latency sample");
            }
            Ok::<_, std::io::Error>(output)
        }));
        let output = match captured {
            Ok(Ok(output)) => output,
            _ => {
                failure = Some(
                    "sample deadline, capture overflow, launch or exact-selection failure; fixture retained",
                );
                break;
            }
        };
        private_file(&sample_root.join("stdout.txt"), &output.stdout);
        private_file(&sample_root.join("stderr.txt"), &output.stderr);
        if !output.status.success() {
            failure = Some("sample failed; inspect retained capture and process diagnostics");
            break;
        }
        if bytes_under(&sample_root) > SAMPLE_CAP {
            failure = Some("sample storage overflow");
            break;
        }
        let sample: AuditedSample =
            serde_json::from_slice(&std::fs::read(sample_root.join("sample.json")).unwrap())
                .unwrap();
        if sample.ordinal != ordinal
            || sample.warmup != (ordinal < WARMUPS)
            || !sample.sample.successful()
        {
            failure = Some("mismatched or unsuccessful sample");
            break;
        }
        let remaining = sample_deadline.saturating_duration_since(std::time::Instant::now());
        let audited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            audit_show(&sample_root, &sample.sample, remaining);
        }));
        if audited.is_err() {
            failure = Some("closed replay or fresh inspection failed; fixture retained");
            break;
        }
        if bytes_under(&sample_root) > SAMPLE_CAP {
            failure = Some("sample storage overflow after inspection");
            break;
        }
        samples.push(sample);
        if ordinal < WARMUPS {
            std::fs::remove_dir_all(sample_root).unwrap();
        }
    }
    let measured: Vec<_> = samples
        .iter()
        .filter(|sample| !sample.warmup)
        .map(|sample| sample.sample.tool_receipt_to_commit_us.unwrap())
        .collect();
    let valid =
        failure.is_none() && samples.len() == WARMUPS + MEASURED && measured.len() == MEASURED;
    let summary = valid.then(|| percentiles(&measured).unwrap());
    let failed_ordinal = (failure.is_some() && attempted > samples.len()).then(|| attempted - 1);
    let failed_capture = failed_ordinal.and_then(|ordinal| {
        load_failed_capture(&root.join(format!("sample-{ordinal:03}/capture.json")))
    });
    let failed_prerequisites = failed_ordinal.and_then(|ordinal| {
        load_failed_capture(&root.join(format!("sample-{ordinal:03}/prerequisites.json")))
    });
    let objective = match (operation, calls) {
        (Operation::Read | Operation::Edit, 1) => Some(PROVISIONAL_P95_US),
        _ => None,
    };
    let report = serde_json::json!({
        "corpus_version":1, "operation":operation, "calls_per_step":calls, "profile":"release with test-support", "host":host,
        "binary_sha256":binary_sha256,"test_binary_sha256":test_binary_sha256,"package_version":env!("CARGO_PKG_VERSION"),
        "failed_capture":failed_capture,
        "failed_prerequisites":failed_prerequisites,
        "evidence_root":root, "workload":"warm 53248-byte file: Read first 2000 lines, Edit unique final function or guarded native no-op Command, fresh State/Workspace per sample",
        "endpoint":"complete typed Provider receipt to final ToolFinished durable append acknowledgement",
        "excluded":"Provider transport/inference/decoding, startup, final answer, rendering, show; no Guard spawn for Read or Edit",
        "attempted":attempted, "warmups":samples.iter().filter(|sample| sample.warmup).count(), "measured":measured.len(),
        "failed_or_incomplete":attempted - samples.len(), "valid":valid, "failure":failure, "samples":samples,
        "p50_p95_max_us":summary, "provisional_p95_objective_us":objective,
        "objective_met":summary.zip(objective).map(|(values, objective)| values[1] <= objective),
    });
    let bytes = serde_json::to_vec_pretty(&report).unwrap();
    assert!(bytes.len() as u64 <= REPORT_CAP);
    private_file(&root.join("report.json"), &bytes);
    println!("{}", std::str::from_utf8(&bytes).unwrap());
    assert!(
        valid,
        "invalid latency invocation: {}; evidence {}",
        failure.unwrap_or("incomplete counts"),
        root.display()
    );
}

#[test]
fn tool_timing_waits_for_acknowledgement_and_audits_closed_history() {
    let _gate = process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for operation in [Operation::Read, Operation::Edit, Operation::Command] {
                assert_timing_acknowledgement(operation).await;
            }
        });
    assert_eq!(
        percentiles(&(1..=100).rev().collect::<Vec<_>>()).unwrap(),
        [50, 95, 100]
    );
    assert_eq!(percentiles(&[7]).unwrap(), [7, 7, 7]);
    assert_eq!(percentiles(&[]), Err(LatencyError::InvalidSampleCount));
    assert_eq!(
        percentiles(&[0; MAX_SAMPLES + 1]),
        Err(LatencyError::InvalidSampleCount)
    );
}

async fn assert_timing_acknowledgement(operation: Operation) {
    for acknowledge in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let (engine, provider, input) = fixture_for(temp.path(), "src/main.rs", operation, 1);
        let recorder = engine_support::record_tool_latency(&engine);
        let requests = Arc::clone(&provider.requests);
        let (gates_sender, gates_receiver) = tokio::sync::oneshot::channel();
        let gates_sender = Mutex::new(Some(gates_sender));
        let provider = Arc::new(Arc::try_unwrap(provider).ok().unwrap().with_before_call(
            0,
            move || {
                let session = requests.lock().unwrap()[0].session_id;
                let provider_gate =
                    arany::FaultPlan::gate_goal_append_acknowledgment(session, |event| {
                        matches!(event, Event::ProviderCallRecorded { .. })
                    });
                let finish_gate =
                    arany::FaultPlan::gate_goal_append_acknowledgment(session, |event| {
                        matches!(event, Event::ToolFinished { .. })
                    });
                let gates = (provider_gate, finish_gate);
                gates_sender
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(gates)
                    .unwrap();
            },
        ));
        let result = {
            let run = run_with(&engine, &provider, input, controls());
            tokio::pin!(run);
            let held = async {
                let ((provider_entered, provider_release), (entered, release)) =
                    gates_receiver.await.unwrap();
                provider_entered.await.unwrap();
                assert!(
                    recorder.has_pending(),
                    "receipt precedes Provider record acknowledgement and Tool admission"
                );
                assert!(recorder.samples().unwrap().is_empty());
                provider_release.send(()).unwrap();
                entered.await.unwrap();
                assert!(recorder.has_pending());
                assert!(
                    recorder.samples().unwrap().is_empty(),
                    "a committed but unacknowledged finish is incomplete"
                );
                if acknowledge {
                    release.send(()).unwrap();
                } else {
                    drop(release);
                }
            };
            tokio::select! {
                () = held => {},
                result = &mut run => panic!("Run ended before finish acknowledgement gate: {result:?}"),
            }
            run.await
        };
        engine.close().await.unwrap();
        let samples = recorder.samples().unwrap();
        assert_eq!(samples.len(), 1);
        assert!(!recorder.has_pending());
        assert_eq!(samples[0].successful(), acknowledge);
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            if acknowledge { 2 } else { 1 }
        );
        let (events, view) =
            arany::test_support::goals::replayed(&temp.path().join("state"), samples[0].session_id)
                .await;
        if acknowledge {
            assert_eq!(view.runs[0], result.unwrap().run);
            assert_operation(&view, operation, 1);
            samples[0].audit(&events).unwrap();
            assert_acknowledged_history(&samples[0], &events);
        } else {
            assert!(result.is_err());
            assert!(samples[0].finished_sequence.is_none());
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event.event, Event::ToolFinished { .. })),
                "lost acknowledgement can leave a durable finish without a timing endpoint"
            );
        }
    }
}

fn assert_acknowledged_history(sample: &Sample, events: &[arany::EventEnvelope]) {
    for disposition in [
        arany::ToolDisposition::Denied,
        arany::ToolDisposition::Failed,
        arany::ToolDisposition::Conflict,
        arany::ToolDisposition::Limit,
        arany::ToolDisposition::Cancelled,
        arany::ToolDisposition::Uncertain,
    ] {
        let mut failed = sample.clone();
        failed.disposition = Some(disposition);
        assert!(!failed.successful());
        assert!(failed.audit(events).is_err());
    }
    let mut wrong = sample.clone();
    wrong.intent_id = Some(Uuid::now_v7());
    assert_eq!(wrong.audit(events), Err(LatencyError::MismatchedStart));
    let mut shifted = events.to_vec();
    let finish = shifted
        .iter_mut()
        .find(|event| Some(event.sequence) == sample.finished_sequence)
        .unwrap();
    finish.created_at_ms = 0;
    let intervals = sample.audit(&shifted).unwrap();
    assert!(intervals.clock_invalid);
    assert_eq!(intervals.provider_to_finish_ms, None);
    assert!(
        sample.successful(),
        "wall-clock invalidity never alters the monotonic sample"
    );
    let mut missing = events.to_vec();
    missing.retain(|event| Some(event.sequence) != sample.finished_sequence);
    assert_eq!(sample.audit(&missing), Err(LatencyError::MissingFinish));
}

#[test]
fn refused_and_failed_tools_never_enter_the_success_distribution() {
    let _gate = process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for operation in [Operation::Read, Operation::Edit, Operation::Command] {
                assert_failed_timing(operation).await;
            }
        });
}

async fn assert_failed_timing(operation: Operation) {
    for (path, fail_append, cancel, invalid) in [
        ("outside/main.rs", false, false, false),
        ("src/missing.rs", false, false, false),
        ("src/main.rs", true, false, false),
        ("src/main.rs", false, true, false),
        ("src/main.rs", false, false, true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        if operation == Operation::Command && path == "src/missing.rs" {
            continue;
        }
        let (engine, provider, input) = failing_fixture(temp.path(), path, operation, invalid);
        let recorder = engine_support::record_tool_latency(&engine);
        let requests = Arc::clone(&provider.requests);
        let cancellation = RunCancellation::new();
        let trigger = cancellation.clone();
        let provider = Arc::new(Arc::try_unwrap(provider).ok().unwrap().with_before_call(
            0,
            move || {
                let session = requests.lock().unwrap()[0].session_id;
                if fail_append {
                    arany::FaultPlan::fail_goal_append(session, |event| {
                        matches!(event, Event::ToolFinished { .. })
                    });
                }
                if cancel {
                    trigger.cancel();
                }
            },
        ));
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                cancellation,
                ..controls()
            },
        )
        .await;
        engine.close().await.unwrap();
        let samples = recorder.samples().unwrap();
        assert!(!recorder.has_pending());
        assert_eq!(samples.len(), 1);
        assert!(!samples[0].successful());
        let (events, view) =
            arany::test_support::goals::replayed(&temp.path().join("state"), samples[0].session_id)
                .await;
        if invalid {
            assert_rejected_proposal(&samples[0], &events, &view);
        } else if fail_append || cancel {
            assert!(samples[0].finished_sequence.is_none());
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event.event, Event::ToolFinished { .. }))
            );
        } else {
            assert!(result.is_ok());
            let expected = if path.starts_with("outside/") {
                arany::ToolDisposition::Denied
            } else if operation == Operation::Edit {
                arany::ToolDisposition::Conflict
            } else {
                arany::ToolDisposition::Failed
            };
            assert_failed_observation(&samples[0], &events, expected);
            samples[0].audit(&events).unwrap();
        }
    }
}

fn failing_fixture(
    root: &Path,
    path: &str,
    operation: Operation,
    invalid: bool,
) -> (Engine, Arc<ScriptedProvider>, arany::RunRequest) {
    let (engine, provider, input) = fixture_for(root, path, operation, 1);
    if operation == Operation::Command && path.starts_with("outside/") {
        private_file(
            &root.join("state/permissions.json"),
            br#"{"permissions":{"defaultMode":"default","deny":["Bash(noop)"]}}"#,
        );
    }
    if operation == Operation::Edit && path == "src/missing.rs" {
        let mut responses = provider.responses.lock().unwrap();
        let Ok(ProviderOutcome::Tools(requests)) = responses.front_mut().unwrap() else {
            panic!("Edit proposal");
        };
        let arany::ToolCall::Edit {
            path,
            expected_digest,
            ..
        } = &mut requests[0].call
        else {
            unreachable!();
        };
        *path = "src/main.rs".into();
        *expected_digest = "0".repeat(64);
    }
    if invalid {
        let mut responses = provider.responses.lock().unwrap();
        let Ok(ProviderOutcome::Tools(requests)) = responses.front_mut().unwrap() else {
            panic!("Tool proposal");
        };
        requests.push(requests[0].clone());
    }
    (engine, provider, input)
}

fn assert_failed_observation(
    sample: &Sample,
    events: &[arany::EventEnvelope],
    expected: arany::ToolDisposition,
) {
    assert_eq!(sample.disposition, Some(expected));
    let observation = events
        .iter()
        .find_map(|event| match &event.event {
            Event::ToolFinished { observation, .. } => Some(observation),
            _ => None,
        })
        .unwrap();
    if expected == arany::ToolDisposition::Denied {
        assert!(
            observation.attestation.is_none(),
            "denied authority never dispatches"
        );
    } else {
        assert!(matches!(
            observation.attestation,
            Some(arany::Attestation::InProcess { .. })
        ));
    }
}

fn assert_rejected_proposal(sample: &Sample, events: &[arany::EventEnvelope], view: &SessionView) {
    assert_eq!(
        sample.provider_disposition,
        Some(ProviderCallDisposition::Rejected)
    );
    assert_eq!(
        view.runs[0].agents[0].provider_calls[0]
            .refusal
            .as_ref()
            .unwrap()
            .cause,
        arany::ProposalRefusalCause::InvalidArguments
    );
    assert!(sample.started_sequence.is_none());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::ToolStarted { .. }))
    );
    sample.audit(events).unwrap();
}
