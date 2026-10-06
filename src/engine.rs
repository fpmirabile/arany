use crate::provider::{AgentPhase, ImageAttachment, Provider, ProviderRequest};
use crate::session::input::{InputError, WorkspaceInputs};
use crate::session::{
    AgentRole, AgentRunId, CollaborationPolicy, CompiledContext, Event,
    MAX_USER_MESSAGE_BYTES as MAX_OBJECTIVE_BYTES, RunConfig, RunId, RunView, SessionId,
};
use crate::store::{SessionRunLock, StateRoot, Store, StoreError};
use crate::telemetry::{Telemetry, TraceOutcome};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::watch;

mod compaction;
mod lifecycle;
mod progress;
mod run_loop;
pub use lifecycle::{
    continue_session, create_session, fork_session, list_sessions, rename_session, resume_session,
    set_session_defaults,
};
use lifecycle::{ensure_session_workspace, fork_with_store, lock_session};
use progress::{ProgressPublisher, append_observed};
pub use progress::{RunProgress, RunUpdate};
use run_loop::{RunControls, RunIdentity, RunLoop};

const OUTPUT_TOKEN_CAP: u32 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid Run request")]
    InvalidRequest,
    #[error("Session not found")]
    MissingSession,
    #[error("no Session for this Workspace")]
    NoSessionForWorkspace,
    #[error("too many Sessions for discovery; use --resume ID")]
    TooManySessions,
    #[error("Session has an active operation")]
    SessionBusy,
    #[error("Session is not idle")]
    SessionNotIdle,
    #[error("Session has no committed Run boundary to fork")]
    ForkNotAtRunBoundary,
    #[error("Session belongs to a different Workspace")]
    WorkspaceMismatch,
    #[error("Workspace input path is invalid")]
    InvalidWorkspacePath,
    #[error("Workspace input must be a regular file")]
    InputNotRegular,
    #[error("Workspace input exceeds its byte limit")]
    InputTooLarge,
    #[error("Workspace input is not UTF-8")]
    InputNotUtf8,
    #[error("explicit include does not exist")]
    MissingInclude,
    #[error("too many explicit includes")]
    TooManyIncludes,
    #[error("state directory overlaps the Workspace")]
    StateOverlap,
    #[error("Workspace input is unavailable")]
    WorkspaceUnavailable,
    #[error("invalid Session history")]
    InvalidHistory,
    #[error("compiled Provider context exceeds its byte limit")]
    ContextTooLarge,
    #[error("image input is not admitted for this custom Provider")]
    ImagesNotAdmitted,
    #[error("Session has no new successful conversation to compact")]
    CompactionInputEmpty,
    #[error("compaction input exceeds its limit")]
    CompactionInputTooLarge,
    #[error("Session is not idle at a committed Run boundary")]
    CompactionNotIdle,
    #[error("Session compaction limit reached; start a new Session")]
    CompactionLimit,
    #[error("Run cancellation did not drain within its deadline")]
    CancellationDrainTimeout,
    #[error("Run coordinator failed")]
    CoordinatorFailed,
    #[error("Run was cancelled before admission")]
    CancelledBeforeStart,
    #[error("state storage failed")]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tool(#[from] crate::tools::ToolError),
}

impl From<InputError> for EngineError {
    fn from(error: InputError) -> Self {
        match error {
            InputError::InvalidPath => Self::InvalidWorkspacePath,
            InputError::NotRegular => Self::InputNotRegular,
            InputError::UnsafeLinkCount => Self::WorkspaceUnavailable,
            InputError::TooLarge => Self::InputTooLarge,
            InputError::InvalidUtf8 => Self::InputNotUtf8,
            InputError::MissingInclude => Self::MissingInclude,
            InputError::TooManyIncludes => Self::TooManyIncludes,
            InputError::StateOverlap => Self::StateOverlap,
            InputError::Io(_) => Self::WorkspaceUnavailable,
        }
    }
}

