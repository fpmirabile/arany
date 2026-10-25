use super::checks::{Presentation, acknowledge_result, assert_result_contains, inline_frame};
use super::*;
use arany::{AgentPhase, AgentRole, Event, ProviderSetupDisposition, ProviderSetupTargetKind};
use serde_json::json;
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const ROUTE_CHECK_CALLS: usize = 3;
const ROUTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const MAX_WARNING_PAGES: usize = 128;

#[test]
fn role_manager_saves_ordered_chains_reopens_inheritance_and_clears_in_every_presentation() {
    for (mode, presentation) in [
        (SetupPtyMode::Manager, Presentation::ScreenReader),
        (SetupPtyMode::InlineManager, Presentation::Inline),
        (SetupPtyMode::FullscreenManager, Presentation::Inline),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut pty = start_setup_pty_mode(&temp, &workspace, &state, mode);
        open_roles(&mut pty, presentation);
        let from = presentation.choose(&mut pty, 3);
        presentation.menu(&mut pty, from, "Edit role chain");
        add_native(&mut pty, presentation, 2, 3, "claude-sonnet-5");
        add_native(&mut pty, presentation, 3, 2, "gpt-5.4");
        add_native(&mut pty, presentation, 4, 3, "claude-opus-5-5");
        let from = presentation.choose(&mut pty, 4);
        presentation.menu(&mut pty, from, "Route action");
        let from = presentation.choose(&mut pty, 3);
        presentation.menu(&mut pty, from, "Edit role chain");
        let from = presentation.choose(&mut pty, 5);
        let (from, result) = acknowledge_result(&mut pty, presentation, from);
        assert_result_contains(&result, "Draft updated");
        presentation.menu(&mut pty, from, "Role routing");
        let from = presentation.choose(&mut pty, 6);
        confirm(&mut pty, presentation, from, "Save role routing?", true);
        let from = pty.transcript.len();
        let (_, result) = acknowledge_result(&mut pty, presentation, from);
        assert_result_contains(&result, "Saved; next submission re-admits every route");
        let (output, transcript) = finish_setup_pty_with_transcript(pty);
        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&transcript).contains(FIRST_ROUTE_KEY_CANARY));
        let saved = std::fs::read(state.join("provider-routing.json")).unwrap();
        let record: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(
            record,
            json!({"version":2,"providers":{
                "primary":null,"child":[
                    {"provider":{"kind":"anthropic","custom_name":null},"model":"claude-sonnet-5","effort":"high","account_id":null},
                    {"provider":{"kind":"anthropic","custom_name":null},"model":"claude-opus-5-5","effort":"medium","account_id":null},
                    {"provider":{"kind":"openai","custom_name":null},"model":"gpt-5.4","effort":"none","account_id":null}
                ],"review":null,"compaction":null
            }})
        );
        assert_edits(&state, &workspace, &[ProviderSetupDisposition::Applied]);
        refuse_unused_fallback(&temp, &workspace, &state);
        reopen_and_clear(&temp, &workspace, &state, presentation, &saved);
    }
}

fn open_roles(pty: &mut SetupPty, presentation: Presentation) {
    let from = presentation.choose(pty, 3);
    presentation.menu(pty, from, "Role routing");
}

fn add_native(
    pty: &mut SetupPty,
    presentation: Presentation,
    append: usize,
    source: usize,
    model: &str,
) {
    let from = presentation.choose(pty, append);
    presentation.menu(pty, from, "Route source");
    let from = presentation.choose(pty, source);
    match presentation {
        Presentation::ScreenReader => wait_for_after(
            &mut pty.output,
            &mut pty.input,
            &mut pty.transcript,
            &mut pty.answered,
            from,
            b"Input:\r\n",
        ),
        Presentation::Inline => {
            inline_frame(pty, from, "Role model");
        }
    }
    let from = pty.transcript.len();
    pty.input
        .write_all(format!("{model}\r").as_bytes())
        .unwrap();
    presentation.menu(pty, from, "Role effort");
    let from = presentation.choose(pty, 1);
    presentation.menu(pty, from, "Edit role chain");
}

