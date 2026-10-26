use super::escape_terminal;
use arany::{AgentRole, AgentStatus, CollaborationPolicy, RunStatus, SessionView};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) const MAX_ROW_WIDTH: usize = 240;
const MAX_ROW_BYTES: usize = 1024;
const MAX_VISIBLE_AGENTS: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DraftAction {
    Submit,
    InspectRun,
    Retain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PresentationModel {
    pub(crate) status_line: String,
    pub(crate) activity_lines: Vec<String>,
    pub(crate) setup_required: bool,
    pub(crate) draft_action: DraftAction,
    pub(crate) empty_chat: bool,
    // The current or last Run has children, so an empty composer can switch agent chats.
    pub(crate) agent_switch: bool,
}

impl PresentationModel {
    pub(crate) fn from_session(view: &SessionView, width: u16) -> Self {
        Self::project(view, width, false)
    }

    pub(crate) fn preparing(view: &SessionView, width: u16) -> Self {
        Self::project(view, width, true)
    }

    fn project(view: &SessionView, width: u16, preparing: bool) -> Self {
        let width = usize::from(width).min(MAX_ROW_WIDTH);
        let run = view.runs.last().filter(|_| !preparing);
        let state = if preparing {
            "preparing"
        } else {
            chat_state_label(run.map(|run| run.status))
        };
        let pinned = run
            .filter(|run| run.status == RunStatus::Active)
            .and_then(|run| run.config.as_ref());
        let setup_required = !preparing && requires_setup(view, run.map(|run| run.status));
        let model = pinned
            .map(|config| config.model.as_str())
            .or(view.defaults.model.as_deref())
            .unwrap_or("model unset");
        let mut fields = vec![state.to_owned()];
        if width >= 14 {
            let model_label = if width >= 50 {
                let pinned_provider = pinned.map(|config| config.provider.profile());
                let provider = pinned_provider
                    .as_deref()
                    .or(view.defaults.provider.as_deref())
                    .unwrap_or("provider unset");
                let provider = safe_truncate(provider, if width >= 80 { 12 } else { 8 });
                let model_limit = if width >= 120 {
                    32
                } else if width >= 80 {
                    20
                } else {
                    width
                        .saturating_sub(state.len() + 4 + UnicodeWidthStr::width(provider.as_str()))
                };
                format!("{provider}/{}", model_id_preview(model, model_limit))
            } else {
                model_id_preview(model, width.saturating_sub(state.len() + 3))
            };
            if !model_label.is_empty() {
                fields.push(model_label);
            }
        }
        if width >= 80 {
            let title = safe_truncate(
                &view.conversation_title(),
                if width >= 120 { 32 } else { 12 },
            );
            let occupied = UnicodeWidthStr::width(fields.join(" · ").as_str());
            if occupied + 3 + UnicodeWidthStr::width(title.as_str()) <= width {
                fields.push(title);
            }
        }
        let status_line = if setup_required && width >= 23 {
            "setup required · /setup".to_owned()
        } else if setup_required && width >= 14 {
            "setup · /setup".to_owned()
        } else if setup_required {
            safe_truncate("setup", width)
        } else if width < 14 {
            safe_truncate(state, width)
        } else {
            safe_truncate(&fields.join(" · "), width)
        };
        let mut activity_lines = run
            .filter(|run| run.status == RunStatus::Active)
            .map(|run| {
                let mut lines: Vec<_> =
                    run.agents
                        .iter()
                        .take(MAX_VISIBLE_AGENTS)
                        .map(|agent| {
                            let role = match agent.role {
                                AgentRole::Primary => "primary".to_owned(),
                                AgentRole::Child => format!("child {}", agent.ordinal),
                            };
                            let status = if agent.role == AgentRole::Primary
                                && agent.status == AgentStatus::Active
                            {
                                "working"
                            } else {
                                agent_state_label(agent.status)
                            };
                            let prefix = format!("{role} · {status}");
                            let tool = run.tools.last().into_iter().chain(agent.tools.last()).find(
                                |tool| {
                                    tool.intent.agent_run_id == agent.id
                                        && tool.observation.is_none()
                                },
                            );
                            let detail = tool
                                .map(|tool| tool.intent.call.label())
                                .or(agent.summary.as_deref())
                                .or_else(|| {
                                    (agent.status == AgentStatus::Active).then_some(
                                        if agent.role == AgentRole::Child {
                                            "Queued or awaiting response; Ctrl+C cancels"
                                        } else {
                                            "Waiting for response; Ctrl+C cancels"
                                        },
                                    )
                                })
                                .or(agent.objective.as_deref());
                            match detail {
                                Some(detail) if !detail.is_empty() => {
                                    let prefix = format!("{prefix} · ");
                                    let remaining = width
                                        .saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
                                    if remaining == 0 {
                                        safe_truncate(&format!("{prefix}…"), width)
                                    } else {
                                        format!("{prefix}{}", safe_truncate(detail, remaining))
                                    }
                                }
                                _ => safe_truncate(&prefix, width),
                            }
                        })
                        .collect();
                if run.agents.len() > MAX_VISIBLE_AGENTS {
                    lines.push(safe_truncate(
                        &format!(
                            "+{} more agents (use /agents)",
                            run.agents.len() - MAX_VISIBLE_AGENTS
                        ),
                        width,
                    ));
                }
                lines
            })
            .unwrap_or_default();
        if preparing {
            activity_lines.push(safe_truncate("Preparing message · Ctrl+C cancels", width));
        }
        Self {
            status_line,
            activity_lines,
            setup_required,
            empty_chat: view.runs.is_empty(),
            agent_switch: !preparing
                && view.runs.last().is_some_and(|run| {
                    run.agents
                        .iter()
                        .any(|agent| agent.role == AgentRole::Child)
                }),
            draft_action: if preparing {
                DraftAction::Retain
            } else if run.is_some_and(|run| run.status == RunStatus::Active) {
                DraftAction::InspectRun
            } else {
                DraftAction::Submit
            },
        }
    }
}

pub(crate) fn tool_capacity_notice(
    view: &SessionView,
    id: Option<uuid::Uuid>,
) -> Option<&'static str> {
    let id = id?;
    let run = view
        .runs
        .last()
        .filter(|run| run.status == RunStatus::Active)?;
    run.tools
        .last()
        .filter(|tool| tool.intent.id == id && tool.observation.is_none())?;
    Some("Tool queued · waiting for capacity · Ctrl+C cancels")
}

