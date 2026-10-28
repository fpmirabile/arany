mod command_tools;
pub(super) use command_tools::manage_command_tools;

use super::setup::SetupError;
use crate::terminal::{AttachedTerminal, Composer, ProfileSelection, TerminalInput};
use arany::permissions::{
    Decision, NetworkDomainEdit, PermissionSettings, SettingsError, TypedGitPrefix, TypedPrefixEdit,
};
use arany::{
    ApprovalMode, CommandEnvironment, CommandEnvironmentError, ProviderKind, StateRoot,
    WorkspacePermissions, native_protection_supported,
};
use std::path::Path;
mod capture;
pub(super) use capture::EnvironmentSession;

pub(super) enum PermissionChoice {
    Selected(WorkspacePermissions),
    Closed(WorkspacePermissions),
    Rejected(WorkspacePermissions, String),
    Exit,
}

pub(in crate::cli) const UNAVAILABLE_NOTICE: &str =
    "Native Tool protection is unavailable on this OS; chat is read only.";

pub(in crate::cli) fn supports_tools(profile: Option<&str>) -> bool {
    native_protection_supported()
        && profile
            .and_then(ProviderKind::parse)
            .is_some_and(ProviderKind::is_native)
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
    state_dir: &Path,
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
            access.prepare_profile_selection(state_dir),
            composer,
            configured,
        )
        .await
    }
}

const BACK: u8 = 0;
const ADD: u8 = 1;
const ENTRY_BASE: u8 = 2;
const CAPTURE: u8 = u8::MAX;
const SAVED: &str = "Typed Git rules saved; they apply from the next Run";
const SETTINGS_NOTE: &str = "The first save creates permissions.json with the default mode, which also governs other Tools.";
const HOST_NOTE: &str = "host; human approval required";
const HOST_READ_DISCLOSURE: &str =
    "This host read can send private pull request data to the current Provider.";

