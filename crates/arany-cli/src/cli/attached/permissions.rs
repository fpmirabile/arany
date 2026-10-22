use super::setup::SetupError;
use crate::terminal::{AttachedTerminal, Composer, ProfileSelection, TerminalInput};
use arany::permissions::{
    Decision, PermissionSettings, SettingsError, TypedGitPrefix, TypedPrefixEdit,
};
use arany::{ApprovalMode, StateRoot, WorkspacePermissions, native_protection_supported};
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
            let path = serde_json::to_string(query.as_deref().unwrap_or_default())
                .expect("serializable path");
            composer.set_file_candidates(query, Vec::new());
            Some(format!("Path suggestions for {path} failed: {error}"))
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
    let access = match open_or_recover(terminal, workspace, "Exit").await? {
        TrustAdmission::Loaded(access) => access,
        TrustAdmission::Exit => return Ok(PermissionChoice::Exit),
    };
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

const BACK: u8 = 0;
const ADD: u8 = 1;
const ENTRY_BASE: u8 = 2;
const SAVED: &str = "Typed Git rules saved; they apply from the next Run";
const SETTINGS_NOTE: &str = "The first save creates permissions.json with the default mode, which also governs other Tools.";
const HOST_NOTE: &str = "unavailable; human approval required";

pub(super) enum PermissionArea {
    FolderTrust,
    TypedGitRules,
}

pub(super) async fn choose_area(
    terminal: &mut AttachedTerminal,
) -> Result<Option<PermissionArea>, SetupError> {
    Ok(
        match super::setup::choose_focused(
            terminal,
            1,
            "Permissions",
            "Choose what to manage",
            &[
                (b'1', "Folder trust"),
                (b'2', "Typed Git rules"),
                (b'0', "Cancel"),
            ],
            None,
        )
        .await?
        {
            Some(b'1') => Some(PermissionArea::FolderTrust),
            Some(b'2') => Some(PermissionArea::TypedGitRules),
            _ => None,
        },
    )
}

fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Ask => "ask",
        Decision::Deny => "deny",
    }
}

fn verb_label(prefix: TypedGitPrefix) -> String {
    if prefix.available() {
        prefix.label().to_owned()
    } else {
        format!("{} ({HOST_NOTE})", prefix.label())
    }
}

fn scope_label(folder: Option<&str>, workspace: &Path) -> String {
    match folder {
        None => "user-wide".into(),
        Some(path) if std::ffi::OsStr::new(path) == workspace.as_os_str() => "this folder".into(),
        Some(path) => format!("folder {}", crate::presentation::escape_terminal(path)),
    }
}

async fn pick<T: Copy>(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
    options: &[(T, String)],
) -> Result<Option<T>, SetupError> {
    let mut rows = options
        .iter()
        .enumerate()
        .map(|(index, (_, label))| {
            let index = u8::try_from(index).expect("pick lists are a few fixed options");
            (ENTRY_BASE + index, label.as_str())
        })
        .collect::<Vec<_>>();
    rows.push((BACK, "Back"));
    Ok(
        super::setup::choose_focused(terminal, 2, title, instruction, &rows, None)
            .await?
            .and_then(|value| value.checked_sub(ENTRY_BASE))
            .and_then(|index| options.get(usize::from(index)))
            .map(|(value, _)| *value),
    )
}

