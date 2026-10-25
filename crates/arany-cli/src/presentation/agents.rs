use super::{
    escaped_terminal_bytes, escaped_terminal_width, model::safe_truncate, provider_failure_notice,
};
use arany::{
    AgentRole, AgentRunId, AgentStatus, AgentView, RunId, RunStatus, RunView, SessionView,
};

pub(crate) const STEERING_NOTE_HEADING: &str = "Steering note (sent during the child's work):";
pub(crate) const MAX_RECENT_RUNS: usize = 16;
const MAX_AGENTS_PER_RUN: usize = 9;
const MAX_EXPANDED_IDLE: usize = 3;
const MAX_OBJECTIVE_PREVIEW: usize = 64;
pub(crate) const MAX_AGENT_SEARCH: usize = 64;
pub(crate) const MAX_AGENT_ROWS: usize = 10;
const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_ROWS: usize = 4096;
// Rows are counted at the narrowest inline width, 16 columns less the indent, so wider layouts stay within the bound.
const NARROWEST_TEXT_CELLS: usize = 14;
const TRANSCRIPT_OMITTED: &str =
    "Earlier activity omitted; arany show prints the whole conversation";
const TEXT_OMITTED: &str = "Text exceeds the transcript limit; arany show prints it";

// A picker row keyed by canonical identity, so committed changes never retarget it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentRow {
    Agent(RunId, AgentRunId),
    Idle,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum AgentFocus {
    // The first row, resolved by the draw that follows a user action.
    #[default]
    First,
    Row(AgentRow),
    // Nothing matched; a later row takes focus only after a user action.
    Nothing,
}

#[derive(Default)]
pub(crate) struct AgentQuery {
    pub(crate) filter: String,
    pub(crate) expanded: bool,
    pub(crate) focus: AgentFocus,
}

pub(crate) struct AgentInspectorModel {
    pub(crate) selected: Option<usize>,
    pub(crate) choices: Vec<String>,
    pub(crate) rows: Vec<AgentRow>,
    // The first collapsed idle row, which expanding the group focuses.
    pub(crate) first_hidden: Option<AgentRow>,
    pub(crate) search: String,
    // The leading `lines` describe the Session; the rest describe the focused row.
    pub(crate) header: usize,
    pub(crate) lines: Vec<String>,
}

impl AgentInspectorModel {
    // What `query.focus` becomes once this model is shown.
    pub(crate) fn shown_focus(&self, focus: AgentFocus) -> AgentFocus {
        match focus {
            AgentFocus::First => self.selected.map_or(AgentFocus::Nothing, |index| {
                AgentFocus::Row(self.rows[index])
            }),
            focus => focus,
        }
    }