pub(super) enum PermissionArea {
    FolderTrust,
    TypedGitRules,
    HostRules,
    CommandEnvironment,
    CommandTools,
    NetworkDomains,
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
                (b'3', "Host command rules"),
                (b'4', "Command environment"),
                (b'5', "Command tools"),
                (b'6', "Network domains"),
                (b'0', "Cancel"),
            ],
            None,
        )
        .await?
        {
            Some(b'1') => Some(PermissionArea::FolderTrust),
            Some(b'2') => Some(PermissionArea::TypedGitRules),
            Some(b'3') => Some(PermissionArea::HostRules),
            Some(b'4') => Some(PermissionArea::CommandEnvironment),
            Some(b'5') => Some(PermissionArea::CommandTools),
            Some(b'6') => Some(PermissionArea::NetworkDomains),
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
    if matches!(prefix, TypedGitPrefix::PrList | TypedGitPrefix::PrView) {
        format!("{} (host; human approval or typed allow)", prefix.label())
    } else if !matches!(prefix, TypedGitPrefix::Push | TypedGitPrefix::PrCreate) {
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
                let index = usize::from(value - ENTRY_BASE);
                entry_edit(terminal).await?.map(|edit| match edit {
                    EntryEdit::Change(decision) => TypedPrefixEdit::Replace { index, decision },
                    EntryEdit::Remove => TypedPrefixEdit::Remove { index },
                })
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
        if requires_host_read_disclosure(&edit, &entries)
            && !confirm_host_read_disclosure(terminal).await?
        {
            notice = Some("Allow rule not saved.".into());
            continue;
        }
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

pub(super) enum EnvironmentOutcome {
    Confirmed(String),
    Uncertain,
}

enum EnvironmentAction {
    Capture,
    Select(String),
    Reveal(String),
    Remove(String),
}

enum EnvironmentStep {
    Act(EnvironmentAction),
    Again,
    Leave,
}

pub(super) async fn manage_command_environment(
    terminal: &mut AttachedTerminal,
    mut session: EnvironmentSession<'_>,
) -> Result<EnvironmentOutcome, SetupError> {
    let mut notice = None::<String>;
    let mut saved = false;
    let mut last_failure = None::<String>;
    loop {
        let Ok(state) = StateRoot::open_existing(&session.admission.state_dir) else {
            return Ok(EnvironmentOutcome::Confirmed(
                "Error: state directory unavailable or unsafe; command environment unchanged"
                    .into(),
            ));
        };
        let environment = match open_environment(&state) {
            Ok(environment) => environment,
            Err(outcome) => return Ok(outcome),
        };
        let names: Vec<String> = environment
            .names()
            .map(|(name, _)| name.to_owned())
            .collect();
        let choice = choose_environment(
            terminal,
            &environment,
            &names,
            notice.take().unwrap_or_default(),
        )
        .await?;
        let action = match environment_step(terminal, choice, &names).await? {
            EnvironmentStep::Act(action) => action,
            EnvironmentStep::Again => continue,
            EnvironmentStep::Leave => {
                return Ok(EnvironmentOutcome::Confirmed(leave_notice(
                    saved,
                    last_failure,
                )));
            }
        };
        let outcome = match action {
            EnvironmentAction::Capture => {
                let summary = session.capture(terminal).await?;
                let summary = match last_failure {
                    Some(failure) => format!("{summary}. Earlier edit failed: {failure}"),
                    None => summary,
                };
                return Ok(EnvironmentOutcome::Confirmed(summary));
            }
            EnvironmentAction::Select(name) => {
                CommandEnvironment::edit(&state, |record| record.select(&name)).map(|_| "Selected.")
            }
            EnvironmentAction::Remove(name) => {
                CommandEnvironment::edit(&state, |record| record.deselect(&name))
                    .map(|_| "Removed.")
            }
            EnvironmentAction::Reveal(name) => {
                let value = environment
                    .names()
                    .find_map(|(candidate, value)| (candidate == name).then_some(value))
                    .unwrap_or_default();
                notice = Some(format!(
                    "{name}={}.",
                    crate::presentation::escape_terminal(value)
                ));
                continue;
            }
        };
        match settle_environment(outcome) {
            Ok(done) => {
                saved |= done.saved;
                last_failure = (!done.saved).then(|| done.notice.clone());
                notice = Some(done.notice);
            }
            Err(outcome) => return Ok(outcome),
        }
    }
}

pub(super) async fn manage_network_rules(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    workspace: &Path,
    view: &arany::SessionView,
) -> Result<String, SetupError> {
    let mut notice = "Domain rules extend a private tools.json network grant; they apply from the next Run. An ask or deny rule removes the domain. HTTPS only; allowed hosts can still receive data.".to_owned();
    let mut saved = false;
    loop {
        let Ok(state) = StateRoot::open_existing(state_dir) else {
            return Ok(
                "Error: state directory unavailable or unsafe; network rules unchanged".into(),
            );
        };
        let settings = match PermissionSettings::load(&state, workspace) {
            Ok(settings) => settings,
            Err(error) => return Ok(format!("Error: {error}; network rules unchanged")),
        };
        let mut rows = vec![(ADD, "Add a domain refused in this conversation".to_owned())];
        rows.extend(
            settings
                .network_domains()
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    (
                        ENTRY_BASE + u8::try_from(index).expect("network rules are capped"),
                        format!(
                            "{} {}",
                            decision_label(entry.decision),
                            crate::presentation::escape_terminal(&entry.domain)
                        ),
                    )
                }),
        );
        rows.push((BACK, "Back".to_owned()));
        let labels = rows
            .iter()
            .map(|(value, label)| (*value, label.as_str()))
            .collect::<Vec<_>>();
        let instruction = [if settings.exists() { "" } else { SETTINGS_NOTE }, &notice].join(" ");
        let choice = super::setup::choose_focused(
            terminal,
            2,
            "Network domains",
            &instruction,
            &labels,
            None,
        )
        .await?;
        let edit = match choice {
            Some(ADD) => add_network_edit(terminal, view).await?,
            Some(value) if value >= ENTRY_BASE => {
                let index = usize::from(value - ENTRY_BASE);
                entry_edit(terminal).await?.map(|edit| match edit {
                    EntryEdit::Change(decision) => NetworkDomainEdit::Replace { index, decision },
                    EntryEdit::Remove => NetworkDomainEdit::Remove { index },
                })
            }
            _ => {
                return Ok(if saved {
                    "Network domain rules saved; they apply from the next Run".into()
                } else {
                    "Network domain rules unchanged".into()
                });
            }
        };
        let Some(edit) = edit else { continue };
        match settings.save_network_edit(&state, workspace, edit) {
            Ok(_) => {
                saved = true;
                notice = "Saved.".into();
            }
            Err(SettingsError::NotConfirmed) => {
                return Ok(
                    "Network domain rules may have changed; reopen /permissions to verify".into(),
                );
            }
            Err(error) => notice = format!("Error: {error}."),
        }
    }
}

