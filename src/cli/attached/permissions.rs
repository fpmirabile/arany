use super::setup::SetupError;
use arany::{ApprovalMode, AttachedTerminal, Composer, TerminalInput, WorkspacePermissions};
use std::path::Path;

pub(super) enum PermissionChoice {
    Selected(WorkspacePermissions),
    Closed(WorkspacePermissions),
    Exit,
}

pub(super) fn refresh_files(workspace: &Path, composer: &mut Composer) -> Option<String> {
    if !composer.file_query_changed() {
        return None;
    }
    let query = composer.file_query();
    let entries = query.as_deref().map_or_else(
        || Ok(Vec::new()),
        |query| {
            let (folder, prefix) = query
                .strip_suffix('/')
                .map_or(("", query), |folder| (folder, ""));
            arany::list_workspace_entries(workspace, folder, prefix)
        },
    );
    match entries {
        Ok(entries) => {
            composer.set_file_candidates(query, entries);
            None
        }
        Err(error) => {
            composer.set_file_candidates(query, Vec::new());
            Some(format!(
                "File suggestions unavailable: {error}; type an exact @file path"
            ))
        }
    }
}

pub(super) fn supports_tools(profile: Option<&str>) -> bool {
    matches!(profile, Some("openai" | "anthropic" | "chatgpt"))
}

pub(super) async fn admit(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    composer: &mut Composer,
) -> Result<PermissionChoice, SetupError> {
    let access = WorkspacePermissions::open(workspace).map_err(|error| {
        SetupError::Recoverable(format!("Could not load folder trust: {error}"))
    })?;
    if access.is_trusted() {
        composer.set_approval_mode(Some(access.mode()));
        Ok(PermissionChoice::Selected(access))
    } else {
        choose(terminal, workspace, composer).await
    }
}

pub(super) async fn choose(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    composer: &mut Composer,
) -> Result<PermissionChoice, SetupError> {
    let mut permissions = WorkspacePermissions::open(workspace).map_err(|error| {
        SetupError::Recoverable(format!("Could not load folder trust: {error}"))
    })?;
    let mut selected = 0usize;
    let mut page = 0usize;
    let id = uuid::Uuid::now_v7();
    let mut answer = Composer::default();
    loop {
        let more = terminal.draw_workspace_permissions(id, workspace, page, selected == 1)?;
        match terminal.next_input().await? {
            TerminalInput::Up if !more => selected = selected.saturating_sub(1),
            TerminalInput::Down if !more => selected = (selected + 1).min(1),
            TerminalInput::PageUp => {
                page = page.saturating_sub(1);
                selected = 0;
            }
            TerminalInput::Down | TerminalInput::PageDown if more => page += 1,
            TerminalInput::Tab if !more => selected = (selected + 1) % 2,
            TerminalInput::Character(character) if terminal.is_linear() => {
                answer.apply(TerminalInput::Character(character));
            }
            TerminalInput::Backspace if terminal.is_linear() => {
                answer.apply(TerminalInput::Backspace);
            }
            TerminalInput::Resize => {
                page = 0;
                selected = 0;
            }
            TerminalInput::Submit if more => page += 1,
            TerminalInput::Submit => {
                let value = answer.text().trim().to_ascii_lowercase();
                if terminal.is_linear() {
                    selected = match value.as_str() {
                        "" | "1" | "read only" | "no" => 0,
                        "2" | "trust" | "trust and remember" | "yes" => 1,
                        _ => {
                            answer.clear();
                            continue;
                        }
                    };
                }
                permissions
                    .remember(selected == 1, ApprovalMode::AutoEdits)
                    .map_err(|error| {
                        SetupError::Recoverable(format!("Could not remember folder trust: {error}"))
                    })?;
                composer.set_approval_mode(permissions.is_trusted().then_some(permissions.mode()));
                terminal.restore_draft_input(composer.text().len())?;
                return Ok(PermissionChoice::Selected(permissions));
            }
            TerminalInput::Escape => {
                terminal.restore_draft_input(composer.text().len())?;
                return Ok(PermissionChoice::Closed(permissions));
            }
            TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                return Ok(PermissionChoice::Exit);
            }
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(composer.text().len())?;
                page = 0;
                selected = 0;
            }
            TerminalInput::Shutdown(signal) => {
                return Err(SetupError::Shutdown(signal));
            }
            _ => {}
        }
    }
}

pub(super) fn cycle(composer: &mut Composer) -> String {
    let Some(mode) = composer.approval_mode() else {
        return "Read only · use /permissions to trust this folder".into();
    };
    let mode = mode.next();
    composer.set_approval_mode(Some(mode));
    if mode == ApprovalMode::Auto {
        "Auto approval · the selected AI reviews actions; additional model usage may apply; uncertain actions ask you".into()
    } else {
        format!("{} · Shift+Tab changes mode", mode.label())
    }
}
