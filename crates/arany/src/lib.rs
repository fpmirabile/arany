#![deny(unsafe_code)]

#[cfg(all(test, unix))]
use test_support::process as test_process;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

mod diagnostics;
mod engine;
#[cfg(any(test, feature = "test-support"))]
mod fault;
mod journal;
mod private_fs;
mod provider;
mod session;
mod store;
pub mod supervisor;
mod telemetry;
mod tools;

pub use tools::permissions;

#[cfg(target_os = "macos")]
pub use tools::tool_guard_unit_main;

pub use diagnostics::{
    DevelopmentFailure, enable_development_diagnostics, record_development_failure,
};
#[cfg(feature = "test-support")]
pub use engine::Action;
pub use engine::{
    ChildSteeringTarget, ConversationRewind, ConversationRewindOutcome, DisplayUpdate, Engine,
    EngineConfig, EngineError, GoalRequest, RewindBothResult, RewindFile, RewindIssue,
    RewindPreview, RewindRequest, RunCancellation, RunControls, RunFuture, RunHandle, RunHints,
    RunOutcome, RunPreview, RunRequest, RunResult, RunSteering, SteeringRefused, ToolGrant,
    continue_session, create_session, fork_session, list_sessions, list_sessions_page,
    rename_session, resume_session, set_session_defaults, validate_workspace_inputs,
};
#[cfg(feature = "test-support")]
pub use fault::FaultPlan;
pub use journal::{Subscription, Tapped};
#[cfg(feature = "test-support")]
pub use provider::semantic;
pub use provider::{
    AdmittedProviders, AdmittedRoster, AgentPhase, Answer, AnthropicProvider, CallLimits,
    ChatGptAdmission, ChatGptProvenance, ChatGptProvider, ChildAssignment, ChildResult,
    CompactionItem, CompactionRequest, CompactionResponse, CustomProfile, CustomProfileCheck,
    CustomProfileError, CustomProfileProvenance, CustomProvider, Delegate, Effort,
    GoalContextFacts, HistoryTurn, ImageAttachment, ImageOrigin, MAX_ANTHROPIC_WORKSPACE_ID_BYTES,
    MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGES, MAX_NATIVE_API_KEY_BYTES, MAX_PROVIDER_FALLBACKS,
    MAX_PROVIDER_RETRIES, MAX_PROVIDER_SWITCHES, ModelEntry, NamedProviderRoutes,
    NativeApiCredentials, NativeCheckError, OpenAiProvider, OutputTokenBound, PlanRequest,
    PreviewText, PromptRole, Provenance, Provider, ProviderError, ProviderFailureReason,
    ProviderFallback, ProviderIdentity, ProviderImage, ProviderOutcome, ProviderRequest,
    ProviderResponse, ProviderRetry, ProviderRole, ProviderRoutes, ProviderTurn,
    ProviderWireProvenance, Providers, ResponsePreview, RetryCause, ReviewVerdict, RosterSnapshot,
    RunInput, ToolUseId, UnansweredStatus, Usage, WorkspaceGuidance, admitted_context_window,
    check_custom_profile, check_native_model_from_env, check_native_model_with_api_key,
    check_native_model_with_credentials, list_native_models, list_native_models_with_api_key,
    list_native_models_with_credentials, probe_chatgpt_model, resolve_native_effort,
    resolve_native_effort_for_run, valid_profile_name, validate_native_api_key,
    validate_native_model_id,
};
pub use session::{
    AgentDisposition, AgentRole, AgentRunId, AgentStatus, AgentView, ChildGrant,
    CollaborationPolicy, CompactionFailure, CompactionRecord, CompactionStatus, CompactionView,
    ContextUsage, DisplayCategory, DisplayChunk, Event, EventEnvelope, ForkLineage, GoalBudget,
    GoalId, GoalLimit, GoalMcpTool, GoalPhase, GoalPlanContract, GoalReport, GoalSkillBinding,
    GoalStatus, GoalSuccessCriterion, GoalView, InstructionFile, InstructionSource,
    MAX_GOAL_ACTIVE_MS, MAX_GOAL_PLAN_MCP_TOOLS, MAX_GOAL_PLAN_SKILLS, MAX_GOAL_PROVIDER_CALLS,
    MAX_GOAL_REPORT_BYTES, MAX_GOAL_RUNS, ProposalRefusal, ProposalRefusalCause,
    ProviderCallDisposition, ProviderCallRecord, ReplayError, RunConfig, RunDisposition,
    RunFailure, RunId, RunStatus, RunStop, RunView, SessionDefaults, SessionId, SessionListItem,
    SessionPage, SessionView, ToolView, WorkspaceIdentity,
};
pub use session::{Checkpoint, CheckpointReason, CheckpointStatus};
pub use session::{rewind, worker};
pub use store::{
    ArchivedHistory, CheckpointReset, HistoryRecovery, HistoryReset, RECORD_FORMATS, Record,
    RecordFormat, RecordKind, RecordStep, StateRoot, Store, StoreError,
};
pub use telemetry::{Telemetry, TelemetryConfigError};
pub use tools::{
    ApprovalCause, ApprovalInbox, ApprovalMode, ApprovalReply, Attestation, CacheUpdate,
    CommandChangeset, DefinitionFilePin, DefinitionRegistryPin, EffectIntent, EnforcementCause,
    FileChange, FileVersion, GuardReceipt, LimitCause, LiteralEdit, MAX_SEARCH_ROWS, MAX_SKILLS,
    NetworkGrant, OutputTotal, ProjectSkills, RuntimeProfileOption, ToolApproval, ToolApprovals,
    ToolCall, ToolContext, ToolDisposition, ToolError, ToolLimits, ToolObservation,
    ToolPolicyReceipt, ToolRequest, ToolResourceClass, ToolResourceProfile, ToolResourceSettings,
    ToolStep, TrustRecordError, TrustReset, UnguardedGrant, UnguardedPrompt, UnguardedReceipt,
    WorkspacePermissions, configured_skill_names, list_workspace_entries, mention_paths,
    native_protection_supported, now_ms, project_skills, runtime_skill_names, tool_guard_main,
};

pub use session::DISPLAY_LIMIT_TEXT;
pub use tools::types::valid_name as valid_tool_name;

pub use tools::catalog::Site;

// Validates each Event like replay but skips its whole-history compaction checks; for presentation views only.
pub fn reduce_into(view: &mut SessionView, envelope: &EventEnvelope) -> Result<(), ReplayError> {
    view.apply_committed(envelope)
}