struct EnvironmentSettled {
    saved: bool,
    notice: String,
}

fn leave_notice(saved: bool, last_failure: Option<String>) -> String {
    let base = if saved {
        "Command environment saved; it applies from the next Run"
    } else {
        "Command environment unchanged"
    };
    match last_failure {
        Some(failure) => format!("{base}. {failure}"),
        None => base.into(),
    }
}

fn open_environment(state: &StateRoot) -> Result<CommandEnvironment, EnvironmentOutcome> {
    CommandEnvironment::load(state).map_err(|error| match error {
        arany::ToolError::CommandEnvironmentUnconfirmed => EnvironmentOutcome::Uncertain,
        error => {
            EnvironmentOutcome::Confirmed(format!("Error: {error}; command environment unchanged"))
        }
    })
}

fn settle_environment(
    outcome: Result<&'static str, CommandEnvironmentError>,
) -> Result<EnvironmentSettled, EnvironmentOutcome> {
    match outcome {
        Ok(done) => Ok(EnvironmentSettled {
            saved: true,
            notice: done.to_owned(),
        }),
        Err(CommandEnvironmentError::Unconfirmed) => Err(EnvironmentOutcome::Uncertain),
        Err(error) => Ok(EnvironmentSettled {
            saved: false,
            notice: format!("Error: {error}."),
        }),
    }
}

impl EnvironmentOutcome {
    pub(super) fn summary(self) -> String {
        match self {
            Self::Confirmed(summary) => summary,
            Self::Uncertain => {
                "Command environment save is uncertain; reopen /permissions to verify.".into()
            }
        }
    }
}

async fn choose_environment(
    terminal: &mut AttachedTerminal,
    environment: &CommandEnvironment,
    names: &[String],
    notice: String,
) -> Result<Option<u8>, SetupError> {
    let mut rows = vec![(
        ADD,
        "Select a name from the Arany process environment".to_owned(),
    )];
    rows.extend(names.iter().enumerate().map(|(index, name)| {
        let index = u8::try_from(index).expect("selected names are capped");
        (ENTRY_BASE + index, format!("{name} · value hidden"))
    }));
    rows.push((BACK, "Back".to_owned()));
    rows.insert(
        1,
        (
            CAPTURE,
            if environment.shell().is_some() {
                "Re-capture from login shell"
            } else {
                "Capture from login shell"
            }
            .to_owned(),
        ),
    );
    let labels = rows
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let instruction = format!(
        "{notice} Source {}{}. PATH {}. Names matching *KEY*, *SECRET* or *TOKEN* are never selectable. Changes apply from the next Run.",
        environment.source(),
        environment
            .shell()
            .map(|shell| format!(
                " · shell {} · SHA-256 {}",
                crate::presentation::escape_terminal(&shell.path),
                shell.sha256
            ))
            .unwrap_or_default(),
        crate::presentation::escape_terminal(&environment.path().join(", ")),
    );
    super::setup::choose_focused(
        terminal,
        2,
        "Command environment",
        &instruction,
        &labels,
        None,
    )
    .await
}

