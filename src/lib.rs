#![forbid(unsafe_code)]

mod diagnostics;
mod engine;
mod presentation;
mod provider;
mod session;
mod store;
mod telemetry;
mod terminal;
mod tools;

pub use diagnostics::{
    DevelopmentFailure, enable_development_diagnostics, record_development_failure,
};
pub use engine::{
    Engine, EngineError, RunCancellation, RunOutcome, RunProgress, RunRequest, RunUpdate,
    continue_session, create_session, fork_session, list_sessions, rename_session, resume_session,
    set_session_defaults,
};
pub use presentation::{Output, escape_terminal, render_exec, render_run_feedback, render_session};
pub use provider::{
    AgentPhase, AnthropicProvider, ChatGptAdmission, ChatGptProvenance, ChatGptProvider,
    ChildResult, CompactionItem, CompactionRequest, CompactionResponse, CustomProfile,
    CustomProfileCheck, CustomProfileError, CustomProfileProvenance, CustomProvider, Delegate,
    Effort, Finish, HistoryTurn, ImageAttachment, ImageOrigin, MAX_ANTHROPIC_WORKSPACE_ID_BYTES,
    MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGES, MAX_NATIVE_API_KEY_BYTES, ModelEntry,
    NativeApiCredentials, NativeCheckError, OpenAiProvider, OutputTokenBound, Provider,
    ProviderError, ProviderFailureReason, ProviderImage, ProviderOutcome, ProviderRequest,
    ProviderResponse, ProviderWireProvenance, UnansweredStatus, check_custom_profile,
    check_native_model_from_env, check_native_model_with_api_key,
    check_native_model_with_credentials, list_native_models, list_native_models_with_api_key,
    list_native_models_with_credentials, probe_chatgpt_model, resolve_native_effort,
    resolve_native_effort_for_run, validate_native_api_key, validate_native_model_id,
};
pub use session::{
    AgentDisposition, AgentRole, AgentRunId, AgentStatus, AgentView, CollaborationPolicy,
    CompactionFailure, CompactionRecord, CompactionStatus, CompactionView, ContextUsage, Event,
    EventEnvelope, ForkLineage, ProviderCallDisposition, ProviderCallRecord, RunConfig,
    RunDisposition, RunId, RunStatus, RunView, SessionDefaults, SessionId, SessionListItem,
    SessionView, ToolView,
};
pub use store::{StateRoot, Store, StoreError};
pub use telemetry::{Telemetry, TelemetryConfigError};
pub use terminal::{
    AttachedTerminal, CommandAvailability, CommandParseError, Composer, ComposerEdit,
    InteractiveCommand, ModelCatalogState, ModelPicker, ShutdownSignal, Submission, TerminalError,
    TerminalInput, command_completions, parse_submission,
};
pub use tools::{
    ApprovalInbox, ApprovalMode, EffectIntent, GuardReceipt, NetworkGrant, ToolApproval,
    ToolApprovals, ToolCall, ToolContext, ToolDisposition, ToolError, ToolLimits, ToolObservation,
    ToolPolicyReceipt, WorkspacePermissions, list_workspace_entries, mention_paths,
    tool_guard_main,
};
