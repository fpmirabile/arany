mod compaction;
mod context;
mod events;
pub(crate) mod input;
mod lineage;
mod reducer;

pub(crate) const MAX_CONTEXT_CONTENT_BYTES: usize = 376 * 1024;
pub(crate) const MAX_HISTORY_RUNS: usize = 32;
pub(crate) const MAX_USER_MESSAGE_BYTES: usize = 8 * 1024;
pub(crate) const MAX_ASSISTANT_MESSAGE_BYTES: usize = 32 * 1024;
pub(crate) const MAX_COMPACTIONS: usize = 64;

pub(crate) use compaction::{
    CompactionInputError, MAX_COMPACTION_OUTPUT_TOKENS, PreparedCompaction,
    validate_compaction_references,
};
pub(crate) use context::CompiledContext;
pub(crate) use events::ReplayError;
pub use events::{
    AgentDisposition, AgentRole, AgentRunId, CollaborationPolicy, CompactionFailure,
    CompactionRecord, CompactionStatus, ContextUsage, Event, EventEnvelope, ForkLineage,
    ProviderCallDisposition, ProviderCallRecord, RunConfig, RunDisposition, RunId, SessionDefaults,
    SessionId,
};
pub(crate) use lineage::prefix_digest;
pub use reducer::{
    AgentStatus, AgentView, CompactionView, RunStatus, RunView, SessionView, ToolView,
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