pub(crate) fn linear_session_lines(view: &SessionView) -> Vec<String> {
    let run = view.runs.last();
    let pinned = run
        .filter(|run| run.status == RunStatus::Active)
        .and_then(|run| run.config.as_ref());
    let pinned_provider = pinned.map(|config| config.provider.profile());
    let provider = pinned_provider
        .as_deref()
        .or(view.defaults.provider.as_deref())
        .unwrap_or("unset");
    let model = pinned
        .map(|config| config.model.as_str())
        .or(view.defaults.model.as_deref())
        .unwrap_or("unset");
    let policy = pinned.map_or(view.defaults.policy, |config| config.policy);
    let mut lines = vec![
        if view.created_sequence == 0 {
            "Conversation: new · not saved".into()
        } else {
            format!("Session: {}", view.id)
        },
        format!(
            "Title: {}",
            safe_truncate(&view.conversation_title(), MAX_ROW_WIDTH)
        ),
        format!("State: {}", chat_state_label(run.map(|run| run.status))),
        format!("Provider: {}", safe_truncate(provider, MAX_ROW_WIDTH)),
        format!("Model: {}", safe_truncate(model, MAX_ROW_WIDTH)),
    ];
    if requires_setup(view, run.map(|run| run.status)) {
        lines.push("Setup: type /setup to choose an account".into());
    }
    lines.push(if pinned.is_some_and(|config| config.tool_policy.is_some()) {
        "Permissions: this Run has pinned primary Tool grants; commands/MCP are isolated and offline; no child effects".into()
    } else { "Permissions: read-only Workspace; Arany requests: selected Provider; OS TLS checks may connect separately; no Tools or sandbox".into() });
    lines.push(match policy {
        CollaborationPolicy::Single => "Collaboration: single".into(),
        CollaborationPolicy::Auto {
            max_active_children,
        } => format!("Collaboration: auto; max children: {max_active_children}"),
        CollaborationPolicy::Team {
            max_active_children,
        } => format!("Collaboration: team; max children: {max_active_children}"),
    });
    if let Some(run) = run {
        if !matches!(run.status, RunStatus::Pending | RunStatus::Active) {
            lines.push(format!("Last task: {}", run_state_label(Some(run.status))));
        }
        if let Some(usage) = pinned.and_then(|config| config.context_usage.as_ref()) {
            lines.push(format!(
                "Request size: {}% of Arany's local limit",
                usage.utilization_percent()
            ));
            lines.push(
                "Includes chat, instructions, files and images; not the model's token limit".into(),
            );
        }
        lines.push(format!("Run: {}", run.id));
        for agent in &run.agents {
            let role = match agent.role {
                AgentRole::Primary => "primary".to_owned(),
                AgentRole::Child => format!("child {}", agent.ordinal),
            };
            lines.push(format!(
                "Agent: {role}; state: {}",
                agent_state_label(agent.status)
            ));
        }
    }
    lines
}

fn requires_setup(view: &SessionView, status: Option<RunStatus>) -> bool {
    view.defaults.provider.is_none()
        && !matches!(status, Some(RunStatus::Pending | RunStatus::Active))
}

fn chat_state_label(status: Option<RunStatus>) -> &'static str {
    match status {
        Some(RunStatus::Pending) => "starting",
        Some(RunStatus::Active) => "working",
        _ => "ready",
    }
}

pub(super) fn run_state_label(status: Option<RunStatus>) -> &'static str {
    status.map_or("idle", RunStatus::label)
}

pub(super) fn agent_state_label(status: AgentStatus) -> &'static str {
    status.label()
}

