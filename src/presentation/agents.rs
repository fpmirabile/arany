use super::{model::safe_truncate, provider_failure_notice};
use crate::session::{AgentRole, AgentStatus, AgentView, RunStatus, RunView, SessionView};

const MAX_RECENT_RUNS: usize = 16;
const MAX_AGENTS_PER_RUN: usize = 9;

pub(crate) struct AgentInspectorModel {
    pub(crate) selected: usize,
    pub(crate) choices: Vec<String>,
    pub(crate) lines: Vec<String>,
}

impl AgentInspectorModel {
    pub(crate) fn recent_agent_count(view: &SessionView) -> usize {
        view.runs
            .iter()
            .rev()
            .take(MAX_RECENT_RUNS)
            .map(|run| run.agents.len().min(MAX_AGENTS_PER_RUN))
            .sum()
    }

    pub(crate) fn from_session(
        view: &SessionView,
        selected: usize,
        width: u16,
        locked: bool,
    ) -> Self {
        let width = usize::from(width);
        let entries = view
            .runs
            .iter()
            .rev()
            .take(MAX_RECENT_RUNS)
            .enumerate()
            .flat_map(|(index, run)| {
                run.agents
                    .iter()
                    .take(MAX_AGENTS_PER_RUN)
                    .map(move |agent| (view.runs.len() - index, run, agent))
            })
            .collect::<Vec<_>>();
        let count = entries.len();
        let selected = selected.min(count.saturating_sub(1));
        let choices = entries
            .iter()
            .map(|(run_number, _, agent)| {
                let role = match agent.role {
                    AgentRole::Primary => "primary".to_owned(),
                    AgentRole::Child => format!("child {}", agent.ordinal),
                };
                safe_truncate(
                    &format!("Run {run_number}: {role}; {:?}", agent.status),
                    width.saturating_sub(2),
                )
            })
            .collect();
        let hidden_runs = view.runs.len().saturating_sub(MAX_RECENT_RUNS);
        let mut lines = vec![
            match view.runs.last() {
                Some(run) => format!(
                    "{} Run: {} agent{}; children: {}",
                    if run.status == RunStatus::Active {
                        "Current"
                    } else {
                        "Last"
                    },
                    run.agents.len(),
                    if run.agents.len() == 1 { "" } else { "s" },
                    run.agents
                        .iter()
                        .filter(|agent| agent.role == AgentRole::Child)
                        .count()
                ),
                None => "No Runs yet".into(),
            },
            format!(
                "History: {count} agents in {} recent Runs; {} older Runs",
                view.runs.len().min(MAX_RECENT_RUNS),
                hidden_runs
            ),
        ];
        lines.push(format!("Next Run: {:?}", view.defaults.policy));
        if locked {
            lines.push(
                view.runs
                    .last()
                    .filter(|run| run.status == RunStatus::Active)
                    .and_then(|run| run.config.as_ref())
                    .map_or_else(
                        || "Current Run: admission in progress; topology locked".into(),
                        |config| format!("Current Run: {:?}; topology locked", config.policy),
                    ),
            );
        } else {
            lines.push("Set next Run: /agents single|auto N|team N".into());
        }
        if let Some((_, run, agent)) = entries.get(selected) {
            lines.extend(agent_lines(view, run, agent, selected, count));
        } else if view.runs.last().is_some_and(|run| run.agents.is_empty()) {
            lines.push("Run admission in progress; no AgentRuns yet".into());
        } else {
            lines.push("No AgentRuns yet".into());
        }
        if hidden_runs > 0
            || entries
                .iter()
                .any(|(_, run, _)| run.agents.len() > MAX_AGENTS_PER_RUN)
        {
            lines.push("Older history: use arany show SESSION_ID".into());
        }
        Self {
            selected,
            choices,
            lines: lines
                .into_iter()
                .map(|line| safe_truncate(&line, width))
                .collect(),
        }
    }
}

