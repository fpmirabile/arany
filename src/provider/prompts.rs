//! System prompt text, loaded byte-for-byte from `prompts/` files.

pub(crate) const COLLABORATION_INSTRUCTIONS: &str = include_str!("prompts/collaboration.txt");
pub(crate) const STEERING_INSTRUCTIONS: &str = include_str!("prompts/steering.txt");
pub(crate) const READ_ONLY_INSTRUCTIONS: &str = include_str!("prompts/read_only.txt");
pub(crate) const RESEARCH_INSTRUCTIONS: &str = include_str!("prompts/researcher.txt");
pub(crate) const PLAN_CONTRACT_INSTRUCTIONS: &str = include_str!("prompts/plan_contract.txt");
pub(crate) const COMPACTION_INSTRUCTIONS: &str = include_str!("prompts/compaction.txt");
pub(crate) const OPENAI_TOOL_INSTRUCTIONS: &str = include_str!("prompts/openai_tool.txt");
pub(crate) const ANTHROPIC_TOOL_INSTRUCTIONS: &str = include_str!("prompts/anthropic_tool.txt");
pub(crate) const TOOL_REVIEW_INSTRUCTIONS: &str = include_str!("prompts/tool_review.txt");