pub(crate) fn safe_truncate(value: &str, width: usize) -> String {
    let width = width.min(MAX_ROW_WIDTH);
    if width == 0 {
        return String::new();
    }
    let mut pieces = Vec::new();
    let mut used_cells = 0_usize;
    let mut used_bytes = 0_usize;
    let mut overflow = false;
    for grapheme in value.graphemes(true) {
        let safe = escape_terminal(grapheme);
        let cells = UnicodeWidthStr::width(safe.as_str());
        if used_cells.saturating_add(cells) > width
            || used_bytes.saturating_add(safe.len()) > MAX_ROW_BYTES
        {
            overflow = true;
            break;
        }
        used_cells += cells;
        used_bytes += safe.len();
        pieces.push((safe, cells));
    }
    if overflow {
        while used_cells == width || used_bytes + '…'.len_utf8() > MAX_ROW_BYTES {
            let Some((piece, cells)) = pieces.pop() else {
                break;
            };
            used_cells -= cells;
            used_bytes -= piece.len();
        }
    }
    let mut result = String::with_capacity(used_bytes + if overflow { 3 } else { 0 });
    for (piece, _) in pieces {
        result.push_str(&piece);
    }
    if overflow {
        result.push('…');
    }
    result
}

pub(crate) fn model_id_preview(id: &str, width: usize) -> String {
    let width = width.min(MAX_ROW_WIDTH);
    if width < 3 || !id.bytes().all(|byte| byte.is_ascii_graphic()) || id.len() <= width {
        return safe_truncate(id, width);
    }
    let left = (width - 1) / 2;
    let right = width - 1 - left;
    format!("{}…{}", &id[..left], &id[id.len() - right..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::{AgentInspectorModel, AgentQuery, run_feedback};
    use arany::{
        AgentRunId, AgentView, ContextUsage, ProviderCallDisposition, ProviderCallRecord,
        RunConfig, RunId, RunView, SessionDefaults, SessionId,
    };

    fn session() -> SessionView {
        SessionView {
            usage: Default::default(),
            rewinds: Default::default(),
            id: SessionId::new(),
            title: "work\u{1b}[31m\nSession: forged".into(),
            title_is_explicit: true,
            inherited_title: None,
            workspace_identity: None,
            defaults: Default::default(),
            host_epoch_floor: 0,
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
            runs: Vec::new(),
            compactions: Vec::new(),
            goals: Vec::new(),
            pending_provider_setup_edit: None,
            provider_setup_uncertain: false,
        }
    }

    #[test]
    fn semantic_rows_are_bounded_in_all_layouts() {
        let mut view = session();
        let linear = linear_session_lines(&view);
        assert_eq!(
            linear[1],
            "Title: work\\u{001b}[31m\\u{000a}Session: forged"
        );
        assert_eq!(linear[2], "State: ready");
        assert_eq!(linear[3], "Provider: unset");
        assert!(
            linear
                .iter()
                .all(|line| !line.contains(['\u{1b}', '\r', '\n']))
        );
        for width in [40, 50, 79, 80, 120] {
            let model = PresentationModel::from_session(&view, width);
            assert!(UnicodeWidthStr::width(model.status_line.as_str()) <= usize::from(width));
            assert!(!model.status_line.contains('\u{1b}'));
            assert!(!model.status_line.contains('\n'));
            assert!(model.activity_lines.is_empty());
            assert_eq!(model.draft_action, DraftAction::Submit);
            assert!(model.setup_required);
            assert_eq!(model.status_line, "setup required · /setup");
            assert!(!model.status_line.contains("read-only"));
        }
        assert_eq!(
            PresentationModel::from_session(&view, 16).status_line,
            "setup · /setup"
        );
        view.defaults = SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: None,
            account_id: None,
            policy: CollaborationPolicy::Single,
        };
        let idle = PresentationModel::from_session(&view, 80);
        assert!(!idle.setup_required);
        assert!(idle.status_line.contains("openai/gpt-5.4"));
        assert!(!idle.status_line.contains("input "));
        let run_id = RunId::new();
        view.runs.push(RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: run_id,
            objective: "task".into(),
            images: Vec::new(),
            config: Some(RunConfig {
                supervisor: None,
                worker_policy: None,
                agent_definitions: None,
                roster: None,
                providers: None,
                provider: arany::ProviderRecord::parse("anthropic").expect("Provider record"),
                model: "claude-sonnet-5".into(),
                effort: None,
                custom_profile_provenance: None,
                saved_api_account_id: None,
                chatgpt_provenance: None,
                output_token_bound: arany::OutputTokenBound::ProviderEnforced,
                context_window_tokens: None,
                policy: CollaborationPolicy::Team {
                    max_active_children: 8,
                },
                output_token_cap: 4096,
                provider_concurrency: 1,
                workspace_device: 1,
                workspace_inode: 1,
                user_instruction_digest: None,
                instruction_sources: Vec::new(),
                launch_directory: String::new(),
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
            }),
            agents: std::iter::once(AgentView {
                profile: None,
                assignment: None,
                definition: None,
                id: AgentRunId::new(),
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                summary: None,
                result: None,
                provider_calls: Vec::new(),
                steering_notes: Vec::new(),
                grant: None,
                tools: Vec::new(),
                status: AgentStatus::Active,
            })
            .chain((1..=5).map(|ordinal| AgentView {
                profile: None,
                assignment: None,
                definition: None,
                id: AgentRunId::new(),
                role: AgentRole::Child,
                ordinal,
                objective: Some("investigate \u{1b}[2J\nRun: forged 👩‍💻".into()),
                summary: None,
                result: None,
                provider_calls: Vec::new(),
                steering_notes: Vec::new(),
                grant: None,
                tools: Vec::new(),
                status: if ordinal == 1 {
                    AgentStatus::Failed
                } else {
                    AgentStatus::Active
                },
            }))
            .collect(),
            assistant_message: None,
            status: RunStatus::Active,
            accepted_sequence: 2,
            finished_sequence: None,
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_recent_files: Vec::new(),
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
        });
        let model = PresentationModel::from_session(&view, 80);
        assert_eq!(model.draft_action, DraftAction::InspectRun);
        let id = uuid::Uuid::now_v7();
        assert!(tool_capacity_notice(&view, Some(id)).is_none());
        let run = view.runs.last_mut().unwrap();
        let intent = arany::EffectIntent {
            host: None,
            host_command: None,
            target: arany::worker::EffectTarget::Workspace,
            id,
            run_id,
            agent_run_id: run.agents[0].id,
            policy_digest: [1; 32],
            enforcement_digest: [1; 32],
            workspace_device: 1,
            workspace_inode: 1,
            call: arany::ToolCall::List { path: ".".into() },
            limits: Default::default(),
            use_count: 1,
        };
        run.tools.push(arany::ToolView {
            checkpoint: None,
            intent: intent.clone(),
            observation: None,
            review_call_index: None,
            active_time_reserved: false,
            narrowing_source: None,
            permission_verdict: None,
            started_sequence: Some(0),
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        });
        assert!(
            tool_capacity_notice(&view, Some(id))
                .unwrap()
                .contains("queued")
        );
        assert!(tool_capacity_notice(&view, None).is_none());
        assert!(tool_capacity_notice(&view, Some(uuid::Uuid::now_v7())).is_none());
        let mut list = crate::presentation::tool_fixture(
            arany::ToolCall::List { path: ".".into() },
            r#"{"entries":["a","b"]}"#,
        );
        let mut read = crate::presentation::tool_fixture(
            arany::ToolCall::Read {
                path: "a".into(),
                offset: None,
                limit: Some(100),
            },
            r#"{"text":"a\nb\n"}"#,
        );
        list.intent.agent_run_id = intent.agent_run_id;
        read.intent.agent_run_id = intent.agent_run_id;
        let tools = vec![list, read];
        for width in [80, 120] {
            let model = PresentationModel::from_session(&view, width);
            assert!(model.activity_lines[0].contains("primary · working"));
            let rows = crate::presentation::tool_groups(&tools).collect::<Vec<_>>();
            assert_eq!(
                rows,
                vec![(
                    0..2,
                    "⏺ Read 1 file, listed 1 directory\n  ⎿ 2 entries, 2 lines".into()
                )]
            );
            assert!(
                rows[0]
                    .1
                    .lines()
                    .all(|line| UnicodeWidthStr::width(line) <= usize::from(width))
            );
        }
        let run = view.runs.last_mut().unwrap();
        let mut intent = intent;
        intent.call = arany::ToolCall::Command {
            program: "cargo".into(),
            args: vec!["check".into()],
            cwd: ".".into(),
        };
        let guard = arany::GuardReceipt {
            contract_version: 1,
            intent_digest: [0; 32],
            enforcement_digest: intent.enforcement_digest,
            limits: intent.limits.clone(),
        };
        run.tools[0].intent = intent.clone();
        run.tools[0].observation = Some(arany::ToolObservation {
            spill: None,
            enforcement_cause: None,
            limit_cause: None,
            intent,
            disposition: arany::ToolDisposition::Succeeded,
            output: String::new(),
            attestation: Some(arany::Attestation::Guard(guard)),
            approval_cause: None,
            cache_update: arany::CacheUpdate::NotApplicable,
        });
        {
            let run = view.runs.last_mut().unwrap();
            run.tools[0].observation.as_mut().unwrap().cache_update = arany::CacheUpdate::Rejected;
            assert!(
                crate::presentation::tool_summary(&run.tools[0])
                    .contains("0 B · cache not saved (limit)")
            );
            assert_eq!(
                crate::presentation::render_cache_update_feedback(run),
                Some(
                    "A build cache update was rejected by its size or quota limit; the previous cache was kept."
                )
            );
            run.tools[0].observation.as_mut().unwrap().cache_update = arany::CacheUpdate::Failed;
            assert_eq!(
                crate::presentation::render_cache_update_feedback(run),
                Some("A build cache update failed; inspect Tool history for details.")
            );
            let observation = run.tools[0].observation.as_mut().unwrap();
            observation.disposition = arany::ToolDisposition::Succeeded;
            observation.cache_update = arany::CacheUpdate::Uncertain;
            assert!(
                crate::presentation::tool_summary(&run.tools[0])
                    .contains("0 B · cache outcome uncertain")
            );
            assert_eq!(
                crate::presentation::render_cache_update_feedback(run),
                Some("Build cache outcome is uncertain; inspect Session history before retrying.")
            );
            let mut finished = run.clone();
            finished.status = RunStatus::Finished;
            assert_eq!(
                run_feedback(&finished).as_deref(),
                Some("Build cache outcome is uncertain; inspect Session history before retrying.")
            );
            finished.status = RunStatus::Failed;
            assert_eq!(
                run_feedback(&finished).as_deref(),
                Some(
                    "Error: The command succeeded but its build cache update is uncertain; the Run stopped without retrying. No answer was committed."
                )
            );
            finished.tools[0].observation.as_mut().unwrap().disposition =
                arany::ToolDisposition::Uncertain;
            assert_eq!(
                run_feedback(&finished).as_deref(),
                Some(
                    "Error: Tool completion is uncertain. Files or effects may have changed; inspect them before retrying. No answer was committed."
                )
            );
            let observation = finished.tools[0].observation.as_mut().unwrap();
            observation.disposition = arany::ToolDisposition::Failed;
            observation.attestation = None;
            observation.cache_update = arany::CacheUpdate::NotApplicable;
            assert!(
                run_feedback(&finished)
                    .unwrap()
                    .contains("inspect the Guard stage")
            );
            let read = arany::ToolCall::Read {
                path: "src/lib.rs".into(),
                offset: None,
                limit: Some(16),
            };
            finished.tools[0].intent.call = read.clone();
            finished.tools[0].observation.as_mut().unwrap().intent.call = read;
            let feedback = run_feedback(&finished).unwrap();
            assert!(feedback.contains("Workspace read stopped before it started"));
            assert!(!feedback.contains("Guard"));
            let mkdir = arany::ToolCall::Mkdir {
                path: "src/new".into(),
            };
            finished.tools[0].intent.call = mkdir.clone();
            finished.tools[0].observation.as_mut().unwrap().intent.call = mkdir;
            assert_eq!(
                run_feedback(&finished).as_deref(),
                Some(
                    "Error: A Workspace action stopped before it started because native Tool protection was unavailable or its Workspace or intent could not be verified. That action did not run; earlier completed actions remain. Use /agents to inspect this Run."
                )
            );
            let observation = run.tools[0].observation.as_mut().unwrap();
            observation.disposition = arany::ToolDisposition::Succeeded;
            observation.cache_update = arany::CacheUpdate::NotApplicable;
        }
        assert!(tool_capacity_notice(&view, Some(id)).is_none());
        view.runs.last_mut().unwrap().tools[0].observation = None;
        view.runs.last_mut().unwrap().status = RunStatus::Finished;
        assert!(tool_capacity_notice(&view, Some(id)).is_none());
        view.runs.last_mut().unwrap().status = RunStatus::Active;
        view.runs.last_mut().unwrap().tools.clear();
        assert!(model.status_line.contains("anthropic/claude-sonnet-5"));
        assert!(!model.status_line.contains("context "));
        for (used, budget, percent) in [(0, 65_536, 0), (1, 2, 50), (65_536, 65_536, 100)] {
            let usage = view
                .runs
                .last_mut()
                .unwrap()
                .config
                .as_mut()
                .unwrap()
                .context_usage
                .as_mut()
                .unwrap();
            usage.used_bytes = used;
            usage.budget_bytes = budget;
            usage.compactable_bytes = used.min(20_000);
            assert!(
                !PresentationModel::from_session(&view, 80)
                    .status_line
                    .contains("context "),
                "request-size details stay out of ordinary status at {percent}%"
            );
        }
        let usage = view
            .runs
            .last_mut()
            .unwrap()
            .config
            .as_mut()
            .unwrap()
            .context_usage
            .as_mut()
            .unwrap();
        usage.used_bytes = 32_000;
        usage.budget_bytes = 65_536;
        usage.compactable_bytes = 20_000;
        assert!(
            PresentationModel::from_session(&view, 50)
                .status_line
                .contains("claude-sonnet-5")
        );
        assert!(
            PresentationModel::from_session(&view, 40)
                .status_line
                .contains("claude-sonnet-5")
        );
        assert_eq!(model.activity_lines.len(), 4);
        assert!(model.activity_lines[0].starts_with("primary · working"));
        assert!(model.activity_lines[1].starts_with("child 1 · failed"));
        assert_eq!(model.activity_lines[3], "+3 more agents (use /agents)");
        let run = view.runs.last_mut().unwrap();
        let child_read = arany::EffectIntent {
            host: None,
            host_command: None,
            target: arany::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: run.id,
            agent_run_id: run.agents[2].id,
            policy_digest: [1; 32],
            enforcement_digest: [1; 32],
            workspace_device: 1,
            workspace_inode: 1,
            call: arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: None,
            },
            limits: Default::default(),
            use_count: 1,
        };
        run.agents[2].tools.push(arany::ToolView {
            checkpoint: None,
            intent: child_read,
            observation: None,
            review_call_index: None,
            active_time_reserved: false,
            narrowing_source: None,
            permission_verdict: None,
            started_sequence: Some(0),
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        });
        let model = PresentationModel::from_session(&view, 80);
        assert!(model.activity_lines[2].contains("read"));
        let linear = linear_session_lines(&view);
        assert!(linear.contains(&"State: working".to_owned()));
        assert!(linear.contains(&"Provider: anthropic".to_owned()));
        assert!(linear.contains(&"Model: claude-sonnet-5".to_owned()));
        assert!(linear.contains(&"Collaboration: team; max children: 8".to_owned()));
        assert!(linear.contains(&"Agent: child 1; state: failed".to_owned()));
        view.title = "New Session".into();
        view.title_is_explicit = false;
        view.runs.last_mut().expect("Run").status = RunStatus::Finished;
        view.runs.last_mut().unwrap().finished_sequence = Some(10);
        view.last_sequence = 10;
        view.runs.last_mut().unwrap().agents[0].status = AgentStatus::Finished;
        view.runs.last_mut().unwrap().agents[0].summary = Some("Recognizable conversation".into());
        let after = PresentationModel::from_session(&view, 120);
        assert!(after.status_line.starts_with("ready · "));
        assert!(after.status_line.contains("task"));
        assert!(!after.status_line.contains("New Session"));
        assert_eq!(after.draft_action, DraftAction::Submit);
        assert!(after.status_line.contains("openai/gpt-5.4"));
        assert!(!after.status_line.contains("context "));
        view.title_is_explicit = true;
        assert_eq!(
            view.conversation_title(),
            "New Session",
            "explicit placeholder rename wins"
        );
        view.title_is_explicit = false;
        for status in [
            RunStatus::Active,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::Interrupted,
        ] {
            view.runs.last_mut().unwrap().status = status;
            assert_eq!(
                view.conversation_title(),
                "task",
                "the first objective names every Run disposition"
            );
        }
        view.runs.last_mut().unwrap().status = RunStatus::Finished;

        for status in [RunStatus::Active, RunStatus::Finished, RunStatus::Failed] {
            view.runs.last_mut().expect("previous Run").status = status;
            for width in [16, 40, 50, 80, 120] {
                let preparing = PresentationModel::preparing(&view, width);
                assert!(preparing.status_line.starts_with("preparing"));
                assert!(!preparing.setup_required);
                assert_eq!(preparing.draft_action, DraftAction::Retain);
                assert_eq!(preparing.activity_lines.len(), 1);
                assert!(preparing.activity_lines[0].starts_with("Preparing"));
                assert!(!preparing.status_line.contains("context "));
                assert!(!preparing.status_line.contains("claude"));
                assert!(!preparing.activity_lines[0].contains("primary"));
                if width >= 40 {
                    assert!(preparing.status_line.contains("gpt-5.4"));
                }
                assert!(UnicodeWidthStr::width(preparing.status_line.as_str()) <= width as usize);
                assert!(
                    UnicodeWidthStr::width(preparing.activity_lines[0].as_str()) <= width as usize
                );
            }
            assert_eq!(view.runs.last().expect("unchanged Run").status, status);
        }
        view.runs.last_mut().expect("previous Run").status = RunStatus::Finished;
        view.defaults.model = Some("bad\u{202e}model".into());
        let hostile = PresentationModel::from_session(&view, 80);
        assert!(hostile.status_line.contains("bad\\u{202e}model"));
        assert!(!hostile.status_line.contains('\u{202e}'));
        view.defaults.model = Some(format!("{}suffix", "same-long-prefix-".repeat(4)));
        for width in [40, 50] {
            let status = PresentationModel::from_session(&view, width).status_line;
            assert!(status.contains("suffix"), "{width}: {status}");
            assert!(status.contains('…'), "{width}: {status}");
            assert!(UnicodeWidthStr::width(status.as_str()) <= width as usize);
        }
        for row in &model.activity_lines {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 80);
            assert!(!row.contains('\u{1b}'));
            assert!(!row.contains('\n'));
        }
        let mut without_agents_view = view.clone();
        without_agents_view
            .runs
            .last_mut()
            .expect("Run")
            .agents
            .clear();
        for status in [
            RunStatus::Active,
            RunStatus::Finished,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            without_agents_view.runs.last_mut().expect("Run").status = status;
            let without_agents = PresentationModel::from_session(&without_agents_view, 40);
            assert!(without_agents.activity_lines.is_empty());
            assert_eq!(
                without_agents.draft_action,
                if status == RunStatus::Active {
                    DraftAction::InspectRun
                } else {
                    DraftAction::Submit
                }
            );
        }
        let clipped = safe_truncate("x👩‍💻y", 3);
        assert!(!clipped.contains('👩') || clipped.contains("👩‍💻"));

        let record = |disposition| {
            let reported = matches!(
                disposition,
                ProviderCallDisposition::Finished
                    | ProviderCallDisposition::Delegated
                    | ProviderCallDisposition::InvalidResponse
            );
            ProviderCallRecord {
                rate_limits: None,
                delegation: None,
                phase: arany::AgentPhase::RootPlan,
                disposition,
                response_id: reported.then(|| "PRIVATE_RESPONSE_ID".into()),
                input_tokens: reported.then_some(123),
                output_tokens: None,
                wire_provenance: None,
                refusal: None,
                failure_reason: None,
                requested_tools: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            }
        };
        let run = &mut view.runs[0];
        run.status = RunStatus::Failed;
        run.agents[0].status = AgentStatus::Failed;
        run.agents[1].status = AgentStatus::Cancelled;
        let mut child_call = record(ProviderCallDisposition::Rejected);
        child_call.phase = arany::AgentPhase::ChildWork;
        run.agents[1].provider_calls = vec![child_call];
        for (disposition, reason) in [
            (
                ProviderCallDisposition::Unavailable,
                Some("Provider unavailable. Check the connection and provider status."),
            ),
            (
                ProviderCallDisposition::Rejected,
                Some(
                    "Provider rejected the request. Check the selected account, model, and usage limits.",
                ),
            ),
            (
                ProviderCallDisposition::InvalidResponse,
                Some("Arany rejected the response during outcome validation."),
            ),
            (
                ProviderCallDisposition::OutputLimit,
                Some("Provider response exceeded Arany's output limit."),
            ),
            (
                ProviderCallDisposition::TimedOut,
                Some("Provider call timed out."),
            ),
            (
                ProviderCallDisposition::TaskPanic,
                Some("Provider task failed internally."),
            ),
            (ProviderCallDisposition::Finished, None),
            (ProviderCallDisposition::Delegated, None),
            (ProviderCallDisposition::Cancelled, None),
        ] {
            view.runs[0].agents[0].provider_calls = vec![
                record(ProviderCallDisposition::Delegated),
                record(disposition),
            ];
            let feedback = run_feedback(&view.runs[0]).expect("failed Run feedback");
            let expected = reason.map_or_else(
                || "Error: Run failed. No answer was committed. Use /agents to inspect this Run.".to_owned(),
                |reason| format!("Error: Run failed. {reason} No answer was committed. Use /agents to inspect this Run."),
            );
            assert_eq!(feedback, expected, "{disposition:?}");
            assert!(!feedback.contains("PRIVATE_RESPONSE_ID"));
            let inspector =
                AgentInspectorModel::from_session(&view, &AgentQuery::default(), 120, false);
            assert_eq!(
                inspector
                    .lines
                    .iter()
                    .find(|line| line.starts_with("Failure:")),
                reason.map(|reason| format!("Failure: {reason}")).as_ref(),
                "{disposition:?}"
            );
        }
        for (cause, expected) in [
            (
                arany::ProposalRefusalCause::InvalidPath,
                "Arany refused an invalid or protected Workspace path. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::InvalidArguments,
                "Arany refused invalid operation arguments. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::Phase,
                "Arany refused an operation unavailable in this phase. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::PlanInvalid,
                "Arany refused an invalid or changed plan. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::PlanBudgetOutsideGoal,
                "Arany refused a plan budget outside the Goal budget. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::PlanBudgetSpent,
                "Arany refused a plan budget already spent by this Run. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::PlanScopeExcludesCall,
                "Arany refused an operation outside the plan scope. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::PlanOutsideGrant,
                "Arany refused a plan outside the admitted grant. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::ArgumentBytes,
                "Arany refused operation arguments above the byte limit. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::LeaseOverlap,
                "Arany refused an operation overlapping an active worker lease. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::NotOffered,
                "Arany refused an operation that was not offered. The proposed action did not run.",
            ),
            (
                arany::ProposalRefusalCause::AnswerInvalid,
                "Arany refused an invalid text answer.",
            ),
            (
                arany::ProposalRefusalCause::EditorDigest,
                "Editor definition digest does not match. No child started.",
            ),
            (
                arany::ProposalRefusalCause::DelegateInvalid,
                "Arany refused an invalid delegation. No children were started for it.",
            ),
        ] {
            let run = &mut view.runs[0];
            run.failure = Some(arany::RunFailure::ProposalRefused);
            let mut call = record(ProviderCallDisposition::Rejected);
            call.failure_reason = Some(arany::ProviderFailureReason::ProposalRefused);
            call.refusal = Some(arany::ProposalRefusal {
                operation: "arany_write".into(),
                cause,
            });
            run.agents[0].provider_calls = vec![call];
            let copy = run_feedback(run).expect("refusal feedback");
            assert_eq!(
                copy,
                format!(
                    "Error: Run failed. {expected} No answer was committed. Use /agents to inspect this Run."
                ),
                "{cause:?}"
            );
            assert!(!copy.contains("Provider"));
        }
        for (cause, expected) in [
            (
                arany::RunFailure::PlanAdmission,
                "Arany could not admit the proposed plan. The proposed action did not run.",
            ),
            (
                arany::RunFailure::ToolContextFull,
                "Tool continuation could not fit in the available context.",
            ),
            (
                arany::RunFailure::WorkersUnverified,
                "Worker results have not passed verification.",
            ),
            (
                arany::RunFailure::CriterionUnmet,
                "The plan's success criterion was not met.",
            ),
            (
                arany::RunFailure::RefusalLimit,
                "The same proposed operation was refused three times.",
            ),
            (
                arany::RunFailure::BudgetExhausted,
                "The Goal budget refused the next action.",
            ),
            (
                arany::RunFailure::DefinitionInvalid,
                "An agent definition could not be loaded.",
            ),
        ] {
            view.runs[0].failure = Some(cause);
            view.runs[0].agents[0].provider_calls.clear();
            assert_eq!(
                run_feedback(&view.runs[0]).unwrap(),
                format!(
                    "Error: Run failed. {expected} No answer was committed. Use /agents to inspect this Run."
                )
            );
        }
        view.runs[0].failure = None;
        for reason in [
            arany::ProviderFailureReason::WorkerAnswerDeferred,
            arany::ProviderFailureReason::WorkerDelegateRefused,
            arany::ProviderFailureReason::WorkerToolRefused,
            arany::ProviderFailureReason::CriterionNotMetInView,
        ] {
            let mut call = record(ProviderCallDisposition::InvalidResponse);
            call.failure_reason = Some(reason);
            view.runs[0].agents[0].provider_calls = vec![call];
            let copy = run_feedback(&view.runs[0]).expect("worker refusal feedback");
            assert!(
                copy.contains("Arany refused the proposed worker coordination outcome."),
                "{reason:?}: {copy}"
            );
            assert!(!copy.contains("Provider"));
        }
        use arany::ProviderFailureReason;
        for (reason, disposition, message) in [
            (
                ProviderFailureReason::AccountAccess,
                ProviderCallDisposition::Rejected,
                "Provider denied account access. Check the selected account and model permissions.",
            ),
            (
                ProviderFailureReason::UsageLimit,
                ProviderCallDisposition::Rejected,
                "Provider reported a usage limit. Check the selected account's usage before trying again.",
            ),
            (
                ProviderFailureReason::RateLimited,
                ProviderCallDisposition::Rejected,
                "Provider rate limited the request. Wait before trying again.",
            ),
            (
                ProviderFailureReason::UsageTemporarilyUnavailable,
                ProviderCallDisposition::Unavailable,
                "Provider usage is temporarily unavailable. Check provider status before trying again.",
            ),
            (
                ProviderFailureReason::ServiceUnavailable,
                ProviderCallDisposition::Unavailable,
                "Provider service unavailable. Check provider status before trying again.",
            ),
        ] {
            let mut call = record(disposition);
            call.failure_reason = Some(reason);
            view.runs[0].agents[0].provider_calls = vec![call];
            assert_eq!(
                run_feedback(&view.runs[0]).unwrap(),
                format!(
                    "Error: Run failed. {message} No answer was committed. Use /agents to inspect this Run."
                )
            );
            for color in [true, false] {
                let inspector =
                    AgentInspectorModel::from_session(&view, &AgentQuery::default(), 120, color);
                assert!(
                    inspector
                        .lines
                        .iter()
                        .any(|line| line == &format!("Failure: {message}"))
                );
            }
        }
        let run = &mut view.runs[0];
        run.agents[0].provider_calls.clear();
        assert_eq!(
            run_feedback(run).unwrap(),
            "Error: Run failed. No answer was committed. Use /agents to inspect this Run."
        );
        run.agents[1].status = AgentStatus::Failed;
        assert!(
            run_feedback(run)
                .unwrap()
                .contains("Provider rejected the request.")
        );
        run.agents[0].status = AgentStatus::Cancelled;
        run.agents[0].provider_calls = vec![record(ProviderCallDisposition::Unavailable)];
        assert!(
            run_feedback(run)
                .unwrap()
                .contains("Provider rejected the request.")
        );
        run.agents
            .iter_mut()
            .for_each(|agent| agent.provider_calls.clear());
        run.tools = vec![crate::presentation::tool_fixture(
            arany::ToolCall::Read {
                path: "file".into(),
                offset: None,
                limit: None,
            },
            "Action denied.",
        )];
        for (cause, expected) in [
            (Some(arany::ApprovalCause::Cancelled), "Run cancelled."),
            (Some(arany::ApprovalCause::Paused), "Goal paused."),
            (
                Some(arany::ApprovalCause::ChannelUnavailable),
                "Approval channel unavailable.",
            ),
            (Some(arany::ApprovalCause::Declined), "Action denied."),
            (
                Some(arany::ApprovalCause::ChildTimeout),
                "Child approval wait timed out.",
            ),
            (None, "Action denied."),
        ] {
            let observation = run.tools[0].observation.as_mut().unwrap();
            observation.disposition = arany::ToolDisposition::Denied;
            observation.attestation = None;
            observation.approval_cause = cause;
            let feedback = run_feedback(run).unwrap();
            if cause == Some(arany::ApprovalCause::ChannelUnavailable) {
                assert_eq!(
                    feedback,
                    "Approval channel unavailable. No further action ran; earlier completed actions remain. Start a new request from an interactive approval channel."
                );
            } else {
                assert_eq!(
                    feedback,
                    format!(
                        "{expected} No further action ran; earlier completed actions remain. Use /permissions or Shift+Tab to review approval settings."
                    )
                );
            }
            if matches!(
                cause,
                Some(arany::ApprovalCause::Cancelled | arany::ApprovalCause::Paused)
            ) {
                run.status = RunStatus::Cancelled;
                assert_eq!(
                    run_feedback(run).unwrap(),
                    format!("{expected} No answer was committed. You can submit a new task.")
                );
                run.status = RunStatus::Failed;
            }
        }
        run.tools.clear();
        run.status = RunStatus::Cancelled;
        assert_eq!(
            run_feedback(run).unwrap(),
            "Run cancelled. No answer was committed. You can submit a new task."
        );
        for status in [
            RunStatus::Pending,
            RunStatus::Active,
            RunStatus::Finished,
            RunStatus::Interrupted,
        ] {
            run.status = status;
            assert_eq!(run_feedback(run), None, "{status:?}");
        }
    }
}