fn confirm(pty: &mut SetupPty, presentation: Presentation, from: usize, title: &str, accept: bool) {
    match presentation {
        Presentation::ScreenReader => {
            acknowledge_linear_pages(
                &mut pty.output,
                &mut pty.input,
                &mut pty.transcript,
                &mut pty.answered,
                (from, b"empty Enter selects 1. Back\r\nInput:\r\n"),
            );
            pty.input
                .write_all(if accept { b"2\r" } else { b"1\r" })
                .unwrap();
        }
        Presentation::Inline => {
            let mut cursor = from;
            for _ in 0..MAX_WARNING_PAGES {
                let screen = inline_frame(pty, cursor, title);
                cursor = pty.transcript.len();
                if screen.iter().any(|row| row.contains("Accept")) {
                    pty.input
                        .write_all(if accept { b"\t\r" } else { b"\r" })
                        .unwrap();
                    return;
                }
                pty.input.write_all(b"\r").unwrap();
            }
            panic!("bounded routing confirmation pages exhausted");
        }
    }
}

fn reopen_and_clear(
    temp: &tempfile::TempDir,
    workspace: &Path,
    state: &Path,
    presentation: Presentation,
    saved: &[u8],
) {
    let mode = match presentation {
        Presentation::ScreenReader => SetupPtyMode::Manager,
        Presentation::Inline => SetupPtyMode::InlineManager,
    };
    let mut pty = start_setup_pty_mode(temp, workspace, state, mode);
    open_roles(&mut pty, presentation);
    let from = presentation.choose(&mut pty, 1);
    let (from, manifest) = acknowledge_result(&mut pty, presentation, from);
    for expected in [
        "Child: explicit",
        "Review: Inherit primary",
        "Compaction: Inherit primary",
        "claude-sonnet-5",
        "claude-opus-5-5",
        "gpt-5.4",
    ] {
        assert_result_contains(&manifest, expected);
    }
    presentation.menu(&mut pty, from, "Role routing");
    let from = presentation.choose(&mut pty, 7);
    confirm(&mut pty, presentation, from, "Clear role routing?", false);
    let from = pty.transcript.len();
    presentation.menu(&mut pty, from, "Role routing");
    assert_eq!(
        std::fs::read(state.join("provider-routing.json")).unwrap(),
        saved
    );
    let from = presentation.choose(&mut pty, 7);
    confirm(&mut pty, presentation, from, "Clear role routing?", true);
    let from = pty.transcript.len();
    let (_, result) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&result, "Cleared");
    assert!(finish_setup_pty(pty).status.success());
    assert!(!state.join("provider-routing.json").exists());
    assert_edits(
        state,
        workspace,
        &[
            ProviderSetupDisposition::Applied,
            ProviderSetupDisposition::Applied,
        ],
    );
}

fn assert_edits(state: &Path, workspace: &Path, expected: &[ProviderSetupDisposition]) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(state).unwrap(),
                workspace.to_owned(),
            )
            .await
            .unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
            let mut dispositions = Vec::new();
            for session in sessions {
                let events = store.load_session(session.id).await.unwrap();
                let view = arany::SessionView::replay(session.id, &events)
                    .unwrap()
                    .unwrap();
                assert!(view.runs.is_empty());
                assert!(!view.provider_setup_blocks_continuation());
                for event in events {
                    match event.event {
                        Event::ProviderSetupEditStarted { target, .. } => {
                            assert_eq!(target.kind, ProviderSetupTargetKind::RoleRouting)
                        }
                        Event::ProviderSetupEditFinished { disposition, .. } => {
                            dispositions.push(disposition)
                        }
                        _ => {}
                    }
                }
            }
            store.close().await.unwrap();
            assert_eq!(dispositions, expected);
        });
}

#[test]
fn concurrent_role_edit_refuses_before_overwriting_the_other_editor() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let mut pty = start_setup_pty(&temp, &workspace, &state);
    open_roles(&mut pty, Presentation::ScreenReader);
    let from = Presentation::ScreenReader.choose(&mut pty, 6);
    let bytes = br#"{"version":1,"providers":{"primary":null,"child":null,"review":null,"compaction":null}}"#;
    super::super::private_file(&state.join("provider-routing.json"), bytes);
    confirm(
        &mut pty,
        Presentation::ScreenReader,
        from,
        "Save role routing?",
        true,
    );
    let from = pty.transcript.len();
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        from,
        b"Input:\r\n",
    );
    pty.input.write_all(b"/exit\r").unwrap();
    let (result, transcript) = finish_setup_pty_with_transcript(pty);
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&transcript).contains("changed"));
    assert_eq!(
        std::fs::read(state.join("provider-routing.json")).unwrap(),
        bytes
    );
    assert_edits(&state, &workspace, &[ProviderSetupDisposition::Failed]);
}

