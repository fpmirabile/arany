use super::escape_terminal;
use crate::session::{AgentRole, AgentStatus, CollaborationPolicy, RunStatus, SessionView};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_ROW_CELLS: usize = 240;
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
}

impl PresentationModel {
    pub(crate) fn from_session(view: &SessionView, width: u16) -> Self {
        Self::project(view, width, false)
    }

    pub(crate) fn preparing(view: &SessionView, width: u16) -> Self {
        Self::project(view, width, true)
    }

    fn project(view: &SessionView, width: u16, preparing: bool) -> Self {
        let width = usize::from(width).min(MAX_ROW_CELLS);
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
                let provider = pinned
                    .map(|config| config.provider.as_str())
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
                let mut lines: Vec<_> = run
                    .agents
                    .iter()
                    .take(MAX_VISIBLE_AGENTS)
                    .map(|agent| {
                        let role = match agent.role {
                            AgentRole::Primary => "primary".to_owned(),
                            AgentRole::Child => format!("child {}", agent.ordinal),
                        };
                        let status = agent_state_label(agent.status);
                        let prefix = format!("{role} · {status}");
                        let tool = run.tools.last().filter(|tool| {
                            tool.intent.agent_run_id == agent.id && tool.observation.is_none()
                        });
                        let detail = tool
                            .map(|tool| tool.intent.call.label())
                            .or(agent.summary.as_deref())
                            .or_else(|| {
                                (agent.status == AgentStatus::Active)
                                    .then_some("Waiting for response; Ctrl+C cancels")
                            })
                            .or(agent.objective.as_deref());
                        match detail {
                            Some(detail) if !detail.is_empty() => {
                                let prefix = format!("{prefix} · ");
                                let remaining =
                                    width.saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
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

pub(crate) fn linear_session_lines(view: &SessionView) -> Vec<String> {
    let run = view.runs.last();
    let pinned = run
        .filter(|run| run.status == RunStatus::Active)
        .and_then(|run| run.config.as_ref());
    let provider = pinned
        .map(|config| config.provider.as_str())
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
            safe_truncate(&view.conversation_title(), MAX_ROW_CELLS)
        ),
        format!("State: {}", chat_state_label(run.map(|run| run.status))),
        format!("Provider: {}", safe_truncate(provider, MAX_ROW_CELLS)),
        format!("Model: {}", safe_truncate(model, MAX_ROW_CELLS)),
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

fn run_state_label(status: Option<RunStatus>) -> &'static str {
    match status {
        None => "idle",
        Some(RunStatus::Pending) => "starting",
        Some(RunStatus::Active) => "working",
        Some(RunStatus::Finished) => "finished",
        Some(RunStatus::Failed) => "failed",
        Some(RunStatus::Cancelled) => "cancelled",
        Some(RunStatus::Interrupted) => "interrupted",
    }
}

fn agent_state_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Active => "working",
        AgentStatus::Finished => "finished",
        AgentStatus::Failed => "failed",
        AgentStatus::Cancelled => "cancelled",
        AgentStatus::Interrupted => "interrupted",
    }
}

pub(crate) fn safe_truncate(value: &str, width: usize) -> String {
    let width = width.min(MAX_ROW_CELLS);
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
    let width = width.min(MAX_ROW_CELLS);
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
    use crate::presentation::{AgentInspectorModel, render_run_feedback};
    use crate::session::{
        AgentRunId, AgentView, ContextUsage, ProviderCallDisposition, ProviderCallRecord,
        RunConfig, RunId, RunView, SessionDefaults, SessionId,
    };

    fn session() -> SessionView {
        SessionView {
            id: SessionId::new(),
            title: "work\u{1b}[31m\nSession: forged".into(),
            title_is_explicit: true,
            inherited_title: None,
            workspace_identity: None,
            defaults: Default::default(),
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
            runs: Vec::new(),
            compactions: Vec::new(),
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
            id: run_id,
            objective: "task".into(),
            images: Vec::new(),
            config: Some(RunConfig {
                provider: "anthropic".into(),
                model: "claude-sonnet-5".into(),
                effort: None,
                custom_profile_provenance: None,
                saved_api_account_id: None,
                chatgpt_provenance: None,
                output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
                policy: CollaborationPolicy::Team {
                    max_active_children: 8,
                },
                output_token_cap: 4096,
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
            }),
            agents: std::iter::once(AgentView {
                id: AgentRunId::new(),
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                summary: None,
                result: None,
                provider_calls: Vec::new(),
                status: AgentStatus::Active,
            })
            .chain((1..=5).map(|ordinal| AgentView {
                id: AgentRunId::new(),
                role: AgentRole::Child,
                ordinal,
                objective: Some("investigate \u{1b}[2J\nRun: forged 👩‍💻".into()),
                summary: None,
                result: None,
                provider_calls: Vec::new(),
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
            tools: Vec::new(),
        });
        let model = PresentationModel::from_session(&view, 80);
        assert_eq!(model.draft_action, DraftAction::InspectRun);
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
        assert!(after.status_line.contains("Recognizable conversation"));
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
                "unaccepted summaries cannot name a conversation"
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
                phase: crate::provider::AgentPhase::RootPlan,
                disposition,
                response_id: reported.then(|| "PRIVATE_RESPONSE_ID".into()),
                input_tokens: reported.then_some(123),
                output_tokens: None,
                wire_provenance: None,
                failure_reason: None,
            }
        };
        let run = &mut view.runs[0];
        run.status = RunStatus::Failed;
        run.agents[0].status = AgentStatus::Failed;
        run.agents[1].status = AgentStatus::Cancelled;
        let mut child_call = record(ProviderCallDisposition::Rejected);
        child_call.phase = crate::provider::AgentPhase::ChildWork;
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
                Some(
                    "Provider returned an incompatible response. Check the selected model and effort.",
                ),
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
            let feedback = render_run_feedback(&view.runs[0]).expect("failed Run feedback");
            let expected = reason.map_or_else(
                || "Error: Run failed. No answer was committed. Use /agents to inspect this Run.".to_owned(),
                |reason| format!("Error: Run failed. {reason} No answer was committed. Use /agents to inspect this Run."),
            );
            assert_eq!(feedback, expected, "{disposition:?}");
            assert!(!feedback.contains("PRIVATE_RESPONSE_ID"));
            let inspector = AgentInspectorModel::from_session(&view, 0, 120, false);
            assert_eq!(
                inspector
                    .lines
                    .iter()
                    .find(|line| line.starts_with("Failure:")),
                reason.map(|reason| format!("Failure: {reason}")).as_ref(),
                "{disposition:?}"
            );
        }
        use crate::provider::ProviderFailureReason;
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
                render_run_feedback(&view.runs[0]).unwrap(),
                format!(
                    "Error: Run failed. {message} No answer was committed. Use /agents to inspect this Run."
                )
            );
            for color in [true, false] {
                let inspector = AgentInspectorModel::from_session(&view, 0, 120, color);
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
            render_run_feedback(run).unwrap(),
            "Error: Run failed. No answer was committed. Use /agents to inspect this Run."
        );
        run.agents[1].status = AgentStatus::Failed;
        assert!(
            render_run_feedback(run)
                .unwrap()
                .contains("Provider rejected the request.")
        );
        run.agents[0].status = AgentStatus::Cancelled;
        run.agents[0].provider_calls = vec![record(ProviderCallDisposition::Unavailable)];
        assert!(
            render_run_feedback(run)
                .unwrap()
                .contains("Provider rejected the request.")
        );
        run.status = RunStatus::Cancelled;
        assert_eq!(
            render_run_feedback(run).unwrap(),
            "Run cancelled. No answer was committed. You can submit a new task."
        );
        for status in [
            RunStatus::Pending,
            RunStatus::Active,
            RunStatus::Finished,
            RunStatus::Interrupted,
        ] {
            run.status = status;
            assert_eq!(render_run_feedback(run), None, "{status:?}");
        }
    }
}