fn agent_lines(
    view: &SessionView,
    run: &RunView,
    agent: &AgentView,
    selected: usize,
    count: usize,
) -> Vec<String> {
    let role = match agent.role {
        AgentRole::Primary => "primary".to_owned(),
        AgentRole::Child => format!("child {}", agent.ordinal),
    };
    let mut lines = vec![
        format!("Agent {}/{count}: {role}; {:?}", selected + 1, agent.status),
        format!("Session: {}", view.id),
        format!("Run: {}", run.id),
        format!("AgentRun: {}", agent.id),
        format!(
            "Objective: {}",
            agent.objective.as_deref().unwrap_or(&run.objective)
        ),
    ];
    if let Some(call) = agent.provider_calls.last() {
        let input = call
            .input_tokens
            .map_or_else(|| "unknown".into(), |value| value.to_string());
        let output = call
            .output_tokens
            .map_or_else(|| "unknown".into(), |value| value.to_string());
        lines.push(format!("Last call input tokens: {input}"));
        lines.push(format!("Last call output tokens: {output}"));
    }
    if agent.status == AgentStatus::Failed
        && let Some(reason) = agent
            .provider_calls
            .last()
            .and_then(provider_failure_notice)
    {
        lines.push(format!("Failure: {reason}"));
    }
    if let Some(summary) = &agent.summary {
        lines.push(format!("Summary: {summary}"));
    }
    if let Some(result) = &agent.result {
        lines.push(format!("Result: {result}"));
    }
    if agent.summary.is_some() || agent.result.is_some() {
        lines.push("Full text: use arany show SESSION_ID".into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        AgentRunId, AgentStatus, CollaborationPolicy, RunId, SessionDefaults, SessionId,
    };

    fn view() -> SessionView {
        SessionView {
            id: SessionId::new(),
            title: "Session".into(),
            title_is_explicit: true,
            inherited_title: None,
            workspace_identity: None,
            defaults: SessionDefaults {
                policy: CollaborationPolicy::Single,
                ..SessionDefaults::default()
            },
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
            runs: Vec::new(),
            compactions: Vec::new(),
        }
    }

    fn run(ordinal: usize) -> RunView {
        RunView {
            id: RunId::new(),
            objective: format!("Run {ordinal}"),
            images: Vec::new(),
            config: None,
            agents: vec![
                AgentView {
                    id: AgentRunId::new(),
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                    summary: Some("safe\u{1b}[31m\u{202e} summary".into()),
                    result: Some("Result\nforged line".into()),
                    provider_calls: Vec::new(),
                    status: AgentStatus::Finished,
                },
                AgentView {
                    id: AgentRunId::new(),
                    role: AgentRole::Child,
                    ordinal: 1,
                    objective: Some("Child assignment".into()),
                    summary: None,
                    result: None,
                    provider_calls: Vec::new(),
                    status: AgentStatus::Failed,
                },
            ],
            assistant_message: None,
            status: RunStatus::Finished,
            accepted_sequence: ordinal as u64,
            finished_sequence: Some(ordinal as u64 + 1),
            tools: Vec::new(),
        }
    }

    #[test]
    fn recent_agents_are_newest_run_first_and_primary_first() {
        let mut view = view();
        view.runs = (0..18).map(run).collect();
        let first = AgentInspectorModel::from_session(&view, 0, 120, false);
        assert_eq!(AgentInspectorModel::recent_agent_count(&view), 32);
        assert_eq!(first.choices.len(), 32);
        assert!(first.choices[0].contains("primary"));
        assert!(first.choices[1].contains("child 1"));
        assert_eq!(first.lines[0], "Last Run: 2 agents; children: 1");
        assert!(first.lines[1].contains("2 older Runs"));
        assert!(first.choices[0].starts_with("Run 18:"));
        assert!(first.choices[2].starts_with("Run 17:"));
        assert!(first.lines.iter().any(|line| line.contains("primary")));
        assert!(first.lines.iter().any(|line| line.contains("Run 17")));
        assert!(!first.lines.iter().any(|line| line == "Objective: Run 1"));
        let child = AgentInspectorModel::from_session(&view, 1, 120, false);
        assert!(child.lines.iter().any(|line| line.contains("child 1")));
        assert!(
            child
                .lines
                .iter()
                .any(|line| line.contains("Child assignment"))
        );
        let prior = AgentInspectorModel::from_session(&view, 2, 120, false);
        assert!(prior.lines.iter().any(|line| line.contains("Run 16")));
        for (input, output) in [
            (Some(12), Some(8)),
            (Some(12), None),
            (None, Some(8)),
            (None, None),
            (Some(u32::MAX), Some(u32::MAX)),
        ] {
            let first = crate::session::ProviderCallRecord {
                phase: crate::provider::AgentPhase::RootPlan,
                disposition: crate::session::ProviderCallDisposition::Delegated,
                response_id: None,
                input_tokens: Some(111),
                output_tokens: Some(222),
                wire_provenance: None,
                failure_reason: None,
            };
            let last = crate::session::ProviderCallRecord {
                phase: crate::provider::AgentPhase::RootSynthesis,
                disposition: crate::session::ProviderCallDisposition::Finished,
                input_tokens: input,
                output_tokens: output,
                ..first.clone()
            };
            view.runs.last_mut().unwrap().agents[0].provider_calls = vec![first, last];
            let inspector = AgentInspectorModel::from_session(&view, 0, 40, false);
            for (label, count) in [("input", input), ("output", output)] {
                let expected = count.map_or_else(|| "unknown".into(), |value| value.to_string());
                assert!(
                    inspector
                        .lines
                        .contains(&format!("Last call {label} tokens: {expected}"))
                );
            }
            assert!(
                !inspector
                    .lines
                    .contains(&"Last call input tokens: 111".to_owned())
            );
        }
    }

    #[test]
    fn hostile_text_is_inert_and_rows_are_bounded() {
        let mut view = view();
        view.runs.push(run(0));
        for width in [40, 50, 79, 80, 120] {
            let model = AgentInspectorModel::from_session(&view, usize::MAX, width, false);
            assert_eq!(model.selected, 1);
            assert!(
                model
                    .lines
                    .iter()
                    .all(|line| !line.contains(['\u{1b}', '\n', '\r']))
            );
            assert!(model.lines.iter().all(|line| line.len() <= 1024));
            assert!(model.lines.iter().all(|line| {
                unicode_width::UnicodeWidthStr::width(line.as_str()) <= usize::from(width)
            }));
            assert!(model.choices.iter().all(|line| {
                unicode_width::UnicodeWidthStr::width(line.as_str()) <= usize::from(width)
            }));
        }
        let model = AgentInspectorModel::from_session(&view, 0, 120, false);
        assert!(model.lines.iter().any(|line| line.contains("\\u{001b}")));
        assert!(model.lines.iter().any(|line| line.contains("\\u{202e}")));
        assert!(model.lines.iter().any(|line| line.contains("\\u{000a}")));
    }

    #[test]
    fn empty_session_has_policy_and_no_agent() {
        let model = AgentInspectorModel::from_session(&view(), 3, 80, false);
        assert_eq!(AgentInspectorModel::recent_agent_count(&view()), 0);
        assert_eq!(model.selected, 0);
        assert!(model.choices.is_empty());
        assert!(
            model
                .lines
                .iter()
                .any(|line| line.contains("No AgentRuns yet"))
        );
        assert!(model.lines.iter().any(|line| line.contains("Set next Run")));

        let active = AgentInspectorModel::from_session(&view(), 0, 80, true);
        assert!(
            active
                .lines
                .iter()
                .any(|line| line.contains("topology locked"))
        );
        assert!(
            !active
                .lines
                .iter()
                .any(|line| line.contains("Set next Run"))
        );
    }
}