fn refuse_unused_fallback(temp: &tempfile::TempDir, workspace: &Path, state: &Path) {
    nix::unistd::mkfifo(
        &workspace.join("AGENTS.md"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(workspace)
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env("ANTHROPIC_API_KEY", "synthetic-routing-key")
        .args([
            "exec",
            "answer",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-5",
            "--effort",
            "high",
            "--collaboration",
            "single",
            "--state-dir",
        ])
        .arg(state)
        .arg("--workspace")
        .arg(workspace)
        .stdin(Stdio::null())
        .bounded_output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(result.stdout, b"");
    assert_eq!(result.stderr, b"error: Provider role route openai/gpt-5.4 refused: selected native API credentials or workspace configuration unavailable\n");
    assert_edits(state, workspace, &[ProviderSetupDisposition::Applied]);
}

#[test]
fn saved_role_choices_feed_the_next_submission_without_rechecking_or_losing_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let (primary, child) = checked_role_peers(&workspace, &state);
    let mut pty = start_setup_pty_mode(&temp, &workspace, &state, SetupPtyMode::Configured);
    save_distinct_child_route(&mut pty);
    assert_eq!(
        primary.calls.load(Ordering::SeqCst),
        ROUTE_CHECK_CALLS,
        "Save must not check or infer on primary"
    );
    assert_eq!(
        child.calls.load(Ordering::SeqCst),
        ROUTE_CHECK_CALLS,
        "Save must not check or infer on child"
    );
    assert_retained_setup_conversation(&state, &workspace, Some(0));
    submit_delegated_turn(&mut pty);
    let (output, transcript) = finish_setup_pty_with_transcript(pty);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&transcript).contains(FIRST_ROUTE_KEY_CANARY));
    let primary_requests = primary.peer.join().expect("primary semantic requests");
    let child_requests = child.peer.join().expect("child semantic requests");
    assert_eq!(primary_requests.len(), 2);
    assert_eq!(primary_requests[0]["objective"], "answer after setup");
    assert_eq!(primary_requests[1]["objective"], "answer after setup");
    assert_eq!(
        primary_requests[1]["child_results"],
        json!([{
            "objective":"inspect the distinct route", "summary":"distinct child report", "result":"distinct child report"
        }])
    );
    assert_eq!(child_requests.len(), 1);
    assert_eq!(child_requests[0]["objective"], "inspect the distinct route");
    assert!(
        child_requests[0]["child_results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        primary.calls.load(Ordering::SeqCst),
        ROUTE_CHECK_CALLS + primary_requests.len()
    );
    assert_eq!(
        child.calls.load(Ordering::SeqCst),
        ROUTE_CHECK_CALLS + child_requests.len()
    );
    assert_routed_run(&state, &workspace);
}

fn submit_delegated_turn(pty: &mut SetupPty) {
    let from = pty.transcript.len();
    pty.input.write_all(b"/agents auto\r").unwrap();
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        from,
        b"Next-Run collaboration: auto",
    );
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        from,
        b"Input:\r\n",
    );
    let from = pty.transcript.len();
    pty.input.write_all(b"answer after setup\r").unwrap();
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        from,
        b"retained setup answer",
    );
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        from,
        b"Input:\r\n",
    );
    pty.input.write_all(b"/exit\r").unwrap();
}

fn save_distinct_child_route(pty: &mut SetupPty) {
    open_roles(pty, Presentation::ScreenReader);
    let from = Presentation::ScreenReader.choose(pty, 3);
    Presentation::ScreenReader.menu(pty, from, "Edit role chain");
    let from = Presentation::ScreenReader.choose(pty, 2);
    Presentation::ScreenReader.menu(pty, from, "Route source");
    let from = Presentation::ScreenReader.choose(pty, 6);
    Presentation::ScreenReader.menu(pty, from, "Custom role route");
    let from = Presentation::ScreenReader.choose(pty, 2);
    Presentation::ScreenReader.menu(pty, from, "Role effort");
    let from = Presentation::ScreenReader.choose(pty, 1);
    Presentation::ScreenReader.menu(pty, from, "Edit role chain");
    let from = Presentation::ScreenReader.choose(pty, 4);
    let (from, _) = acknowledge_result(pty, Presentation::ScreenReader, from);
    Presentation::ScreenReader.menu(pty, from, "Role routing");
    let from = Presentation::ScreenReader.choose(pty, 6);
    confirm(
        pty,
        Presentation::ScreenReader,
        from,
        "Save role routing?",
        true,
    );
    let from = pty.transcript.len();
    let (from, _) = acknowledge_result(pty, Presentation::ScreenReader, from);
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        from,
        b"Setup closed\r\nInput:\r\n",
    );
}

