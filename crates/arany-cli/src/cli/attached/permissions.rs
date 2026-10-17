use super::setup::SetupError;
use crate::terminal::{AttachedTerminal, Composer, ProfileSelection, TerminalInput};
use arany::{ApprovalMode, WorkspacePermissions, native_protection_supported};
use std::path::Path;

pub(super) enum PermissionChoice {
    Selected(WorkspacePermissions),
    Closed(WorkspacePermissions),
    Rejected(WorkspacePermissions, String),
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
        composer.set_approval_mode(native_protection_supported().then_some(access.mode()));
        Ok(PermissionChoice::Selected(access))
    } else {
        choose(
            terminal,
            access.prepare_profile_selection(),
            composer,
            configured,
        )
        .await
    }
}

pub(super) async fn choose(
    terminal: &mut AttachedTerminal,
    mut permissions: WorkspacePermissions,
    composer: &mut Composer,
    configured: bool,
) -> Result<PermissionChoice, SetupError> {
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
    let mut explanation_pages_seen = false;
    loop {
        let (more, visible_page) = terminal.draw_workspace_permissions(
            id,
            permissions.path(),
            ProfileSelection {
                options: &options,
                selected: &selected_profiles,
                focused: focused_profile,
                enabled: explanation_pages_seen,
                discovery_error: permissions.discovery_error(),
            },
            page,
            selected == 1,
            configured,
        )?;
        page = visible_page;
        if focused_profile.is_none() && !more {
            explanation_pages_seen = true;
        }
        match terminal.next_input().await? {
            TerminalInput::Up if explanation_pages_seen && focused_profile.is_some() => {
                focused_profile = focused_profile.and_then(|index| index.checked_sub(1));
            }
            TerminalInput::Down if explanation_pages_seen && focused_profile.is_some() => {
                let index = focused_profile.expect("matched selected profile");
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
            TerminalInput::Tab if !explanation_pages_seen => {}
            TerminalInput::Tab if explanation_pages_seen && focused_profile.is_some() => {
                focused_profile = None;
            }
            TerminalInput::Tab if explanation_pages_seen && !options.is_empty() => {
                focused_profile = Some(0)
            }
            TerminalInput::Tab if explanation_pages_seen && !more => selected = (selected + 1) % 2,
            TerminalInput::Character(' ') if !terminal.is_linear() => {
                if let Some(index) = focused_profile {
                    toggle_profile(&mut selected_profiles, &options, index);
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
                explanation_pages_seen = false;
            }
            TerminalInput::Submit if explanation_pages_seen && focused_profile.is_some() => {
                let index = focused_profile.expect("matched selected profile");
                toggle_profile(&mut selected_profiles, &options, index);
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
                if let Err(error) = permissions.remember_profiles(
                    selected == 1,
                    ApprovalMode::AutoEdits,
                    &selected_tokens,
                ) {
                    terminal.restore_draft_input(composer.text().len())?;
                    return Ok(PermissionChoice::Rejected(
                        permissions,
                        format!("Could not remember folder trust: {error}"),
                    ));
                }
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
                explanation_pages_seen = false;
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
    let mut words = input
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if words
        .first()
        .and_then(|word| word.parse::<usize>().ok())
        .is_some_and(|number| number >= 3)
    {
        words.insert(0, "trust".into());
    }
    let words = words.iter().map(String::as_str).collect::<Vec<_>>();
    match words.as_slice() {
        []
        | ["1" | "read" | "read-only" | "no" | "config" | "use-config"]
        | ["read", "only"]
        | ["use", "config"] => Some((0, vec![false; options.len()])),
        ["2" | "trust" | "yes"] => Some((1, current.to_vec())),
        ["2" | "trust" | "yes", "none"] => Some((1, vec![false; options.len()])),
        ["2" | "trust" | "yes", numbers @ ..] => {
            let mut selected = vec![false; options.len()];
            for number in numbers {
                let index = number.parse::<usize>().ok()?.checked_sub(3)?;
                let option = options.get(index)?;
                if selected
                    .iter()
                    .zip(options)
                    .any(|(chosen, other)| *chosen && other.group == option.group)
                {
                    return None;
                }
                selected[index] = true;
            }
            Some((1, selected))
        }
        _ => None,
    }
}

/// Keeps at most one profile per runtime kind; a checked profile replaces its siblings.
fn toggle_profile(selected: &mut [bool], options: &[arany::RuntimeProfileOption], index: usize) {
    let check = !selected[index];
    let group = options[index].group;
    for (chosen, option) in selected.iter_mut().zip(options) {
        if option.group == group {
            *chosen = false;
        }
    }
    selected[index] = check;
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

#[cfg(test)]
mod tests {
    use super::{parse_linear_selection, toggle_profile};

    #[cfg(all(unix, debug_assertions))]
    #[test]
    fn rejected_save_keeps_the_trust_record_and_reports_the_error() {
        use super::{PermissionChoice, choose};
        use crate::terminal::{AttachedTerminal, Composer, TerminalMode};
        use arany::WorkspacePermissions;
        use std::io::Write;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        const FIXTURE: &str = "ARANY_TEST_PERMISSION_REJECTION_ROOT";
        const TEST: &str = "cli::attached::permissions::tests::rejected_save_keeps_the_trust_record_and_reports_the_error";
        let Some(root) = std::env::var_os(FIXTURE) else {
            let temp = tempfile::tempdir().unwrap();
            // The only candidate is the cache under HOME, which is also the workspace: saving it overlaps.
            std::fs::create_dir_all(temp.path().join("home/.cargo/registry/cache")).unwrap();
            let (master, slave) = crate::test_process::cloexec_pty(None);
            let mut input = master.try_clone().unwrap();
            let mut answered = false;
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .env_clear()
                .env(FIXTURE, temp.path())
                .env("HOME", temp.path().join("home"))
                .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
                .env("TERM", "dumb")
                .current_dir(temp.path())
                .args(["--exact", TEST, "--nocapture"])
                .process_group(0)
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::piped())
                .stderr(Stdio::from(slave));
            let mut child = command.spawn().unwrap();
            drop(command);
            let output = crate::test_process::capture_terminal(
                &mut child,
                master,
                Duration::from_secs(30),
                64 * 1024,
                |bytes| {
                    if !answered
                        && String::from_utf8_lossy(bytes)
                            .replace("\r\n", "\n")
                            .contains(crate::test_process::READ_ONLY_CONSENT_PROMPT)
                    {
                        input.write_all(b"trust 3\r").unwrap();
                        answered = true;
                    }
                },
            );
            assert!(
                output.status.success(),
                "rejected save child: {:?}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(answered, "linear consent prompt was not shown");
            crate::test_process::assert_single_test_completed(&output.stdout, "rejected save");
            return;
        };
        let root = std::path::PathBuf::from(root);
        let workspace = root.join("home");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut terminal =
                AttachedTerminal::acquire_with_mode(true, TerminalMode::default()).unwrap();
            let mut composer = Composer::default();
            let access = WorkspacePermissions::open(&workspace)
                .unwrap()
                .prepare_profile_selection();
            let choice = match choose(&mut terminal, access, &mut composer, false).await {
                Ok(choice) => choice,
                Err(_) => panic!("a failed save must not abort the chooser"),
            };
            let PermissionChoice::Rejected(permissions, message) = choice else {
                panic!("a failed save must return Rejected");
            };
            assert!(
                message.contains("Could not remember folder trust"),
                "{message}"
            );
            assert!(!permissions.is_trusted());
            assert!(
                !root.join("account/workspace-permissions.json").exists(),
                "a rejected save must not write the trust record"
            );
        });
    }

    fn option(token: &str, group: &'static str) -> arany::RuntimeProfileOption {
        arany::RuntimeProfileOption {
            token: token.into(),
            label: token.into(),
            group,
            version: None,
            path: std::path::PathBuf::from("/opt/profile"),
            detected: true,
            selected: false,
        }
    }

    #[test]
    fn linear_permission_read_only_phrase_is_exact_and_clears_profiles() {
        let options = [option("rust-example", "Rust")];
        let selected = [true];

        assert_eq!(
            parse_linear_selection("read only", &options, &selected),
            Some((0, vec![false]))
        );
        for input in ["read only 3", "read only trust", "read only none"] {
            assert_eq!(parse_linear_selection(input, &options, &selected), None);
        }
        assert_eq!(
            parse_linear_selection("trust 3", &options, &selected),
            Some((1, vec![true]))
        );
    }

    #[test]
    fn linear_permission_accepts_config_phrase_and_bare_profile_numbers() {
        let options = [option("node-a", "Node"), option("rust", "Rust")];
        let none = [false, false];

        assert_eq!(
            parse_linear_selection("use config", &options, &none),
            Some((0, vec![false; 2]))
        );
        assert_eq!(
            parse_linear_selection("4", &options, &none),
            Some((1, vec![false, true]))
        );
        assert_eq!(
            parse_linear_selection("trust", &options, &[true, false]),
            Some((1, vec![true, false]))
        );
        assert_eq!(parse_linear_selection("trust 6", &options, &none), None);
    }

    #[test]
    fn linear_permission_rejects_two_profiles_of_one_runtime_kind() {
        let options = [
            option("node-v20", "Node"),
            option("node-v22", "Node"),
            option("rust", "Rust"),
        ];
        let none = [false; 3];

        assert_eq!(parse_linear_selection("trust 3 4", &options, &none), None);
        assert_eq!(
            parse_linear_selection("trust 4 5", &options, &none),
            Some((1, vec![false, true, true]))
        );
    }

    #[test]
    fn inline_profile_toggle_replaces_sibling_of_the_same_runtime_kind() {
        let options = [
            option("node-v20", "Node"),
            option("node-v22", "Node"),
            option("rust", "Rust"),
        ];
        let mut selected = [false; 3];

        toggle_profile(&mut selected, &options, 0);
        toggle_profile(&mut selected, &options, 1);
        assert_eq!(selected, [false, true, false]);
        toggle_profile(&mut selected, &options, 2);
        assert_eq!(selected, [false, true, true]);
        toggle_profile(&mut selected, &options, 1);
        assert_eq!(selected, [false, false, true]);
    }
}
