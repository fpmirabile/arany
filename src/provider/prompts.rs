//! System prompt text, loaded byte-for-byte from `prompts/` files.

pub(super) const COLLABORATION_INSTRUCTIONS: &str = include_str!("prompts/collaboration.txt");
pub(super) const STEERING_INSTRUCTIONS: &str = include_str!("prompts/steering.txt");
pub(super) const READ_ONLY_INSTRUCTIONS: &str = include_str!("prompts/read_only.txt");
pub(super) const RESEARCH_INSTRUCTIONS: &str = include_str!("prompts/researcher.txt");
pub(super) const PLAN_CONTRACT_INSTRUCTIONS: &str = include_str!("prompts/plan_contract.txt");
pub(super) const COMPACTION_INSTRUCTIONS: &str = include_str!("prompts/compaction.txt");
pub(super) const OPENAI_TOOL_INSTRUCTIONS: &str = include_str!("prompts/openai_tool.txt");
pub(super) const ANTHROPIC_TOOL_INSTRUCTIONS: &str = include_str!("prompts/anthropic_tool.txt");
pub(crate) const TOOL_REVIEW_INSTRUCTIONS: &str = include_str!("prompts/tool_review.txt");