struct RolePeer {
    calls: Arc<AtomicUsize>,
    peer: thread::JoinHandle<Vec<serde_json::Value>>,
}

fn checked_role_peers(workspace: &Path, state: &Path) -> (RolePeer, RolePeer) {
    let primary = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let child = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    write_role_profiles(state, &primary, &child);
    let primary = serve_role(primary, AgentRole::Primary, "model-1");
    let child = serve_role(child, AgentRole::Child, "model-2");
    for (name, credential_env, key) in [
        ("local", "ARANY_PROVIDER_LOCAL_KEY", "test-key"),
        (
            "distinct-child",
            "ARANY_PROVIDER_FIRST_ROUTE_KEY",
            FIRST_ROUTE_KEY_CANARY,
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env(credential_env, key)
            .current_dir(workspace)
            .args(["provider", "check", name, "--state-dir"])
            .arg(state)
            .stdin(Stdio::null())
            .bounded_output()
            .unwrap();
        assert!(output.status.success(), "synthetic role conformance");
        assert_eq!(output.stderr, b"");
        assert_eq!(
            crate::check_output(&output.stdout, state, arany::CheckDisposition::Succeeded),
            format!("Provider: {name}\nStatus: custom verified\n").as_bytes()
        );
    }
    (primary, child)
}

fn write_role_profiles(state: &Path, primary: &TcpListener, child: &TcpListener) {
    let profiles = [
        (
            "local",
            "model-1",
            primary.local_addr().unwrap().port(),
            "ARANY_PROVIDER_LOCAL_KEY",
        ),
        (
            "distinct-child",
            "model-2",
            child.local_addr().unwrap().port(),
            "ARANY_PROVIDER_FIRST_ROUTE_KEY",
        ),
    ]
    .into_iter()
    .map(
        |(name, model, port, credential_env)| arany::CustomProfileDraft {
            name: name.into(),
            protocol: arany::CustomProtocol::OpenaiResponses,
            endpoint: format!("http://127.0.0.1:{port}/v1/responses"),
            model: model.into(),
            credential_env: credential_env.into(),
            messages_auth: arany::MessagesAuth::ApiKey,
            efforts: Vec::new(),
        },
    )
    .collect::<Vec<_>>();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            arany::CustomProfile::write_profiles(
                &StateRoot::admit(state).unwrap(),
                None,
                &profiles,
            )
            .await
            .unwrap();
        });
}

fn serve_role(listener: TcpListener, role: AgentRole, model: &'static str) -> RolePeer {
    listener.set_nonblocking(true).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::clone(&calls);
    let peer = thread::spawn(move || {
        let checks = [
            json!("ok"),
            json!({"outcome":{"type":"delegate","children":[{"objective":"synthetic child","profile":null}]}}),
            json!("synthetic summary"),
        ];
        for (index, text) in checks.into_iter().enumerate() {
            let (mut stream, wire) = role_request(&listener, role);
            assert_eq!(wire["model"], model);
            served.fetch_add(1, Ordering::SeqCst);
            super::super::loopback::send_response_for_model(
                &mut stream,
                index,
                model,
                text,
                wire["stream"] == true,
            );
        }
        let script = match role {
            AgentRole::Primary => vec![
                (
                    AgentPhase::RootPlan,
                    json!({"outcome":{"type":"delegate","children":[{"objective":"inspect the distinct route","profile":null}]}}),
                ),
                (AgentPhase::RootSynthesis, json!("retained setup answer")),
            ],
            AgentRole::Child => vec![(AgentPhase::ChildWork, json!("distinct child report"))],
        };
        let mut requests = Vec::new();
        for (index, (phase, text)) in script.into_iter().enumerate() {
            let (mut stream, wire) = role_request(&listener, role);
            assert_eq!(wire["model"], model);
            let input = crate::semantic::input(&wire);
            assert_eq!(input["phase"], serde_json::to_value(phase).unwrap());
            requests.push(input);
            served.fetch_add(1, Ordering::SeqCst);
            super::super::loopback::send_response_for_model(
                &mut stream,
                ROUTE_CHECK_CALLS + index,
                model,
                text,
                wire["stream"] == true,
            );
        }
        requests
    });
    RolePeer { calls, peer }
}

