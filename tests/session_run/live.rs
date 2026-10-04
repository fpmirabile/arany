use arany::{
    AgentPhase, AgentRole, AgentStatus, CollaborationPolicy, Effort, Output, OutputTokenBound,
    ProviderCallDisposition, ProviderWireProvenance, RunStatus, SessionId, SessionView, StateRoot,
    Store, render_exec, resolve_native_effort, validate_native_model_id,
};
use std::{
    fs::File,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const LIVE_DEADLINE: Duration = Duration::from_secs(360);
const MAX_CHANNEL_BYTES: u64 = 128 * 1024;

struct LiveChild(Option<Child>);

impl Drop for LiveChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn selected_credential(name: &str) -> String {
    assert_eq!(
        std::env::var("ARANY_LIVE_CONFORMANCE").ok().as_deref(),
        Some("1"),
        "paid live conformance requires explicit ARANY_LIVE_CONFORMANCE=1"
    );
    std::env::var(name).unwrap_or_else(|_| panic!("selected {name} unavailable"))
}

fn selected_model(profile: &str) -> String {
    let model = std::env::var("ARANY_LIVE_MODEL")
        .expect("paid live conformance requires an explicit ARANY_LIVE_MODEL");
    assert!(
        resolve_native_effort(profile, &model, None).is_ok(),
        "selected live model is not admitted for this Provider"
    );
    model
}

fn wait_live(mut child: LiveChild, stdout: &Path, stderr: &Path) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    let deadline = Instant::now() + LIVE_DEADLINE;
    let status = loop {
        if let Some(status) = child
            .0
            .as_mut()
            .expect("live child")
            .try_wait()
            .expect("live status")
        {
            break status;
        }
        for channel in [stdout, stderr] {
            assert!(
                std::fs::metadata(channel)
                    .expect("live channel metadata")
                    .len()
                    <= MAX_CHANNEL_BYTES,
                "live output exceeded channel bound"
            );
        }
        assert!(
            Instant::now() < deadline,
            "live Provider exceeded parent deadline"
        );
        thread::sleep(Duration::from_millis(20));
    };
    child
        .0
        .take()
        .expect("live child")
        .wait()
        .expect("reap live child");
    let output = std::fs::read(stdout).expect("live stdout");
    let errors = std::fs::read(stderr).expect("live stderr");
    assert!(output.len() as u64 <= MAX_CHANNEL_BYTES);
    assert!(errors.len() as u64 <= MAX_CHANNEL_BYTES);
    (status, output, errors)
}

fn replay_live_jsonl(state: &Path, output: &[u8], objective: &str) -> SessionView {
    let first_line = output
        .split(|byte| *byte == b'\n')
        .next()
        .expect("live JSONL first line");
    let first: serde_json::Value =
        serde_json::from_slice(first_line).expect("live JSONL first Event");
    let session_id = first["session_id"]
        .as_str()
        .expect("Session ID")
        .parse::<SessionId>()
        .expect("typed Session ID");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("live replay runtime");
    let store = Store::open_read_only(StateRoot::open_existing(state).expect("live state"))
        .expect("read-only live Store");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("persisted live Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict live replay")
        .expect("live Session");
    assert_eq!(view.runs.len(), 1);
    let run = &view.runs[0];
    assert_eq!(run.objective, objective);
    assert!(
        output == render_exec(run, &events, Output::Jsonl, true).as_bytes(),
        "live JSONL differs from committed Events"
    );
    runtime.block_on(store.close()).expect("close live Store");
    view
}

