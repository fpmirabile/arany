use super::super::setup::{self, SetupError};
use crate::presentation::escape_terminal;
use crate::terminal::AttachedTerminal;
use arany::WorkspacePermissions;
use std::path::Path;

const RUN: u8 = 2;
const KEEP_OFF: u8 = 1;
const WARNING: &str = "Hooks run on this computer with your authority, outside Arany's sandbox. They often call other project files (package scripts, lint or pre-commit configuration) that the model can edit, so pinning the hook files protects only partly. While hooks run, Git writes need your approval or your own allow rule, never AI review.";

// Asks whether the folder's active Git hooks may run; `force` asks even when a saved answer covers them.
pub(in crate::cli::attached) async fn ask(
    terminal: &mut AttachedTerminal,
    permissions: &mut WorkspacePermissions,
    state_dir: &Path,
    force: bool,
) -> Result<Option<String>, SetupError> {
    if !permissions.is_trusted() {
        return Ok(force.then(|| "Trust this folder before choosing its Git hooks".into()));
    }
    let Some(question) = permissions.git_hooks(state_dir).await else {
        return Ok(force.then(|| "This folder has no active Git hooks".into()));
    };
    if question.saved.is_some() && !force {
        return Ok(None);
    }
    let framework = if question.pre_commit_framework {
        " (pre-commit framework)"
    } else {
        ""
    };
    let instruction = format!(
        "Git hooks in {}: {}{framework}. {WARNING}",
        escape_terminal(&question.directory.display().to_string()),
        escape_terminal(&question.names.join(", ")),
    );
    let accepted = match setup::choose_focused(
        terminal,
        1,
        "Git hooks",
        &instruction,
        &[(KEEP_OFF, "Keep hooks off"), (RUN, "Run these hooks")],
        question
            .saved
            .map(|accepted| if accepted { RUN } else { KEEP_OFF }),
    )
    .await?
    {
        Some(RUN) => true,
        Some(KEEP_OFF) => false,
        _ => return Ok(None),
    };
    Ok(Some(
        match permissions.remember_git_hooks(&question, accepted) {
            Ok(()) if accepted => "Git hooks run from the next Run".into(),
            Ok(()) => "Git hooks stay off".into(),
            Err(error) => format!("Could not save the Git hooks answer: {error}"),
        },
    ))
}
