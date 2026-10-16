use super::{
    Admission, DefaultsError, LoopError, load_view, open_state, recover_defaults_change,
    update_defaults,
};
use crate::cli::{chatgpt, exec::ProviderArg};
use arany::RecordKind;
use arany::{
    AttachedTerminal, CollaborationPolicy, CommandAvailability, CommandParseError,
    CompletionLayout, Composer, CustomProfile, Effort, InteractiveCommand, SessionId, SessionView,
    StateRoot, Submission, TerminalInput, TerminalMode, fork_session, rename_session,
    resume_session, validate_native_model_id,
};
use std::str::FromStr;

const CACHE_USAGE: &str = "Usage: /cache clear [all]";
const CACHE_CLEAR_FAILED: &str = "Error: private build cache could not be cleared";

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UiPreferences {
    version: u8,
    preferences: Preferences,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Preferences {
    pub completion: CompletionLayout,
    pub view: TerminalMode,
    pub goal_budget: arany::GoalBudget,
    pub resources: arany::ToolResourceSettings,
}

fn apply_setting(preferences: &mut Preferences, value: &str) -> bool {
    let original = *preferences;
    let completion = preferences.completion;
    let view = preferences.view;
    if let Some(completion) = [completion, completion.toggled()]
        .into_iter()
        .find(|completion| completion.label() == value)
    {
        preferences.completion = completion;
    } else if let Some(view) = [view, view.toggled()]
        .into_iter()
        .find(|view| view.label() == value)
    {
        preferences.view = view;
    } else if let Some((name, raw)) = value.split_once('=') {
        let parsed = match name {
            "calls" => raw
                .parse::<u32>()
                .ok()
                .map(|limit| preferences.goal_budget.max_provider_calls = Some(limit))
                .or_else(|| {
                    (raw == "unlimited").then(|| {
                        preferences.goal_budget.max_provider_calls = None;
                    })
                }),
            "tokens" => raw
                .parse::<u64>()
                .ok()
                .map(|limit| preferences.goal_budget.max_tokens = Some(limit))
                .or_else(|| {
                    (raw == "unlimited").then(|| {
                        preferences.goal_budget.max_tokens = None;
                    })
                }),
            "active-ms" => raw
                .parse::<u64>()
                .ok()
                .map(|limit| preferences.goal_budget.max_active_ms = Some(limit))
                .or_else(|| {
                    (raw == "unlimited").then(|| {
                        preferences.goal_budget.max_active_ms = None;
                    })
                }),
            "default-timeout-ms" => raw.parse::<u32>().ok().map(|limit| {
                preferences.resources.default_command_timeout_ms = limit;
            }),
            "max-timeout-ms" => raw.parse::<u32>().ok().map(|limit| {
                preferences.resources.max_command_timeout_ms = limit;
            }),
            "build-memory-bytes" => raw.parse::<u64>().ok().map(|limit| {
                preferences.resources.build_memory_bytes = limit;
            }),
            "build-processes" => raw.parse::<u32>().ok().map(|limit| {
                preferences.resources.build_processes = limit;
            }),
            "build-project-bytes" => raw.parse::<u64>().ok().map(|limit| {
                preferences.resources.build_project_bytes = limit;
            }),
            "build-cache-bytes" => raw.parse::<u64>().ok().map(|limit| {
                preferences.resources.build_cache_bytes = limit;
            }),
            _ => None,
        };
        if parsed.is_none()
            || preferences.goal_budget.validate().is_err()
            || !preferences.resources.valid()
        {
            *preferences = original;
            return false;
        }
    } else {
        return false;
    }
    true
}

fn setting_rows(preferences: Preferences) -> Vec<String> {
    let show =
        |value: Option<u64>| value.map_or_else(|| "unlimited".into(), |value| value.to_string());
    vec![
        format!("Slash menu: {}", preferences.completion.label()),
        format!("Chat view: {}", preferences.view.label()),
        format!(
            "Goal provider calls: {}",
            show(preferences.goal_budget.max_provider_calls.map(u64::from))
        ),
        format!("Goal tokens: {}", show(preferences.goal_budget.max_tokens)),
        format!(
            "Goal active milliseconds: {}",
            show(preferences.goal_budget.max_active_ms)
        ),
        format!(
            "Default command timeout ms: {}",
            preferences.resources.default_command_timeout_ms
        ),
        format!(
            "Maximum command timeout ms: {}",
            preferences.resources.max_command_timeout_ms
        ),
        format!(
            "Build memory bytes: {}",
            preferences.resources.build_memory_bytes
        ),
        format!(
            "Build processes/threads: {}",
            preferences.resources.build_processes
        ),
        format!(
            "Build writable project bytes: {}",
            preferences.resources.build_project_bytes
        ),
        format!(
            "Build cache bytes: {}",
            preferences.resources.build_cache_bytes
        ),
    ]
}

fn linear_settings_instruction(preferences: Preferences) -> String {
    format!(
        "Current menu: {} (tabs|combined); view: {} (fullscreen|classic).\nSettings: calls={} tokens={} active-ms={} default-timeout-ms={} max-timeout-ms={} build-memory-bytes={} build-processes={} build-project-bytes={} build-cache-bytes={}.\nSet a listed key=value or exact menu/view choice; Enter saves; q cancels.",
        preferences.completion.label(),
        preferences.view.label(),
        preferences
            .goal_budget
            .max_provider_calls
            .map_or_else(|| "unlimited".into(), |value| value.to_string()),
        preferences
            .goal_budget
            .max_tokens
            .map_or_else(|| "unlimited".into(), |value| value.to_string()),
        preferences
            .goal_budget
            .max_active_ms
            .map_or_else(|| "unlimited".into(), |value| value.to_string()),
        preferences.resources.default_command_timeout_ms,
        preferences.resources.max_command_timeout_ms,
        preferences.resources.build_memory_bytes,
        preferences.resources.build_processes,
        preferences.resources.build_project_bytes,
        preferences.resources.build_cache_bytes,
    )
}

fn cycle_setting(preferences: &mut Preferences, row: usize, forward: bool) {
    fn step<T: Copy + Eq>(current: T, values: &[T], forward: bool) -> T {
        let index = values
            .iter()
            .position(|value| *value == current)
            .unwrap_or(0);
        values[if forward {
            (index + 1) % values.len()
        } else {
            (index + values.len() - 1) % values.len()
        }]
    }
    match row {
        0 => preferences.completion = preferences.completion.toggled(),
        1 => preferences.view = preferences.view.toggled(),
        2 => {
            preferences.goal_budget.max_provider_calls = step(
                preferences.goal_budget.max_provider_calls,
                &[Some(64), Some(32), Some(16), None],
                forward,
            );
        }
        3 => {
            preferences.goal_budget.max_tokens = step(
                preferences.goal_budget.max_tokens,
                &[None, Some(1_000_000), Some(100_000)],
                forward,
            );
        }
        4 => {
            preferences.goal_budget.max_active_ms = step(
                preferences.goal_budget.max_active_ms,
                &[Some(arany::MAX_GOAL_ACTIVE_MS), Some(15 * 60 * 1000), None],
                forward,
            );
        }
        5 => {
            let values = [120_000, 60_000, 30_000];
            let next = step(
                preferences.resources.default_command_timeout_ms,
                &values,
                forward,
            );
            if next <= preferences.resources.max_command_timeout_ms {
                preferences.resources.default_command_timeout_ms = next;
            }
        }
        6 => {
            let values = [600_000, 300_000, 120_000];
            let next = step(
                preferences.resources.max_command_timeout_ms,
                &values,
                forward,
            );
            if next >= preferences.resources.default_command_timeout_ms {
                preferences.resources.max_command_timeout_ms = next;
            }
        }
        7 => {
            preferences.resources.build_memory_bytes = step(
                preferences.resources.build_memory_bytes,
                &[
                    4 * 1024_u64.pow(3),
                    2 * 1024_u64.pow(3),
                    1024_u64.pow(3),
                    512 * 1024 * 1024,
                ],
                forward,
            );
        }
        8 => {
            preferences.resources.build_processes = step(
                preferences.resources.build_processes,
                &[256, 128, 64, 32],
                forward,
            );
        }
        9 => {
            preferences.resources.build_project_bytes = step(
                preferences.resources.build_project_bytes,
                &[
                    1024_u64.pow(3),
                    512 * 1024 * 1024,
                    256 * 1024 * 1024,
                    64 * 1024 * 1024,
                ],
                forward,
            );
        }
        10 => {
            preferences.resources.build_cache_bytes = step(
                preferences.resources.build_cache_bytes,
                &[8 * 1024_u64.pow(3), 4 * 1024_u64.pow(3), 1024_u64.pow(3), 0],
                forward,
            );
        }
        _ => {}
    }
}

pub(super) async fn restore_settings(admission: &Admission) -> Result<Preferences, String> {
    restore_settings_at(&admission.state_dir).await
}

pub(super) async fn restore_settings_at(
    state_dir: &std::path::Path,
) -> Result<Preferences, String> {
    let path = state_dir.to_owned();
    tokio::task::spawn_blocking(move || {
        let root = match StateRoot::open_existing(&path) {
            Ok(root) => root,
            Err(arany::StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if matches!(std::fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
                    return Ok(Preferences::default());
                }
                return Err("display settings unavailable or unsafe".into());
            }
            Err(_) => return Err("display settings unavailable or unsafe".into()),
        };
        read_settings(&root)
    })
    .await
    .map_err(|_| "display settings unavailable".to_owned())?
}

fn read_settings(root: &StateRoot) -> Result<Preferences, String> {
    let Some(record) = root
        .record(RecordKind::UiPreferences)
        .read()
        .map_err(|_| "display settings unavailable or unsafe")?
    else {
        return Ok(Preferences::default());
    };
    let record: UiPreferences = serde_json::from_slice(&record)
        .map_err(|_| "invalid display settings; use /settings to replace them")?;
    if record.version != 3
        || record.preferences.goal_budget.validate().is_err()
        || !record.preferences.resources.valid()
    {
        return Err("unsupported settings; use /settings to replace them".into());
    }
    Ok(record.preferences)
}

async fn save_settings(admission: &Admission, preferences: Preferences) -> Result<(), String> {
    if preferences.goal_budget.validate().is_err() || !preferences.resources.valid() {
        return Err("invalid settings".into());
    }
    let path = admission.state_dir.clone();
    let workspace = admission.workspace.clone();
    tokio::task::spawn_blocking(move || {
        let root = StateRoot::admit(&path).map_err(|_| "display settings unavailable or unsafe")?;
        let record = serde_json::to_vec(&UiPreferences {
            version: 3,
            preferences,
        })
        .map_err(|_| "display settings unavailable")?;
        root.with_account_replacement_lock(&workspace, || {
            root.record(RecordKind::UiPreferences).replace(&record)
        })
        .map_err(|_| "display settings busy or unsafe")?
        .map_err(|_| "could not save display settings")
    })
    .await
    .map_err(|_| "display settings unavailable".to_owned())?
    .map_err(str::to_owned)
}

async fn choose_settings(
    terminal: &mut AttachedTerminal,
    mut chosen: Preferences,
) -> Result<Option<Preferences>, String> {
    let linear = terminal.is_linear();
    let mut selected_row = 0usize;
    let mut answer = Composer::default();
    let mut invalid = false;
    loop {
        let rows = setting_rows(chosen);
        if linear {
            let instruction = linear_settings_instruction(chosen);
            terminal.draw_setup_text_input(
                "Settings",
                &instruction,
                &answer,
                invalid.then_some("Type a valid setting and value; q cancels"),
            )
        } else {
            let choices: Vec<_> = rows
                .iter()
                .enumerate()
                .map(|(index, row)| (u8::try_from(index + 1).unwrap_or(u8::MAX), row.as_str()))
                .collect();
            terminal.draw_setup_choices(
                0,
                "Settings",
                "↑↓ selects a setting; ←→ changes it; Enter saves",
                &choices,
                selected_row,
                invalid.then_some("The selected settings could not be saved"),
            )
        }
        .map_err(|error| error.to_string())?;
        match terminal
            .next_input()
            .await
            .map_err(|error| error.to_string())?
        {
            TerminalInput::Up if !linear => selected_row = selected_row.saturating_sub(1),
            TerminalInput::Down if !linear => selected_row = (selected_row + 1).min(rows.len() - 1),
            TerminalInput::Left if !linear => cycle_setting(&mut chosen, selected_row, false),
            TerminalInput::Right if !linear => cycle_setting(&mut chosen, selected_row, true),
            TerminalInput::Character(character) if linear => {
                answer.apply(TerminalInput::Character(character));
            }
            TerminalInput::Backspace if linear => {
                answer.apply(TerminalInput::Backspace);
            }
            TerminalInput::Submit if linear => {
                let value = answer.take().trim().to_ascii_lowercase();
                match value.as_str() {
                    "" => return Ok(Some(chosen)),
                    "q" => return Ok(None),
                    value => invalid = !apply_setting(&mut chosen, value),
                }
            }
            TerminalInput::Submit => return Ok(Some(chosen)),
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                return Ok(None);
            }
            TerminalInput::Suspend => terminal
                .suspend_and_resume(answer.text().len())
                .map_err(|error| error.to_string())?,
            TerminalInput::Shutdown(signal) => {
                return Err(format!("terminated by {}", signal.name()));
            }
            TerminalInput::LineRejected => {
                answer.clear();
                invalid = true;
            }
            _ => {}
        }
    }
}

