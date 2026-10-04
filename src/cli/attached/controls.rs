use super::{Admission, load_view, persist_defaults, recover_defaults_change};
use crate::cli::{chatgpt, exec::ProviderArg};
use arany::{
    CollaborationPolicy, CommandAvailability, CommandParseError, Composer, CustomProfile, Effort,
    InteractiveCommand, SessionId, SessionView, StateRoot, Submission, create_session,
    fork_session, rename_session, resume_session, validate_native_model_id,
};
use std::str::FromStr;

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
) -> Result<String, String> {
    let Some(profile) = view.defaults.provider.as_deref() else {
        return Ok("Select /provider first".into());
    };
    if validate_native_model_id(model).is_err() {
        return Ok("Error: invalid native model".into());
    }
    if let Some(name) = profile.strip_prefix("custom:") {
        let root = StateRoot::open_existing(&admission.state_dir)
            .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
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
    if let Err(error) = persist_defaults(admission, session_id, defaults).await {
        return recover_defaults_change(error);
    }
    *view = load_view(&admission.state_dir, session_id).await?;
    Ok(format!(
        "Model: {model} · {}",
        effort.map_or("default", Effort::as_str)
    ))
}

pub(super) enum ActiveSubmission {
    RetainedDraft,
    Rejected(String),
    Notice(String),
    OpenAgents,
    OpenHelp,
    ReadClipboard,
}

pub(super) fn handle_active_submission(
    composer: &mut Composer,
    view: &SessionView,
    starting_sequence: u64,
) -> ActiveSubmission {
    let notice = match composer.submission() {
        Ok(Submission::Objective(_)) => return ActiveSubmission::RetainedDraft,
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
            Err("Title must contain 1 to 128 UTF-8 bytes")
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
        _ => Ok(()),
    }
}

pub(super) async fn handle_command(
    admission: &Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
    command: InteractiveCommand,
    argument: Option<&str>,
) -> Result<String, String> {
    let notice = match command {
        InteractiveCommand::New => {
            let defaults = view.defaults.clone();
            let root = StateRoot::admit(&admission.state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            let new_id = create_session(root, admission.workspace.clone(), None)
                .await
                .map_err(|error| error.to_string())?;
            if defaults != arany::SessionDefaults::default() {
                persist_defaults(admission, new_id, defaults).await?;
            }
            let new_view = load_view(&admission.state_dir, new_id).await?;
            *session_id = new_id;
            *view = new_view;
            format!("New Session {session_id}")
        }
        InteractiveCommand::Resume => {
            let Some(id) = argument.and_then(|value| SessionId::from_str(value).ok()) else {
                return Ok("Usage: /resume SESSION_ID".into());
            };
            let root = StateRoot::open_existing(&admission.state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            match resume_session(root, admission.workspace.clone(), id).await {
                Ok(resumed) => {
                    *session_id = id;
                    *view = resumed;
                    format!("Resumed Session {id}")
                }
                Err(error) => format!("Error: {error}"),
            }
        }
        InteractiveCommand::Fork => {
            let source_id = match argument {
                Some(value) => match SessionId::from_str(value) {
                    Ok(id) => id,
                    Err(_) => return Ok("Usage: /fork [SESSION_ID]".into()),
                },
                None => *session_id,
            };
            let root = StateRoot::open_existing(&admission.state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            match fork_session(root, admission.workspace.clone(), source_id, None).await {
                Ok(id) => {
                    let forked = load_view(&admission.state_dir, id).await?;
                    *session_id = id;
                    *view = forked;
                    format!("Forked Session {id}")
                }
                Err(error) => format!("Error: {error}"),
            }
        }
        InteractiveCommand::Rename => {
            let Some(title) = argument else {
                return Ok("Usage: /rename TITLE".into());
            };
            let root = StateRoot::open_existing(&admission.state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            match rename_session(
                root,
                admission.workspace.clone(),
                *session_id,
                title.to_owned(),
            )
            .await
            {
                Ok(()) => {
                    *view = load_view(&admission.state_dir, *session_id).await?;
                    format!("Renamed Session: {title}")
                }
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
                    if let Err(error) = persist_defaults(admission, *session_id, defaults).await {
                        return recover_defaults_change(error);
                    }
                    *view = load_view(&admission.state_dir, *session_id).await?;
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
                    if let Err(error) = persist_defaults(admission, *session_id, defaults).await {
                        return recover_defaults_change(error);
                    }
                    *view = load_view(&admission.state_dir, *session_id).await?;
                    format!("Next-Run collaboration: {policy:?}")
                }
                Err(error) => error.into(),
            },
            None => format!("Next-Run collaboration: {:?}", view.defaults.policy),
        },
        InteractiveCommand::Status => format!("Session {session_id} · {} Runs", view.runs.len()),
        InteractiveCommand::Help => {
            return Err("Help requires the terminal command registry".into());
        }
        InteractiveCommand::Permissions => {
            if admission.tools {
                "Tools requested from private tools.json; native protection must be admitted before a Run. File mutation requires the write grant; command changes are discarded. Primary only; commands/MCP have no network, host home or account state. Arany requests: selected Provider; OS TLS checks may connect separately.".into()
            } else {
                "Read-only Workspace; Arany requests: selected Provider; OS TLS checks may connect separately; no Tools or sandbox".into()
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
            Some(run) => match run
                .config
                .as_ref()
                .and_then(|config| config.context_usage.as_ref())
                .filter(|_| run.status == arany::RunStatus::Active)
            {
                Some(usage) => format!(
                    "Run {:?} · input {}/{} B · {} agents · Session {} · Run {}",
                    run.status,
                    usage.used_bytes,
                    usage.budget_bytes,
                    run.agents.len(),
                    view.id,
                    run.id
                ),
                None => format!(
                    "Session {} · Run {} · {:?} · {} agents",
                    view.id,
                    run.id,
                    run.status,
                    run.agents.len()
                ),
            },
            None => format!("Session {} · Run admission in progress", view.id),
        },
        InteractiveCommand::Sessions => {
            format!("Current Session {}; switching locked for this run", view.id)
        }
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
                "Primary has pinned guarded Tool grants: at most 16 Tool calls and 64 KiB of Tool context. Commands/MCP use a private selected-project copy without network or host credentials; their file changes are discarded. Children remain read-only. Arany requests: selected Provider; OS TLS checks may connect separately.".into()
            } else {
                "Read-only Workspace; Arany requests: selected Provider; OS TLS checks may connect separately; no Tools or sandbox".into()
            }
        }
        InteractiveCommand::Quit => {
            "Ctrl+C cancels the active Run; /quit exits after it ends".into()
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
    use crate::cli::attached::EntryMode;
    use arany::{
        AgentRole, AgentRunId, AgentStatus, AgentView, ContextUsage, RunConfig, RunId, RunStatus,
        RunView, SessionDefaults, Store, TerminalInput,
    };

    #[test]
    fn active_submissions_keep_objectives_local_and_lock_mutations() {
        let mut view = SessionView {
            id: SessionId::new(),
            title: "Current".into(),
            workspace_identity: None,
            defaults: SessionDefaults::default(),
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
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
            ActiveSubmission::RetainedDraft
        ));
        assert_eq!(composer.text(), "//literal");

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
            ("/models unused", "Usage: /models"),
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

        for command in ["/new", "/models"] {
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
                status: AgentStatus::Active,
            }],
            assistant_message: None,
            status: RunStatus::Active,
            accepted_sequence: 2,
            finished_sequence: None,
            tools: Vec::new(),
        });
        assert_eq!(
            active_command_notice(&view, InteractiveCommand::Status, starting_sequence),
            format!("Session {} · Run {run_id} · Active · 1 agents", view.id)
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
        assert!(notice.starts_with("Run Active · input 32000/65536 B · 1 agents"));
        assert!(notice.contains(&format!("Session {} · Run {run_id}", view.id)));
        assert!("Run Active · input 32000/65536 B".chars().count() <= 40);
        for status in [RunStatus::Finished, RunStatus::Failed, RunStatus::Cancelled] {
            view.runs.last_mut().expect("latest Run").status = status;
            assert_eq!(
                active_command_notice(&view, InteractiveCommand::Status, starting_sequence),
                format!("Session {} · Run {run_id} · {status:?} · 1 agents", view.id),
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
            (
                "/status",
                format!("Session {} · Run admission in progress", view.id),
            ),
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
            ActiveSubmission::RetainedDraft
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
    fn idle_validation_rejects_local_arguments_before_any_effect() {
        let mut view = SessionView {
            id: SessionId::new(),
            title: "Current".into(),
            workspace_identity: None,
            defaults: SessionDefaults::default(),
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
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
                "Title must contain 1 to 128 UTF-8 bytes",
            ),
            (
                InteractiveCommand::Rename,
                "",
                "Title must contain 1 to 128 UTF-8 bytes",
            ),
            (
                InteractiveCommand::Rename,
                oversized_unicode_title.as_str(),
                "Title must contain 1 to 128 UTF-8 bytes",
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
        assert!(validate_idle_command(&view, InteractiveCommand::Quit, None).is_ok());
        for title in ["x".repeat(128), "é".repeat(64)] {
            assert!(validate_idle_command(&view, InteractiveCommand::Rename, Some(&title)).is_ok());
        }
    }

    #[tokio::test]
    async fn lifecycle_commands_switch_only_after_a_valid_durable_operation() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let mut admission = Admission {
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            workspace,
            include_paths: Vec::new(),
            tools: false,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
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
            assert_eq!(
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    InteractiveCommand::Model,
                    Some("model-1 low"),
                )
                .await
                .expect_err("Workspace failure stays fatal"),
                "Session belongs to a different Workspace"
            );
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
        store
            .append(
                original_id,
                arany::Event::MessageAccepted {
                    run_id: RunId::new(),
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
        for defaults in [
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
        ] {
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
            assert_eq!(notice, format!("New Session {session_id}"));
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
            assert_eq!(
                events.len(),
                1 + usize::from(defaults != SessionDefaults::default())
            );
            assert!(
                events
                    .iter()
                    .all(|event| event.run_id.is_none() && event.agent_run_id.is_none())
            );
            assert!(matches!(
                events[0].event,
                arany::Event::SessionStarted { .. }
            ));
            let reopened = store
                .load_view(session_id)
                .await
                .expect("new replay")
                .expect("new Session");
            assert_eq!(reopened.defaults, defaults);
            assert!(reopened.runs.is_empty() && reopened.compactions.is_empty());
            store.close().await.expect("close replay Store");
        }
    }
}
