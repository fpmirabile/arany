pub(crate) mod checkpoint;
pub use checkpoint::{Checkpoint, CheckpointReason, CheckpointStatus};
mod compaction;
mod context;
mod context_window;
pub use context_window::{ContextWindow, MIN_CONTEXT_WINDOW_PERCENT};
pub(crate) mod environment;
pub use environment::{
    EnvironmentCaptureEvent, EnvironmentCaptureIntent, EnvironmentCapturePhase,
    EnvironmentCaptureStage, EnvironmentCaptureView, LoginShellIdentity,
};
mod events;
pub(crate) mod input;
pub use input::{IncludeFit, PartialInclude};
mod lineage;
mod precondition;
pub(crate) use precondition::{digest_known, fill_expected_digest};
mod reducer;
pub mod rewind;
mod stop;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use reducer::HookView;
#[cfg(all(any(test, feature = "test-support"), target_os = "linux"))]
pub use reducer::selection_failure_session;
#[cfg(test)]
pub(crate) mod tests;
pub mod worker;

pub(crate) const MAX_CONTEXT_CONTENT_BYTES: usize = 376 * 1024;
// The largest calibrated ceiling: 95% of the largest admitted window at the maximum bytes per token.
pub(crate) const MAX_WINDOW_CONTENT_BYTES: usize = crate::provider::MAX_CONTEXT_WINDOW_TOKENS
    as usize
    * MAX_CALIBRATED_BYTES_PER_TOKEN as usize
    * CALIBRATED_WINDOW_PERCENT as usize
    / 100;

// 90% of the window at a conservative 3 bytes per token.
pub(crate) fn content_ceiling(context_window_tokens: Option<u32>) -> usize {
    context_window_tokens.map_or(MAX_CONTEXT_CONTENT_BYTES, |tokens| {
        (tokens as usize * 27 / 10).min(MAX_WINDOW_CONTENT_BYTES)
    })
}

const CALIBRATED_WINDOW_PERCENT: u64 = 95;
pub(crate) const MAX_CALIBRATED_BYTES_PER_TOKEN: u32 = 4;

// Committed bytes per reported token may raise the baseline, up to the maximum ratio and 95% of the window.
pub(crate) fn calibrated_content_ceiling(
    context_window_tokens: u32,
    content_bytes: u32,
    input_tokens: u32,
) -> usize {
    let bytes = u64::from(content_bytes)
        .min(u64::from(input_tokens) * u64::from(MAX_CALIBRATED_BYTES_PER_TOKEN));
    let observed = (u64::from(context_window_tokens) * bytes * CALIBRATED_WINDOW_PERCENT)
        .checked_div(u64::from(input_tokens) * 100)
        .unwrap_or(0);
    usize::try_from(observed).unwrap_or(usize::MAX).clamp(
        content_ceiling(Some(context_window_tokens)),
        MAX_WINDOW_CONTENT_BYTES,
    )
}

pub(crate) fn max_calibrated_content_ceiling(context_window_tokens: u32) -> usize {
    calibrated_content_ceiling(context_window_tokens, MAX_CALIBRATED_BYTES_PER_TOKEN, 1)
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
    AUTO_COMPACTION_HISTORY_BYTES, COMPACTION_COMPILER_VERSION, COMPACTION_SUMMARY_SEPARATOR_BYTES,
    CompactionInputError, MAX_COMPACTION_INPUT_BYTES, MAX_COMPACTION_OUTPUT_TOKENS,
    MAX_COMPACTION_RECENT_CONTEXT_BYTES, MAX_COMPACTION_SUMMARY_BYTES, MAX_COMPACTION_TURNS,
    MIN_RECORDED_COMPACTION_COMPILER_VERSION, PreparedCompaction, join_recent_files, recent_files,
    recent_files_across, recent_files_match, render_recent_files, validate_compaction_references,
    validate_compactions_with_sequence, validate_goal_context_compactions_with_sequence,
};
pub(crate) use context::CompiledContext;
pub use events::{
    AgentDisposition, AgentRole, AgentRunId, BudgetAction, BudgetChargeBasis, BudgetRefusal,
    BudgetRefusalCause, ChildGrant, CollaborationPolicy, CompactionFailure, CompactionRecord,
    CompactionStatus, ContextUsage, DISPLAY_LIMIT_TEXT, DisplayCategory, DisplayChunk,
    EffectTarget, Event, EventEnvelope, ForkLineage, GoalBudget, GoalId, GoalMcpTool, GoalPhase,
    GoalPlanContract, GoalReport, GoalSkillBinding, GoalStatus, GoalSuccessCriterion,
    IncludeSource, InstructionFile, InstructionSource, MAX_BUDGET_REFUSAL_BYTES,
    MAX_DISPLAY_CHUNK_BYTES, MAX_DISPLAY_CHUNKS, MAX_DISPLAY_COMMENTARY_ITEMS,
    MAX_DISPLAY_TEXT_BYTES, MAX_GOAL_ACTIVE_MS, MAX_GOAL_PLAN_MCP_TOOLS, MAX_GOAL_PLAN_SKILLS,
    MAX_GOAL_PROVIDER_CALLS, MAX_GOAL_REPORT_BYTES, MAX_GOAL_RUNS, ProposalRefusal,
    ProposalRefusalCause, ProviderCallDisposition, ProviderCallRecord, ProviderSetupDisposition,
    ProviderSetupTarget, ProviderSetupTargetKind, RecentFile, ReplayError, RunConfig,
    RunDisposition, RunFailure, RunId, SessionDefaults, SessionId, ToolIntentRecord,
    ToolPreflightRefusal, ToolRefusal, ToolRefusalReason, WorkspaceIdentity,
};
pub(crate) use events::{
    GoalSuccessState, GoalUsage, MAX_CHILD_RESULT_BYTES, MAX_CHILD_TOOL_CALLS, MAX_CHILD_TURNS,
    MAX_GOAL_PLAN_CONTRACT_BYTES, MAX_STEP_TOOL_CALLS, STORE_ACTION_OVERHEAD_MS,
    requested_command_resources, run_started_fits,
};
pub(crate) use lineage::prefix_digest;
#[cfg(test)]
pub(crate) use reducer::rewind_tests::{
    finished as rewind_finished_fixture, started as rewind_started_fixture,
};
#[cfg(test)]
pub(crate) use reducer::worker_tests::{
    compacted_fixture as worker_compacted_fixture, completed_fixture as worker_protocol_fixture,
    host_protocol_fixture, session_host_protocol_fixture,
};
pub use reducer::{
    AgentStatus, AgentView, ApiUsage, CompactionView, GoalView, PlanUsage, RunStatus, RunView,
    SessionView, TokenCount, ToolView, UsageGroup, UsageView,
};
pub(crate) use reducer::{
    ForkInheritance, MAX_HOST_SESSION_GRANTS, coordination_ready, criterion_unmet_in_view,
    editor_tool_failed, evidence_admitted, validate_editor_request, validate_primary_tool_lease,
    validate_primary_tool_scope, validate_tool_started, verification_passed, wave_settled,
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
