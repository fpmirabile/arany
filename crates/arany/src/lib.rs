#![deny(unsafe_code)]

#[cfg(all(test, unix))]
use test_support::process as test_process;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

mod diagnostics;
mod engine;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use engine::MAX_DELEGATION_DEADLINE;
mod fault;
mod journal;
pub mod limits;
mod private_fs;
mod provider;
mod session;
mod store;
pub mod supervisor;
mod telemetry;
mod timestamp;
mod tools;
mod validate;

pub use validate::bounded_vec;

pub use timestamp::utc_date;
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
    budget_refusal_summary, continue_session, create_session, fork_session, list_sessions,
    list_sessions_page, rename_session, resume_session, set_session_defaults,
    start_provider_setup_edit, validate_workspace_inputs,
};
#[cfg(feature = "test-support")]
pub use fault::FaultPlan;
pub(crate) use journal::check::CheckJournal;
pub use journal::check::{
    CHECK_FORMATS, ChatGptCheck, ChatGptCheckError, ChatGptCheckRequest, CheckCallKind,
    CheckCallToken, CheckDisposition, CheckEvent, CheckId, CheckOpenError, CheckProbe,
    CheckReceipt, CheckStart, CheckTarget, CheckUsage, CheckView,
};
pub use journal::{Subscription, Tapped};
#[cfg(feature = "test-support")]
pub use provider::semantic;
pub use provider::{
    AdmittedProviders, AdmittedRoster, AgentPhase, Answer, AnthropicProvider, CallLimits,
    ChatGptAdmission, ChatGptProvenance, ChatGptProvider, ChildAssignment, ChildResult,
    CompactionItem, CompactionRequest, CompactionResponse, CustomCheckStatus, CustomProfile,
    CustomProfileCheck, CustomProfileDraft, CustomProfileError, CustomProfileProvenance,
    CustomProtocol, CustomProvider, Delegate, Effort, GoalContextFacts, HistoryTurn,
    ImageAttachment, ImageOrigin, LimitFailure, LimitSource, LimitUnavailable,
    MAX_ANTHROPIC_WORKSPACE_ID_BYTES, MAX_CATALOG_MODELS, MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES,
    MAX_CUSTOM_PROFILE_EFFORTS_BYTES, MAX_CUSTOM_PROFILE_ENDPOINT_BYTES,
    MAX_CUSTOM_PROFILE_MODEL_BYTES, MAX_CUSTOM_PROFILE_NAME_BYTES, MAX_ENCODED_IMAGE_BYTES,
    MAX_IMAGE_BYTES, MAX_LIMIT_TIMESTAMP, MAX_MESSAGE_IMAGES, MAX_NATIVE_API_KEY_BYTES,
    MAX_PROVIDER_FALLBACKS, MAX_PROVIDER_RETRIES, MAX_PROVIDER_SWITCHES, MessagesAuth, ModelEntry,
    NamedProviderRoutes, NativeApiCredentials, NativeCheckError, NativeCheckRequest,
    OpenAiProvider, OutputTokenBound, PlanRequest, PlanWindow, PreviewText, PromptRole, Provenance,
    Provider, ProviderError, ProviderFailureReason, ProviderFallback, ProviderIdentity,
    ProviderImage, ProviderKind, ProviderOutcome, ProviderRecord, ProviderRequest,
    ProviderResponse, ProviderRetry, ProviderRole, ProviderRoutes, ProviderTurn,
    ProviderWireProvenance, Providers, RateLimitSlot, RateLimits, ResponsePreview, RetryCause,
    ReviewVerdict, RosterSnapshot, RunInput, ToolUseId, UnansweredStatus, Usage, UsedPercent,
    WorkspaceGuidance, admitted_context_window, check_custom_profile,
    check_custom_profile_value_with_cancellation, check_custom_profile_with_cancellation,
    check_native_model, check_native_model_from_env, list_native_models,
    list_native_models_with_api_key, list_native_models_with_credentials, probe_chatgpt_model,
    resolve_native_effort, resolve_native_effort_for_run, valid_profile_name,
    validate_native_api_key, validate_native_model_id,
};
pub use session::{
    AgentDisposition, AgentRole, AgentRunId, AgentStatus, AgentView, BudgetAction,
    BudgetChargeBasis, BudgetRefusal, BudgetRefusalCause, ChildGrant, CollaborationPolicy,
    CompactionFailure, CompactionRecord, CompactionStatus, CompactionView, ContextUsage,
    DisplayCategory, DisplayChunk, Event, EventEnvelope, ForkLineage, GoalBudget, GoalId,
    GoalLimit, GoalMcpTool, GoalPhase, GoalPlanContract, GoalReport, GoalSkillBinding, GoalStatus,
    GoalSuccessCriterion, GoalView, InstructionFile, InstructionSource, MAX_BUDGET_REFUSAL_BYTES,
    MAX_GOAL_ACTIVE_MS, MAX_GOAL_PLAN_MCP_TOOLS, MAX_GOAL_PLAN_SKILLS, MAX_GOAL_PROVIDER_CALLS,
    MAX_GOAL_REPORT_BYTES, MAX_GOAL_RUNS, MAX_USER_MESSAGE_BYTES, PlanUsage, ProposalRefusal,
    ProposalRefusalCause, ProviderCallDisposition, ProviderCallRecord, ProviderSetupDisposition,
    ProviderSetupTarget, ProviderSetupTargetKind, RecentFile, ReplayError, RunConfig,
    RunDisposition, RunFailure, RunFailureFeedback, RunId, RunStatus, RunStop, RunView,
    SessionDefaults, SessionId, SessionListItem, SessionPage, SessionView, TokenCount,
    ToolIntentRecord, ToolPreflightRefusal, ToolRefusal, ToolRefusalReason, ToolView, UsageGroup,
    UsageView, WorkspaceIdentity, escape_status_text,
};
pub use session::{Checkpoint, CheckpointReason, CheckpointStatus};
pub use session::{rewind, worker};
pub use store::{
    ArchivedHistory, CheckpointReset, HistoryRecovery, HistoryReset, InspectedEvent,
    InspectionCause, InspectionFailure, ProviderRecordEdit, ProviderRecordTarget, RECORD_FORMATS,
    Record, RecordFormat, RecordKind, RecordStep, SessionInspection, StateRoot, Store, StoreError,
};
pub use telemetry::{Telemetry, TelemetryConfigError};
pub use tools::{
    ApprovalCause, ApprovalInbox, ApprovalMode, ApprovalReply, Attestation, CacheUpdate,
    CommandChangeset, DEFAULT_PR_LIMIT, DefinitionFilePin, DefinitionRegistryPin, EffectIntent,
    EnforcementCause, FileChange, FileReceipt, FileSuggestionSource, FileVersion, GuardReceipt,
    HostIntent, LimitCause, LiteralEdit, MAX_SEARCH_ROWS, MAX_SKILLS, NetworkGrant, OutputSpill,
    OutputTotal, PrState, ProjectSkills, RuntimeProfileOption, SUGGESTION_LIFETIME,
    SUGGESTION_QUERY_TIME, SuggestionIndex, ToolApproval, ToolApprovals, ToolCall, ToolContext,
    ToolDisposition, ToolError, ToolLimits, ToolObservation, ToolPolicyReceipt, ToolRequest,
    ToolResourceClass, ToolResourceProfile, ToolResourceSettings, ToolStep, TrustRecordError,
    TrustReset, UnguardedGrant, UnguardedPrompt, UnguardedReceipt, WorkspacePermissions,
    configured_skill_names, mention_paths, native_protection_supported, now_ms, project_skills,
    runtime_skill_names, tool_guard_main, tool_host_main,
};

pub use session::DISPLAY_LIMIT_TEXT;
pub fn valid_tool_name(value: &str) -> bool {
    validate::valid_name(value)
}

pub use tools::catalog::{Family, Site};

// Validates each Event like replay but skips its whole-history compaction checks; for presentation views only.
pub fn reduce_into(view: &mut SessionView, envelope: &EventEnvelope) -> Result<(), ReplayError> {
    view.apply_committed(envelope)
}

#[cfg(unix)]
pub use store::{
    StorageCategory, StorageError, StorageReason, StorageReport, StorageStatus, StorageUsage,
    collect_storage,
};
