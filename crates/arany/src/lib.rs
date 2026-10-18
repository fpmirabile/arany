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
    DisplayUpdate, Engine, EngineConfig, EngineError, GoalRequest, RunCancellation, RunControls,
    RunFuture, RunHandle, RunHints, RunOutcome, RunPreview, RunRequest, RunResult, RunSteering,
    SteeringRefused, ToolGrant, continue_session, create_session, fork_session, list_sessions,
    rename_session, resume_session, set_session_defaults, validate_workspace_inputs,
};
#[cfg(feature = "test-support")]
pub use fault::FaultPlan;
pub use journal::{Subscription, Tapped};
#[cfg(feature = "test-support")]
pub use provider::semantic;
pub use provider::{
    AdmittedProviders, AdmittedRoster, AgentPhase, AnthropicProvider, CallLimits, ChatGptAdmission,
    ChatGptProvenance, ChatGptProvider, ChildAssignment, ChildResult, CompactionItem,
    CompactionRequest, CompactionResponse, CustomProfile, CustomProfileCheck, CustomProfileError,
    CustomProfileProvenance, CustomProvider, Delegate, Effort, Finish, GoalContextFacts,
    HistoryTurn, ImageAttachment, ImageOrigin, MAX_ANTHROPIC_WORKSPACE_ID_BYTES, MAX_IMAGE_BYTES,
    MAX_MESSAGE_IMAGES, MAX_NATIVE_API_KEY_BYTES, MAX_PROVIDER_FALLBACKS, MAX_PROVIDER_SWITCHES,
    ModelEntry, NamedProviderRoutes, NativeApiCredentials, NativeCheckError, OpenAiProvider,
    OutputTokenBound, PreviewText, PromptRole, Provenance, Provider, ProviderError,
    ProviderFailureReason, ProviderFallback, ProviderIdentity, ProviderImage, ProviderOutcome,
    ProviderRequest, ProviderResponse, ProviderRole, ProviderRoutes, ProviderTurn,
    ProviderWireProvenance, Providers, ResponsePreview, RosterSnapshot, RunInput, ToolUseId,
    UnansweredStatus, Usage, check_custom_profile, check_native_model_from_env,
    check_native_model_with_api_key, check_native_model_with_credentials, list_native_models,
    list_native_models_with_api_key, list_native_models_with_credentials, probe_chatgpt_model,
    resolve_native_effort, resolve_native_effort_for_run, valid_profile_name,
    validate_native_api_key, validate_native_model_id,
};
pub use session::worker;
pub use session::{
    AgentDisposition, AgentRole, AgentRunId, AgentStatus, AgentView, ChildGrant,
    CollaborationPolicy, CompactionFailure, CompactionRecord, CompactionStatus, CompactionView,
    ContextUsage, DisplayCategory, DisplayChunk, Event, EventEnvelope, ForkLineage, GoalBudget,
    GoalId, GoalMcpTool, GoalPhase, GoalPlanContract, GoalReport, GoalSkillBinding, GoalStatus,
    GoalSuccessCriterion, GoalView, MAX_GOAL_ACTIVE_MS, MAX_GOAL_PLAN_MCP_TOOLS,
    MAX_GOAL_PLAN_SKILLS, MAX_GOAL_PROVIDER_CALLS, MAX_GOAL_REPORT_BYTES, MAX_GOAL_RUNS,
    ProviderCallDisposition, ProviderCallRecord, ReplayError, RunConfig, RunDisposition, RunId,
    RunStatus, RunView, SessionDefaults, SessionId, SessionListItem, SessionView, ToolView,
    WorkspaceIdentity,
};
pub use store::{HistoryReset, Record, RecordKind, StateRoot, Store, StoreError};
pub use telemetry::{Telemetry, TelemetryConfigError};
pub use tools::{
    ApprovalInbox, ApprovalMode, ApprovalReply, Attestation, CacheUpdate, CommandChangeset,
    DefinitionFilePin, DefinitionRegistryPin, EffectIntent, EnforcementCause, FileChange,
    FileVersion, GuardReceipt, MAX_SKILLS, NetworkGrant, ProjectSkills, RuntimeProfileOption,
    ToolApproval, ToolApprovals, ToolCall, ToolContext, ToolDisposition, ToolError, ToolLimits,
    ToolObservation, ToolPolicyReceipt, ToolRequest, ToolResourceClass, ToolResourceProfile,
    ToolResourceSettings, ToolStep, TrustRecordError, TrustReset, UnguardedPrompt,
    UnguardedReceipt, WorkspacePermissions, configured_skill_names, list_workspace_entries,
    mention_paths, native_protection_supported, project_skills, runtime_skill_names,
    tool_guard_main,
};

pub use session::DISPLAY_LIMIT_TEXT;
pub use tools::types::valid_name as valid_tool_name;

pub use tools::catalog::Site;

pub fn reduce_into(view: &mut SessionView, envelope: &EventEnvelope) -> Result<(), ReplayError> {
    view.apply_committed(envelope)
}
