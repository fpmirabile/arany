use super::checks::{Presentation, acknowledge_result, assert_result_contains, inline_frame};
use super::routing::confirm;
use super::*;
use arany::{Event, ProviderSetupDisposition, ProviderSetupTargetKind};
use serde_json::json;

#[test]
fn roster_manager_creates_previews_saves_and_removes_without_activation_in_every_presentation() {
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
        create_draft(&mut pty, presentation);
        let from = presentation.choose(&mut pty, 3);
        let (from, preview) = acknowledge_result(&mut pty, presentation, from);
        for expected in [
            "arany --profile coding",
            "arany --profile research",
            "child=inspect",
            "child=main",
            "Inherit primary",
            "native API",
        ] {
            assert_result_contains(&preview, expected);
        }
        presentation.menu(&mut pty, from, "Roster profiles");
        let from = presentation.choose(&mut pty, 4);
        confirm(&mut pty, presentation, from, "Save roster?", false);
        let from = pty.transcript.len();
        presentation.menu(&mut pty, from, "Roster profiles");
        assert!(!state.join("provider-roster.json").exists());
        assert_edits(&state, &workspace, &[]);
        let from = presentation.choose(&mut pty, 4);
        confirm(&mut pty, presentation, from, "Save roster?", true);
        let from = pty.transcript.len();
        let (_, result) = acknowledge_result(&mut pty, presentation, from);
        assert_result_contains(&result, "current conversation selection is unchanged");
        assert_result_contains(&result, "arany --profile research");
        assert!(finish_setup_pty(pty).status.success());
        assert_edits(
            &state,
            &workspace,
            &[(
                ProviderSetupTargetKind::Roster,
                ProviderSetupDisposition::Applied,
            )],
        );
        let saved = std::fs::read(state.join("provider-roster.json")).unwrap();
        let record: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(
            record["presets"],
            json!([
                {"name":"coding", "providers":{"primary":"main","child":"inspect","review":null,"compaction":null},"child_profiles":["main","inspect"]},
                {"name":"research", "providers":{"primary":"main","child":"main","review":null,"compaction":null},"child_profiles":["main"]}
            ])
        );
        clear_conflict(&temp, &workspace, &state, presentation, &saved);
        remove_presets(&temp, &workspace, &state, presentation);
        assert_edits(
            &state,
            &workspace,
            &[
                (
                    ProviderSetupTargetKind::Roster,
                    ProviderSetupDisposition::Applied,
                ),
                (
                    ProviderSetupTargetKind::RoleRouting,
                    ProviderSetupDisposition::Applied,
                ),
                (
                    ProviderSetupTargetKind::Roster,
                    ProviderSetupDisposition::Applied,
                ),
            ],
        );
    }
}

fn create_draft(pty: &mut SetupPty, presentation: Presentation) {
    open(pty, presentation);
    select(pty, presentation, 1, "Route profiles");
    add_profile(pty, presentation, 1, "main", (2, "gpt-5.4"));
    add_profile(pty, presentation, 2, "inspect", (3, "claude-sonnet-5"));
    select(pty, presentation, 4, "Roster profiles");
    select(pty, presentation, 2, "Named presets");
    add_preset(pty, presentation, 1, "coding", 2);
    add_preset(pty, presentation, 2, "research", 1);
    select(pty, presentation, 4, "Roster profiles");
}

fn open(pty: &mut SetupPty, presentation: Presentation) {
    let from = presentation.choose(pty, 4);
    presentation.menu(pty, from, "Roster profiles");
    let fresh = String::from_utf8_lossy(&pty.transcript[from..]);
    assert!(!fresh.contains("Clear role routing"));
}

fn select(pty: &mut SetupPty, presentation: Presentation, choice: usize, title: &str) {
    let from = presentation.choose(pty, choice);
    presentation.menu(pty, from, title);
}

fn field(
    pty: &mut SetupPty,
    presentation: Presentation,
    from: usize,
    title: &str,
    value: &str,
) -> usize {
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
            inline_frame(pty, from, title);
        }
    }
    let from = pty.transcript.len();
    pty.input
        .write_all(format!("{value}\r").as_bytes())
        .unwrap();
    from
}

fn add_profile(
    pty: &mut SetupPty,
    presentation: Presentation,
    add: usize,
    name: &str,
    (source, model): (usize, &str),
) {
    let from = presentation.choose(pty, add);
    let from = field(pty, presentation, from, "Roster name", name);
    presentation.menu(pty, from, "Roster chain");
    select(pty, presentation, 1, "Route source");
    let from = presentation.choose(pty, source);
    let from = field(pty, presentation, from, "Role model", model);
    presentation.menu(pty, from, "Role effort");
    select(pty, presentation, 1, "Roster chain");
    select(pty, presentation, 3, "Route profiles");
}

fn add_preset(
    pty: &mut SetupPty,
    presentation: Presentation,
    add: usize,
    name: &str,
    child: usize,
) {
    let from = presentation.choose(pty, add);
    let from = field(pty, presentation, from, "Roster name", name);
    presentation.menu(pty, from, "Preset roles");
    select(pty, presentation, 2, "Role profile");
    select(pty, presentation, child, "Preset roles");
    if child == 2 {
        select(pty, presentation, 5, "Eligible children");
        select(pty, presentation, 2, "Eligible children");
        select(pty, presentation, 3, "Preset roles");
    }
    select(pty, presentation, 6, "Named presets");
}