async fn environment_step(
    terminal: &mut AttachedTerminal,
    choice: Option<u8>,
    names: &[String],
) -> Result<EnvironmentStep, SetupError> {
    Ok(match choice {
        Some(CAPTURE) => EnvironmentStep::Act(EnvironmentAction::Capture),
        Some(ADD) => {
            match super::setup::read_setup_value(
                terminal,
                "Command environment",
                "Variable name",
                "",
                CommandEnvironment::NAME_LIMIT,
            )
            .await?
            {
                Some(name) => EnvironmentStep::Act(EnvironmentAction::Select(name)),
                None => EnvironmentStep::Again,
            }
        }
        Some(value) if value >= ENTRY_BASE => {
            let name = names[usize::from(value - ENTRY_BASE)].clone();
            let options = [
                (true, "Reveal value".to_owned()),
                (false, "Remove name".to_owned()),
            ];
            match pick(terminal, "Variable", "Reveal or remove this name", &options).await? {
                Some(true) => EnvironmentStep::Act(EnvironmentAction::Reveal(name)),
                Some(false) => EnvironmentStep::Act(EnvironmentAction::Remove(name)),
                None => EnvironmentStep::Again,
            }
        }
        _ => EnvironmentStep::Leave,
    })
}

async fn add_network_edit(
    terminal: &mut AttachedTerminal,
    view: &arany::SessionView,
) -> Result<Option<NetworkDomainEdit>, SetupError> {
    let mut seen = std::collections::HashSet::new();
    let domains = view
        .runs
        .iter()
        .rev()
        .flat_map(|run| run.tools.iter().rev())
        .filter_map(|tool| tool.observation.as_ref())
        .flat_map(arany::ToolObservation::refused_domains)
        .filter(|domain| seen.insert(domain.clone()))
        .take(arany::permissions::MAX_RULES)
        .collect::<Vec<_>>();
    let options = domains
        .iter()
        .enumerate()
        .map(|(index, domain)| (index, crate::presentation::escape_terminal(domain)))
        .collect::<Vec<_>>();
    let Some(index) = pick(
        terminal,
        "Refused domain",
        "Choose a domain a command could not reach. Allowing it lets commands send data to that host over HTTPS.",
        &options,
    )
    .await?
    else {
        return Ok(None);
    };
    let domain = domains[index].clone();
    Ok(pick(
        terminal,
        "Domain decision",
        "Choose a decision",
        &decisions(),
    )
    .await?
    .map(|decision| NetworkDomainEdit::Insert { domain, decision }))
}

pub(super) enum HostRuleEditOutcome {
    Confirmed(String),
    Uncertain,
}

pub(super) async fn manage_host_rules(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    workspace: &Path,
    view: &arany::SessionView,
) -> Result<HostRuleEditOutcome, SetupError> {
    let mut notice = "Host rules authorize every eligible suffix in this folder and exact cwd. They apply next Run. This is not confinement.".to_owned();
    let mut saved = false;
    let mut last_error = None;
    loop {
        let Ok(state) = StateRoot::open_existing(state_dir) else {
            return Ok(HostRuleEditOutcome::Confirmed(host_rule_edit_notice(
                Some("state unavailable"),
                saved,
            )));
        };
        let settings = match PermissionSettings::load(&state, workspace) {
            Ok(settings) => settings,
            Err(error) => {
                return Ok(HostRuleEditOutcome::Confirmed(host_rule_edit_notice(
                    Some(&error.to_string()),
                    saved,
                )));
            }
        };
        let choice = choose_host_rule(terminal, &settings, workspace, &notice).await?;
        let edit = match choice {
            Some(ADD) => add_host_edit(terminal, view, workspace).await?,
            Some(value) if value >= ENTRY_BASE => {
                let index = usize::from(value - ENTRY_BASE);
                entry_edit(terminal).await?.map(|edit| match edit {
                    EntryEdit::Change(decision) => {
                        arany::permissions::HostPrefixEdit::Replace { index, decision }
                    }
                    EntryEdit::Remove => arany::permissions::HostPrefixEdit::Remove { index },
                })
            }
            _ => {
                return Ok(HostRuleEditOutcome::Confirmed(host_rule_edit_notice(
                    last_error.as_deref(),
                    saved,
                )));
            }
        };
        let Some(edit) = edit else { continue };
        match settings.save_host_edit(&state, workspace, edit) {
            Ok(_) => {
                saved = true;
                last_error = None;
                notice = host_rule_edit_notice(None, saved);
            }
            Err(SettingsError::NotConfirmed) => return Ok(HostRuleEditOutcome::Uncertain),
            Err(error) => {
                last_error = Some(error.to_string());
                notice = host_rule_edit_notice(last_error.as_deref(), saved);
            }
        }
    }
}