pub(super) async fn settings(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    composer: &mut Composer,
    saved_view: &mut TerminalMode,
) -> Result<String, String> {
    let linear = terminal.is_linear();
    let mut current = restore_settings(admission).await?;
    current.completion = composer.completion_layout();
    current.view = *saved_view;
    let notice = match choose_settings(terminal, current).await? {
        None => "Settings unchanged".to_owned(),
        Some(chosen) => match save_settings(admission, chosen).await {
            Ok(()) => {
                let view_changed = chosen.view != *saved_view;
                *saved_view = chosen.view;
                composer.set_completion_layout(chosen.completion);
                if !linear && view_changed {
                    terminal
                        .set_mode(chosen.view, composer.text().len())
                        .map_err(|error| error.to_string())?;
                }
                format!(
                    "Settings saved · calls {} · active ms {} · command max {} ms{}",
                    chosen
                        .goal_budget
                        .max_provider_calls
                        .map_or_else(|| "unlimited".into(), |value| value.to_string()),
                    chosen
                        .goal_budget
                        .max_active_ms
                        .map_or_else(|| "unlimited".into(), |value| value.to_string()),
                    chosen.resources.max_command_timeout_ms,
                    if linear {
                        " · visual sessions"
                    } else if chosen.view != terminal.mode() {
                        " · sessions without --classic"
                    } else {
                        ""
                    },
                )
            }
            Err(error) => format!("Error: {error}; current settings kept"),
        },
    };
    terminal
        .restore_draft_input(composer.text().len())
        .map_err(|error| error.to_string())?;
    Ok(notice)
}

fn parse_model_argument(argument: &str) -> Result<(&str, Option<Option<Effort>>), &'static str> {
    let mut parts = argument.split_ascii_whitespace();
    let model = parts.next().ok_or("Usage: /model MODEL [EFFORT|default]")?;
    if validate_native_model_id(model).is_err() {
        return Err("invalid native model");
    }
    let effort = match parts.next() {
        None => None,
        Some("default") => Some(None),
        Some(value) => Some(Some(
            Effort::from_str(value)
                .map_err(|_| "Effort unavailable for the selected Provider/model")?,
        )),
    };
    if parts.next().is_some() {
        return Err("Usage: /model MODEL [EFFORT|default]");
    }
    Ok((model, effort))
}