fn mode(presentation: Presentation) -> SetupPtyMode {
    match presentation {
        Presentation::ScreenReader => SetupPtyMode::Manager,
        Presentation::Inline => SetupPtyMode::InlineManager,
    }
}

fn clear_conflict(
    temp: &tempfile::TempDir,
    workspace: &Path,
    state: &Path,
    presentation: Presentation,
    saved: &[u8],
) {
    super::super::private_file(&state.join("provider-routing.json"), br#"{"version":1,"providers":{"primary":null,"child":null,"review":null,"compaction":null}}"#);
    for name in ["coding", "research"] {
        let output = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(workspace)
            .args(["exec", "--profile", name, "--state-dir"])
            .arg(state)
            .arg("answer")
            .stdin(Stdio::null())
            .bounded_output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            b"error: --profile conflicts with provider-routing.json; choose one routing source\n"
        );
    }
    let mut pty = start_setup_pty_mode(temp, workspace, state, mode(presentation));
    select(&mut pty, presentation, 4, "Roster profiles");
    let from = presentation.choose(&mut pty, 5);
    confirm(&mut pty, presentation, from, "Clear role routing?", false);
    let from = pty.transcript.len();
    presentation.menu(&mut pty, from, "Roster profiles");
    assert!(state.join("provider-routing.json").exists());
    let from = presentation.choose(&mut pty, 5);
    confirm(&mut pty, presentation, from, "Clear role routing?", true);
    let from = pty.transcript.len();
    let (from, result) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&result, "Cleared");
    presentation.menu(&mut pty, from, "Roster profiles");
    assert!(!String::from_utf8_lossy(&pty.transcript[from..]).contains("Clear role routing"));
    let _ = presentation.choose(&mut pty, 5);
    assert!(finish_setup_pty(pty).status.success());
    assert!(!state.join("provider-routing.json").exists());
    assert_eq!(
        std::fs::read(state.join("provider-roster.json")).unwrap(),
        saved
    );
}

fn remove_presets(
    temp: &tempfile::TempDir,
    workspace: &Path,
    state: &Path,
    presentation: Presentation,
) {
    let mut pty = start_setup_pty_mode(temp, workspace, state, mode(presentation));
    open(&mut pty, presentation);
    select(&mut pty, presentation, 1, "Route profiles");
    select(&mut pty, presentation, 1, "Profile action");
    select(&mut pty, presentation, 1, "Roster chain");
    let from = pty.transcript.len();
    pty.input.write_all(b"\x1b").unwrap();
    presentation.menu(&mut pty, from, "Route profiles");
    select(&mut pty, presentation, 1, "Profile action");
    let from = presentation.choose(&mut pty, 2);
    let (from, result) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&result, "coding, research");
    presentation.menu(&mut pty, from, "Route profiles");
    select(&mut pty, presentation, 4, "Roster profiles");
    select(&mut pty, presentation, 2, "Named presets");
    for _ in 0..2 {
        select(&mut pty, presentation, 1, "Preset action");
        select(&mut pty, presentation, 2, "Named presets");
    }
    select(&mut pty, presentation, 2, "Roster profiles");
    let from = presentation.choose(&mut pty, 4);
    confirm(&mut pty, presentation, from, "Save roster?", true);
    let from = pty.transcript.len();
    let (_, result) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&result, "Removed the roster record");
    assert!(finish_setup_pty(pty).status.success());
    assert!(!state.join("provider-roster.json").exists());
}

#[test]
fn concurrent_clear_failure_retains_the_edited_roster_and_settles_before_save() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let routing = prepare_saved_roster(&state);
    let presentation = Presentation::ScreenReader;
    let mut pty = start_setup_pty_mode(&temp, &workspace, &state, SetupPtyMode::Manager);
    edit_saved_child(&mut pty, presentation);
    select(&mut pty, presentation, 3, "Roster profiles");
    let from = presentation.choose(&mut pty, 5);
    acknowledge_linear_pages(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        (from, b"empty Enter selects 1. Back\r\nInput:\r\n"),
    );
    let concurrent = [routing, b"\n"].concat();
    super::super::private_file(&state.join("provider-routing.json"), &concurrent);
    let from = pty.transcript.len();
    pty.input.write_all(b"2\r").unwrap();
    let (from, result) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&result, "changed");
    presentation.menu(&mut pty, from, "Roster profiles");
    let from = presentation.choose(&mut pty, 4);
    confirm(&mut pty, presentation, from, "Save roster?", true);
    let from = pty.transcript.len();
    let (_, result) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&result, "Saved");
    assert!(finish_setup_pty(pty).status.success());
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("provider-roster.json")).unwrap())
            .unwrap();
    assert_eq!(saved["presets"][0]["providers"]["child"], "inspect");
    assert_eq!(
        std::fs::read(state.join("provider-routing.json")).unwrap(),
        concurrent
    );
    assert_edits(
        &state,
        &workspace,
        &[
            (
                ProviderSetupTargetKind::RoleRouting,
                ProviderSetupDisposition::Failed,
            ),
            (
                ProviderSetupTargetKind::Roster,
                ProviderSetupDisposition::Applied,
            ),
        ],
    );
}