async fn choose_host_rule(
    terminal: &mut AttachedTerminal,
    settings: &PermissionSettings,
    workspace: &Path,
    notice: &str,
) -> Result<Option<u8>, SetupError> {
    let mut rows = vec![(ADD, "Add from a recent eligible host command".to_owned())];
    rows.extend(
        settings
            .host_prefixes()
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                (
                    ENTRY_BASE + u8::try_from(index).expect("rules bounded"),
                    format!(
                        "{} {} · {} · cwd {}",
                        decision_label(rule.decision),
                        crate::presentation::escape_terminal(
                            &serde_json::to_string(&rule.prefix).expect("prefix")
                        ),
                        scope_label(Some(rule.workspace()), workspace),
                        crate::presentation::escape_terminal(&rule.cwd)
                    ),
                )
            }),
    );
    rows.push((BACK, "Back".into()));
    let labels = rows
        .iter()
        .map(|(key, label)| (*key, label.as_str()))
        .collect::<Vec<_>>();
    let instruction = [if settings.exists() { "" } else { SETTINGS_NOTE }, notice].join(" ");
    super::setup::choose_focused(
        terminal,
        2,
        "Host command rules",
        &instruction,
        &labels,
        None,
    )
    .await
}

fn host_rule_edit_notice(reason: Option<&str>, saved: bool) -> String {
    let Some(reason) = reason else {
        return if saved {
            "Host command rules saved; they apply from the next Run".into()
        } else {
            "Host command rules unchanged".into()
        };
    };
    let status = if saved {
        "earlier host rule edits were saved; reopen /permissions to verify their current state"
    } else {
        "host rules unchanged"
    };
    format!("Error: {reason}; {status}")
}

async fn add_host_edit(
    terminal: &mut AttachedTerminal,
    view: &arany::SessionView,
    workspace: &Path,
) -> Result<Option<arany::permissions::HostPrefixEdit>, SetupError> {
    let candidates = host_candidates(
        view.runs
            .iter()
            .rev()
            .flat_map(|run| run.tools.iter().rev()),
        workspace,
    );
    let options = candidates
        .iter()
        .enumerate()
        .map(|(index, tool)| (index, host_command_label(&tool.intent.call)))
        .collect::<Vec<_>>();
    let Some(index) = pick(
        terminal,
        "Host command",
        "Choose a recent command to pin. No command runs here.",
        &options,
    )
    .await?
    else {
        return Ok(None);
    };
    let intent = &candidates[index].intent;
    let arany::ToolCall::CommandUnguarded { args, .. } = &intent.call else {
        unreachable!("host candidate")
    };
    let prefixes = (1..=args.len() + 1)
        .filter_map(|count| {
            arany::permissions::HostPrefixRule::from_intent(intent, count)
                .ok()
                .map(|rule| {
                    (
                        count,
                        crate::presentation::escape_terminal(
                            &serde_json::to_string(&rule.prefix).expect("prefix"),
                        ),
                    )
                })
        })
        .collect::<Vec<_>>();
    let Some(count) = pick(terminal, "Host prefix", "Choose literal tokens. ONE TOKEN authorizes every eligible argument to this executable. Host effects cannot be rewound; private data may reach the Provider.", &prefixes).await? else { return Ok(None) };
    let Some(decision) = pick(
        terminal,
        "Host decision",
        "Choose a decision; every eligible suffix is covered.",
        &decisions(),
    )
    .await?
    else {
        return Ok(None);
    };
    let Ok(mut rule) = arany::permissions::HostPrefixRule::from_intent(intent, count) else {
        return Ok(None);
    };
    rule.decision = decision;
    Ok(Some(arany::permissions::HostPrefixEdit::Insert(Box::new(
        rule,
    ))))
}