fn typed_model_selection<'a>(
    view: &SessionView,
    argument: &'a str,
) -> Result<(&'a str, Option<Effort>), &'static str> {
    let profile = view
        .defaults
        .provider
        .as_deref()
        .ok_or("Select /provider first")?;
    let (model, chosen) = parse_model_argument(argument)?;
    let effort = chosen.unwrap_or_else(|| {
        if view.defaults.model.as_deref() == Some(model) {
            view.defaults.effort
        } else if profile.starts_with("custom:") {
            None
        } else {
            Some(Effort::Low)
        }
    });
    if profile == "chatgpt" && effort.is_none() {
        return Err("ChatGPT requires an explicit effort");
    }
    if !profile.starts_with("custom:")
        && profile != "chatgpt"
        && arany::resolve_native_effort_for_run(profile, model, effort).is_err()
    {
        return Err("invalid native model/effort; choose a model and effort with /model");
    }
    Ok((model, effort))
}

pub(super) async fn select_model(
    admission: &Admission,
    session_id: SessionId,
    view: &mut SessionView,
    model: &str,
    effort: Option<Effort>,
) -> Result<String, LoopError> {
    let Some(profile) = view.defaults.provider.as_deref() else {
        return Ok("Select /provider first".into());
    };
    if validate_native_model_id(model).is_err() {
        return Ok("Error: invalid native model".into());
    }
    if let Some(name) = profile.strip_prefix("custom:") {
        let root = open_state(admission)?;
        let custom = match CustomProfile::load_named(&root, name) {
            Ok(custom) => custom,
            Err(error) => return Ok(format!("Error: {error}")),
        };
        if custom.model() != model {
            return Ok("Error: Selected model does not match its custom profile".into());
        }
        if effort.is_some_and(|effort| !custom.efforts().contains(&effort)) {
            return Ok("Error: Effort unavailable for the selected Provider/model".into());
        }
    } else if profile == "chatgpt" {
        if effort.is_none() {
            return Ok("Error: ChatGPT requires an explicit effort".into());
        }
    } else if arany::resolve_native_effort_for_run(profile, model, effort).is_err() {
        return Ok(
            "Error: invalid native model/effort; choose a model and effort with /model".into(),
        );
    }
    let mut defaults = view.defaults.clone();
    defaults.model = Some(model.to_owned());
    defaults.effort = effort;
    let reload_error = match update_defaults(admission, session_id, view, defaults).await {
        Ok(()) => None,
        Err(DefaultsError::Reload(error)) => Some(error),
        Err(error) => return recover_defaults_change(error),
    };
    let notice = match &reload_error {
        Some(error) => super::settings_reload_notice(error),
        None => format!(
            "Model: {model} · {}",
            effort.map_or("default", Effort::as_str)
        ),
    };
    // After a Reload failure, view.defaults still holds the saved selection.
    let notice = super::remembered_session_notice(admission, view, notice).await;
    match reload_error {
        Some(_) => Err(LoopError::Notice(notice)),
        None => Ok(notice),
    }
}

pub(super) enum ActiveSubmission {
    SteeringNote(String),
    Pause,
    Rejected(String),
    Notice(String),
    OpenAgents,
    OpenHelp,
    OpenTranscript,
    ReadClipboard,
}

pub(super) fn handle_active_submission(
    composer: &mut Composer,
    view: &SessionView,
    starting_sequence: u64,
) -> ActiveSubmission {
    let notice = match composer.submission() {
        Ok(Submission::Command {
            command: InteractiveCommand::Transcript,
            argument: None,
        }) => {
            composer.take();
            return ActiveSubmission::OpenTranscript;
        }
        Ok(Submission::Objective(text)) => return ActiveSubmission::SteeringNote(text.to_owned()),
        Ok(Submission::Command {
            command: InteractiveCommand::Goal,
            argument: Some("pause"),
        }) => {
            composer.take();
            return ActiveSubmission::Pause;
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Paste,
            argument: None,
        }) => {
            composer.take();
            return ActiveSubmission::ReadClipboard;
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Agents,
            argument: None,
        }) => {
            composer.take();
            return ActiveSubmission::OpenAgents;
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Help,
            argument: None,
        }) => {
            composer.take();
            return ActiveSubmission::OpenHelp;
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Goal,
            argument: Some(_),
        }) => "Goal changes are locked for this run; /goal shows status".into(),
        Ok(Submission::Command { command, .. }) => {
            active_command_notice(view, command, starting_sequence)
        }
        Err(CommandParseError::Empty) => "Run in progress... Ctrl+C cancels".into(),
        Err(CommandParseError::Unknown { suggestion }) => {
            return ActiveSubmission::Rejected(match suggestion {
                Some(name) => format!("Unknown command; did you mean /{name}?"),
                None => "Unknown command".into(),
            });
        }
        Err(error) => return ActiveSubmission::Rejected(error.to_string()),
    };
    composer.take();
    ActiveSubmission::Notice(notice)
}

pub(super) fn validate_idle_command(
    view: &SessionView,
    command: InteractiveCommand,
    argument: Option<&str>,
) -> Result<(), &'static str> {
    match command {
        InteractiveCommand::Resume
            if argument.is_some_and(|id| SessionId::from_str(id).is_err()) =>
        {
            Err("Usage: /resume SESSION_ID")
        }
        InteractiveCommand::Fork if argument.is_some_and(|id| SessionId::from_str(id).is_err()) => {
            Err("Usage: /fork [SESSION_ID]")
        }
        InteractiveCommand::Rename if argument.is_none() => Err("Usage: /rename TITLE"),
        InteractiveCommand::Rename
            if argument.is_some_and(|title| title.is_empty() || title.len() > 128) =>
        {
            Err("Title is empty or too long; use a shorter title")
        }
        InteractiveCommand::Provider
            if argument.is_some_and(|profile| ProviderArg::from_str(profile).is_err()) =>
        {
            Err("Provider must be openai, anthropic, chatgpt, or custom:NAME")
        }
        InteractiveCommand::Model if argument.is_some() => {
            typed_model_selection(view, argument.expect("model argument")).map(|_| ())
        }
        InteractiveCommand::Agents
            if argument.is_some_and(|policy| parse_policy(policy).is_err()) =>
        {
            Err("invalid collaboration policy")
        }
        InteractiveCommand::Cache if !matches!(argument, Some("clear" | "clear all")) => {
            Err(CACHE_USAGE)
        }
        _ => Ok(()),
    }
}

