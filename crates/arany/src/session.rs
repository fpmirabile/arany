pub(crate) mod checkpoint;
pub use checkpoint::{Checkpoint, CheckpointReason, CheckpointStatus};
mod compaction;
mod context;
mod events;
pub(crate) mod input;
mod lineage;
mod reducer;
pub mod rewind;
mod stop;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use reducer::HookView;
#[cfg(test)]
pub(crate) mod tests;
pub mod worker;

pub(crate) const MAX_CONTEXT_CONTENT_BYTES: usize = 376 * 1024;
pub(crate) const MAX_WINDOW_CONTENT_BYTES: usize = 1024 * 1024;

// 90% of the window at a conservative 3 bytes per token.
pub(crate) fn content_ceiling(context_window_tokens: Option<u32>) -> usize {
    context_window_tokens.map_or(MAX_CONTEXT_CONTENT_BYTES, |tokens| {
        (tokens as usize * 27 / 10).min(MAX_WINDOW_CONTENT_BYTES)
    })
}
pub(crate) const MAX_HISTORY_RUNS: usize = 32;
pub const MAX_USER_MESSAGE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_STEERING_NOTE_BYTES: usize = 8 * 1024;
// The history header is fixed; each note's "\n[N] " label is charged inside MAX_STEERING_NOTE_BYTES.
pub(crate) const MAX_STEERING_CONTEXT_BYTES: usize = MAX_STEERING_NOTE_BYTES + 256;
// Charge the longest history label so rendered notes cannot exceed the Run's byte bound.
pub(crate) const MAX_STEERING_LABEL_BYTES: usize = 8;
pub(crate) const MAX_CHILD_STEERING_LABEL_BYTES: usize = 24;
pub(crate) const MAX_ASSISTANT_MESSAGE_BYTES: usize = 32 * 1024;