pub(super) async fn manage_rules(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    workspace: &Path,
) -> Result<String, SetupError> {
    let mut notice = None::<String>;
    let mut saved = false;
    let mut uncertain = false;
    loop {
        let Ok(state) = StateRoot::open_existing(state_dir) else {
            return Ok(
                "Error: state directory unavailable or unsafe; permissions unchanged".into(),
            );
        };
        let settings = match PermissionSettings::load(&state, workspace) {
            Ok(settings) => settings,
            Err(error) => return Ok(format!("Error: {error}; permissions unchanged")),
        };
        let entries = settings.typed_prefixes();
        let mut rows = vec![(ADD, "Add a rule".to_owned())];
        rows.extend(entries.iter().enumerate().map(|(index, entry)| {
            let index = u8::try_from(index).expect("typed rules are capped at 128");
            (
                ENTRY_BASE + index,
                format!(
                    "{} {} · {}",
                    decision_label(entry.decision),
                    verb_label(entry.prefix),
                    scope_label(entry.folder.as_deref(), workspace)
                ),
            )
        }));
        rows.push((BACK, "Back".to_owned()));
        let instruction = [
            notice.take().unwrap_or_else(|| "Typed Git rules.".into()),
            if settings.exists() {
                String::new()
            } else {
                SETTINGS_NOTE.into()
            },
            "Rules apply from the next Run.".into(),
        ]
        .join(" ");
        let labels = rows
            .iter()
            .map(|(value, label)| (*value, label.as_str()))
            .collect::<Vec<_>>();
        let choice = super::setup::choose_focused(
            terminal,
            2,
            "Typed Git rules",
            &instruction,
            &labels,
            None,
        )
        .await?;
        let edit = match choice {
            Some(ADD) => add_edit(terminal).await?,
            Some(value) if value >= ENTRY_BASE => {
                entry_edit(terminal, usize::from(value - ENTRY_BASE)).await?
            }
            _ => {
                return Ok(if uncertain {
                    "Typed Git rules may have changed; reopen /permissions to verify".into()
                } else if saved {
                    SAVED.into()
                } else {
                    "Typed Git rules unchanged".into()
                });
            }
        };
        let Some(edit) = edit else { continue };
        match settings.save_edit(&state, workspace, edit) {
            Ok(_) => {
                saved = true;
                notice = Some("Saved.".into());
            }
            Err(error) => {
                uncertain |= match error {
                    SettingsError::NotConfirmed => true,
                    SettingsError::Invalid
                    | SettingsError::Conflict
                    | SettingsError::Busy
                    | SettingsError::Missing
                    | SettingsError::Folder => false,
                };
                notice = Some(format!("Error: {error}."));
            }
        }
    }
}

async fn add_edit(terminal: &mut AttachedTerminal) -> Result<Option<TypedPrefixEdit>, SetupError> {
    let verbs = TypedGitPrefix::ALL
        .map(|prefix| (prefix, verb_label(prefix)))
        .to_vec();
    let Some(prefix) = pick(terminal, "Rule verb", "Choose the operation", &verbs).await? else {
        return Ok(None);
    };
    let Some(decision) = pick(
        terminal,
        "Rule decision",
        "Choose the decision",
        &decisions(),
    )
    .await?
    else {
        return Ok(None);
    };
    let scopes = [
        (true, "Current folder".to_owned()),
        (false, "User-wide".to_owned()),
    ];
    let Some(folder) = pick(
        terminal,
        "Rule scope",
        "Choose where the rule applies",
        &scopes,
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(TypedPrefixEdit::Insert {
        prefix,
        decision,
        folder,
    }))
}

fn decisions() -> Vec<(Decision, String)> {
    [Decision::Allow, Decision::Ask, Decision::Deny]
        .map(|decision| (decision, decision_label(decision).to_owned()))
        .to_vec()
}

async fn entry_edit(
    terminal: &mut AttachedTerminal,
    index: usize,
) -> Result<Option<TypedPrefixEdit>, SetupError> {
    let mut options = decisions()
        .into_iter()
        .map(|(decision, label)| (Some(decision), format!("Change to {label}")))
        .collect::<Vec<_>>();
    options.push((None, "Remove rule".into()));
    Ok(
        pick(terminal, "Rule", "Change or remove this rule", &options)
            .await?
            .map(|decision| match decision {
                Some(decision) => TypedPrefixEdit::Replace { index, decision },
                None => TypedPrefixEdit::Remove { index },
            }),
    )
}

pub(super) enum TrustAdmission {
    Loaded(WorkspacePermissions),
    Exit,
}