fn live_route(profile: &str, model: &str, credential_name: &str, team: bool) {
    let credential = selected_credential(credential_name);
    let temp = tempfile::tempdir().expect("private live root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("empty synthetic Workspace");
    let state = temp.path().join("state");
    let stdout_path = temp.path().join("stdout");
    let stderr_path = temp.path().join("stderr");
    let stdout = File::create(&stdout_path).expect("live stdout file");
    let stderr = File::create(&stderr_path).expect("live stderr file");
    let objective = if team {
        "Complete this synthetic check with exactly one independent child task. Return one short sentence."
    } else {
        "Return one short sentence confirming this synthetic check."
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env(credential_name, credential)
        .current_dir(&workspace)
        .arg("exec")
        .arg("--state-dir")
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", profile, "--model", model])
        .args(["--collaboration", if team { "team" } else { "single" }]);
    if profile == "anthropic" {
        match std::env::var("ANTHROPIC_WORKSPACE_ID") {
            Ok(workspace) => {
                arany::NativeApiCredentials::new(
                    "anthropic",
                    "synthetic-validation-key".into(),
                    Some(workspace.clone()),
                )
                .expect("explicit live Anthropic workspace selector");
                command.env("ANTHROPIC_WORKSPACE_ID", workspace);
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(_) => panic!("invalid live Anthropic workspace selector encoding"),
        }
    }
    if team {
        command.args(["--max-active-children", "1"]);
    }
    command
        .args(["--output", "jsonl", objective])
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let child = LiveChild(Some(command.spawn().expect("live product process")));
    let (status, output, errors) = wait_live(child, &stdout_path, &stderr_path);
    assert!(status.success(), "{profile} team={team} exit: {status}");
    assert!(
        errors.is_empty(),
        "successful JSONL has an empty stderr channel"
    );
    let view = replay_live_jsonl(&state, &output, objective);
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Finished);
    let config = run.config.as_ref().expect("pinned live config");
    assert_eq!(config.provider, profile);
    assert_eq!(config.model, model);
    assert_eq!(
        config.effort,
        Some(resolve_native_effort(profile, model, None).expect("reviewed live effort"))
    );
    assert_eq!(
        config.policy,
        if team {
            CollaborationPolicy::Team {
                max_active_children: 1,
            }
        } else {
            CollaborationPolicy::Single
        }
    );
    assert!(config.custom_profile_provenance.is_none());
    assert!(
        run.assistant_message
            .as_ref()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(run.agents.len(), if team { 2 } else { 1 });
    assert_eq!(run.agents[0].role, AgentRole::Primary);
    assert_eq!(run.agents[0].status, AgentStatus::Finished);
    if team {
        assert_eq!(run.agents[1].role, AgentRole::Child);
        assert_eq!(run.agents[1].ordinal, 1);
        assert_eq!(run.agents[1].status, AgentStatus::Finished);
    }
}

fn selected_chatgpt_model() -> (String, Effort) {
    assert_eq!(
        std::env::var("ARANY_LIVE_CONFORMANCE").ok().as_deref(),
        Some("1"),
        "paid live conformance requires ARANY_LIVE_CONFORMANCE=1"
    );
    assert_eq!(
        std::env::var("ARANY_LIVE_CHATGPT").ok().as_deref(),
        Some("1"),
        "ChatGPT-plan use requires ARANY_LIVE_CHATGPT=1"
    );
    let model = std::env::var("ARANY_LIVE_CHATGPT_MODEL")
        .expect("select an account-visible ARANY_LIVE_CHATGPT_MODEL");
    assert!(
        validate_native_model_id(&model).is_ok(),
        "invalid live model ID"
    );
    let effort = std::env::var("ARANY_LIVE_CHATGPT_EFFORT")
        .expect("select ARANY_LIVE_CHATGPT_EFFORT")
        .parse::<Effort>()
        .expect("invalid live ChatGPT effort");
    (model, effort)
}

fn chatgpt_live_command(workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .current_dir(workspace)
        .stdin(Stdio::null());
    for name in [
        "HOME",
        "USER",
        "LOGNAME",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

fn capture_chatgpt_live(
    command: &mut Command,
    temp: &Path,
    name: &str,
) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    let stdout_path = temp.join(format!("{name}-stdout"));
    let stderr_path = temp.join(format!("{name}-stderr"));
    command
        .stdout(Stdio::from(
            File::create(&stdout_path).expect("live stdout file"),
        ))
        .stderr(Stdio::from(
            File::create(&stderr_path).expect("live stderr file"),
        ));
    let child = LiveChild(Some(command.spawn().expect("live ChatGPT product process")));
    wait_live(child, &stdout_path, &stderr_path)
}

#[test]
#[ignore = "paid live ChatGPT-plan check and direct Run; requires selected account, two opt-ins, model, and effort"]
fn chatgpt_plan_direct_turn_replays_exact_events() {
    let (model, effort) = selected_chatgpt_model();
    let temp = tempfile::tempdir().expect("private live root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("empty synthetic Workspace");
    let state = temp.path().join("state");

    let mut check = chatgpt_live_command(&workspace);
    check
        .args(["provider", "check", "chatgpt"])
        .arg(&model)
        .args(["--effort", effort.as_str(), "--accept-cost"]);
    let (status, output, errors) = capture_chatgpt_live(&mut check, temp.path(), "check");
    assert!(status.success(), "ChatGPT synthetic check exited {status}");
    assert!(errors.is_empty(), "successful check wrote stderr");
    let lines = std::str::from_utf8(&output)
        .expect("check output UTF-8")
        .lines()
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "unexpected check receipt");
    let account_id = lines[1]
        .strip_prefix("Account: ")
        .expect("check account line")
        .parse::<Uuid>()
        .expect("checked account UUID");
    let expected = format!(
        "Provider: chatgpt\nAccount: {account_id}\nModel: {model}\nEffort: {effort}\nStatus: synthetic conformance passed; optional diagnostic, not Run authorization\n"
    );
    assert!(
        output == expected.as_bytes(),
        "check receipt differs from selected tuple"
    );

    let objective = "Return one short sentence confirming this synthetic check.";
    let mut run = chatgpt_live_command(&workspace);
    run.arg("exec")
        .arg("--state-dir")
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "chatgpt", "--model"])
        .arg(&model)
        .args([
            "--effort",
            effort.as_str(),
            "--collaboration",
            "single",
            "--output",
            "jsonl",
            objective,
        ]);
    let (status, output, errors) = capture_chatgpt_live(&mut run, temp.path(), "run");
    assert!(status.success(), "ChatGPT direct Run exited {status}");
    assert!(errors.is_empty(), "successful JSONL wrote stderr");
    let view = replay_live_jsonl(&state, &output, objective);
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Finished);
    assert!(
        run.assistant_message
            .as_ref()
            .is_some_and(|text| !text.is_empty())
    );
    let config = run.config.as_ref().expect("pinned ChatGPT Run config");
    assert_eq!(config.provider, "chatgpt");
    assert_eq!(config.model, model);
    assert_eq!(config.effort, Some(effort));
    assert_eq!(config.policy, CollaborationPolicy::Single);
    assert_eq!(config.provider_concurrency, 1);
    assert_eq!(
        config.output_token_bound,
        OutputTokenBound::LocalAcceptanceOnly
    );
    assert!(config.custom_profile_provenance.is_none());
    assert!(config.saved_api_account_id.is_none());
    let provenance = config
        .chatgpt_provenance
        .as_ref()
        .expect("account-bound ChatGPT evidence");
    assert_eq!(provenance.account_id, account_id);
    assert_eq!(
        provenance.admission,
        arany::ChatGptAdmission::AccountConsent
    );
    assert_ne!(provenance.evidence_fingerprint, [0; 32]);
    assert_eq!(run.agents.len(), 1);
    let primary = &run.agents[0];
    assert_eq!(primary.role, AgentRole::Primary);
    assert_eq!(primary.status, AgentStatus::Finished);
    assert_eq!(primary.provider_calls.len(), 1);
    let call = &primary.provider_calls[0];
    assert_eq!(call.phase, AgentPhase::RootPlan);
    assert_eq!(call.disposition, ProviderCallDisposition::Finished);
    assert_eq!(
        call.wire_provenance,
        Some(ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
    );
    assert!(call.response_id.is_some());
    assert!(call.input_tokens.is_some_and(|count| count > 0));
    assert!(
        call.output_tokens
            .is_some_and(|count| count > 0 && count <= 4096)
    );
}

#[test]
#[ignore = "paid live OpenAI conformance; requires explicit opt-in, model, and selected API key"]
fn native_openai_direct_and_team_replay() {
    let model = selected_model("openai");
    live_route("openai", &model, "OPENAI_API_KEY", false);
    live_route("openai", &model, "OPENAI_API_KEY", true);
}

#[test]
#[ignore = "paid live Anthropic conformance; requires explicit opt-in, model, and selected API key"]
fn native_anthropic_direct_and_team_replay() {
    let model = selected_model("anthropic");
    live_route("anthropic", &model, "ANTHROPIC_API_KEY", false);
    live_route("anthropic", &model, "ANTHROPIC_API_KEY", true);
}
