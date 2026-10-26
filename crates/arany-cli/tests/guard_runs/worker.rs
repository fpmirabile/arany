use super::*;
use arany::test_support::goals::replayed;
use arany_cli::presentation::{Output, render_session};
use arany_cli::protocol::EventLine;

pub(super) struct WorkerObservationBundle {
    pub(super) exit: Option<i32>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) events: Vec<arany::EventEnvelope>,
    pub(super) view: SessionView,
}

pub(super) async fn closed_bundle(
    state: &Path,
    session: SessionId,
    workspace: &Path,
) -> WorkerObservationBundle {
    use process::BoundedOutput;
    let (events, view) = replayed(state, session).await;
    let show = |output| {
        std::process::Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(workspace)
            .args(["show", "--state-dir"])
            .arg(state)
            .args(["--output", output])
            .arg(session.to_string())
            .bounded_output_for(Duration::from_secs(30), 4 * 1024 * 1024)
            .unwrap()
    };
    let text = show("text");
    assert!(text.status.success());
    assert!(text.stderr.is_empty());
    assert_eq!(
        text.stdout,
        render_session(&view, &events, Output::Text).as_bytes()
    );
    let jsonl = show("jsonl");
    assert!(jsonl.status.success());
    assert!(jsonl.stderr.is_empty());
    let actual: Vec<EventLine> = String::from_utf8(jsonl.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        actual,
        events.iter().map(EventLine::from).collect::<Vec<_>>()
    );
    WorkerObservationBundle {
        exit: text.status.code(),
        stdout: text.stdout,
        stderr: text.stderr,
        events,
        view,
    }
}

pub(super) async fn worker_case(case: &'static str) {
    let observed =
        arany::test_support::worker_teams::run_case(Path::new(env!("CARGO_BIN_EXE_arany")), case)
            .await;
    let state = observed.directory.path().join("state");
    let workspace = observed.directory.path().join("workspace");
    let bundle = closed_bundle(&state, observed.session, &workspace).await;
    assert_eq!(bundle.events, observed.events);
    assert_eq!(bundle.view, observed.view);
    assert_eq!(bundle.exit, Some(0));
    assert!(bundle.stderr.is_empty());
    assert!(!bundle.stdout.is_empty());
    if case == "success" {
        assert!(
            bundle.view.runs[0].agents[1]
                .tools
                .iter()
                .filter(|t| matches!(t.intent.call, arany::ToolCall::Write { .. }))
                .all(|t| t.checkpoint.is_some())
        );
    }
}

#[tokio::test]
async fn foreground_editor_writes_live_files_checkpoints_verifies_and_reopens() {
    worker_case("success").await;
}

#[tokio::test]
async fn cancelled_editor_keeps_landed_workspace_effects_and_causal_release() {
    worker_case("cancel_landed").await;
}

#[tokio::test]
async fn unsupported_editor_modes_and_invalid_assignments_refuse_before_spawn() {
    for case in ["background", "command"].into_iter().chain(
        arany::test_support::worker_teams::INVALID_EDITOR_CASES
            .iter()
            .map(|(case, _)| *case),
    ) {
        worker_case(case).await;
    }
}

#[tokio::test]
async fn editor_workspace_uncertainty_stops_the_run_without_releasing_or_retrying() {
    worker_case("uncertain").await;
}

#[tokio::test]
async fn editor_reassignment_uses_fresh_identity_and_current_workspace() {
    arany::test_support::worker_teams::resume_by_id(Path::new(env!("CARGO_BIN_EXE_arany"))).await;
}

#[tokio::test]
async fn editor_hooks_keep_child_identity_and_settle_before_continuation() {
    worker_case("hooks").await;
}

#[tokio::test]
async fn no_change_editor_still_requires_primary_verification_and_missing_approval_grants_nothing()
{
    worker_case("no_change").await;
    worker_case("no_channel").await;
}

#[tokio::test]
async fn foreground_editor_discovery_and_writes_share_active_time_without_overlapping_reservations()
{
    worker_case("discovery").await;
}

#[tokio::test]
async fn primary_verification_of_assignment_cannot_reuse_stale_goal_evidence() {
    worker_case("stale_goal").await;
}

#[tokio::test]
async fn later_foreground_wave_requires_fresh_primary_verification_of_earlier_edits() {
    worker_case("waves_stale").await;
    worker_case("waves_fresh").await;
}

#[tokio::test]
async fn foreground_editor_active_time_exhaustion_settles_the_lease_and_closes_the_goal() {
    worker_case("active_time").await;
}

#[tokio::test]
async fn foreground_editor_delegation_deadline_keeps_goal_receipt_unexhausted() {
    worker_case("delegation_deadline").await;
}

#[tokio::test]
async fn foreground_editor_capacity_stop_releases_the_lease_before_returning_store_error() {
    worker_case("capacity").await;
}
