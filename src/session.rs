mod compaction;
mod context;
mod events;
pub(crate) mod input;
mod lineage;
mod reducer;

pub(crate) const MAX_CONTEXT_CONTENT_BYTES: usize = 376 * 1024;
pub(crate) const MAX_HISTORY_RUNS: usize = 32;
pub(crate) const MAX_USER_MESSAGE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_STEERING_NOTE_BYTES: usize = 8 * 1024;
// The history header is fixed; each note's "\n[N] " label is charged inside MAX_STEERING_NOTE_BYTES.
pub(crate) const MAX_STEERING_CONTEXT_BYTES: usize = MAX_STEERING_NOTE_BYTES + 256;
// Charged per recorded note against the Run's byte bound: a label "\n[N] " for any note count
// up to 9999 is at most 8 bytes, and every note is at least one byte.
pub(crate) const MAX_STEERING_LABEL_BYTES: usize = 8;
pub(crate) const MAX_ASSISTANT_MESSAGE_BYTES: usize = 32 * 1024;

pub(crate) use compaction::{
    CompactionInputError, MAX_COMPACTION_INPUT_BYTES, MAX_COMPACTION_OUTPUT_TOKENS,
    MAX_COMPACTION_SUMMARY_BYTES, PreparedCompaction, validate_compaction_references,
};
pub(crate) use context::CompiledContext;
pub use events::{
    AgentDisposition, AgentRole, AgentRunId, ChildGrant, CollaborationPolicy, CompactionFailure,
    CompactionRecord, CompactionStatus, ContextUsage, DISPLAY_LIMIT_TEXT, DisplayCategory,
    DisplayChunk, Event, EventEnvelope, ForkLineage, GoalBudget, GoalId, GoalMcpTool, GoalPhase,
    GoalPlanContract, GoalReport, GoalSkillBinding, GoalStatus, GoalSuccessCriterion,
    MAX_DISPLAY_CHUNK_BYTES, MAX_DISPLAY_CHUNKS, MAX_DISPLAY_COMMENTARY_ITEMS,
    MAX_DISPLAY_TEXT_BYTES, MAX_GOAL_ACTIVE_MS, MAX_GOAL_PLAN_MCP_TOOLS, MAX_GOAL_PLAN_SKILLS,
    MAX_GOAL_PROVIDER_CALLS, MAX_GOAL_REPORT_BYTES, MAX_GOAL_RUNS, ProviderCallDisposition,
    ProviderCallRecord, RunConfig, RunDisposition, RunId, SessionDefaults, SessionId,
};
pub(crate) use events::{
    GoalSuccessState, GoalUsage, MAX_CHILD_TOOL_CALLS, MAX_GOAL_ACTION_ACTIVE_MS,
    MAX_GOAL_PLAN_CONTRACT_BYTES, MAX_STEP_TOOL_CALLS, ReplayError, payload_version,
};
pub(crate) use lineage::prefix_digest;
pub use reducer::{
    AgentStatus, AgentView, CompactionView, GoalView, RunStatus, RunView, SessionView, ToolView,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionListItem {
    pub id: SessionId,
    pub title: String,
    pub last_sequence: u64,
    pub created_at: String,
    pub last_activity_at: String,
    pub defaults: SessionDefaults,
}

pub(crate) fn placeholder_title(title: &str) -> bool {
    matches!(title, "New Session" | "Forked Session")
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