const RESET_WARNING: &str = "Reset deletes the remembered trust and runtime profile selections for every folder on this computer. Conversations, saved accounts and model choices are kept. Each folder asks for trust again. This cannot be undone.";

fn recovery_choices(resettable: bool, exit: &'static str) -> Vec<(u8, &'static str)> {
    let mut choices = vec![(b'1', "Retry")];
    if resettable {
        choices.push((b'2', "Reset folder trust"));
    }
    choices.push((b'0', exit));
    choices
}

// The reset pins the bytes shown in the dialog; confirmation deletes only those bytes.
pub(super) async fn open_or_recover(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    exit: &'static str,
) -> Result<TrustAdmission, SetupError> {
    let mut notice = None;
    loop {
        let error = match WorkspacePermissions::open(workspace) {
            Ok(access) => return Ok(TrustAdmission::Loaded(access)),
            Err(error) => error,
        };
        let reset = if error.is_resettable() {
            WorkspacePermissions::prepare_reset(workspace).ok()
        } else {
            None
        };
        let reason = notice.take().unwrap_or_else(|| format!("{error}."));
        let instruction = if reset.is_some() {
            format!(
                "{reason} Reset deletes only the remembered folder trust list; conversations, accounts and models are kept."
            )
        } else {
            reason
        };
        let choices = recovery_choices(reset.is_some(), exit);
        match super::setup::choose_focused(
            terminal,
            1,
            "Folder trust recovery",
            &instruction,
            &choices,
            None,
        )
        .await?
        {
            Some(b'1') => {}
            Some(b'2') => {
                if let Some(reset) = reset {
                    let confirmed = super::setup::review_warning(
                        terminal,
                        "Reset folder trust?",
                        RESET_WARNING,
                        "Delete remembered folder trust?",
                    )
                    .await?;
                    if confirmed && let Err(error) = reset.apply() {
                        notice = Some(format!("Reset did not complete: {error}; retry."));
                    }
                }
            }
            _ => return Ok(TrustAdmission::Exit),
        }
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
            TerminalInput::Up if more || (selected == 0 && page > 0) => {
                page = page.saturating_sub(1);
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
            // The cache under HOME overlaps the workspace, so selecting it must reject saving.
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
                        let answer = std::fs::read(temp.path().join("answer")).unwrap();
                        assert!((1..=32).contains(&answer.len()));
                        input.write_all(&answer).unwrap();
                        answered = true;
                    }
                },
            );
            assert!(
                output.status.success(),
                "rejected save child: stdout={:?}; stderr={:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
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
            let mut terminal = AttachedTerminal::acquire_with_mode(
                true,
                TerminalMode::default(),
                Default::default(),
            )
            .unwrap();
            let mut composer = Composer::default();
            let access = WorkspacePermissions::open(&workspace)
                .unwrap()
                .prepare_profile_selection();
            let cache = workspace.join(".cargo/registry/cache");
            let index = access
                .runtime_profile_choices()
                .iter()
                .position(|option| option.detected && option.path == cache)
                .expect("detected overlapping Cargo cache candidate");
            // Choices 1 and 2 are the fixed read-only and trust entries; profiles start at 3.
            const FIRST_PROFILE_NUMBER: usize = 3;
            std::fs::write(
                root.join("answer"),
                format!("trust {}\r", index + FIRST_PROFILE_NUMBER),
            )
            .unwrap();
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

    #[test]
    fn trust_recovery_offers_reset_only_for_resettable_causes() {
        use arany::TrustRecordError;

        let labels = |choices: Vec<(u8, &str)>| {
            choices
                .into_iter()
                .map(|(_, label)| label.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(super::recovery_choices(true, "Exit")),
            ["Retry", "Reset folder trust", "Exit"]
        );
        assert_eq!(
            labels(super::recovery_choices(false, "Cancel")),
            ["Retry", "Cancel"]
        );
        assert!(!TrustRecordError::RecordNotPrivate.is_resettable());
        assert!(TrustRecordError::UnsupportedVersion(1).is_resettable());
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