pub(super) async fn handle_command(
    admission: &Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
    command: InteractiveCommand,
    argument: Option<&str>,
) -> Result<String, LoopError> {
    let notice = match command {
        InteractiveCommand::New => {
            let defaults = view.defaults.clone();
            let new_view = super::new_conversation(defaults);
            *session_id = new_view.id;
            *view = new_view;
            "New conversation · send a message to save it".into()
        }
        InteractiveCommand::Resume => {
            let Some(id) = argument.and_then(|value| SessionId::from_str(value).ok()) else {
                return Ok("Usage: /resume SESSION_ID".into());
            };
            let root = open_state(admission)?;
            match resume_session(root, admission.workspace.clone(), id).await {
                Ok(resumed) => {
                    let resumed =
                        match super::prepare_resume(admission, resumed, Some(&view.defaults)).await
                        {
                            Ok(resumed) => resumed,
                            Err(error) => return Ok(format!("Error: {error}")),
                        };
                    *session_id = id;
                    *view = resumed;
                    super::remembered_session_notice(
                        admission,
                        view,
                        format!(
                            "Resumed: {}",
                            arany::escape_terminal(&view.conversation_title())
                        ),
                    )
                    .await
                }
                Err(error) => format!("Error: {error}"),
            }
        }
        InteractiveCommand::Goal if argument == Some("pause") => {
            "No Goal is running; /goal pause works during a Run".into()
        }
        InteractiveCommand::Goal => arany::goal_status(view),
        InteractiveCommand::Fork => {
            if argument.is_none() && view.created_sequence == 0 {
                return Ok("Send a message before forking this conversation".into());
            }
            let source_id = match argument {
                Some(value) => match SessionId::from_str(value) {
                    Ok(id) => id,
                    Err(_) => return Ok("Usage: /fork [SESSION_ID]".into()),
                },
                None => *session_id,
            };
            let root = open_state(admission)?;
            match fork_session(root, admission.workspace.clone(), source_id, None).await {
                Ok(id) => {
                    let forked = match load_view(&admission.state_dir, id).await {
                        Ok(forked) => forked,
                        Err(error) => {
                            return Ok(format!(
                                "Error: Forked Session {id}, but it could not be opened: {error}"
                            ));
                        }
                    };
                    let forked = match super::prepare_resume(
                        admission,
                        forked,
                        Some(&view.defaults),
                    )
                    .await
                    {
                        Ok(forked) => forked,
                        Err(error) => {
                            return Ok(format!(
                                "Error: Forked Session {id}, but it could not be opened: {error}"
                            ));
                        }
                    };
                    *session_id = id;
                    *view = forked;
                    super::remembered_session_notice(
                        admission,
                        view,
                        format!("Forked Session {id}"),
                    )
                    .await
                }
                Err(error) => format!("Error: {error}"),
            }
        }
        InteractiveCommand::Rename => {
            let Some(title) = argument else {
                return Ok("Usage: /rename TITLE".into());
            };
            if title.is_empty() || title.len() > 128 {
                return Ok("Error: invalid Run request".into());
            }
            if view.created_sequence == 0 {
                view.title = title.to_owned();
                view.title_is_explicit = true;
                return Ok(format!("Renamed conversation: {title}"));
            }
            let root = open_state(admission)?;
            match rename_session(
                root,
                admission.workspace.clone(),
                *session_id,
                title.to_owned(),
            )
            .await
            {
                Ok(()) => match load_view(&admission.state_dir, *session_id).await {
                    Ok(renamed) => {
                        *view = renamed;
                        format!("Renamed Session: {title}")
                    }
                    Err(error) => {
                        format!("Error: Renamed Session, but it could not be reloaded: {error}")
                    }
                },
                Err(error) => format!("Error: {error}"),
            }
        }
        InteractiveCommand::Provider => match argument {
            Some(profile) if ProviderArg::from_str(profile).is_ok() => {
                let account_id = if profile == "chatgpt" {
                    match chatgpt::selected_account_id() {
                        Ok(id) => Some(id),
                        Err(chatgpt::AuthorizationError::NoSelectedAccount) => {
                            return Ok(
                                "Error: no selected ChatGPT account; use /setup to connect".into()
                            );
                        }
                        Err(error) => return Ok(format!("Error: {error}")),
                    }
                } else if view.defaults.provider.as_deref() == Some(profile) {
                    view.defaults.account_id
                } else {
                    None
                };
                let mut defaults = view.defaults.clone();
                if defaults.provider.as_deref() != Some(profile)
                    || defaults.account_id != account_id
                {
                    defaults.provider = Some(profile.to_owned());
                    defaults.model = None;
                    defaults.effort = None;
                    defaults.account_id = account_id;
                    if let Err(error) =
                        update_defaults(admission, *session_id, view, defaults).await
                    {
                        return recover_defaults_change(error);
                    }
                }
                if view.defaults.model.is_none() {
                    format!("Provider: {profile}; set /model")
                } else {
                    format!("Provider: {profile}")
                }
            }
            Some(_) => "Provider must be openai, anthropic, chatgpt, or custom:NAME".into(),
            None => format!(
                "Provider: {}",
                view.defaults.provider.as_deref().unwrap_or("not selected")
            ),
        },
        InteractiveCommand::Model => {
            let Some(argument) = argument else {
                return Err("Model selection requires the terminal catalog".into());
            };
            let (model, effort) = match typed_model_selection(view, argument) {
                Ok(parsed) => parsed,
                Err(error) => return Ok(format!("Error: {error}")),
            };
            return select_model(admission, *session_id, view, model, effort).await;
        }
        InteractiveCommand::Agents => match argument {
            Some(argument) => match parse_policy(argument) {
                Ok(policy) => {
                    let mut defaults = view.defaults.clone();
                    defaults.policy = policy;
                    if let Err(error) =
                        update_defaults(admission, *session_id, view, defaults).await
                    {
                        return recover_defaults_change(error);
                    }
                    format!("Next-Run collaboration: {policy}")
                }
                Err(error) => error.into(),
            },
            None => format!("Next-Run collaboration: {}", view.defaults.policy),
        },
        InteractiveCommand::Status if view.created_sequence == 0 => {
            "New conversation · not saved · 0 Runs".into()
        }
        InteractiveCommand::Status => format!("Session {session_id} · {} Runs", view.runs.len()),
        InteractiveCommand::Help => {
            return Err("Help requires the terminal command registry".into());
        }
        InteractiveCommand::Permissions => {
            if admission.tools {
                "Tools requested from private tools.json; native protection must be admitted before a Run. File mutation requires the write grant; command changes are discarded. Primary only; commands/MCP have no network, host home or account state. Arany requests: selected Provider; OS TLS checks may connect separately.".into()
            } else if let Some(access) = admission
                .workspace_permissions
                .as_ref()
                .filter(|access| access.is_trusted())
            {
                let profiles = access.selected_runtime_profiles();
                format!(
                    "Folder trusted; file Tools use the Workspace. Commands/MCP use a private selected-project copy without network or account state; only these selected read-only toolchains and caches are mounted: {}. Arany requests: selected Provider; OS TLS checks may connect separately.",
                    if profiles.is_empty() {
                        "none".into()
                    } else {
                        profiles.join("; ")
                    }
                )
            } else {
                "Read-only Workspace; Arany requests: selected Provider; OS TLS checks may connect separately; no Tools or sandbox".into()
            }
        }
        InteractiveCommand::Cache => {
            let Ok(root) = StateRoot::open_existing(&admission.state_dir) else {
                return Ok(CACHE_CLEAR_FAILED.into());
            };
            let (result, cleared) = match argument {
                Some("clear") => (
                    root.clear_workspace_build_cache(&admission.workspace),
                    "Cleared this Workspace's private build cache",
                ),
                Some("clear all") => (
                    root.clear_all_build_caches(),
                    "Cleared all private build caches",
                ),
                _ => return Ok(format!("Error: {CACHE_USAGE}")),
            };
            match result {
                Ok(()) => cleared.into(),
                Err(arany::StoreError::BuildCacheBusy) => {
                    "Build cache is in use; retry after its command finishes".into()
                }
                Err(_) => CACHE_CLEAR_FAILED.into(),
            }
        }
        _ => "Command is not available in this build".into(),
    };
    Ok(notice)
}

pub(super) fn active_command_notice(
    view: &SessionView,
    command: InteractiveCommand,
    starting_sequence: u64,
) -> String {
    if command.availability(true) == CommandAvailability::Locked {
        return "Command locked for this run".into();
    }
    let run = view
        .runs
        .last()
        .filter(|run| run.accepted_sequence > starting_sequence);
    match command {
        InteractiveCommand::Status => match run {
            Some(run) => {
                let agents = format!(
                    "{} agent{}",
                    run.agents.len(),
                    if run.agents.len() == 1 { "" } else { "s" }
                );
                match run
                    .config
                    .as_ref()
                    .and_then(|config| config.context_usage.as_ref())
                    .filter(|_| run.status == arany::RunStatus::Active)
                {
                    Some(usage) => format!(
                        "Working · request {}% of local limit · {agents}. Includes chat, instructions, files and images; not model tokens. Details: /agents",
                        usage.utilization_percent(),
                    ),
                    None => format!("Task: {} · {agents} · details: /agents", run.status),
                }
            }
            None => "Preparing message · Ctrl+C cancels".into(),
        },
        InteractiveCommand::Goal => arany::goal_status(view),
        InteractiveCommand::Agents => {
            "Collaboration locked for this run; /agents opens details".into()
        }
        InteractiveCommand::Provider => {
            let provider = run
                .and_then(|run| run.config.as_ref())
                .map_or(view.defaults.provider.as_deref(), |config| {
                    Some(config.provider.as_str())
                });
            format!(
                "Provider: {}; locked for this run",
                provider.unwrap_or("not selected")
            )
        }
        InteractiveCommand::Model => {
            let model = run
                .and_then(|run| run.config.as_ref())
                .map_or(view.defaults.model.as_deref(), |config| {
                    Some(config.model.as_str())
                });
            let effort = run
                .and_then(|run| run.config.as_ref())
                .map_or(view.defaults.effort, |config| config.effort);
            format!(
                "Model: {} · {}; locked for this run",
                model.unwrap_or("not selected"),
                effort.map_or("default", Effort::as_str)
            )
        }
        InteractiveCommand::Permissions => {
            if run
                .and_then(|run| run.config.as_ref())
                .is_some_and(|config| config.tool_policy.is_some())
            {
                "Primary has pinned guarded Tool grants: at most 16 Tool calls with bounded Tool context. Commands/MCP use a private selected-project copy without network or account state; only the selected read-only toolchains and caches from home are mounted, and their file changes are discarded. Children remain read-only. Arany requests: selected Provider; OS TLS checks may connect separately.".into()
            } else {
                "Read-only Workspace; Arany requests: selected Provider; OS TLS checks may connect separately; no Tools or sandbox".into()
            }
        }
        InteractiveCommand::Exit => {
            "Ctrl+C cancels the active Run; /exit exits after it ends".into()
        }
        _ => "Command locked for this run".into(),
    }
}

