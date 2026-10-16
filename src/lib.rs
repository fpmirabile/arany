#![deny(unsafe_code)]

#[cfg(all(test, unix))]
#[allow(dead_code)]
#[path = "../tests/common/process.rs"]
mod test_process;

mod diagnostics;
mod engine;
#[cfg(any(test, feature = "test-support"))]
mod fault;
mod presentation;
mod private_fs;
mod provider;
mod session;
mod store;
mod telemetry;
mod terminal;
mod tools;

#[cfg(target_os = "macos")]
pub use tools::tool_guard_unit_main;

pub use diagnostics::{
    DevelopmentFailure, enable_development_diagnostics, record_development_failure,
};
#[cfg(feature = "test-support")]
pub use engine::Action;
pub use engine::{
    DisplayUpdate, Engine, EngineError, GoalRequest, RunCancellation, RunOutcome, RunPreview,
    RunProgress, RunRequest, RunSteering, RunUpdate, SteeringRefused, continue_session,
    create_session, fork_session, list_sessions, rename_session, resume_session,
    set_session_defaults, validate_workspace_inputs,
};
#[cfg(feature = "test-support")]
pub use fault::FaultPlan;
pub use presentation::{
    Output, escape_terminal, goal_status, render_cache_update_feedback, render_exec,
    render_run_feedback, render_session,
};
#[cfg(feature = "test-support")]
pub use provider::semantic;
pub use provider::{
    AgentPhase, AnthropicProvider, CallLimits, ChatGptAdmission, ChatGptProvenance,
    ChatGptProvider, ChildResult, CompactionItem, CompactionRequest, CompactionResponse,
    CustomProfile, CustomProfileCheck, CustomProfileError, CustomProfileProvenance, CustomProvider,
    Delegate, Effort, Finish, GoalContextFacts, HistoryTurn, ImageAttachment, ImageOrigin,
    MAX_ANTHROPIC_WORKSPACE_ID_BYTES, MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGES,
    MAX_NATIVE_API_KEY_BYTES, ModelEntry, NativeApiCredentials, NativeCheckError, OpenAiProvider,
    OutputTokenBound, PreviewText, PromptRole, Provenance, Provider, ProviderError,
    ProviderFailureReason, ProviderIdentity, ProviderImage, ProviderOutcome, ProviderRequest,
    ProviderResponse, ProviderTurn, ProviderWireProvenance, ResponsePreview, RunInput, ToolUseId,
    UnansweredStatus, Usage, check_custom_profile, check_native_model_from_env,
    check_native_model_with_api_key, check_native_model_with_credentials, list_native_models,
    list_native_models_with_api_key, list_native_models_with_credentials, probe_chatgpt_model,
    resolve_native_effort, resolve_native_effort_for_run, validate_native_api_key,
    validate_native_model_id,
};
pub use session::{
    AgentDisposition, AgentRole, AgentRunId, AgentStatus, AgentView, ChildGrant,
    CollaborationPolicy, CompactionFailure, CompactionRecord, CompactionStatus, CompactionView,
    ContextUsage, DisplayCategory, DisplayChunk, Event, EventEnvelope, ForkLineage, GoalBudget,
    GoalId, GoalMcpTool, GoalPhase, GoalPlanContract, GoalReport, GoalSkillBinding, GoalStatus,
    GoalSuccessCriterion, GoalView, MAX_GOAL_ACTIVE_MS, MAX_GOAL_PLAN_MCP_TOOLS,
    MAX_GOAL_PLAN_SKILLS, MAX_GOAL_PROVIDER_CALLS, MAX_GOAL_REPORT_BYTES, MAX_GOAL_RUNS,
    ProviderCallDisposition, ProviderCallRecord, RunConfig, RunDisposition, RunId, RunStatus,
    RunView, SessionDefaults, SessionId, SessionListItem, SessionView, ToolView,
};
pub use store::{HistoryReset, Record, RecordKind, StateRoot, Store, StoreError};
pub use telemetry::{Telemetry, TelemetryConfigError};
pub use terminal::{
    AttachedTerminal, CommandAvailability, CommandParseError, CompletionLayout, Composer,
    ComposerEdit, InteractiveCommand, ModelCatalogState, ModelPicker, ProfileSelection,
    ShutdownSignal, Submission, TerminalError, TerminalInput, TerminalMode, TerminalNotice,
    command_completions, parse_submission,
};
pub use tools::{
    ApprovalInbox, ApprovalMode, CacheUpdate, CommandChangeset, EffectIntent, FileChange,
    FileVersion, GuardReceipt, MAX_SKILLS, NetworkGrant, ProjectSkills, RuntimeProfileOption,
    ToolApproval, ToolApprovals, ToolCall, ToolContext, ToolDisposition, ToolError, ToolLimits,
    ToolObservation, ToolPolicyReceipt, ToolRequest, ToolResourceClass, ToolResourceProfile,
    ToolResourceSettings, ToolStep, WorkspacePermissions, configured_skill_names,
    list_workspace_entries, mention_paths, native_protection_supported, project_skills,
    runtime_skill_names, tool_guard_main,
};
