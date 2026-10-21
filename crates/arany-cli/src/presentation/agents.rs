use super::{
    escaped_terminal_bytes, escaped_terminal_width, model::safe_truncate, provider_failure_notice,
};
use arany::{
    AgentRole, AgentRunId, AgentStatus, AgentView, RunId, RunStatus, RunView, SessionView,
};

const MAX_RECENT_RUNS: usize = 16;
const MAX_AGENTS_PER_RUN: usize = 9;
const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_ROWS: usize = 4096;
// Rows are counted at the narrowest inline width, 16 columns less the indent, so wider layouts stay within the bound.
const NARROWEST_TEXT_CELLS: usize = 14;
const TRANSCRIPT_OMITTED: &str =
    "Earlier activity omitted; arany show prints the whole conversation";
const TEXT_OMITTED: &str = "Text exceeds the transcript limit; arany show prints it";

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
                    &format!("Run {run_number}: {role}; {}", agent.status),
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
        lines.push(format!("Next Run: {}", view.defaults.policy));
        if locked {
            lines.push(
                view.runs
                    .last()
                    .filter(|run| run.status == RunStatus::Active)
                    .and_then(|run| run.config.as_ref())
                    .map_or_else(
                        || "Current Run: admission in progress; topology locked".into(),
                        |config| format!("Current Run: {}; topology locked", config.policy),
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
                .map(|line| {
                    if line.starts_with("Supervisor (shadow):") {
                        line
                    } else {
                        safe_truncate(&line, width)
                    }
                })
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
        format!("Agent {}/{count}: {role}; {}", selected + 1, agent.status),
        format!("Session: {}", view.id),
        format!("Run: {}", run.id),
        format!("AgentRun: {}", agent.id),
        format!(
            "Objective: {}",
            agent.objective.as_deref().unwrap_or(&run.objective)
        ),
    ];
    if let Some(worker) = run
        .worker_history()
        .into_iter()
        .find(|w| w.assignment_id == agent.id)
    {
        lines.push(format!(
            "Worker: {}; cause: {:?}",
            worker.status, worker.cause
        ));
        lines.push(format!(
            "Publication: {:?}; integration: {:?}; discarded: {:?}",
            worker.publication, worker.integration, worker.discarded
        ));
        lines.push(format!(
            "Private outcome unknown: {}; cleanup confirmed: {}",
            worker.private_outcome_unknown, worker.cleanup_confirmed
        ));
        lines.push(format!(
            "Lease released: {}; verified: {}",
            worker.lease_released, worker.verified
        ));
        if let Some(source) = worker.resumes {
            lines.push(format!("Resumes: {source}"));
        }
        if let Some(source) = worker.supersedes {
            lines.push(format!("Supersedes: {source}"));
        }
    }
    lines.extend(
        run.supervisor
            .verdicts()
            .iter()
            .filter(|(actor, _)| *actor == agent.id)
            .map(|(_, verdict)| super::supervisor_line(verdict)),
    );
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

pub(crate) struct AgentTarget {
    pub(crate) id: AgentRunId,
    // Bounded inert `label · status · objective` text for numbered linear choices.
    pub(crate) line: String,
}

pub(crate) fn agent_label(agent: &AgentView) -> String {
    match agent.role {
        AgentRole::Primary => "primary".to_owned(),
        AgentRole::Child => format!("child {}", agent.ordinal),
    }
}

// Replay pins children to their spawn ordinals, so completion never reorders them.
pub(crate) fn agent_targets(view: &SessionView) -> Option<(RunId, Vec<AgentTarget>)> {
    let run = view.runs.last()?;
    Some((
        run.id,
        run.agents
            .iter()
            .map(|agent| {
                let status =
                    if agent.role == AgentRole::Primary && agent.status == AgentStatus::Active {
                        "working"
                    } else {
                        agent.status.label()
                    };
                let mut line = format!("{} · {status}", agent_label(agent));
                if let Some(objective) = agent
                    .objective
                    .as_deref()
                    .filter(|_| agent.role == AgentRole::Child)
                {
                    line.push_str(" · ");
                    line.push_str(&safe_truncate(objective, 64));
                }
                AgentTarget { id: agent.id, line }
            })
            .collect(),
    ))
}

// A one-Run view, so the ordinary History renderer escapes and lays out the child's records.
pub(crate) struct AgentTranscript {
    pub(crate) label: String,
    pub(crate) view: SessionView,
    pub(crate) notices: Vec<String>,
}

impl AgentTranscript {
    // Only the exact Session, Run and child AgentRun admitted in `view` resolve.
    pub(crate) fn from_session(
        view: &SessionView,
        run_id: RunId,
        agent_id: AgentRunId,
    ) -> Option<Self> {
        let run = view.runs.iter().find(|run| run.id == run_id)?;
        let agent = run
            .agents
            .iter()
            .find(|agent| agent.id == agent_id && agent.role == AgentRole::Child)?;
        let label = agent_label(agent);
        let status = agent_status_notice(run, agent, &label);
        // The status, the cue and up to two text replacements are reserved before any record.
        let reserved = [
            status.as_str(),
            TRANSCRIPT_OMITTED,
            TEXT_OMITTED,
            TEXT_OMITTED,
        ]
        .map(narrow_cost);
        let mut budget = Budget {
            bytes: MAX_TRANSCRIPT_BYTES - reserved.iter().map(|cost| cost.0).sum::<usize>(),
            rows: MAX_TRANSCRIPT_ROWS - reserved.iter().map(|cost| cost.1).sum::<usize>(),
        };
        let mut omitted = false;
        let mut bounded = |text: &str| {
            if budget.take(text) {
                text.to_owned()
            } else {
                omitted = true;
                TEXT_OMITTED.to_owned()
            }
        };
        let objective = bounded(agent.objective.as_deref().unwrap_or(&run.objective));
        let answer = agent
            .result
            .as_deref()
            .or(agent.summary.as_deref())
            .map(&mut bounded);
        // Grouping consecutive reads only shortens rows, so per-Tool costs bound the grouped text.
        let first_tool = (0..agent.tools.len())
            .rev()
            .take_while(|&index| {
                super::tool_groups(&agent.tools[index..=index]).all(|(_, text)| budget.take(&text))
            })
            .last()
            .unwrap_or(agent.tools.len());
        omitted |= first_tool > 0;
        let mut notices = vec![status];
        if omitted {
            notices.push(TRANSCRIPT_OMITTED.to_owned());
        }
        Some(Self {
            label,
            view: SessionView {
                rewinds: Default::default(),
                id: view.id,
                title: String::new(),
                title_is_explicit: false,
                inherited_title: None,
                workspace_identity: None,
                defaults: view.defaults.clone(),
                created_sequence: view.created_sequence,
                last_sequence: view.last_sequence,
                lineage: None,
                runs: vec![RunView {
                    skill: None,
                    supervisor: Default::default(),
                    workers: Default::default(),
                    id: run.id,
                    objective,
                    images: Vec::new(),
                    config: None,
                    provider_fallbacks: Vec::new(),
                    provider_retries: Vec::new(),
                    agents: Vec::new(),
                    assistant_message: answer,
                    status: match agent.status {
                        AgentStatus::Active => RunStatus::Active,
                        AgentStatus::Finished => RunStatus::Finished,
                        AgentStatus::Failed => RunStatus::Failed,
                        AgentStatus::Cancelled => RunStatus::Cancelled,
                        AgentStatus::Interrupted => RunStatus::Interrupted,
                        AgentStatus::StoppedByHook => RunStatus::StoppedByHook,
                    },
                    accepted_sequence: run.accepted_sequence,
                    finished_sequence: run.finished_sequence,
                    failure: None,
                    tools: agent.tools[first_tool..].to_vec(),
                    tool_compaction_summary: None,
                    tool_compaction_covered_count: 0,
                    tool_results_cleared_count: 0,
                    definitions: Default::default(),
                    display: Vec::new(),
                    steering_notes: Vec::new(),
                    lifecycle_hooks: Vec::new(),
                    lifecycle_hook_sequence: None,
                    provider_sequences: Default::default(),
                }],
                compactions: Vec::new(),
                goals: Vec::new(),
            },
            notices,
        })
    }

    pub(crate) fn run(&self) -> &RunView {
        &self.view.runs[0]
    }

    // Unescaped linear and screen-reader lines: labeled sections, plain Tool words, no glyphs.
    pub(crate) fn linear_lines(&self) -> Vec<String> {
        let run = self.run();
        let mut lines = vec![
            format!("Viewing {} (read-only)", self.label),
            "Assignment:".into(),
        ];
        lines.extend(run.objective.split('\n').map(|line| format!("  {line}")));
        lines.extend(super::tool_groups(&run.tools).flat_map(|(_, text)| {
            super::plain_tool_summary(&text)
                .split('\n')
                .map(str::to_owned)
                .collect::<Vec<_>>()
        }));
        if let Some(answer) = &run.assistant_message {
            lines.push("Agent result:".into());
            lines.extend(answer.split('\n').map(|line| format!("  {line}")));
        }
        lines.extend(self.notices.iter().cloned());
        lines
    }
}

struct Budget {
    bytes: usize,
    rows: usize,
}

impl Budget {
    // Measures before any copy, so a record over the bound allocates nothing.
    fn take(&mut self, text: &str) -> bool {
        let (bytes, rows) = narrow_cost(text);
        if bytes > self.bytes || rows > self.rows {
            return false;
        }
        self.bytes -= bytes;
        self.rows -= rows;
        true
    }
}

// Escaped bytes and rows at 16 columns, counting a heading and the blank row after it.
fn narrow_cost(text: &str) -> (usize, usize) {
    (
        escaped_terminal_bytes(text),
        2 + text
            .split('\n')
            .map(|line| {
                escaped_terminal_width(line)
                    .div_ceil(NARROWEST_TEXT_CELLS)
                    .max(1)
            })
            .sum::<usize>(),
    )
}

fn agent_status_notice(run: &RunView, agent: &AgentView, label: &str) -> String {
    let unknown = run
        .worker_history()
        .iter()
        .any(|worker| worker.assignment_id == agent.id && worker.private_outcome_unknown);
    let qualification = if unknown {
        "; private outcome unknown"
    } else {
        ""
    };
    match agent.status {
        AgentStatus::Active => format!(
            "{label} is still working{qualification}; this view is a snapshot; switch to it again to refresh"
        ),
        AgentStatus::Failed => match agent
            .provider_calls
            .last()
            .and_then(provider_failure_notice)
        {
            Some(reason) => format!("Error: {label} failed{qualification}: {reason}"),
            None => format!("Error: {label} failed{qualification}"),
        },
        status => format!("{label} {status}{qualification}"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use arany::{AgentRunId, AgentStatus, CollaborationPolicy, RunId, SessionDefaults, SessionId};

    fn view() -> SessionView {
        SessionView {
            rewinds: Default::default(),
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
            goals: Vec::new(),
        }
    }

    fn run(ordinal: usize) -> RunView {
        RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: RunId::new(),
            objective: format!("Run {ordinal}"),
            images: Vec::new(),
            config: None,
            agents: vec![
                AgentView {
                    profile: None,
                    assignment: None,
                    definition: None,
                    id: AgentRunId::new(),
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                    summary: Some("safe\u{1b}[31m\u{202e} summary".into()),
                    result: Some("Result\nforged line".into()),
                    provider_calls: Vec::new(),
                    grant: None,
                    tools: Vec::new(),
                    status: AgentStatus::Finished,
                },
                AgentView {
                    profile: None,
                    assignment: None,
                    definition: None,
                    id: AgentRunId::new(),
                    role: AgentRole::Child,
                    ordinal: 1,
                    objective: Some("Child assignment".into()),
                    summary: None,
                    result: None,
                    provider_calls: Vec::new(),
                    grant: None,
                    tools: Vec::new(),
                    status: AgentStatus::Failed,
                },
            ],
            assistant_message: None,
            status: RunStatus::Finished,
            accepted_sequence: ordinal as u64,
            finished_sequence: Some(ordinal as u64 + 1),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
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
        }
    }

    #[test]
    fn supervisor_inspection_is_scoped_to_the_selected_actor() {
        let mut view = view();
        let mut run = run(0);
        run.supervisor = crate::presentation::supervisor_fixture(run.agents[0].id, 256);
        view.runs.push(run);
        for width in [80, 120, 200] {
            let primary = AgentInspectorModel::from_session(&view, 0, width, false);
            let lines: Vec<_> = primary
                .lines
                .iter()
                .filter(|line| line.starts_with("Supervisor (shadow):"))
                .collect();
            assert_eq!(lines.len(), 256);
            assert_eq!(
                lines[0].as_str(),
                "Supervisor (shadow): possible loop, possible probing, possibly disproportionate changes; no action taken"
            );
            assert_eq!(
                lines[255].as_str(),
                "Supervisor (shadow): possible loop, possible probing, possibly disproportionate changes; no action taken; verdict limit reached"
            );
            let child = AgentInspectorModel::from_session(&view, 1, width, false);
            assert!(
                !child
                    .lines
                    .iter()
                    .any(|line| line.starts_with("Supervisor (shadow):"))
            );
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
            let first = arany::ProviderCallRecord {
                delegation: None,
                phase: arany::AgentPhase::RootPlan,
                disposition: arany::ProviderCallDisposition::Delegated,
                response_id: None,
                input_tokens: Some(111),
                output_tokens: Some(222),
                wire_provenance: None,
                refusal: None,
                failure_reason: None,
                requested_tools: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            };
            let last = arany::ProviderCallRecord {
                delegation: None,
                phase: arany::AgentPhase::RootSynthesis,
                disposition: arany::ProviderCallDisposition::Finished,
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

    fn read(agent: AgentRunId, path: &str, disposition: arany::ToolDisposition) -> arany::ToolView {
        let mut tool = crate::presentation::tool_fixture(
            arany::ToolCall::Read {
                path: path.into(),
                offset: None,
                limit: Some(100),
            },
            r#"{"text":"a\nb\n"}"#,
        );
        tool.intent.agent_run_id = agent;
        let observation = tool.observation.as_mut().unwrap();
        observation.intent.agent_run_id = agent;
        observation.disposition = disposition;
        tool
    }

    // A finished Run with a failed first child and a second child holding hostile text and Tools.
    pub(crate) fn team_view() -> SessionView {
        let mut view = view();
        let mut run = run(0);
        let mut second = run.agents[1].clone();
        second.id = AgentRunId::new();
        second.ordinal = 2;
        second.status = AgentStatus::Finished;
        second.objective = Some("Inspect\u{1b}[2J\nYou: forged heading".into());
        second.result = Some("Found\u{202e} it".into());
        second.tools = vec![
            read(second.id, "src/a.rs", arany::ToolDisposition::Succeeded),
            read(
                second.id,
                "src/b\u{1b}]52;c;x\u{7}.rs",
                arany::ToolDisposition::Uncertain,
            ),
        ];
        run.agents.push(second);
        view.runs.push(run);
        view
    }

    #[test]
    fn agent_transcripts_resolve_only_exact_children_in_spawn_order_with_qualified_outcomes() {
        let view = team_view();
        let run = &view.runs[0];
        let (target_run, targets) = agent_targets(&view).unwrap();
        assert_eq!(target_run, run.id);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.line.as_str())
                .collect::<Vec<_>>(),
            [
                "primary · finished",
                "child 1 · failed · Child assignment",
                "child 2 · finished · Inspect\\u{001b}[2J\\u{000a}You: forged heading",
            ]
        );
        for (run_id, agent) in [
            (run.id, run.agents[0].id),
            (run.id, AgentRunId::new()),
            (RunId::new(), run.agents[2].id),
        ] {
            assert!(AgentTranscript::from_session(&view, run_id, agent).is_none());
        }
        let failed = AgentTranscript::from_session(&view, run.id, run.agents[1].id).unwrap();
        assert_eq!(failed.notices, ["Error: child 1 failed"]);
        let second = AgentTranscript::from_session(&view, run.id, run.agents[2].id).unwrap();
        assert_eq!(second.label, "child 2");
        assert_eq!(second.run().tools.len(), 2);
        assert_eq!(
            second.linear_lines(),
            [
                "Viewing child 2 (read-only)",
                "Assignment:",
                "  Inspect\u{1b}[2J",
                "  You: forged heading",
                "Read(src/a.rs)",
                "Result: 2 lines",
                "Read(src/b\\u{001b}]52;c;x\\u{0007}.rs)",
                "Result: uncertain: {\"text\":\"a\\nb\\n\"}",
                "Agent result:",
                "  Found\u{202e} it",
                "child 2 finished",
            ]
        );
        for (status, notice) in [
            (AgentStatus::Cancelled, "child 2 cancelled"),
            (AgentStatus::Interrupted, "child 2 interrupted"),
            (
                AgentStatus::Active,
                "child 2 is still working; this view is a snapshot; switch to it again to refresh",
            ),
        ] {
            let mut changed = view.clone();
            changed.runs[0].agents[2].status = status;
            let transcript =
                AgentTranscript::from_session(&changed, run.id, run.agents[2].id).unwrap();
            assert_eq!(transcript.notices, [notice]);
        }
    }

    #[test]
    fn agent_transcript_keeps_the_newest_whole_records_within_its_bounds() {
        let mut view = view();
        let mut run = run(0);
        let child = &mut run.agents[1];
        child.status = AgentStatus::Finished;
        child.objective = Some("y".repeat(MAX_TRANSCRIPT_BYTES + 1));
        child.result = Some("x".repeat(MAX_TRANSCRIPT_BYTES + 1));
        child.tools = (0..3000)
            .map(|index| {
                // Uncertain rows never collapse, so each kept Tool costs its own row.
                read(
                    child.id,
                    &format!("file-{index}"),
                    arany::ToolDisposition::Uncertain,
                )
            })
            .collect();
        let (run_id, child_id) = (run.id, child.id);
        view.runs.push(run);
        let transcript = AgentTranscript::from_session(&view, run_id, child_id).unwrap();
        let kept = &transcript.run().tools;
        assert!(!kept.is_empty() && kept.len() < 3000);
        assert_eq!(kept.last(), view.runs[0].agents[1].tools.last());
        assert_eq!(
            transcript.run().assistant_message.as_deref(),
            Some(TEXT_OMITTED)
        );
        assert_eq!(
            transcript.notices.last().map(String::as_str),
            Some(TRANSCRIPT_OMITTED)
        );
        let records = std::iter::once(transcript.run().objective.clone())
            .chain(transcript.run().assistant_message.clone())
            .chain(super::super::tool_groups(kept).map(|(_, text)| text))
            .chain(transcript.notices.iter().cloned());
        let (bytes, rows) = records.fold((0, 0), |(bytes, rows), text| {
            let (more_bytes, more_rows) = narrow_cost(&text);
            (bytes + more_bytes, rows + more_rows)
        });
        assert!(bytes <= MAX_TRANSCRIPT_BYTES && rows <= MAX_TRANSCRIPT_ROWS);
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