pub(crate) use compaction::{
    AUTO_COMPACTION_HISTORY_BYTES, COMPACTION_COMPILER_VERSION, CompactionInputError,
    MAX_COMPACTION_INPUT_BYTES, MAX_COMPACTION_OUTPUT_TOKENS, MAX_COMPACTION_SUMMARY_BYTES,
    MAX_COMPACTION_TURNS, MIN_RECORDED_COMPACTION_COMPILER_VERSION, PreparedCompaction,
    validate_compaction_references,
};
pub(crate) use context::CompiledContext;
#[cfg(test)]
pub(crate) use events::PAYLOAD_VERSIONS;
pub use events::{
    AgentDisposition, AgentRole, AgentRunId, BudgetAction, BudgetChargeBasis, BudgetRefusal,
    BudgetRefusalCause, ChildGrant, CollaborationPolicy, CompactionFailure, CompactionRecord,
    CompactionStatus, ContextUsage, DISPLAY_LIMIT_TEXT, DisplayCategory, DisplayChunk,
    EffectTarget, Event, EventEnvelope, ForkLineage, GoalBudget, GoalId, GoalMcpTool, GoalPhase,
    GoalPlanContract, GoalReport, GoalSkillBinding, GoalStatus, GoalSuccessCriterion,
    InstructionFile, InstructionSource, MAX_BUDGET_REFUSAL_BYTES, MAX_DISPLAY_CHUNK_BYTES,
    MAX_DISPLAY_CHUNKS, MAX_DISPLAY_COMMENTARY_ITEMS, MAX_DISPLAY_TEXT_BYTES, MAX_GOAL_ACTIVE_MS,
    MAX_GOAL_PLAN_MCP_TOOLS, MAX_GOAL_PLAN_SKILLS, MAX_GOAL_PROVIDER_CALLS, MAX_GOAL_REPORT_BYTES,
    MAX_GOAL_RUNS, ProposalRefusal, ProposalRefusalCause, ProviderCallDisposition,
    ProviderCallRecord, ProviderSetupDisposition, ProviderSetupTarget, ProviderSetupTargetKind,
    ReplayError, RunConfig, RunDisposition, RunFailure, RunId, SessionDefaults, SessionId,
    ToolIntentRecord, WorkspaceIdentity,
};
pub(crate) use events::{
    GoalSuccessState, GoalUsage, MAX_CHILD_TOOL_CALLS, MAX_CHILD_TURNS,
    MAX_GOAL_PLAN_CONTRACT_BYTES, MAX_STEP_TOOL_CALLS, STORE_ACTION_OVERHEAD_MS,
    goal_finished_v7_payload, goal_finished_v7_to_v8, payload_version,
};
pub(crate) use events::{
    add_command_selection, expiring_intent_text, expiring_payload, goal_started_v7_payload,
    goal_started_v7_to_v8, intent_without_expiry, pre_command_selection_payload,
    pre_file_removal_payload, pre_gh_pr_list_payload, pre_gh_pr_view_payload,
    pre_git_write_payload, pre_glob_payload, pre_host_git_payload, pre_host_payload,
    pre_host_prefix_payload, pre_multiedit_payload, pre_role_routing_payload,
    pre_roster_setup_payload, pre_typed_prefix_payload, provider_call_v18_payload,
    provider_call_v18_to_v19, provider_call_v19_payload, provider_call_v19_to_v20,
    provider_call_v22_payload, rebind_intent_pins, replace_intent_pin, rewind_started_v1_payload,
    rewind_started_v1_to_v2, run_started_fits, run_started_v18_payload, run_started_v18_to_v19,
    run_started_v19_payload, run_started_v19_to_v20, run_started_v23_payload,
    run_started_v23_to_v24,
};
pub(crate) use lineage::{PrefixHasher, PrefixRow, prefix_digest};
#[cfg(test)]
pub(crate) use reducer::rewind_tests::{
    finished as rewind_finished_fixture, started as rewind_started_fixture,
};
#[cfg(test)]
pub(crate) use reducer::worker_tests::{
    compacted_fixture as worker_compacted_fixture,
    completed_command_fixture as worker_command_protocol_fixture,
    completed_fixture as worker_protocol_fixture, host_protocol_fixture,
    session_host_protocol_fixture,
};
pub use reducer::{
    AgentStatus, AgentView, CompactionView, GoalView, PlanUsage, RunStatus, RunView, SessionView,
    TokenCount, ToolView, UsageGroup, UsageView,
};
pub(crate) use reducer::{
    ForkInheritance, MAX_HOST_SESSION_GRANTS, coordination_ready, criterion_unmet_in_view,
    evidence_admitted, private_criterion_evidence, private_failure, validate_editor_request,
    verification_passed, wave_settled,
};
pub use stop::{GoalLimit, RunFailureFeedback, RunStop, escape_status_text};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionListItem {
    pub id: SessionId,
    pub title: String,
    pub last_sequence: u64,
    pub created_at: String,
    pub last_activity_at: String,
    pub defaults: SessionDefaults,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPage {
    pub items: Vec<SessionListItem>,
    pub more: bool,
}

pub(crate) fn placeholder_title(title: &str) -> bool {
    matches!(title, "New Session" | "Forked Session")
}

const MAX_ANSWER_SUMMARY_BYTES: usize = 2 * 1024;

pub(crate) fn answer_summary(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let mut end = line.len().min(MAX_ANSWER_SUMMARY_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

pub(crate) fn title_preview(text: &str) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let mut title = String::new();
    'words: for word in text.split_whitespace() {
        for grapheme in word.graphemes(true) {
            if title.len() + grapheme.len() > 128 {
                break 'words;
            }
            title.push_str(grapheme);
        }
        if title.len() == 128 {
            break;
        }
        title.push(' ');
    }
    if title.trim().is_empty() {
        "Empty conversation".into()
    } else {
        title.trim_end().to_owned()
    }
}

fn children_noun(count: u8) -> &'static str {
    if count == 1 { "child" } else { "children" }
}

impl std::fmt::Display for CollaborationPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single => f.write_str("single"),
            Self::Auto {
                max_active_children,
            } => write!(
                f,
                "auto · up to {max_active_children} {}",
                children_noun(*max_active_children)
            ),
            Self::Team {
                max_active_children,
            } => write!(
                f,
                "team · up to {max_active_children} {}",
                children_noun(*max_active_children)
            ),
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl RunStatus {
    pub const fn label(self) -> &'static str {
        match self {
            RunStatus::Pending => "starting",
            RunStatus::Active => "working",
            RunStatus::Finished => "finished",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::StoppedByHook => "stopped by hook",
            RunStatus::Interrupted => "interrupted",
        }
    }
}

impl AgentStatus {
    pub const fn label(self) -> &'static str {
        match self {
            AgentStatus::Active => "active",
            AgentStatus::Finished => "finished",
            AgentStatus::Failed => "failed",
            AgentStatus::Cancelled => "cancelled",
            AgentStatus::StoppedByHook => "stopped by hook",
            AgentStatus::Interrupted => "interrupted",
        }
    }
}