pub struct RunRequest {
    pub session_id: Option<SessionId>,
    pub title: Option<String>,
    pub objective: String,
    pub images: Vec<ImageAttachment>,
    pub workspace: PathBuf,
    pub include_paths: Vec<PathBuf>,
    pub policy: CollaborationPolicy,
}

pub struct RunOutcome {
    pub session_id: SessionId,
    pub run: RunView,
}

#[derive(Clone)]
pub struct RunCancellation {
    sender: watch::Sender<bool>,
    receiver: watch::Receiver<bool>,
}

impl RunCancellation {
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(false);
        Self { sender, receiver }
    }

    /// Returns true only for the first cancellation request.
    pub fn cancel(&self) -> bool {
        !self.sender.send_replace(true)
    }

    fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Waits for sticky cancellation; dropping the waiter does not undo started work.
    pub async fn cancelled(&mut self) {
        if self.is_cancelled() {
            return;
        }
        while self.receiver.changed().await.is_ok() {
            if *self.receiver.borrow_and_update() {
                return;
            }
        }
    }
}

impl Default for RunCancellation {
    fn default() -> Self {
        Self::new()
    }
}

struct ProviderSelection {
    name: String,
    model: String,
    effort: Option<crate::provider::Effort>,
    concurrency: u8,
    custom_profile_provenance: Option<crate::provider::CustomProfileProvenance>,
    saved_api_account_id: Option<uuid::Uuid>,
    chatgpt_provenance: Option<crate::provider::ChatGptProvenance>,
    output_token_bound: crate::provider::OutputTokenBound,
}

pub struct Engine<P: Provider> {
    state: StateRoot,
    store: Store,
    provider: Arc<P>,
    telemetry: Telemetry,
    tools: Option<(crate::tools::config::Config, PathBuf)>,
    workspace_permissions: Option<crate::tools::WorkspacePermissions>,
    tool_approvals: Option<crate::tools::ToolApprovals>,
}