fn host_candidates<'a>(
    tools: impl Iterator<Item = &'a arany::ToolView>,
    workspace: &Path,
) -> Vec<&'a arany::ToolView> {
    let mut seen = std::collections::HashSet::new();
    tools
        .filter(|tool| {
            tool.intent
                .host_command
                .as_ref()
                .is_some_and(|pin| Path::new(&pin.workspace) == workspace)
                && arany::permissions::HostPrefixRule::from_intent(&tool.intent, 1).is_ok()
        })
        .filter(|tool| match &tool.intent.call {
            arany::ToolCall::CommandUnguarded {
                program, args, cwd, ..
            } => seen.insert((program, args, cwd)),
            _ => false,
        })
        .take(arany::permissions::MAX_RULES)
        .collect()
}

fn host_command_label(call: &arany::ToolCall) -> String {
    let arany::ToolCall::CommandUnguarded {
        program, args, cwd, ..
    } = call
    else {
        return String::new();
    };
    let tokens = std::iter::once(program).chain(args).collect::<Vec<_>>();
    let mut label = serde_json::to_string(&tokens).expect("command tokens");
    if !cwd.is_empty() {
        label.push_str(&format!(
            " · cwd {}",
            serde_json::to_string(cwd).expect("command cwd")
        ));
    }
    crate::presentation::escape_terminal(&label)
}

fn requires_host_read_disclosure(
    edit: &TypedPrefixEdit,
    entries: &[arany::permissions::TypedPrefixEntry],
) -> bool {
    match edit {
        TypedPrefixEdit::Insert {
            prefix, decision, ..
        } => {
            matches!(*prefix, TypedGitPrefix::PrList | TypedGitPrefix::PrView)
                && *decision == Decision::Allow
        }
        TypedPrefixEdit::Replace { index, decision } => {
            *decision == Decision::Allow
                && entries.get(*index).is_some_and(|entry| {
                    matches!(
                        entry.prefix,
                        TypedGitPrefix::PrList | TypedGitPrefix::PrView
                    )
                })
        }
        TypedPrefixEdit::Remove { .. } => false,
    }
}