fn role_request(
    listener: &TcpListener,
    role: AgentRole,
) -> (std::net::TcpStream, serde_json::Value) {
    let mut stream =
        crate::process::accept_with_deadline(listener, ROUTE_REQUEST_TIMEOUT, "role request");
    let key = match role {
        AgentRole::Primary => "test-key",
        AgentRole::Child => FIRST_ROUTE_KEY_CANARY,
    };
    let bytes = super::super::loopback::read_request_for(
        &mut stream,
        "/v1/responses",
        &format!("authorization: bearer {key}"),
    );
    let wire = serde_json::from_slice(&bytes).unwrap();
    (stream, wire)
}

fn assert_routed_run(state: &Path, workspace: &Path) {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let sessions = arany::list_sessions(StateRoot::open_existing(state).unwrap(), workspace.to_owned()).await.unwrap();
        assert_eq!(sessions.len(), 1);
        let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
        let events = store.load_session(sessions[0].id).await.unwrap();
        store.close().await.unwrap();
        let view = SessionView::replay(sessions[0].id, &events).unwrap().unwrap();
        assert_eq!(view.title, "Setup retained title");
        assert_eq!(view.defaults.provider.as_deref(), Some("custom:local"));
        assert_eq!(view.defaults.model.as_deref(), Some("model-1"));
        assert_eq!(view.runs.len(), 1);
        let run = &view.runs[0];
        assert_eq!(run.status, arany::RunStatus::Finished);
        let config = events.iter().find_map(|event| match &event.event {
            Event::RunStarted { config, .. } => Some(config), _ => None,
        }).unwrap();
        let pins = config.providers.as_ref().expect("saved distinct role pins");
        assert_eq!(pins.primary.len(), 1);
        assert_eq!(pins.child.len(), 1);
        assert_eq!(pins.primary[0].profile, "custom:local");
        assert_eq!(pins.primary[0].model, "model-1");
        assert_eq!(pins.child[0].profile, "custom:distinct-child");
        assert_eq!(pins.child[0].model, "model-2");
        assert_ne!(pins.child, pins.primary);
        assert_eq!(pins.review, pins.primary);
        assert_eq!(pins.compaction, pins.primary);
        assert_eq!(run.agents.len(), 2);
        let child = run.agents.iter().find(|agent| agent.role == AgentRole::Child).unwrap();
        assert_eq!(child.result.as_deref(), Some("distinct child report"));
        assert_eq!(child.provider_calls.len(), 1);
        assert_eq!(child.provider_calls[0].phase, AgentPhase::ChildWork);
        assert!(events.iter().any(|event| matches!(&event.event, Event::ProviderCallRecorded { agent_run_id, record, .. }
            if *agent_run_id == child.id && record.phase == AgentPhase::ChildWork)));
    });
}

#[test]
fn invalid_role_model_refuses_without_opening_an_empty_effort_picker_or_publishing() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let mut pty = start_setup_pty(&temp, &workspace, &state);
    open_roles(&mut pty, Presentation::ScreenReader);
    let from = Presentation::ScreenReader.choose(&mut pty, 3);
    Presentation::ScreenReader.menu(&mut pty, from, "Edit role chain");
    let from = Presentation::ScreenReader.choose(&mut pty, 2);
    Presentation::ScreenReader.menu(&mut pty, from, "Route source");
    let from = Presentation::ScreenReader.choose(&mut pty, 2);
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        from,
        b"Input:\r\n",
    );
    let from = pty.transcript.len();
    pty.input.write_all(b"invalid model\r").unwrap();
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        from,
        b"invalid Provider role model",
    );
    wait_for_after(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        from,
        b"Input:\r\n",
    );
    pty.input.write_all(b"/exit\r").unwrap();
    assert!(finish_setup_pty(pty).status.success());
    assert!(!state.join("provider-routing.json").exists());
    assert_edits(&state, &workspace, &[]);
}