impl<P: Provider + 'static> Engine<P> {
    async fn lock_lineage(
        &self,
        view: &crate::session::SessionView,
    ) -> Result<Vec<SessionRunLock>, EngineError> {
        let mut source = view.lineage.map(|lineage| lineage.source_session_id);
        let mut locks = Vec::new();
        while let Some(id) = source {
            if locks.len() == 8 {
                return Err(StoreError::ReplayLimit.into());
            }
            let mut lock = self.lock_session(id)?;
            lock.keep();
            locks.push(lock);
            let ancestor = self
                .store
                .load_view(id)
                .await?
                .ok_or(EngineError::InvalidHistory)?;
            source = ancestor.lineage.map(|lineage| lineage.source_session_id);
        }
        Ok(locks)
    }

    fn lock_session(&self, session_id: SessionId) -> Result<SessionRunLock, EngineError> {
        lock_session(&self.state, session_id)
    }

    pub fn open(state: StateRoot, provider: P) -> Result<Self, EngineError> {
        Self::open_with_telemetry(state, provider, Telemetry::disabled())
    }

    pub fn open_with_telemetry(
        state: StateRoot,
        provider: P,
        telemetry: Telemetry,
    ) -> Result<Self, EngineError> {
        let store = Store::open(state.try_clone()?)?;
        Ok(Self {
            state,
            store,
            provider: Arc::new(provider),
            telemetry,
            tools: None,
            workspace_permissions: None,
            tool_approvals: None,
        })
    }

    pub async fn close(self) -> Result<(), EngineError> {
        self.store.close().await?;
        Ok(())
    }

    pub fn enable_tools(&mut self, guard_executable: PathBuf) -> Result<(), EngineError> {
        let config = crate::tools::config::Config::load(&self.state)?;
        self.tools = Some((config, guard_executable));
        self.workspace_permissions = None;
        Ok(())
    }

    pub fn set_tool_approvals(&mut self, approvals: crate::tools::ToolApprovals) {
        self.tool_approvals = Some(approvals);
    }

    pub fn enable_trusted_workspace(
        &mut self,
        permissions: crate::tools::WorkspacePermissions,
        workspace: &std::path::Path,
        guard_executable: PathBuf,
        approvals: crate::tools::ToolApprovals,
    ) -> Result<(), EngineError> {
        self.tools = Some((permissions.config(workspace)?, guard_executable));
        self.workspace_permissions = Some(permissions);
        self.tool_approvals = Some(approvals);
        Ok(())
    }

    pub async fn fork_session(
        &mut self,
        source_session_id: SessionId,
        workspace: PathBuf,
        title: Option<String>,
    ) -> Result<SessionId, EngineError> {
        fork_with_store(
            &self.state,
            &self.store,
            source_session_id,
            &workspace,
            title,
        )
        .await
    }

    pub async fn run(&mut self, request: RunRequest) -> Result<RunOutcome, EngineError> {
        self.run_with_cancel(request, RunCancellation::new()).await
    }

    pub async fn run_with_cancel(
        &mut self,
        request: RunRequest,
        cancellation: RunCancellation,
    ) -> Result<RunOutcome, EngineError> {
        self.run_inner(request, cancellation, None)
            .await
            .inspect_err(crate::diagnostics::engine_failure)
    }

    pub async fn run_with_progress(
        &mut self,
        request: RunRequest,
        cancellation: RunCancellation,
        progress: RunProgress,
    ) -> Result<RunOutcome, EngineError> {
        self.run_inner(request, cancellation, Some(progress))
            .await
            .inspect_err(crate::diagnostics::engine_failure)
    }

    async fn run_inner(
        &mut self,
        request: RunRequest,
        cancellation: RunCancellation,
        progress: Option<RunProgress>,
    ) -> Result<RunOutcome, EngineError> {
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        let selection = self.select_provider()?;
        self.validate_request(&request)?;
        if self.tools.is_some() && selection.custom_profile_provenance.is_some() {
            return Err(EngineError::InvalidRequest);
        }
        if selection.custom_profile_provenance.is_some() && !request.images.is_empty() {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let session_id = request.session_id.unwrap_or_default();
        let mut session_lock = self.lock_session(session_id)?;
        let mut history = if request.session_id.is_some() {
            Some(
                self.store
                    .load_view(session_id)
                    .await?
                    .ok_or(EngineError::MissingSession)?,
            )
        } else {
            None
        };
        if history.is_some() {
            session_lock.keep();
        }
        let _lineage_locks = match &history {
            Some(view) => self.lock_lineage(view).await?,
            None => Vec::new(),
        };
        let children = usize::from(request.policy.max_children());
        let lifecycle_slots = if children == 0 { 7 } else { 8 + 3 * children };
        let tool_slots = if self.tools.is_some() {
            (if self.tool_approvals.is_some() { 4 } else { 3 }) * crate::tools::MAX_TOOL_CALLS
        } else {
            0
        };
        self.store
            .admit_operation(
                session_id,
                lifecycle_slots + tool_slots + usize::from(request.session_id.is_none()),
            )
            .await?;
        if let Some(permissions) = &self.workspace_permissions {
            permissions.verify(&request.workspace)?;
        }
        let tools = self
            .tools
            .as_ref()
            .map(|(config, executable)| {
                crate::tools::ToolRuntime::admit(
                    config.clone(),
                    &request.workspace,
                    &self.state,
                    executable.clone(),
                )
            })
            .transpose()?;
        if let (Some(permissions), Some(runtime)) = (&self.workspace_permissions, &tools)
            && !permissions.matches_identity(runtime.workspace_identity())
        {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        let tool_context = tools
            .as_ref()
            .map(crate::tools::ToolRuntime::context)
            .transpose()?;
        let inputs = match &tools {
            Some(runtime) => WorkspaceInputs::load_pinned(
                &request.workspace,
                &self.state,
                &request.include_paths,
                Some(runtime.workspace_identity()),
            )?,
            None => WorkspaceInputs::load(&request.workspace, &self.state, &request.include_paths)?,
        };
        let (workspace_device, workspace_inode) = inputs.root_identity()?;
        if let Some(view) = &history {
            ensure_session_workspace(view, workspace_device, workspace_inode)?;
        }
        let context_started = SystemTime::now();
        let context = CompiledContext::compile(
            history.as_ref(),
            &request.objective,
            &request.images,
            &inputs,
            request.policy,
            tools
                .as_ref()
                .map_or(0, |_| crate::tools::MAX_TOOL_CONTEXT_BYTES),
        )
        .map_err(|_| EngineError::ContextTooLarge)?;
        if selection.custom_profile_provenance.is_some() && !context.images.is_empty() {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let context_ended = SystemTime::now();
        let config = RunConfig {
            provider: selection.name,
            model: selection.model.clone(),
            effort: selection.effort,
            custom_profile_provenance: selection.custom_profile_provenance,
            saved_api_account_id: selection.saved_api_account_id,
            chatgpt_provenance: selection.chatgpt_provenance,
            output_token_bound: selection.output_token_bound,
            policy: request.policy,
            output_token_cap: OUTPUT_TOKEN_CAP,
            provider_concurrency: selection.concurrency,
            workspace_device,
            workspace_inode,
            instruction_digest: inputs
                .instructions
                .as_ref()
                .map(|(_, snapshot)| snapshot.digest),
            include_digests: inputs
                .includes
                .iter()
                .map(|snapshot| snapshot.digest)
                .collect(),
            history_run_ids: context.history_run_ids,
            excluded_history_runs: context.excluded_history_runs,
            context_usage: Some(context.context_usage),
            compaction_event_sequence: context.compaction_event_sequence,
            compaction_content_digest: context.compaction_content_digest,
            tool_policy: tools.as_ref().map(crate::tools::ToolRuntime::receipt),
        };
        config.validate().map_err(|_| EngineError::InvalidRequest)?;

        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }

        if request.session_id.is_none() {
            self.store
                .append(
                    session_id,
                    Event::SessionStarted {
                        title: request.title.unwrap_or_else(|| "New Session".into()),
                        workspace_identity: Some((workspace_device, workspace_inode)),
                    },
                )
                .await?;
            session_lock.keep();
        }
        let run_id = RunId::new();
        let agent_run_id = AgentRunId::new();
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        let trace_provider = crate::telemetry::TraceProvider::from_profile(&config.provider);
        let publisher = if let Some(progress) = progress {
            let view = match history.take() {
                Some(view) => view,
                None => self
                    .store
                    .load_view(session_id)
                    .await?
                    .ok_or(EngineError::InvalidHistory)?,
            };
            Some(ProgressPublisher::new(view, progress, run_id))
        } else {
            None
        };
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        let accepted_sequence = append_observed(
            &self.store,
            publisher.as_ref(),
            None,
            session_id,
            Event::MessageAccepted {
                run_id,
                text: request.objective.clone(),
                images: request.images,
            },
        )
        .await?;
        let started_sequence = append_observed(
            &self.store,
            publisher.as_ref(),
            None,
            session_id,
            Event::RunStarted { run_id, config },
        )
        .await?;
        let trace = self.telemetry.begin_run(
            session_id,
            run_id,
            trace_provider,
            Some((context_started, context_ended)),
        );
        trace.event_committed(
            accepted_sequence,
            crate::telemetry::TraceEventKind::MessageAccepted,
        );
        trace.event_committed(
            started_sequence,
            crate::telemetry::TraceEventKind::RunStarted,
        );
        append_observed(
            &self.store,
            publisher.as_ref(),
            Some(&trace),
            session_id,
            Event::AgentSpawned {
                run_id,
                agent_run_id,
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
            },
        )
        .await?;

        let provider_request = ProviderRequest {
            run_id,
            agent_run_id,
            phase: AgentPhase::RootPlan,
            collaboration: request.policy,
            model: selection.model,
            instructions: inputs.instructions.map(|(_, snapshot)| snapshot.content),
            objective: request.objective,
            images: context.images,
            includes: inputs
                .includes
                .into_iter()
                .map(|snapshot| snapshot.model_include())
                .collect(),
            history: context.history,
            context_summary: context.summary,
            child_results: Vec::new(),
            max_output_tokens: OUTPUT_TOKEN_CAP,
            tools: tool_context,
        };
        RunLoop::new(
            &self.store,
            Arc::clone(&self.provider),
            RunIdentity {
                session_id,
                run_id,
                primary_id: agent_run_id,
            },
            RunControls {
                policy: request.policy,
                concurrency: selection.concurrency,
                cancellation,
                tools: tools.as_ref(),
                approvals: self.tool_approvals.as_ref(),
            },
            publisher.as_ref(),
            &trace,
        )
        .execute(provider_request)
        .await?;
        drop(publisher);
        let view = self
            .store
            .load_view(session_id)
            .await?
            .ok_or(EngineError::InvalidHistory)?;
        let run = view
            .runs
            .last()
            .filter(|run| run.id == run_id)
            .ok_or(EngineError::InvalidHistory)?
            .clone();
        let trace_outcome = match run.status {
            crate::session::RunStatus::Finished => TraceOutcome::Finished,
            crate::session::RunStatus::Failed => TraceOutcome::Failed,
            crate::session::RunStatus::Cancelled => TraceOutcome::Cancelled,
            _ => return Err(EngineError::InvalidHistory),
        };
        trace.finish(trace_outcome);
        Ok(RunOutcome { session_id, run })
    }

    fn validate_request(&self, request: &RunRequest) -> Result<(), EngineError> {
        if request.objective.trim().is_empty() && request.images.is_empty()
            || request.objective.len() > MAX_OBJECTIVE_BYTES
            || !crate::provider::valid_image_collection(&request.images)
            || request.session_id.is_some() && request.title.is_some()
            || request
                .title
                .as_ref()
                .is_some_and(|title| title.is_empty() || title.len() > 128)
            || request.policy.validate().is_err()
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }

    fn select_provider(&self) -> Result<ProviderSelection, EngineError> {
        let selection = ProviderSelection {
            name: self.provider.profile_name().to_owned(),
            model: self.provider.model_name().to_owned(),
            effort: self.provider.reasoning_effort(),
            concurrency: self.provider.max_concurrent_calls(),
            custom_profile_provenance: self.provider.custom_profile_provenance(),
            saved_api_account_id: self.provider.saved_api_account_id(),
            chatgpt_provenance: self.provider.chatgpt_provenance(),
            output_token_bound: self.provider.output_token_bound(),
        };
        if selection.name.is_empty()
            || selection.name.len() > 128
            || selection.model.is_empty()
            || selection.model.len() > 128
            || selection.name.starts_with("custom:")
                != selection.custom_profile_provenance.is_some()
            || !crate::provider::valid_saved_api_account_id(
                &selection.name,
                selection.saved_api_account_id,
            )
            || (selection.name == "chatgpt") != selection.chatgpt_provenance.is_some()
            || (selection.name == "chatgpt")
                != (selection.output_token_bound
                    == crate::provider::OutputTokenBound::LocalAcceptanceOnly)
            || selection.name == "chatgpt"
                && (selection.effort.is_none() || selection.concurrency != 1)
            || selection
                .chatgpt_provenance
                .as_ref()
                .is_some_and(|provenance| !provenance.valid())
            || selection
                .custom_profile_provenance
                .as_ref()
                .is_some_and(|provenance| {
                    provenance.capability_evidence_version == 1 && selection.effort.is_some()
                })
            || selection
                .custom_profile_provenance
                .as_ref()
                .is_some_and(|provenance| !provenance.valid())
            || !(1..=8).contains(&selection.concurrency)
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(selection)
    }
}