async fn confirm_host_read_disclosure(terminal: &mut AttachedTerminal) -> Result<bool, SetupError> {
    Ok(matches!(
        super::setup::choose_focused(
            terminal,
            2,
            "Private PR disclosure",
            HOST_READ_DISCLOSURE,
            &[(b'1', "Save allow rule"), (b'0', "Cancel")],
            None,
        )
        .await?,
        Some(b'1')
    ))
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

enum EntryEdit {
    Change(Decision),
    Remove,
}

async fn entry_edit(terminal: &mut AttachedTerminal) -> Result<Option<EntryEdit>, SetupError> {
    let mut options = decisions()
        .into_iter()
        .map(|(decision, label)| (Some(decision), format!("Change to {label}")))
        .collect::<Vec<_>>();
    options.push((None, "Remove rule".into()));
    Ok(
        pick(terminal, "Rule", "Change or remove this rule", &options)
            .await?
            .map(|decision| match decision {
                Some(decision) => EntryEdit::Change(decision),
                None => EntryEdit::Remove,
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
                    terminal.restore_draft_input(composer.input_capacity())?;
                    return Ok(PermissionChoice::Rejected(
                        permissions,
                        format!("Could not remember folder trust: {error}"),
                    ));
                }
                composer.set_approval_mode(
                    (native_protection_supported() && permissions.is_trusted())
                        .then_some(permissions.mode()),
                );
                terminal.restore_draft_input(composer.input_capacity())?;
                return Ok(PermissionChoice::Selected(permissions));
            }
            TerminalInput::Escape => {
                terminal.restore_draft_input(composer.input_capacity())?;
                return Ok(PermissionChoice::Closed(permissions));
            }
            TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                return Ok(PermissionChoice::Exit);
            }
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(composer.input_capacity())?;
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
                if option.exclusive
                    && selected
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
    if options[index].exclusive {
        let group = options[index].group;
        for (chosen, option) in selected.iter_mut().zip(options) {
            if option.group == group {
                *chosen = false;
            }
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
    #[test]
    fn an_unconfirmed_first_creation_or_edit_is_uncertain_never_unchanged() {
        use super::{
            CommandEnvironment, CommandEnvironmentError, EnvironmentOutcome, open_environment,
            settle_environment,
        };
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let state = arany::StateRoot::admit(&path).unwrap();
        arany::FaultPlan::fail_record_sync(path.clone(), "command-environment.json");
        let Err(outcome) = open_environment(&state) else {
            panic!("an unconfirmed creation must not open the editor");
        };
        assert!(matches!(outcome, EnvironmentOutcome::Uncertain));
        let summary = outcome.summary();
        assert!(summary.contains("reopen /permissions to verify"));
        assert!(!summary.contains("unchanged"));
        assert!(path.join("command-environment.json").is_file());
        assert!(open_environment(&state).is_ok());
        let edit = CommandEnvironment::edit(&state, |_| Ok(())).map(|_| "Saved.");
        assert!(settle_environment(edit).is_ok());
        let Err(outcome) = settle_environment(Err(CommandEnvironmentError::Unconfirmed)) else {
            panic!("an unconfirmed edit must stop the editor");
        };
        assert!(matches!(outcome, EnvironmentOutcome::Uncertain));
    }

    use super::{
        HOST_READ_DISCLOSURE, parse_linear_selection, requires_host_read_disclosure, toggle_profile,
    };
    use arany::permissions::{Decision, TypedGitPrefix, TypedPrefixEdit};

    #[test]
    fn host_rule_editor_feedback_distinguishes_failed_saved_and_unchanged_edits() {
        for reason in ["state unavailable", "permission settings are invalid"] {
            assert_eq!(
                super::host_rule_edit_notice(Some(reason), false),
                format!("Error: {reason}; host rules unchanged")
            );
            assert_eq!(
                super::host_rule_edit_notice(Some(reason), true),
                format!(
                    "Error: {reason}; earlier host rule edits were saved; reopen /permissions to verify their current state"
                )
            );
        }
        assert_eq!(
            super::host_rule_edit_notice(None, false),
            "Host command rules unchanged"
        );
        assert_eq!(
            super::host_rule_edit_notice(None, true),
            "Host command rules saved; they apply from the next Run"
        );
    }

    fn host_candidate(value: &str, cwd: &str) -> arany::ToolView {
        let pin = serde_json::from_value(serde_json::json!({
            "state_roots": ["/state", "/account"], "permissions_digest": null,
            "workspace": "/project", "executable": "/usr/bin/printf", "device": 1, "inode": 2,
            "sha256": "a".repeat(64), "path": "/usr/bin:/bin", "environment_digest": vec![4; 32],
        }))
        .unwrap();
        arany::ToolView {
            intent: arany::EffectIntent {
                host: None,
                rewrite: Vec::new(),
                host_command: Some(pin),
                target: arany::worker::EffectTarget::Workspace,
                id: uuid::Uuid::now_v7(),
                run_id: arany::RunId::new(),
                agent_run_id: arany::AgentRunId::new(),
                policy_digest: [1; 32],
                enforcement_digest: [2; 32],
                workspace_device: 1,
                workspace_inode: 2,
                call: arany::ToolCall::CommandUnguarded {
                    program: "printf".into(),
                    args: vec![value.into()],
                    cwd: cwd.into(),
                    justification: "Reason hidden from picker labels".into(),
                    prefix_rule: None,
                },
                limits: arany::ToolLimits::default(),
                use_count: 1,
            },
            checkpoint: None,
            observation: None,
            review_call_index: None,
            active_time_reserved: false,
            narrowing_source: None,
            permission_verdict: None,
            started_sequence: Some(0),
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        }
    }

    #[test]
    fn host_picker_deduplicates_before_limiting_and_labels_only_command_and_cwd() {
        let newest = host_candidate("current", "");
        let nested = host_candidate("current", "nested");
        let older = host_candidate("older", "");
        let mut tools = vec![newest; arany::permissions::MAX_RULES];
        tools.extend([nested, older]);
        let candidates = super::host_candidates(tools.iter(), std::path::Path::new("/project"));
        assert_eq!(candidates.len(), 3);
        assert!(std::ptr::eq(candidates[0], &tools[0]));
        let labels = candidates
            .iter()
            .map(|tool| super::host_command_label(&tool.intent.call))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                r#"["printf","current"]"#,
                r#"["printf","current"] · cwd "nested""#,
                r#"["printf","older"]"#,
            ]
        );
        let hostile = host_candidate("line\u{1b}[31m", "nested\nfolder");
        assert_eq!(
            super::host_command_label(&hostile.intent.call),
            r#"["printf","line\u001b[31m"] · cwd "nested\nfolder""#
        );
    }

    #[test]
    fn host_command_picker_labels_preserve_argument_boundaries() {
        let single = host_candidate("alpha beta", "");
        let mut split = host_candidate("alpha", "");
        if let arany::ToolCall::CommandUnguarded { args, .. } = &mut split.intent.call {
            args.push("beta".into());
        }
        let tools = [single, split];
        let candidates = super::host_candidates(tools.iter(), std::path::Path::new("/project"));
        assert_eq!(candidates.len(), 2);
        let labels = candidates
            .iter()
            .map(|tool| super::host_command_label(&tool.intent.call))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [r#"["printf","alpha beta"]"#, r#"["printf","alpha","beta"]"#]
        );
    }

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
                .prepare_profile_selection(&arany::StateRoot::account_path().unwrap());
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
            exclusive: true,
        }
    }

    #[test]
    fn independent_profiles_toggle_alone_while_toolchain_kinds_keep_one_choice() {
        let independent = |token, group| arany::RuntimeProfileOption {
            exclusive: false,
            ..option(token, group)
        };
        let options = [
            option("rust-a", "Rust"),
            option("rust-b", "Rust"),
            independent("user-a", "User programs"),
            independent("user-b", "User programs"),
        ];
        let mut selected = [true, false, false, false];
        toggle_profile(&mut selected, &options, 1);
        assert_eq!(selected, [false, true, false, false]);
        toggle_profile(&mut selected, &options, 2);
        toggle_profile(&mut selected, &options, 3);
        assert_eq!(selected, [false, true, true, true]);
        toggle_profile(&mut selected, &options, 2);
        assert_eq!(selected, [false, true, false, true]);

        assert_eq!(
            parse_linear_selection("trust 5 6", &options, &selected),
            Some((1, vec![false, false, true, true]))
        );
        assert_eq!(
            parse_linear_selection("trust 3 4", &options, &selected),
            None
        );
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

    #[test]
    fn host_read_allow_edits_require_provider_disclosure_confirmation() {
        for prefix in [TypedGitPrefix::PrList, TypedGitPrefix::PrView] {
            assert!(requires_host_read_disclosure(
                &TypedPrefixEdit::Insert {
                    prefix,
                    decision: Decision::Allow,
                    folder: false,
                },
                &[]
            ));
            for previous in [Decision::Ask, Decision::Deny] {
                let entries = [arany::permissions::TypedPrefixEntry {
                    prefix,
                    decision: previous,
                    folder: None,
                }];
                assert!(requires_host_read_disclosure(
                    &TypedPrefixEdit::Replace {
                        index: 0,
                        decision: Decision::Allow,
                    },
                    &entries
                ));
                assert!(!requires_host_read_disclosure(
                    &TypedPrefixEdit::Replace {
                        index: 0,
                        decision: Decision::Deny,
                    },
                    &entries
                ));
            }
        }
        assert!(HOST_READ_DISCLOSURE.contains("private pull request data"));
        assert!(HOST_READ_DISCLOSURE.contains("current Provider"));
    }
}
