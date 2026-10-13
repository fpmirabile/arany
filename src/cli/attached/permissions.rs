use super::setup::SetupError;
use arany::{
    ApprovalMode, AttachedTerminal, Composer, TerminalInput, WorkspacePermissions,
    native_protection_supported,
};
use std::path::Path;

pub(super) enum PermissionChoice {
    Selected(WorkspacePermissions),
    Closed(WorkspacePermissions),
    Exit,
}

pub(super) const UNAVAILABLE_NOTICE: &str =
    "Native Tool protection is unavailable on this OS; chat is read only.";

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
    native_protection_supported() && matches!(profile, Some("openai" | "anthropic" | "chatgpt"))
}

pub(super) fn refresh_mode(
    composer: &mut Composer,
    profile: Option<&str>,
    configured: bool,
    permissions: Option<&WorkspacePermissions>,
) {
    if !supports_tools(profile)
        || (!configured && !permissions.is_some_and(WorkspacePermissions::is_trusted))
    {
        composer.set_approval_mode(None);
    } else if composer.approval_mode().is_none() {
        composer.set_approval_mode(Some(if configured {
            ApprovalMode::AutoEdits
        } else {
            permissions.expect("trusted permissions").mode()
        }));
    }
}

pub(super) async fn admit(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    composer: &mut Composer,
    configured: bool,
) -> Result<PermissionChoice, SetupError> {
    composer.set_approval_mode(None);
    let access = WorkspacePermissions::open(workspace).map_err(|error| {
        SetupError::Recoverable(format!("Could not load folder trust: {error}"))
    })?;
    if access.is_trusted() {
        access.verify_runtime_grants().map_err(|error| {
            SetupError::Recoverable(format!(
                "Could not validate folder runtime profiles: {error}"
            ))
        })?;
        composer.set_approval_mode(native_protection_supported().then_some(access.mode()));
        Ok(PermissionChoice::Selected(access))
    } else {
        choose(terminal, workspace, composer, configured).await
    }
}

pub(super) async fn choose(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    composer: &mut Composer,
    configured: bool,
) -> Result<PermissionChoice, SetupError> {
    let mut permissions = WorkspacePermissions::open(workspace).map_err(|error| {
        SetupError::Recoverable(format!("Could not load folder trust: {error}"))
    })?;
    let mut selected = 0usize;
    let mut page = 0usize;
    let id = uuid::Uuid::now_v7();
    let mut answer = Composer::default();
    let options = permissions.runtime_profile_choices();
    let mut selected_profiles = options
        .iter()
        .map(|option| option.selected)
        .collect::<Vec<_>>();
    let mut focused_profile = None;
    loop {
        let more = terminal.draw_workspace_permissions(
            id,
            &permissions,
            (&selected_profiles, focused_profile),
            page,
            selected == 1,
            configured,
        )?;
        match terminal.next_input().await? {
            TerminalInput::Up if focused_profile.is_some() => {
                focused_profile = focused_profile.and_then(|index| index.checked_sub(1));
                if focused_profile.is_none() {
                    page = usize::MAX;
                }
            }
            TerminalInput::Down if let Some(index) = focused_profile => {
                focused_profile = Some((index + 1).min(options.len().saturating_sub(1)));
            }
            TerminalInput::Up if !more => selected = selected.saturating_sub(1),
            TerminalInput::Down if !more => selected = (selected + 1).min(1),
            TerminalInput::PageUp => {
                page = page.saturating_sub(1);
                selected = 0;
                focused_profile = None;
            }
            TerminalInput::Down | TerminalInput::PageDown if more => page += 1,
            TerminalInput::Tab if focused_profile.is_some() => {
                focused_profile = None;
                page = usize::MAX;
            }
            TerminalInput::Tab if !options.is_empty() => focused_profile = Some(0),
            TerminalInput::Tab if !more => selected = (selected + 1) % 2,
            TerminalInput::Character(' ') if !terminal.is_linear() => {
                if let Some(index) = focused_profile {
                    selected_profiles[index] = !selected_profiles[index];
                    selected = 1;
                }
            }
            TerminalInput::Character(character) if terminal.is_linear() => {
                answer.apply(TerminalInput::Character(character));
            }
            TerminalInput::Backspace if terminal.is_linear() => {
                answer.apply(TerminalInput::Backspace);
            }
            TerminalInput::Resize => {
                page = 0;
                selected = 0;
                focused_profile = None;
            }
            TerminalInput::Submit if focused_profile.is_some() => {
                let index = focused_profile.expect("matched selected profile");
                selected_profiles[index] = !selected_profiles[index];
                selected = 1;
            }
            TerminalInput::Submit if more => page += 1,
            TerminalInput::Submit => {
                if terminal.is_linear() {
                    let Some((choice, profiles)) =
                        parse_linear_selection(answer.text(), &options, &selected_profiles)
                    else {
                        answer.clear();
                        continue;
                    };
                    selected = choice;
                    selected_profiles = profiles;
                }
                if selected == 0 {
                    selected_profiles.fill(false);
                }
                let selected_tokens = options
                    .iter()
                    .zip(selected_profiles.iter().copied())
                    .filter(|(_, selected)| *selected)
                    .map(|(option, _)| option.token.clone())
                    .collect::<Vec<_>>();
                permissions
                    .remember_profiles(selected == 1, ApprovalMode::AutoEdits, &selected_tokens)
                    .map_err(|error| {
                        SetupError::Recoverable(format!("Could not remember folder trust: {error}"))
                    })?;
                composer.set_approval_mode(
                    (native_protection_supported() && permissions.is_trusted())
                        .then_some(permissions.mode()),
                );
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
                focused_profile = None;
            }
            TerminalInput::Shutdown(signal) => {
                return Err(SetupError::Shutdown(signal));
            }
            _ => {}
        }
    }
}

fn parse_linear_selection(
    input: &str,
    options: &[arany::RuntimeProfileOption],
    current: &[bool],
) -> Option<(usize, Vec<bool>)> {
    let mut words = input.split_whitespace();
    let choice = words.next().unwrap_or_default().to_ascii_lowercase();
    let rest = words.collect::<Vec<_>>();
    match choice.as_str() {
        "" | "1" | "read" | "read-only" | "no" | "config" | "use-config" => {
            rest.is_empty().then(|| (0, vec![false; options.len()]))
        }
        "2" | "trust" | "yes" => {
            if rest.is_empty() {
                return Some((1, current.to_vec()));
            }
            let mut selected = vec![false; options.len()];
            if rest == ["none"] {
                return Some((1, selected));
            }
            let mut seen = std::collections::BTreeSet::new();
            for word in rest {
                let number = word.parse::<usize>().ok()?;
                let index = number.checked_sub(3)?;
                if index >= options.len() || !seen.insert(index) {
                    return None;
                }
                selected[index] = true;
            }
            Some((1, selected))
        }
        _ => None,
    }
}

pub(super) fn cycle(composer: &mut Composer) -> String {
    if !native_protection_supported() {
        return UNAVAILABLE_NOTICE.into();
    }
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
