use super::*;
use crate::tools::types::CommandResult;

pub(super) fn call(case: &str) -> Option<ToolCall> {
    // Hold the payload until the host checks cache contention and injects any lower drift.
    case.starts_with("command_overlay").then(|| ToolCall::Command {
        program: "build".into(),
        args: vec!["-c".into(), "set -eu; test ! -e sibling; test \"$(cat notes/ignored)\" = retained; while read -r signal < notes/released && test \"$signal\" = hold; do :; done; printf 'edited 0' > notes/0; printf discarded > notes/ignored".into()],
        cwd: ".".into(),
        resource_class: None,
        timeout_ms: None,
    })
}

pub(in crate::test_support::worker_teams) fn configure(workspace: &Path, case: &str) {
    if call(case).is_none() {
        return;
    }
    private_file(&workspace.join("sibling"), b"ungranted");
    private_file(&workspace.join("notes/.gitignore"), b"ignored\n");
    private_file(&workspace.join("notes/ignored"), b"retained");
    private_file(&workspace.join("notes/released"), b"hold\n");
}

pub(in crate::test_support::worker_teams) fn dispatch_gate(
    case: &str,
) -> Option<tokio::sync::oneshot::Receiver<()>> {
    call(case).map(FaultPlan::observe_guard_dispatch)
}

pub(in crate::test_support::worker_teams) fn prepare_approval(case: &str, intent: &EffectIntent) {
    if case == "command_overlay_uncertain" && matches!(intent.call, ToolCall::Command { .. }) {
        FaultPlan::lose_guard_receipt(intent.id);
    }
}

pub(in crate::test_support::worker_teams) fn dispatched(
    state: &Path,
    workspace: &Path,
    case: &str,
) {
    let root = StateRoot::open_existing(state).unwrap();
    assert!(matches!(
        root.begin_build_cache_with_limit(
            cache_identity(workspace),
            crate::store::BUILD_CACHE_DEFAULT_VIEW_BYTES
        ),
        Err(StoreError::BuildCacheBusy)
    ));
    assert!(matches!(
        root.clear_workspace_build_cache(workspace),
        Err(StoreError::BuildCacheBusy)
    ));
    if case == "command_overlay_conflict" {
        private_file(&workspace.join("notes/0"), b"host");
    }
    private_file(&workspace.join("notes/released"), b"release\n");
}

fn cache_identity(workspace: &Path) -> crate::store::BuildCacheIdentity {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(workspace).unwrap();
    crate::store::BuildCacheIdentity::new(workspace, metadata.dev(), metadata.ino()).unwrap()
}

pub(in crate::test_support::worker_teams) fn assert_cache_reusable(
    case: &str,
    state: &Path,
    workspace: &Path,
) {
    if !case.starts_with("command_overlay") || case == "command_overlay_uncertain" {
        return;
    }
    let root = StateRoot::open_existing(state).unwrap();
    root.clear_workspace_build_cache(workspace).unwrap();
    let lease = root
        .begin_build_cache_with_limit(
            cache_identity(workspace),
            crate::store::BUILD_CACHE_DEFAULT_VIEW_BYTES,
        )
        .unwrap();
    drop(lease);
}

pub(super) fn assert_stopped(case: &str, workspace: &Path, view: &SessionView) -> bool {
    let disposition = match case {
        "command_overlay_conflict" => ToolDisposition::Conflict,
        "command_overlay_uncertain" => ToolDisposition::Uncertain,
        _ => return false,
    };
    let run = &view.runs[0];
    let editor = &run.agents[1];
    assert_eq!(
        editor.tools.len(),
        1,
        "no publication or effect retry: {case}"
    );
    let command = &editor.tools[0];
    assert_eq!(command.intent.agent_run_id, editor.id);
    assert_eq!(
        command.observation.as_ref().unwrap().disposition,
        disposition
    );
    assert!(run.worker_history()[0].changed_paths.is_empty());
    assert_eq!(
        std::fs::read(workspace.join("notes/0")).unwrap(),
        if case == "command_overlay_conflict" {
            b"host".as_slice()
        } else {
            b"original 0"
        }
    );
    assert_eq!(
        std::fs::read(workspace.join("notes/ignored")).unwrap(),
        b"retained"
    );
    assert_ne!(run.status, RunStatus::Finished);
    assert_eq!(
        editor.assignment.as_ref().unwrap().released.is_none(),
        case == "command_overlay_uncertain"
    );
    if case == "command_overlay_uncertain" {
        assert!(!view.goals[0].can_resume(view));
    }
    true
}

pub(super) fn assert_published(case: &str, workspace: &Path, editor: &AgentView) {
    if !matches!(case, "command_overlay" | "command_overlay_cancel") {
        return;
    }
    let command = &editor.tools[0];
    assert_eq!(command.intent.agent_run_id, editor.id);
    assert_eq!(
        editor.assignment.as_ref().unwrap().pins.grant.read_paths,
        ["notes/0"]
    );
    let observation = command.observation.as_ref().unwrap();
    let Some(Attestation::Guard(receipt)) = &observation.attestation else {
        panic!("command receipt")
    };
    assert_eq!(receipt.intent_digest, command.intent.digest());
    assert_eq!(receipt.limits, command.intent.limits);
    let result: CommandResult = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(result.exit_code, Some(0));
    let selection = result.selection.unwrap();
    assert_eq!(selection.ignored_entries, 1);
    assert_eq!(selection.discarded_outputs, 1);
    assert_eq!(result.changeset.files.len(), 1);
    let change = &result.changeset.files[0];
    assert_eq!(change.path, "notes/0");
    assert_eq!(
        change.before.as_ref().unwrap().bytes().unwrap(),
        b"original 0"
    );
    assert_eq!(change.after.as_ref().unwrap().bytes().unwrap(), b"edited 0");
    assert_eq!(
        std::fs::read(workspace.join("notes/ignored")).unwrap(),
        b"retained"
    );
    let publication = &editor.tools[1];
    assert_eq!(
        publication.checkpoint.as_ref().unwrap().status,
        CheckpointStatus::Captured
    );
    assert!(
        matches!(&publication.intent.call, ToolCall::ApplyChangeset { source: crate::worker::ChangesetSource::CommandReceipt { source_intent_digest, changeset } } if *source_intent_digest == command.intent.digest() && *changeset == result.changeset)
    );
    assert!(matches!(
        publication.observation.as_ref().unwrap().attestation,
        Some(Attestation::InProcess { .. })
    ));
}