    pub(crate) fn from_session(
        view: &SessionView,
        query: &AgentQuery,
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
        let total = entries.len();
        let needle = query.filter.to_ascii_lowercase();
        let matches = entries
            .iter()
            .filter_map(|(run_number, run, agent)| {
                let objective = agent.objective.as_deref().unwrap_or(&run.objective);
                let label = format!(
                    "Run {run_number}: {}; {} · {}",
                    agent_label(agent),
                    agent.status,
                    safe_truncate(objective, MAX_OBJECTIVE_PREVIEW)
                );
                label.to_ascii_lowercase().contains(&needle).then_some((
                    AgentRow::Agent(run.id, agent.id),
                    *run,
                    *agent,
                    label,
                ))
            })
            .collect::<Vec<_>>();
        let found = matches.len();
        // Settled idle rows beyond the first few collapse into one row; focus keeps its own row visible.
        let mut idle = 0;
        let mut hidden = Vec::new();
        let mut shown = Vec::new();
        for entry in matches {
            if entry.2.status == AgentStatus::Finished {
                idle += 1;
                if idle > MAX_EXPANDED_IDLE
                    && !query.expanded
                    && query.focus != AgentFocus::Row(entry.0)
                {
                    hidden.push(entry.0);
                    continue;
                }
            }
            shown.push(entry);
        }
        let mut rows = shown.iter().map(|entry| entry.0).collect::<Vec<_>>();
        let mut choices = shown
            .iter()
            .map(|entry| safe_truncate(&entry.3, width.saturating_sub(2)))
            .collect::<Vec<_>>();
        if !hidden.is_empty() {
            rows.push(AgentRow::Idle);
            choices.push(safe_truncate(
                &idle_agents(hidden.len()),
                width.saturating_sub(2),
            ));
        }
        let selected = match query.focus {
            AgentFocus::First => (!rows.is_empty()).then_some(0),
            AgentFocus::Row(row) => rows.iter().position(|shown| *shown == row),
            AgentFocus::Nothing => None,
        };
        let plural = |count: usize| if count == 1 { "" } else { "s" };
        let search = if query.filter.is_empty() {
            format!("Type to search {total} agent{}", plural(total))
        } else if found == 0 {
            format!("Search: {} · no matching agents", query.filter)
        } else {
            format!(
                "Search: {} · {found} of {total} agent{}",
                query.filter,
                plural(total)
            )
        };
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
                    plural(run.agents.len()),
                    run.agents
                        .iter()
                        .filter(|agent| agent.role == AgentRole::Child)
                        .count()
                ),
                None => "No Runs yet".into(),
            },
            format!(
                "History: {total} agents in {} recent Runs; {} older Runs",
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
        let header = lines.len();
        match selected.map(|index| (index, rows[index])) {
            Some((index, AgentRow::Agent(..))) => {
                let (_, run, agent, _) = shown[index];
                lines.extend(agent_lines(view, run, agent, index, shown.len()));
            }
            Some((_, AgentRow::Idle)) => lines.push(format!(
                "{} collapsed; Enter or its number shows them",
                idle_agents(hidden.len())
            )),
            None if total == 0 && view.runs.last().is_some_and(|run| run.agents.is_empty()) => {
                lines.push("Run admission in progress; no AgentRuns yet".into());
            }
            None if total == 0 => lines.push("No AgentRuns yet".into()),
            None if found == 0 => lines.push("No matching agents; edit the search".into()),
            None => lines.push("No agent focused; Up or Down focuses one".into()),
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
            rows,
            first_hidden: hidden.first().copied(),
            search: safe_truncate(&search, width),
            header,
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

fn idle_agents(count: usize) -> String {
    format!("{count} idle agent{}", if count == 1 { "" } else { "s" })
}

fn agent_lines(
    view: &SessionView,
    run: &RunView,
    agent: &AgentView,
    selected: usize,
    count: usize,
) -> Vec<String> {
    let role = agent_label(agent);
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

#[derive(Clone)]
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
    Some((run.id, run_targets(run)))
}

pub(crate) fn run_targets(run: &RunView) -> Vec<AgentTarget> {
    run.agents
        .iter()
        .map(|agent| {
            let status = if agent.role == AgentRole::Primary && agent.status == AgentStatus::Active
            {
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
        .collect()
}

// A one-Run view, so the ordinary History renderer escapes and lays out the child's records.
pub(crate) struct AgentTranscript {
    pub(crate) label: String,
    pub(crate) view: SessionView,
    pub(crate) notices: Vec<String>,
    // The child's index of the first kept Tool, so appended rows stay keyed across windows.
    tool_offset: usize,
    note_offset: usize,
}

// What an append-only surface (Classic scrollback, linear output) already holds of one transcript.
#[derive(Default)]
pub(crate) struct Appended {
    assignment: bool,
    notes: usize,
    tools: usize,
    result: bool,
    status: Option<String>,
    omitted: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) enum TranscriptPart<'a> {
    Assignment(&'a str),
    SteeringNote(&'a str),
    Tool(String),
    Result(&'a str),
    Notice(&'a str),
}

impl TranscriptPart<'_> {
    // Unescaped linear and screen-reader lines: labeled sections, plain Tool words, no glyphs.
    pub(crate) fn linear_lines(&self) -> Vec<String> {
        let section = |heading: &str, text: &str| {
            std::iter::once(heading.to_owned())
                .chain(text.split('\n').map(|line| format!("  {line}")))
                .collect()
        };
        match self {
            Self::Assignment(text) => section("Assignment:", text),
            Self::SteeringNote(text) => section(STEERING_NOTE_HEADING, text),
            Self::Tool(text) => super::plain_tool_summary(text)
                .split('\n')
                .map(str::to_owned)
                .collect(),
            Self::Result(text) => section("Agent result:", text),
            Self::Notice(text) => vec![(*text).to_owned()],
        }
    }
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
        let first_note = (0..agent.steering_notes.len())
            .rev()
            .take_while(|&index| {
                budget.take(&format!("{STEERING_NOTE_HEADING}\n"))
                    && budget.take(&agent.steering_notes[index])
            })
            .last()
            .unwrap_or(agent.steering_notes.len());
        omitted |= first_note > 0;
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
                usage: Default::default(),
                rewinds: Default::default(),
                id: view.id,
                title: String::new(),
                title_is_explicit: false,
                inherited_title: None,
                workspace_identity: None,
                defaults: view.defaults.clone(),
                host_epoch_floor: 0,
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
                    // Siblings and the primary finish the Run; that alone changes nothing here.
                    finished_sequence: None,
                    failure: None,
                    tools: agent.tools[first_tool..].to_vec(),
                    tool_compaction_summary: None,
                    tool_compaction_covered_count: 0,
                    tool_results_cleared_count: 0,
                    definitions: Default::default(),
                    display: Vec::new(),
                    steering_notes: agent.steering_notes[first_note..].to_vec(),
                    lifecycle_hooks: Vec::new(),
                    lifecycle_hook_sequence: None,
                    provider_sequences: Default::default(),
                }],
                compactions: Vec::new(),
                goals: Vec::new(),
                pending_provider_setup_edit: None,
                provider_setup_uncertain: false,
            },
            notices,
            tool_offset: first_tool,
            note_offset: first_note,
        })
    }

    pub(crate) fn run(&self) -> &RunView {
        &self.view.runs[0]
    }

    // Equal displays need no redraw, whatever the Session's sequence.
    pub(crate) fn same_display(&self, other: &Self) -> bool {
        self.view.runs == other.view.runs && self.notices == other.notices
    }

    // The first linear print: a read-only heading, then every record so far.
    pub(crate) fn linear_lines(&self, appended: &mut Appended) -> Vec<String> {
        *appended = Appended::default();
        std::iter::once(format!("Viewing {} (read-only)", self.label))
            .chain(
                self.unappended(appended)
                    .iter()
                    .flat_map(TranscriptPart::linear_lines),
            )
            .collect()
    }

    // Later linear prints: only new records, under a heading naming the agent.
    pub(crate) fn linear_update(&self, appended: &mut Appended) -> Vec<String> {
        let parts = self.unappended(appended);
        if parts.is_empty() {
            return Vec::new();
        }
        std::iter::once(format!("New activity from {}:", self.label))
            .chain(parts.iter().flat_map(TranscriptPart::linear_lines))
            .collect()
    }

    // Whole records not yet appended, in reading order. A working child's Tool group waits for
    // its boundary, and a changed status appends once.
    pub(crate) fn unappended(&self, appended: &mut Appended) -> Vec<TranscriptPart<'_>> {
        let run = self.run();
        let mut parts = Vec::new();
        if !std::mem::replace(&mut appended.assignment, true) {
            parts.push(TranscriptPart::Assignment(&run.objective));
        }
        let groups: Vec<_> = if run.status == RunStatus::Active {
            super::settled_tool_groups(run).collect()
        } else {
            super::tool_groups(&run.tools).collect()
        };
        for (range, text) in groups {
            if self.tool_offset + range.start >= appended.tools {
                appended.tools = self.tool_offset + range.end;
                parts.push(TranscriptPart::Tool(text));
            }
        }
        for (index, text) in run.steering_notes.iter().enumerate() {
            if self.note_offset + index >= appended.notes {
                appended.notes = self.note_offset + index + 1;
                parts.push(TranscriptPart::SteeringNote(text));
            }
        }
        if let Some(answer) = &run.assistant_message
            && !std::mem::replace(&mut appended.result, true)
        {
            parts.push(TranscriptPart::Result(answer));
        }
        let status = &self.notices[0];
        if appended.status.as_ref() != Some(status) {
            appended.status = Some(status.clone());
            parts.push(TranscriptPart::Notice(status));
        }
        if let Some(cue) = self.notices.get(1)
            && !std::mem::replace(&mut appended.omitted, true)
        {
            parts.push(TranscriptPart::Notice(cue));
        }
        parts
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
        AgentStatus::Active => {
            format!("{label} is working{qualification}; its committed activity appears here")
        }
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
            usage: Default::default(),
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

    fn focused(view: &SessionView, run: usize, agent: usize) -> AgentQuery {
        let run = &view.runs[run];
        AgentQuery {
            focus: AgentFocus::Row(AgentRow::Agent(run.id, run.agents[agent].id)),
            ..AgentQuery::default()
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
                    steering_notes: Vec::new(),
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
                    steering_notes: Vec::new(),
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
            let primary =
                AgentInspectorModel::from_session(&view, &AgentQuery::default(), width, false);
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
            let child =
                AgentInspectorModel::from_session(&view, &focused(&view, 0, 1), width, false);
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
        let first = AgentInspectorModel::from_session(&view, &AgentQuery::default(), 120, false);
        // 16 failed children stay; three idle primaries stay and 13 collapse into one row.
        assert_eq!(first.choices.len(), 20);
        assert_eq!(first.choices[19], "13 idle agents");
        assert_eq!(first.search, "Type to search 32 agents");
        assert!(first.choices[0].contains("primary"));
        assert!(first.choices[1].contains("child 1"));
        assert_eq!(first.lines[0], "Last Run: 2 agents; children: 1");
        assert!(first.lines[1].contains("2 older Runs"));
        assert!(first.choices[0].starts_with("Run 18:"));
        assert!(first.choices[2].starts_with("Run 17:"));
        assert!(first.lines.iter().any(|line| line.contains("primary")));
        assert!(first.lines.iter().any(|line| line.contains("Run 17")));
        assert!(!first.lines.iter().any(|line| line == "Objective: Run 1"));
        let child = AgentInspectorModel::from_session(&view, &focused(&view, 17, 1), 120, false);
        assert!(child.lines.iter().any(|line| line.contains("child 1")));
        assert!(
            child
                .lines
                .iter()
                .any(|line| line.contains("Child assignment"))
        );
        let prior = AgentInspectorModel::from_session(&view, &focused(&view, 16, 0), 120, false);
        assert!(prior.lines.iter().any(|line| line.contains("Run 16")));
        for (input, output) in [
            (Some(12), Some(8)),
            (Some(12), None),
            (None, Some(8)),
            (None, None),
            (Some(u32::MAX), Some(u32::MAX)),
        ] {
            let first = arany::ProviderCallRecord {
                rate_limits: None,
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
            let inspector =
                AgentInspectorModel::from_session(&view, &AgentQuery::default(), 40, false);
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

    // An active primary and six children: child 3 failed, the other five finished and idle.
    fn large_team() -> SessionView {
        let mut view = view();
        let mut run = run(0);
        run.status = RunStatus::Active;
        run.agents[0].status = AgentStatus::Active;
        let template = run.agents[1].clone();
        run.agents.truncate(1);
        for ordinal in 1..=6 {
            let mut child = template.clone();
            child.id = AgentRunId::new();
            child.ordinal = ordinal;
            child.objective = Some(format!("task {ordinal}"));
            child.status = if ordinal == 3 {
                AgentStatus::Failed
            } else {
                AgentStatus::Finished
            };
            run.agents.push(child);
        }
        view.runs.push(run);
        view
    }

    #[test]
    fn picker_detail_counts_only_visible_agents() {
        let view = large_team();
        for (query, expected) in [
            (AgentQuery::default(), "Agent 1/5: primary; active"),
            (focused(&view, 0, 4), "Agent 5/5: child 4; finished"),
        ] {
            let model = AgentInspectorModel::from_session(&view, &query, 80, true);
            assert_eq!(model.rows.last(), Some(&AgentRow::Idle));
            assert_eq!(model.lines[model.header], expected);
        }
    }

    #[test]
    fn picker_search_collapses_idle_rows_and_keeps_focus_by_identity() {
        let mut view = large_team();
        let agents = view.runs[0].agents.clone();
        let run_id = view.runs[0].id;
        let id = |index: usize| AgentRow::Agent(run_id, agents[index].id);
        let pick = |filter: &str, expanded: bool, focus: AgentFocus| AgentQuery {
            filter: filter.into(),
            expanded,
            focus,
        };
        let all = AgentInspectorModel::from_session(&view, &AgentQuery::default(), 80, true);
        // Three idle rows stay; the failed child keeps its place; the rest wait behind one row.
        assert_eq!(
            all.rows,
            [id(0), id(1), id(2), id(3), id(4), AgentRow::Idle]
        );
        assert_eq!(all.choices[1], "Run 1: child 1; finished · task 1");
        assert_eq!(all.choices[5], "2 idle agents");
        assert_eq!(all.first_hidden, Some(id(5)));
        assert_eq!(all.search, "Type to search 7 agents");
        assert_eq!(all.selected, Some(0));
        assert_eq!(all.shown_focus(AgentFocus::First), AgentFocus::Row(id(0)));
        let group = AgentInspectorModel::from_session(
            &view,
            &pick("", false, AgentFocus::Row(AgentRow::Idle)),
            80,
            true,
        );
        assert_eq!(group.selected, Some(5));
        assert_eq!(
            group.lines[group.header],
            "2 idle agents collapsed; Enter or its number shows them"
        );
        // Expansion shows every row in its committed order; a focused idle row is never hidden.
        let expanded = AgentInspectorModel::from_session(
            &view,
            &pick("", true, AgentFocus::Row(id(5))),
            80,
            true,
        );
        assert_eq!(expanded.rows, (0..7).map(id).collect::<Vec<_>>());
        assert_eq!(expanded.selected, Some(5));
        let focused = AgentInspectorModel::from_session(
            &view,
            &pick("", false, AgentFocus::Row(id(6))),
            80,
            true,
        );
        assert_eq!(
            focused.rows,
            [id(0), id(1), id(2), id(3), id(4), id(6), AgentRow::Idle]
        );
        assert_eq!(focused.choices[6], "1 idle agent");
        // Search is local, ASCII case-insensitive and covers the row's label and objective.
        let search = AgentInspectorModel::from_session(
            &view,
            &pick("TASK 3", false, AgentFocus::First),
            80,
            true,
        );
        assert_eq!(search.rows, [id(3)]);
        assert_eq!(search.search, "Search: TASK 3 · 1 of 7 agents");
        assert!(
            search
                .lines
                .contains(&"Agent 1/1: child 3; failed".to_owned())
        );
        let failed = AgentInspectorModel::from_session(
            &view,
            &pick("failed", false, AgentFocus::First),
            80,
            true,
        );
        assert_eq!(failed.rows, [id(3)]);
        // No match focuses nothing, and a later match takes no focus until the user acts.
        let none = AgentInspectorModel::from_session(
            &view,
            &pick("zz", false, AgentFocus::First),
            80,
            true,
        );
        assert!(none.rows.is_empty());
        assert_eq!(none.selected, None);
        assert_eq!(none.search, "Search: zz · no matching agents");
        assert_eq!(
            none.lines[none.header],
            "No matching agents; edit the search"
        );
        assert_eq!(none.shown_focus(AgentFocus::First), AgentFocus::Nothing);
        let later = AgentInspectorModel::from_session(
            &view,
            &pick("", false, AgentFocus::Nothing),
            80,
            true,
        );
        assert_eq!(later.selected, None);
        // Committed changes keep the focused identity: a newer Run and a finished child move rows.
        let chosen = pick("", false, AgentFocus::Row(id(4)));
        assert_eq!(
            AgentInspectorModel::from_session(&view, &chosen, 80, true).selected,
            Some(4)
        );
        view.runs[0].agents[3].status = AgentStatus::Finished;
        view.runs.push(run(1));
        let moved = AgentInspectorModel::from_session(&view, &chosen, 80, true);
        assert_eq!(moved.selected, Some(5));
        assert_eq!(moved.rows[5], id(4));
        assert!(moved.lines.contains(&"Objective: task 4".to_owned()));
        // A focused row that vanishes leaves nothing focused rather than its neighbour.
        view.runs[0].agents.remove(4);
        let gone = AgentInspectorModel::from_session(&view, &chosen, 80, true);
        assert_eq!(gone.selected, None);
        assert_eq!(gone.shown_focus(chosen.focus), chosen.focus);
        assert_eq!(
            gone.lines[gone.header],
            "No agent focused; Up or Down focuses one"
        );
    }

    #[test]
    fn hostile_text_is_inert_and_rows_are_bounded() {
        let mut view = view();
        view.runs.push(run(0));
        for width in [40, 50, 79, 80, 120] {
            let model =
                AgentInspectorModel::from_session(&view, &focused(&view, 0, 1), width, false);
            assert_eq!(model.selected, Some(1));
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
        let model = AgentInspectorModel::from_session(&view, &AgentQuery::default(), 120, false);
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
            second.linear_lines(&mut Appended::default()),
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
                "child 2 is working; its committed activity appears here",
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
    fn a_followed_child_appends_each_committed_record_once_and_ignores_siblings() {
        let mut view = team_view();
        let (run, child) = (view.runs[0].id, view.runs[0].agents[2].id);
        let agent = &mut view.runs[0].agents[2];
        agent.status = AgentStatus::Active;
        agent.result = None;
        let settled = agent.tools[0].clone();
        let mut pending = agent.tools[1].clone();
        pending.observation = None;
        agent.tools = vec![settled.clone()];
        let transcript =
            |view: &SessionView| AgentTranscript::from_session(view, run, child).unwrap();
        let mut appended = Appended::default();
        let mut shown = transcript(&view);
        // A working child's trailing read group waits for its boundary.
        assert_eq!(
            shown.linear_lines(&mut appended),
            [
                "Viewing child 2 (read-only)",
                "Assignment:",
                "  Inspect\u{1b}[2J",
                "  You: forged heading",
                "child 2 is working; its committed activity appears here",
            ]
        );
        let mut steps = Vec::new();
        for change in 0..5 {
            let agents = &mut view.runs[0].agents;
            match change {
                // A sibling and the primary finishing change nothing shown.
                0 => {
                    agents[1].status = AgentStatus::Finished;
                    agents[1].result = Some("sibling result".into());
                    agents[0].status = AgentStatus::Finished;
                    view.runs[0].finished_sequence = Some(99);
                }
                1 => agents[2].tools.push(pending.clone()),
                2 => agents[2].tools[1] = team_view().runs[0].agents[2].tools[1].clone(),
                3 => {
                    agents[2].status = AgentStatus::Finished;
                    agents[2].result = Some("Found\nit".into());
                }
                // A reload after a gap or resync yields the same facts at a later sequence.
                _ => {}
            }
            view.last_sequence += 1;
            let fresh = transcript(&view);
            steps.push((
                fresh.same_display(&shown),
                fresh.linear_update(&mut appended),
            ));
            shown = fresh;
        }
        let quiet = (true, Vec::<String>::new());
        assert_eq!(
            steps,
            [
                quiet.clone(),
                (false, Vec::new()),
                // The next committed Tool fixes the read group's boundary.
                (
                    false,
                    vec![
                        "New activity from child 2:".into(),
                        "Read(src/a.rs)".into(),
                        "Result: 2 lines".into(),
                        "Read(src/b\\u{001b}]52;c;x\\u{0007}.rs)".into(),
                        "Result: uncertain: {\"text\":\"a\\nb\\n\"}".into(),
                    ]
                ),
                (
                    false,
                    vec![
                        "New activity from child 2:".into(),
                        "Agent result:".into(),
                        "  Found".into(),
                        "  it".into(),
                        "child 2 finished".into(),
                    ]
                ),
                quiet,
            ]
        );
        assert!(
            !steps
                .iter()
                .flat_map(|(_, lines)| lines)
                .any(|line| line.contains("sibling"))
        );
    }

    #[test]
    fn committed_child_notes_are_scoped_escaped_bounded_and_appended_once() {
        let mut view = team_view();
        let (run, child) = (view.runs[0].id, view.runs[0].agents[2].id);
        let mut appended = Appended::default();
        let original = AgentTranscript::from_session(&view, run, child).unwrap();
        original.linear_lines(&mut appended);
        view.runs[0].agents[1]
            .steering_notes
            .push("sibling canary".into());
        assert!(original.same_display(&AgentTranscript::from_session(&view, run, child).unwrap()));
        view.runs[0].agents[2].steering_notes =
            vec!["first\nYou: forged\u{1b}[2J".into(), "second".into()];
        let changed = AgentTranscript::from_session(&view, run, child).unwrap();
        assert!(!original.same_display(&changed));
        let lines = changed.linear_update(&mut appended);
        assert_eq!(
            lines,
            [
                "New activity from child 2:",
                STEERING_NOTE_HEADING,
                "  first",
                "  You: forged\u{1b}[2J",
                STEERING_NOTE_HEADING,
                "  second"
            ]
        );
        assert!(!super::super::escape_terminal(&lines.join("\n")).contains('\u{1b}'));
        assert!(changed.linear_update(&mut appended).is_empty());
        let reload = AgentTranscript::from_session(&view, run, child).unwrap();
        assert!(changed.same_display(&reload));
        assert!(reload.linear_update(&mut appended).is_empty());
        view.runs[0].agents[2].steering_notes =
            vec!["x".repeat(MAX_TRANSCRIPT_BYTES + 1), "kept".into()];
        let capped = AgentTranscript::from_session(&view, run, child).unwrap();
        assert_eq!(capped.run().steering_notes, ["kept"]);
        assert_eq!(capped.notices.last().unwrap(), TRANSCRIPT_OMITTED);
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
        let model = AgentInspectorModel::from_session(&view(), &AgentQuery::default(), 80, false);
        assert_eq!(model.selected, None);
        assert!(model.choices.is_empty());
        assert!(
            model
                .lines
                .iter()
                .any(|line| line.contains("No AgentRuns yet"))
        );
        assert!(model.lines.iter().any(|line| line.contains("Set next Run")));

        let active = AgentInspectorModel::from_session(&view(), &AgentQuery::default(), 80, true);
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