#[test]
fn routing_inspection_failures_retain_roster_edits_until_a_reviewed_save() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let routing = prepare_saved_roster(&state);
    let original = std::fs::read(state.join("provider-roster.json")).unwrap();
    let presentation = Presentation::ScreenReader;
    let mut pty = start_setup_pty_mode(&temp, &workspace, &state, SetupPtyMode::Manager);
    edit_saved_child(&mut pty, presentation);
    let routing_path = state.join("provider-routing.json");
    std::fs::set_permissions(&routing_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let from = presentation.choose(&mut pty, 3);
    let (from, error) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&error, "state directory is not private");
    assert!(String::from_utf8_lossy(&pty.transcript).contains("Roster draft retained"));
    presentation.menu(&mut pty, from, "Roster profiles");
    assert!(!String::from_utf8_lossy(&pty.transcript[from..]).contains("Clear role routing"));
    assert_eq!(
        std::fs::read(state.join("provider-roster.json")).unwrap(),
        original
    );

    let from = presentation.choose(&mut pty, 4);
    wait_linear_page(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
        (from, 1),
    );
    std::fs::set_permissions(&routing_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (from, error) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&error, "state directory is not private");
    presentation.menu(&mut pty, from, "Roster profiles");
    let from = presentation.choose(&mut pty, 3);
    let (from, preview) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&preview, "child=inspect");
    presentation.menu(&mut pty, from, "Roster profiles");
    let from = presentation.choose(&mut pty, 4);
    confirm(&mut pty, presentation, from, "Save roster?", true);
    let from = pty.transcript.len();
    let (_, saved) = acknowledge_result(&mut pty, presentation, from);
    assert_result_contains(&saved, "Saved");
    assert!(finish_setup_pty(pty).status.success());
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("provider-roster.json")).unwrap())
            .unwrap();
    assert_eq!(saved["presets"][0]["providers"]["child"], "inspect");
    assert_eq!(std::fs::read(routing_path).unwrap(), routing);
    assert_edits(
        &state,
        &workspace,
        &[(
            ProviderSetupTargetKind::Roster,
            ProviderSetupDisposition::Applied,
        )],
    );
}

fn prepare_saved_roster(state: &Path) -> &'static [u8] {
    StateRoot::admit(state).unwrap();
    let route = json!({"provider":"openai","model":"gpt-5.4","effort":"none","account_id":null});
    let record = json!({"version":1,"profiles":[
        {"name":"main","routes":[route]}, {"name":"inspect","routes":[route]}
    ],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main","inspect"]}]});
    super::super::private_file(
        &state.join("provider-roster.json"),
        &serde_json::to_vec(&record).unwrap(),
    );
    let routing = br#"{"version":1,"providers":{"primary":null,"child":null,"review":null,"compaction":null}}"#;
    super::super::private_file(&state.join("provider-routing.json"), routing);
    routing
}

fn edit_saved_child(pty: &mut SetupPty, presentation: Presentation) {
    select(pty, presentation, 4, "Roster profiles");
    select(pty, presentation, 2, "Named presets");
    select(pty, presentation, 1, "Preset action");
    select(pty, presentation, 1, "Preset roles");
    select(pty, presentation, 2, "Role profile");
    select(pty, presentation, 2, "Preset roles");
    select(pty, presentation, 6, "Named presets");
}

fn assert_edits(
    state: &Path,
    workspace: &Path,
    expected: &[(ProviderSetupTargetKind, ProviderSetupDisposition)],
) {
    if expected.is_empty() {
        assert_eq!(
            std::fs::metadata(state.join("events.sqlite3"))
                .unwrap()
                .len(),
            0,
            "cancelled setup creates no journal or Session"
        );
        return;
    }
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
            let mut edits = vec![];
            for session in sessions {
                let events = store.load_session(session.id).await.unwrap();
                let view = SessionView::replay(session.id, &events).unwrap().unwrap();
                assert!(view.runs.is_empty());
                assert_eq!(view.defaults, SessionDefaults::default());
                assert!(!view.provider_setup_blocks_continuation());
                let mut pending = None;
                for event in events {
                    match event.event {
                        Event::ProviderSetupEditStarted { target, .. } => {
                            pending = Some(target.kind)
                        }
                        Event::ProviderSetupEditFinished { disposition, .. } => {
                            edits
                                .push((pending.take().expect("matching edit intent"), disposition));
                        }
                        _ => {}
                    }
                }
            }
            store.close().await.unwrap();
            assert_eq!(edits.len(), expected.len());
            for expected_edit in expected {
                assert_eq!(
                    edits.iter().filter(|edit| *edit == expected_edit).count(),
                    expected
                        .iter()
                        .filter(|edit| *edit == expected_edit)
                        .count(),
                );
            }
        });
}