fn parse_policy(argument: &str) -> Result<CollaborationPolicy, &'static str> {
    let mut parts = argument.split_ascii_whitespace();
    let mode = parts.next().ok_or("invalid collaboration policy")?;
    let count = parts
        .next()
        .map(str::parse::<u8>)
        .transpose()
        .map_err(|_| "invalid collaboration policy")?;
    if parts.next().is_some() {
        return Err("invalid collaboration policy");
    }
    match (mode, count) {
        ("single", None) => Ok(CollaborationPolicy::Single),
        ("auto", count) if count.unwrap_or(3) <= 8 => Ok(CollaborationPolicy::Auto {
            max_active_children: count.unwrap_or(3),
        }),
        ("team", count) if (1..=8).contains(&count.unwrap_or(3)) => Ok(CollaborationPolicy::Team {
            max_active_children: count.unwrap_or(3),
        }),
        _ => Err("invalid collaboration policy"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::attached::{EntryMode, persist_defaults};
    use arany::{
        AgentRole, AgentRunId, AgentStatus, AgentView, ContextUsage, RunConfig, RunId, RunStatus,
        RunView, SessionDefaults, Store, TerminalInput, create_session,
    };

    #[test]
    fn active_submissions_keep_objectives_local_and_lock_mutations() {
        let mut view = SessionView {
            id: SessionId::new(),
            title: "Current".into(),
            title_is_explicit: true,
            inherited_title: None,
            workspace_identity: None,
            defaults: SessionDefaults::default(),
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
            goals: Vec::new(),
            runs: Vec::new(),
            compactions: Vec::new(),
        };
        let starting_sequence = view.last_sequence;
        let mut composer = Composer::default();
        for character in "//literal".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::SteeringNote(ref text) if text == "/literal"
        ));
        assert_eq!(composer.text(), "//literal");

        composer.clear();
        for character in "/goal pause".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::Pause
        ));
        assert!(
            composer.is_empty(),
            "/goal pause is a control, not steering text"
        );
        for character in "/goal pause now".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::Notice(ref notice) if notice == "Goal changes are locked for this run; /goal shows status"
        ));

        composer.clear();
        for character in "/provder".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        composer.apply(TerminalInput::Left);
        composer.apply(TerminalInput::Left);
        let caret = composer.cursor_byte_offset();
        let ActiveSubmission::Rejected(notice) =
            handle_active_submission(&mut composer, &view, starting_sequence)
        else {
            panic!("unknown active command must be rejected locally")
        };
        assert_eq!(notice, "Unknown command; did you mean /provider?");
        assert_eq!(
            composer.text(),
            "/provder",
            "local rejection keeps the draft"
        );
        assert_eq!(composer.cursor_byte_offset(), caret);

        for (command, expected) in [
            ("/status unused", "Usage: /status"),
            ("/help unused", "Usage: /help"),
            ("/permissions unused", "Usage: /permissions"),
            ("/exit unused", "Usage: /exit"),
            ("/clear unused", "Usage: /clear"),
        ] {
            composer.clear();
            for character in command.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            composer.apply(TerminalInput::Left);
            composer.apply(TerminalInput::Left);
            let caret = composer.cursor_byte_offset();
            let ActiveSubmission::Rejected(notice) =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("unexpected active command arguments must be rejected locally")
            };
            assert_eq!(notice, expected);
            assert_eq!(composer.text(), command);
            assert_eq!(composer.cursor_byte_offset(), caret);
            assert_eq!(view.defaults, SessionDefaults::default());
        }

        composer.clear();
        for character in "/provider anthropic".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let ActiveSubmission::Notice(notice) =
            handle_active_submission(&mut composer, &view, starting_sequence)
        else {
            panic!("command must remain local")
        };
        assert_eq!(notice, "Provider: not selected; locked for this run");
        assert!(composer.is_empty());

        for character in "/agents".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::OpenAgents
        ));
        assert!(composer.is_empty());
        assert_eq!(view.defaults, SessionDefaults::default());

        for command in ["/model", "/model gpt-5.4"] {
            for character in command.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            let ActiveSubmission::Notice(notice) =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("model inspection must remain local")
            };
            assert_eq!(
                notice, "Model: not selected · default; locked for this run",
                "{command}"
            );
            assert!(composer.is_empty());
            assert_eq!(view.defaults, SessionDefaults::default());
        }

        for character in "/help".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::OpenHelp
        ));
        assert!(composer.is_empty());

        for character in "/paste".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::ReadClipboard
        ));
        assert!(composer.is_empty());
        assert_eq!(view.defaults, SessionDefaults::default());

        for command in ["/new", "/compact", "/settings"] {
            for character in command.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            let ActiveSubmission::Notice(notice) =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("command must remain local")
            };
            assert_eq!(notice, "Command locked for this run", "{command}");
            assert!(composer.is_empty());
        }

        let run_id = RunId::new();
        view.runs.push(RunView {
            id: run_id,
            objective: "Current objective".into(),
            images: Vec::new(),
            config: None,
            agents: vec![AgentView {
                id: AgentRunId::new(),
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                summary: None,
                result: None,
                provider_calls: Vec::new(),
                grant: None,
                tools: Vec::new(),
                status: AgentStatus::Active,
            }],
            assistant_message: None,
            status: RunStatus::Active,
            accepted_sequence: 2,
            finished_sequence: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            display: Vec::new(),
            steering_notes: Vec::new(),
        });
        assert_eq!(
            active_command_notice(&view, InteractiveCommand::Status, starting_sequence),
            "Task: working · 1 agent · details: /agents"
        );
        view.runs.last_mut().expect("active Run").config = Some(RunConfig {
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            effort: None,
            custom_profile_provenance: None,
            saved_api_account_id: None,
            chatgpt_provenance: None,
            output_token_bound: arany::OutputTokenBound::ProviderEnforced,
            policy: CollaborationPolicy::Single,
            output_token_cap: 1024,
            provider_concurrency: 1,
            workspace_device: 1,
            workspace_inode: 1,
            instruction_digest: None,
            include_digests: Vec::new(),
            history_run_ids: Vec::new(),
            excluded_history_runs: 0,
            context_usage: Some(ContextUsage {
                used_bytes: 32_000,
                budget_bytes: 65_536,
                compactable_bytes: 20_000,
                tool_history_bytes: 0,
            }),
            compaction_event_sequence: None,
            compaction_content_digest: None,
            tool_policy: None,
        });
        let notice = active_command_notice(&view, InteractiveCommand::Status, starting_sequence);
        assert!(notice.starts_with("Working · request 49% of local limit · 1 agent."));
        assert!(notice.contains("not model tokens"));
        assert!(!notice.contains(&run_id.to_string()));
        assert!("Working · request 49% of local limit".chars().count() <= 40);
        for status in [RunStatus::Finished, RunStatus::Failed, RunStatus::Cancelled] {
            view.runs.last_mut().expect("latest Run").status = status;
            assert_eq!(
                active_command_notice(&view, InteractiveCommand::Status, starting_sequence),
                format!("Task: {status} · 1 agent · details: /agents"),
                "current terminal facts remain visible without active context usage"
            );
            assert_eq!(
                active_command_notice(&view, InteractiveCommand::Model, starting_sequence),
                "Model: gpt-5.4 · default; locked for this run"
            );
        }
        view.runs.last_mut().expect("previous Run").status = RunStatus::Finished;
        assert_eq!(
            active_command_notice(&view, InteractiveCommand::Agents, starting_sequence),
            "Collaboration locked for this run; /agents opens details"
        );
        assert_eq!(
            active_command_notice(&view, InteractiveCommand::Model, starting_sequence),
            "Model: gpt-5.4 · default; locked for this run"
        );
        view.defaults.provider = Some("anthropic".into());
        view.defaults.model = Some("claude-sonnet-5".into());
        view.defaults.effort = Some(arany::Effort::Low);
        view.last_sequence = 4;
        view.runs
            .last_mut()
            .expect("previous Run")
            .finished_sequence = Some(4);
        let starting_sequence = view.last_sequence;
        let defaults = view.defaults.clone();
        for (command, expected) in [
            (
                "/provider",
                "Provider: anthropic; locked for this run".to_owned(),
            ),
            (
                "/model",
                "Model: claude-sonnet-5 · low; locked for this run".to_owned(),
            ),
            ("/status", "Preparing message · Ctrl+C cancels".to_owned()),
        ] {
            for character in command.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            let ActiveSubmission::Notice(notice) =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("admission inspection must remain local")
            };
            assert_eq!(notice, expected, "{command} before current Run progress");
            assert!(composer.is_empty());
            assert_eq!(view.defaults, defaults);
        }
        let image: arany::ImageAttachment = serde_json::from_str(r#"{"media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"}"#).unwrap();
        composer.attach_image(image.clone()).unwrap();
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::SteeringNote(_)
        ));
        for command in ["/help", "/agents", "/paste", "/provider"] {
            composer.insert_paste(command).unwrap();
            let result = handle_active_submission(&mut composer, &view, starting_sequence);
            assert!(matches!(
                result,
                ActiveSubmission::OpenHelp
                    | ActiveSubmission::OpenAgents
                    | ActiveSubmission::ReadClipboard
                    | ActiveSubmission::Notice(_)
            ));
            assert_eq!(
                composer.images(),
                std::slice::from_ref(&image),
                "active control discarded images: {command}"
            );
            assert_eq!(composer.text(), "");
        }
    }

    #[test]
    fn collaboration_command_accepts_only_bounded_next_run_policy() {
        let cases = [
            ("single", Some(CollaborationPolicy::Single)),
            (
                "auto",
                Some(CollaborationPolicy::Auto {
                    max_active_children: 3,
                }),
            ),
            (
                "auto 0",
                Some(CollaborationPolicy::Auto {
                    max_active_children: 0,
                }),
            ),
            (
                "team 8",
                Some(CollaborationPolicy::Team {
                    max_active_children: 8,
                }),
            ),
            ("single 1", None),
            ("auto 9", None),
            ("team 0", None),
            ("team 2 extra", None),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_policy(input).ok(), expected, "{input}");
        }
    }

    #[test]
    fn invalid_linear_settings_leave_the_draft_unchanged() {
        let mut preferences = Preferences::default();
        let original = preferences;
        for value in [
            "calls=0",
            "tokens=0",
            "active-ms=0",
            "default-timeout-ms=600001",
            "max-timeout-ms=119999",
            "build-memory-bytes=0",
            "build-processes=257",
            "build-project-bytes=1073741825",
            "build-project-bytes=1",
            "build-project-bytes=1073741823",
            "build-cache-bytes=8589934593",
            "build-cache-bytes=1",
            "build-cache-bytes=8589934591",
        ] {
            assert!(!apply_setting(&mut preferences, value), "{value}");
            assert_eq!(
                preferences, original,
                "invalid value changed draft: {value}"
            );
        }
        assert!(apply_setting(&mut preferences, "calls=unlimited"));
        assert_eq!(preferences.goal_budget.max_provider_calls, None);
    }

    #[test]
    fn idle_validation_rejects_local_arguments_before_any_effect() {
        let mut view = SessionView {
            id: SessionId::new(),
            title: "Current".into(),
            title_is_explicit: true,
            inherited_title: None,
            workspace_identity: None,
            defaults: SessionDefaults::default(),
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
            goals: Vec::new(),
            runs: Vec::new(),
            compactions: Vec::new(),
        };
        let oversized_title = "x".repeat(129);
        let oversized_unicode_title = "é".repeat(65);
        for (command, argument, expected) in [
            (
                InteractiveCommand::Provider,
                "other",
                "Provider must be openai, anthropic, chatgpt, or custom:NAME",
            ),
            (
                InteractiveCommand::Agents,
                "team 0",
                "invalid collaboration policy",
            ),
            (
                InteractiveCommand::Resume,
                "not-an-id",
                "Usage: /resume SESSION_ID",
            ),
            (
                InteractiveCommand::Rename,
                oversized_title.as_str(),
                "Title is empty or too long; use a shorter title",
            ),
            (
                InteractiveCommand::Rename,
                "",
                "Title is empty or too long; use a shorter title",
            ),
            (
                InteractiveCommand::Rename,
                oversized_unicode_title.as_str(),
                "Title is empty or too long; use a shorter title",
            ),
        ] {
            assert_eq!(
                validate_idle_command(&view, command, Some(argument)),
                Err(expected),
                "{command:?} {argument}"
            );
        }
        assert_eq!(
            validate_idle_command(&view, InteractiveCommand::Model, Some("gpt-5.4")),
            Err("Select /provider first")
        );
        assert_eq!(
            validate_idle_command(&view, InteractiveCommand::Model, Some("gpt-5.4 medium")),
            Err("Select /provider first")
        );
        view.defaults.provider = Some("openai".into());
        assert!(
            validate_idle_command(&view, InteractiveCommand::Model, Some("gpt-5.4 medium")).is_ok()
        );
        assert_eq!(
            typed_model_selection(&view, "gpt-5.4 max"),
            Err("invalid native model/effort; choose a model and effort with /model")
        );
        assert_eq!(
            typed_model_selection(&view, "unknown-native-model"),
            Ok(("unknown-native-model", Some(Effort::Low)))
        );
        view.defaults.model = Some("gpt-5.4".into());
        view.defaults.effort = Some(Effort::High);
        assert_eq!(
            typed_model_selection(&view, "gpt-5.4"),
            Ok(("gpt-5.4", Some(Effort::High)))
        );
        view.defaults.provider = Some("chatgpt".into());
        assert_eq!(
            typed_model_selection(&view, "gpt-5.4 default"),
            Err("ChatGPT requires an explicit effort")
        );
        assert_eq!(
            validate_idle_command(&view, InteractiveCommand::Model, Some("gpt-5.4 impossible")),
            Err("Effort unavailable for the selected Provider/model")
        );
        assert!(validate_idle_command(&view, InteractiveCommand::Agents, Some("team 2")).is_ok());
        assert!(validate_idle_command(&view, InteractiveCommand::Exit, None).is_ok());
        for title in ["x".repeat(128), "é".repeat(64)] {
            assert!(validate_idle_command(&view, InteractiveCommand::Rename, Some(&title)).is_ok());
        }
    }

    #[tokio::test]
    async fn display_settings_reopen_without_session_authority_and_reject_unsafe_records() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let admission = Admission {
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace,
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        let combined_classic = Preferences {
            completion: CompletionLayout::Combined,
            view: TerminalMode::Classic,
            ..Preferences::default()
        };
        assert_eq!(
            restore_settings(&admission).await.unwrap(),
            Preferences {
                completion: CompletionLayout::Tabs,
                view: TerminalMode::Fullscreen,
                ..Preferences::default()
            }
        );
        assert!(
            !admission.state_dir.exists(),
            "reading defaults creates no State"
        );
        for preferences in [combined_classic, Preferences::default()] {
            save_settings(&admission, preferences).await.unwrap();
            assert_eq!(restore_settings(&admission).await.unwrap(), preferences);
            assert_eq!(
                std::fs::metadata(admission.state_dir.join("events.sqlite3"))
                    .unwrap()
                    .len(),
                0
            );
        }
        let root = StateRoot::open_existing(&admission.state_dir).unwrap();
        for document in [
            br#"{"version":1,"completion":"tabs"}"#.as_slice(),
            br#"{"version":2,"completion":"tabs","view":"classic"}"#.as_slice(),
            br#"{"version":3,"preferences":{"completion":"tabs","view":"classic"}}"#.as_slice(),
            br#"{"version":2,"preferences":{"completion":"tabs"}}"#.as_slice(),
            br#"{"version":2,"preferences":{"completion":"unknown","view":"classic"}}"#.as_slice(),
            br#"{"version":2,"preferences":{"completion":"tabs","view":"unknown"}}"#.as_slice(),
            br#"{"version":2,"preferences":{"completion":"tabs","view":"classic","tools":true}}"#
                .as_slice(),
            br#"{"version":2,"preferences":{"completion":"tabs","view":"classic"},"tools":true}"#
                .as_slice(),
            br#"{"version":2}"#.as_slice(),
            b"malformed",
        ] {
            root.record(RecordKind::UiPreferences)
                .replace(document)
                .unwrap();
            assert!(restore_settings(&admission).await.is_err());
            assert_eq!(
                root.record(RecordKind::UiPreferences)
                    .read()
                    .unwrap()
                    .unwrap(),
                document
            );
        }
        save_settings(&admission, combined_classic).await.unwrap();
        let original = root
            .record(RecordKind::UiPreferences)
            .read()
            .unwrap()
            .unwrap();
        assert_eq!(
            original,
            br#"{"version":3,"preferences":{"completion":"combined","view":"classic","goal_budget":{"max_provider_calls":64,"max_tokens":null,"max_active_ms":1800000,"max_runs":16},"resources":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592}}}"#
        );
        let mut invalid = Preferences::default();
        invalid.resources.build_processes = 0;
        assert!(save_settings(&admission, invalid).await.is_err());
        assert_eq!(
            root.record(RecordKind::UiPreferences)
                .read()
                .unwrap()
                .unwrap(),
            original,
            "invalid settings never replace the current record"
        );
        assert!(
            root.record(RecordKind::UiPreferences)
                .replace(&vec![b'x'; 1025])
                .is_err()
        );
        assert_eq!(
            root.record(RecordKind::UiPreferences)
                .read()
                .unwrap()
                .unwrap(),
            original
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let pending = admission.state_dir.join("ui-preferences.pending");
            let outside = temp.path().join("outside");
            std::fs::write(&outside, b"unchanged").unwrap();
            symlink(&outside, &pending).unwrap();
            assert!(
                save_settings(&admission, Preferences::default())
                    .await
                    .is_err()
            );
            assert_eq!(
                restore_settings(&admission).await.unwrap(),
                combined_classic
            );
            assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
            std::fs::remove_file(pending).unwrap();
            let file = admission.state_dir.join("ui-preferences.json");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(restore_settings(&admission).await.is_err());
            assert!(
                save_settings(&admission, Preferences::default())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn idle_pause_reports_that_no_goal_is_running() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let admission = Admission {
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace,
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        let root = StateRoot::admit(&admission.state_dir).expect("state");
        let mut session_id = create_session(root, admission.workspace.clone(), None)
            .await
            .expect("Session");
        let mut view = load_view(&admission.state_dir, session_id)
            .await
            .expect("view");
        let notice = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Goal,
            Some("pause"),
        )
        .await
        .expect("local notice");
        assert_eq!(notice, "No Goal is running; /goal pause works during a Run");
    }

    #[tokio::test]
    async fn lifecycle_commands_switch_only_after_a_valid_durable_operation() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let mut admission = Admission {
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace,
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        let root = StateRoot::admit(&admission.state_dir).expect("state");
        let original_id = create_session(root, admission.workspace.clone(), None)
            .await
            .expect("original Session");
        let mut session_id = original_id;
        let mut view = load_view(&admission.state_dir, session_id)
            .await
            .expect("original view");

        let invalid = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Resume,
            Some("not-an-id"),
        )
        .await
        .expect("local notice");
        assert_eq!(invalid, "Usage: /resume SESSION_ID");
        assert_eq!(session_id, original_id);

        let missing = SessionId::new().to_string();
        let original_title = view.title.clone();
        for command in [InteractiveCommand::Resume, InteractiveCommand::Fork] {
            let rejected = handle_command(
                &admission,
                &mut session_id,
                &mut view,
                command,
                Some(&missing),
            )
            .await
            .expect("recoverable missing Session");
            assert_eq!(rejected, "Error: Session not found");
            assert_eq!(session_id, original_id);
            assert_eq!(view.title, original_title);
        }
        let rejected_rename = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Rename,
            Some(""),
        )
        .await
        .expect("recoverable canonical title rejection");
        assert_eq!(rejected_rename, "Error: invalid Run request");
        assert_eq!(session_id, original_id);
        assert_eq!(view.title, original_title);

        handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Rename,
            Some("Renamed"),
        )
        .await
        .expect("rename notice");
        assert_eq!(view.title, "Renamed");

        for (command, argument) in [
            (InteractiveCommand::Provider, "openai"),
            (InteractiveCommand::Model, "gpt-5.4"),
            (InteractiveCommand::Model, "gpt-5.4 medium"),
        ] {
            handle_command(
                &admission,
                &mut session_id,
                &mut view,
                command,
                Some(argument),
            )
            .await
            .expect("selected next-Run setting");
        }
        assert_eq!(view.defaults.effort, Some(Effort::Medium));
        let invalid_effort = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Model,
            Some("gpt-5.4 max"),
        )
        .await
        .expect("unavailable effort notice");
        assert_eq!(
            invalid_effort,
            "Error: invalid native model/effort; choose a model and effort with /model"
        );
        assert_eq!(view.defaults.effort, Some(Effort::Medium));
        let unreviewed_model = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Model,
            Some("gpt-5.4-mini"),
        )
        .await
        .expect("unreviewed model notice");
        assert_eq!(unreviewed_model, "Model: gpt-5.4-mini · low");
        assert_eq!(view.defaults.model.as_deref(), Some("gpt-5.4-mini"));
        assert_eq!(view.defaults.effort, Some(Effort::Low));
        handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Provider,
            Some("anthropic"),
        )
        .await
        .expect("provider change");
        assert_eq!(view.defaults.effort, None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = admission.state_dir.join("provider-profiles.json");
            let profile = serde_json::json!({
                "version": 1,
                "profiles": [{
                    "name": "local",
                    "protocol": "openai-responses",
                    "endpoint": "http://127.0.0.1:9321/v1/responses",
                    "model": "model-1",
                    "credential_env": "ARANY_PROVIDER_LOCAL_KEY",
                    "outcome_encoding": "json_schema",
                    "privacy": "user_authorized",
                    "max_output_tokens": 4096,
                    "capability_evidence_version": 2,
                    "efforts": ["low", "high"]
                }]
            });
            std::fs::write(&file, serde_json::to_vec(&profile).unwrap()).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            for (command, argument) in [
                (InteractiveCommand::Provider, "custom:local"),
                (InteractiveCommand::Model, "model-1"),
                (InteractiveCommand::Model, "model-1 high"),
            ] {
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    command,
                    Some(argument),
                )
                .await
                .expect("custom next-Run selection");
            }
            assert_eq!(view.defaults.effort, Some(Effort::High));
            assert_eq!(
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    InteractiveCommand::Model,
                    Some("model-1 high"),
                )
                .await
                .unwrap(),
                "Model: model-1 · high"
            );
            assert_eq!(
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    InteractiveCommand::Model,
                    Some("model-1 max"),
                )
                .await
                .unwrap(),
                "Error: Effort unavailable for the selected Provider/model"
            );
            assert_eq!(view.defaults.effort, Some(Effort::High));
        }

        let cannot_fork = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Fork,
            None,
        )
        .await
        .expect("local fork notice");
        assert_eq!(
            cannot_fork,
            "Error: Session has no committed Run boundary to fork"
        );
        assert_eq!(session_id, original_id);

        #[cfg(unix)]
        {
            use rustix::fs::{FlockOperation, flock};
            let held = std::fs::File::open(
                admission
                    .state_dir
                    .join(format!("session-{session_id}.lock")),
            )
            .expect("existing Session operation lock");
            flock(&held, FlockOperation::NonBlockingLockExclusive).expect("held operation");
            let before = view.clone();
            let store = Store::open_read_only(
                StateRoot::open_existing(&admission.state_dir).expect("State"),
            )
            .expect("source Store");
            let prefix = store.load_session(session_id).await.expect("source prefix");
            store.close().await.expect("close source Store");
            for (command, argument) in [
                (InteractiveCommand::Provider, "anthropic"),
                (InteractiveCommand::Model, "model-1 high"),
                (InteractiveCommand::Model, "model-1 default"),
                (InteractiveCommand::Model, "model-1 low"),
                (InteractiveCommand::Agents, "single"),
            ] {
                let notice = handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    command,
                    Some(argument),
                )
                .await
                .expect("recoverable busy selection");
                assert_eq!(
                    notice,
                    "Error: Session has an active operation; selection unchanged. Try again when it finishes"
                );
                assert_eq!(view, before);
                assert_eq!(session_id, original_id);
                let store = Store::open_read_only(
                    StateRoot::open_existing(&admission.state_dir).expect("State"),
                )
                .expect("rejected selection Store");
                assert_eq!(
                    store
                        .load_session(session_id)
                        .await
                        .expect("unchanged prefix"),
                    prefix
                );
                store.close().await.expect("close rejection Store");
            }
            flock(&held, FlockOperation::Unlock).expect("release operation");
            let workspace = admission.workspace.clone();
            admission.workspace = temp.path().join("other-workspace");
            std::fs::create_dir(&admission.workspace).expect("other Workspace");
            assert!(matches!(
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    InteractiveCommand::Model,
                    Some("model-1 low"),
                )
                .await
                .expect_err("Workspace failure stays fatal"),
                LoopError::Fatal(error) if error == "Session belongs to a different Workspace"
            ));
            assert_eq!(view, before);
            admission.workspace = workspace;
            assert_eq!(
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    InteractiveCommand::Model,
                    Some("model-1 low"),
                )
                .await
                .expect("explicit choice after unlock"),
                "Model: model-1 · low"
            );
            assert_eq!(view.defaults.effort, Some(Effort::Low));
        }

        let store = Store::open(StateRoot::open_existing(&admission.state_dir).expect("State"))
            .expect("interrupted source Store");
        let goal_id = arany::GoalId::new();
        let run_id = RunId::new();
        let workspace_identity = view.workspace_identity.expect("Session Workspace identity");
        store
            .append(
                original_id,
                arany::Event::GoalStarted {
                    goal_id,
                    objective: "Prior accepted objective".into(),
                    workspace_identity,
                    budget: arany::GoalBudget::default(),
                    phase: arany::GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: arany::ToolResourceSettings::default(),
                },
            )
            .await
            .expect("start interrupted source Goal");
        store
            .append(
                original_id,
                arany::Event::GoalRunLinked {
                    goal_id,
                    run_id,
                    phase: arany::GoalPhase::Implementation,
                },
            )
            .await
            .expect("link interrupted source Run");
        store
            .append(
                original_id,
                arany::Event::MessageAccepted {
                    run_id,
                    text: "Prior accepted objective".into(),
                    images: Vec::new(),
                },
            )
            .await
            .expect("accepted source message");
        store.close().await.expect("close interrupted source Store");
        view = load_view(&admission.state_dir, original_id)
            .await
            .expect("interrupted source");
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        let current_defaults = view.defaults.clone();
        handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::New,
            None,
        )
        .await
        .expect("new Session notice");
        assert_ne!(session_id, original_id);
        assert_eq!(view.defaults, current_defaults);
        assert_eq!(view.title, "New Session");
        assert!(view.runs.is_empty());

        handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Resume,
            Some(&original_id.to_string()),
        )
        .await
        .expect("resume notice");
        assert_eq!(session_id, original_id);
        assert_eq!(view.title, "Renamed");
        assert_eq!(view.runs[0].objective, "Prior accepted objective");

        admission.defaults = SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: Some(Effort::Low),
            account_id: None,
            policy: CollaborationPolicy::Single,
        };
        for (index, defaults) in [
            SessionDefaults::default(),
            SessionDefaults {
                provider: Some("anthropic".into()),
                model: Some("claude-sonnet-5".into()),
                effort: Some(Effort::High),
                account_id: Some(uuid::Uuid::now_v7()),
                policy: CollaborationPolicy::Team {
                    max_active_children: 2,
                },
            },
            SessionDefaults {
                provider: Some("chatgpt".into()),
                model: None,
                effort: None,
                account_id: Some(uuid::Uuid::now_v7()),
                policy: CollaborationPolicy::Auto {
                    max_active_children: 1,
                },
            },
            current_defaults,
        ]
        .into_iter()
        .enumerate()
        {
            persist_defaults(&admission, session_id, defaults.clone())
                .await
                .expect("current selection");
            view = load_view(&admission.state_dir, session_id)
                .await
                .expect("current view");
            let source_id = session_id;
            let source_view = view.clone();
            let store = Store::open_read_only(
                StateRoot::open_existing(&admission.state_dir).expect("State"),
            )
            .expect("source Store");
            let source_events = store.load_session(source_id).await.expect("source prefix");
            store.close().await.expect("close source Store");
            let notice = handle_command(
                &admission,
                &mut session_id,
                &mut view,
                InteractiveCommand::New,
                None,
            )
            .await
            .expect("fresh conversation");
            assert_ne!(session_id, source_id);
            assert_eq!(notice, "New conversation · send a message to save it");
            assert_eq!(view.title, "New Session");
            assert_eq!(view.defaults, defaults);
            assert!(view.runs.is_empty() && view.compactions.is_empty());
            let store = Store::open_read_only(
                StateRoot::open_existing(&admission.state_dir).expect("State"),
            )
            .expect("closed replay Store");
            assert_eq!(
                store
                    .load_session(source_id)
                    .await
                    .expect("unchanged prefix"),
                source_events
            );
            let reopened_source = store
                .load_view(source_id)
                .await
                .expect("source replay")
                .expect("source Session");
            assert_eq!(reopened_source.title, source_view.title);
            assert_eq!(reopened_source.defaults, source_view.defaults);
            let events = store.load_session(session_id).await.expect("new prefix");
            assert!(events.is_empty(), "new/clear must leave no empty history");
            assert!(
                store
                    .load_view(session_id)
                    .await
                    .expect("unsaved lookup")
                    .is_none()
            );
            store.close().await.expect("close replay Store");
            let title = [
                "Local title",
                "New Session",
                "Forked Session",
                "Local title",
            ][index];
            handle_command(
                &admission,
                &mut session_id,
                &mut view,
                InteractiveCommand::Rename,
                Some(title),
            )
            .await
            .expect("local title");
            assert_eq!(view.created_sequence, 0);
            super::super::materialize_session(&admission, &mut session_id, &mut view)
                .await
                .expect("first submission");
            let reopened = load_view(&admission.state_dir, session_id)
                .await
                .expect("new replay");
            assert_eq!(reopened.defaults, defaults);
            assert_eq!(reopened.title, title);
            assert!(reopened.title_is_explicit);
            assert!(reopened.runs.is_empty() && reopened.compactions.is_empty());
        }
    }
}
