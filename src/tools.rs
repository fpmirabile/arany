mod approval;
mod cancel;
pub(crate) mod catalog;
pub(crate) mod config;
mod dispatch;
mod fs;
mod git;
mod guard;
mod mcp;
mod process;
mod skills;
pub(crate) mod toolchains;
pub(crate) mod types;
mod workspace;

#[cfg(test)]
mod tests;

use crate::session::{AgentRunId, RunId};
use crate::store::StateRoot;
pub use approval::{ApprovalInbox, ApprovalMode, ToolApproval, ToolApprovals};
pub use cancel::Cancellation;
use config::Config;
pub(crate) use skills::parse_json;
pub use skills::{ProjectSkills, project_skills};
pub use workspace::{RuntimeProfileOption, WorkspacePermissions, mention_paths};
/// Whether this OS implements native Guard protection for typed file Tools.
pub const fn native_protection_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

pub fn configured_skill_names(state: &StateRoot) -> Result<Vec<String>, ToolError> {
    Ok(Config::load(state)?
        .skills
        .into_iter()
        .map(|skill| skill.name)
        .collect())
}

pub fn runtime_skill_names(state: &StateRoot, workspace: &Path) -> Result<Vec<String>, ToolError> {
    let config = Config::load(state)?;
    let project = if config.permits_project_skills() {
        project_skills(workspace)?.names
    } else {
        Vec::new()
    };
    let mut names = config
        .skills
        .into_iter()
        .map(|skill| skill.name)
        .collect::<Vec<_>>();
    for name in project {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.len() > MAX_SKILLS {
        return Err(ToolError::Limit);
    }
    Ok(names)
}

pub fn list_workspace_entries(
    workspace: &Path,
    folder: &str,
    prefix: &str,
) -> Result<Vec<String>, ToolError> {
    fs::workspace_entries(workspace, folder, prefix)
}

pub(crate) fn workspace_identity(workspace: &Path) -> Result<(u64, u64), ToolError> {
    let root = fs::open_directory(workspace)?;
    fs::identity(&root)
}
use catalog::Site;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
pub use types::{
    Attestation, CacheUpdate, CommandChangeset, EffectIntent, FileChange, FileVersion,
    GuardReceipt, MAX_SKILLS, NetworkGrant, ToolCall, ToolContext, ToolDisposition, ToolLimits,
    ToolObservation, ToolPolicyReceipt, ToolRequest, ToolResourceClass, ToolResourceProfile,
    ToolResourceSettings, ToolStep,
};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tools require a private tools.json; see docs/tools.md")]
    MissingConfiguration,
    #[error("Tool configuration is invalid or unsafe")]
    Configuration,
    #[error("required native Tool protection is unavailable")]
    ProtectionUnavailable,
    #[error("Guard waiting queue is full; retry when capacity is available")]
    QueueFull,
    #[error("pinned Tool input changed")]
    ChangedInput,
    #[error("Tool path is not admitted")]
    Path,
    #[error("Tool resource limit reached")]
    Limit,
    #[error("Tool file precondition changed")]
    Conflict,
    #[error("Tool operation failed")]
    Operation,
    #[error("Tool effect completion is uncertain")]
    Uncertain,
    #[error("Guard rejected its {0} stage")]
    GuardRejected(guard::GuardStage),
    #[error("Guard rejected at {0}")]
    GuardBootstrap(guard::BootstrapFailure),
}

#[derive(Clone)]
pub(crate) struct ToolRuntime {
    config: Config,
    workspace: PathBuf,
    // Opened once at admission; `None` only in a runtime rebuilt inside a Guard unit, which never reads in process.
    workspace_root: Option<std::sync::Arc<cap_std::fs::Dir>>,
    workspace_identity: (u64, u64),
    guard_executable: PathBuf,
    enforcement_digest: [u8; 32],
    process_enforcement_digest: [u8; 32],
    context_limit_bytes: usize,
    queue_progress: Option<tokio::sync::watch::Sender<Option<uuid::Uuid>>>,
    active_action_gate: Option<tokio::sync::mpsc::Sender<ActiveActionRequest>>,
    cache_state: Option<std::sync::Arc<StateRoot>>,
    #[cfg(target_os = "macos")]
    system_programs: Option<std::sync::Arc<guard::SystemPrograms>>,
}

pub(crate) type ActiveActionRequest = (
    tokio::time::Instant,
    tokio::sync::oneshot::Sender<Result<ActiveToolWindow, ToolError>>,
);

#[derive(Clone, Copy)]
pub(crate) struct ActiveToolWindow {
    pub(crate) preparation_deadline: tokio::time::Instant,
    pub(crate) go_deadline: tokio::time::Instant,
    pub(crate) payload_deadline: tokio::time::Instant,
    pub(crate) completion_deadline: tokio::time::Instant,
}

impl ToolRuntime {
    pub(crate) fn with_queue_progress(
        &self,
        sender: Option<tokio::sync::watch::Sender<Option<uuid::Uuid>>>,
    ) -> Self {
        let mut runtime = self.clone();
        runtime.queue_progress = sender;
        runtime
    }

    pub(crate) fn with_active_action_gate(
        &self,
        sender: tokio::sync::mpsc::Sender<ActiveActionRequest>,
    ) -> Self {
        let mut runtime = self.clone();
        runtime.active_action_gate = Some(sender);
        runtime
    }

    pub(crate) async fn begin_active_action(
        &self,
        admitted_at: tokio::time::Instant,
    ) -> Result<Option<ActiveToolWindow>, ToolError> {
        let Some(sender) = &self.active_action_gate else {
            return Ok(None);
        };
        let (reply, receipt) = tokio::sync::oneshot::channel();
        sender
            .send((admitted_at, reply))
            .await
            .map_err(|_| ToolError::Operation)?;
        receipt
            .await
            .map_err(|_| ToolError::Operation)
            .and_then(|result| result.map(Some))
    }

    pub(crate) fn before_dispatch(
        &self,
        window: Option<ActiveToolWindow>,
        runtime_ms: u32,
    ) -> Result<(), ToolError> {
        if window.is_some_and(|window| {
            let now = tokio::time::Instant::now();
            now >= window.go_deadline
                || now
                    .checked_add(std::time::Duration::from_millis(u64::from(runtime_ms)))
                    .is_none_or(|payload_end| payload_end > window.payload_deadline)
        }) {
            return Err(ToolError::Limit);
        }
        Ok(())
    }

    pub(crate) fn queue_changed(&self, id: uuid::Uuid, queued: bool) {
        if let Some(sender) = &self.queue_progress {
            sender.send_replace(queued.then_some(id));
        }
    }
    pub(crate) fn workspace_identity(&self) -> (u64, u64) {
        self.workspace_identity
    }

    pub(crate) fn set_context_limit(&mut self, limit: usize) -> Result<(), ToolError> {
        if !(types::MIN_TOOL_CONTEXT_BYTES..=types::MAX_TOOL_CONTEXT_BYTES).contains(&limit) {
            return Err(ToolError::Limit);
        }
        self.context_limit_bytes = limit;
        Ok(())
    }

    pub(crate) fn context_limit_bytes(&self) -> usize {
        self.context_limit_bytes
    }

    /// Result bytes one Tool may return after its serialized `overhead` and `reserved` extra bytes;
    /// `None` when fewer than 128 bytes remain.
    pub(crate) fn result_capacity(&self, overhead: usize, reserved: usize) -> Option<usize> {
        let capacity = self
            .context_limit_bytes
            .saturating_sub(overhead.saturating_add(512 + reserved))
            / 6;
        (capacity >= 128).then_some(capacity)
    }
    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        self.config.allows(call)
    }
    pub(crate) fn allows_goal_contract(&self, contract: &crate::session::GoalPlanContract) -> bool {
        let criterion_allowed = match &contract.success_criterion {
            crate::session::GoalSuccessCriterion::ToolCallSucceeded { call } => {
                self.config.allows(call)
            }
            crate::session::GoalSuccessCriterion::WorkspaceFileDigest { path, .. }
            | crate::session::GoalSuccessCriterion::WorkspaceFileContent { path, .. } => {
                self.config.in_workspace_scope(path)
            }
        };
        criterion_allowed
            && contract
                .paths
                .iter()
                .all(|path| self.config.in_workspace_scope(path))
            && contract.commands.iter().all(|command| {
                self.config
                    .commands
                    .iter()
                    .any(|program| program.name == *command)
            })
            && contract.skills.iter().all(|binding| {
                self.config.allows(&ToolCall::Skill {
                    name: binding.name.clone(),
                    resource: binding.resource.clone(),
                })
            })
            && contract.mcp_tools.iter().all(|binding| {
                self.config.mcp.iter().any(|server| {
                    server.name == binding.server && server.tools.contains(&binding.tool)
                })
            })
    }

    pub(crate) fn effective_cache_bytes(&self, requested: u64) -> Result<(u64, u64), ToolError> {
        if requested == 0 {
            return Ok((0, 0));
        }
        let Some(state) = self.cache_state.as_ref() else {
            return Ok((0, 0));
        };
        let identity = crate::store::BuildCacheIdentity::new(
            &self.workspace,
            self.workspace_identity.0,
            self.workspace_identity.1,
        )
        .map_err(|_| ToolError::Operation)?;
        match state.effective_build_cache_bytes(&identity, requested) {
            Ok(limits) => Ok(limits),
            Err(crate::store::StoreError::StorageFull) => Ok((0, 0)),
            Err(_) => Err(ToolError::Operation),
        }
    }

    pub(crate) fn admit(
        mut config: Config,
        workspace: &Path,
        state: &StateRoot,
        guard_executable: PathBuf,
    ) -> Result<Self, ToolError> {
        #[cfg(target_os = "macos")]
        let capture = guard::FixtureSpan::begin(guard::FixtureStage::ToolAdmission);
        guard::check_available()?;
        let enforcement_digest = guard::enforcement_digest(&[])?;
        let root = fs::open_directory(workspace)?;
        state
            .ensure_outside_workspace(&root)
            .map_err(|_| ToolError::Path)?;
        fs::reject_overlap(workspace, state.path())?;
        let account = StateRoot::account_path().map_err(|_| ToolError::Path)?;
        fs::reject_overlap(workspace, &account)?;
        for protected in [workspace, state.path(), account.as_path()] {
            for runtime in guard::runtime_roots() {
                fs::reject_overlap(Path::new(runtime), protected)?;
            }
        }
        for grant in &config.runtime_grants {
            grant.reject_overlap(&[workspace, state.path(), account.as_path()])?;
        }
        let workspace_identity = fs::identity(&root)?;
        config.discover_skills(workspace)?;
        if fs::identity(&fs::open_directory(workspace)?)? != workspace_identity {
            return Err(ToolError::ChangedInput);
        }
        for program in config
            .commands
            .iter()
            .chain(config.mcp.iter().map(|server| &server.program))
        {
            program.verify()?;
        }
        if let Some(git) = &config.git {
            git::verify_host_executable(git)?;
        }
        for path in config.resource_paths() {
            fs::reject_overlap(Path::new(path), state.path())?;
            fs::reject_overlap(Path::new(path), &account)?;
        }
        #[cfg(target_os = "macos")]
        let system_programs = if config.commands.is_empty() && config.mcp.is_empty() {
            None
        } else {
            Some(std::sync::Arc::new(guard::SystemPrograms::admit()?))
        };
        #[cfg(target_os = "macos")]
        let process_enforcement_digest = guard::macos_runtime_enforcement_digest(
            system_programs.as_deref(),
            &config.runtime_grants,
        )?;
        #[cfg(not(target_os = "macos"))]
        let process_enforcement_digest = guard::enforcement_digest(&config.runtime_grants)?;
        #[cfg(target_os = "macos")]
        capture.complete();
        Ok(Self {
            config,
            queue_progress: None,
            active_action_gate: None,
            cache_state: Some(std::sync::Arc::new(
                state
                    .try_clone()
                    .map_err(|_| ToolError::ProtectionUnavailable)?,
            )),
            workspace: workspace.to_owned(),
            workspace_root: Some(std::sync::Arc::new(root)),
            workspace_identity,
            guard_executable,
            enforcement_digest,
            process_enforcement_digest,
            context_limit_bytes: types::MAX_TOOL_CONTEXT_BYTES,
            #[cfg(target_os = "macos")]
            system_programs,
        })
    }

    pub(crate) fn receipt(&self) -> ToolPolicyReceipt {
        let mut receipt = self.config.receipt();
        receipt.max_context_bytes = self.context_limit_bytes as u32;
        receipt
    }

    fn skill_rows(&self) -> Vec<serde_json::Value> {
        self.config.skills.iter().map(|skill| serde_json::json!({"name":skill.name,"description":skill.description.chars().take(96).collect::<String>()})).collect()
    }

    pub(crate) fn context(&self) -> Result<ToolContext, ToolError> {
        let catalog = serde_json::json!({
            "host_clock": {"unix_ms": now_ms(), "timezone":"UTC"},
            "workspace_paths": self.config.workspace_paths,
            "write": self.config.write,
            "file_tools": catalog::FILE_TOOLS,
            "commands": self.config.commands.iter().map(|program| serde_json::json!({"name":program.name,"interpreter":program.interpreter,"inputs":program.inputs.iter().map(|input| format!("/inputs/{}/{}", program.name, input.destination)).collect::<Vec<_>>()})).collect::<Vec<_>>(),
            "command_contract": catalog::COMMAND_CONTRACT,
            "skills": self.skill_rows(),
            "mcp_servers": self.config.mcp.iter().map(|server| serde_json::json!({"name":server.name,"tools":server.tools,"protocol":catalog::MCP_PROTOCOL})).collect::<Vec<_>>(),
            "git": if self.config.git_available() {
                serde_json::json!({"operations":catalog::git_operations(),"scope":catalog::GIT_SCOPE})
            } else {
                serde_json::json!("unavailable")
            },
        }).to_string();
        if catalog.len() > types::MAX_TOOL_CATALOG_BYTES {
            return Err(ToolError::Limit);
        }
        Ok(ToolContext {
            catalog,
            history_summary: None,
            steps: Vec::new(),
        })
    }

    /// Catalog for a read-only researcher child; `tool_calls_remaining == 0` offers only Finish.
    pub(crate) fn research_context(
        &self,
        tool_calls_remaining: usize,
    ) -> Result<ToolContext, ToolError> {
        let catalog = serde_json::json!({
            "host_clock": {"unix_ms": now_ms(), "timezone": "UTC"},
            "workspace_paths": self.config.workspace_paths,
            "write": false,
            "file_tools": catalog::RESEARCH_FILE_TOOLS,
            "commands": [],
            "skills": self.skill_rows(),
            "mcp_servers": self.config.mcp.iter().map(|server| serde_json::json!({"name":server.name,"tools":server.tools,"protocol":catalog::RESEARCH_MCP_PROTOCOL})).collect::<Vec<_>>(),
            "tool_calls_remaining": tool_calls_remaining,
        })
        .to_string();
        if catalog.len() > types::MAX_TOOL_CATALOG_BYTES {
            return Err(ToolError::Limit);
        }
        Ok(ToolContext {
            catalog,
            history_summary: None,
            steps: Vec::new(),
        })
    }

    pub(crate) fn intent(
        &self,
        run_id: RunId,
        agent_run_id: AgentRunId,
        call: ToolCall,
    ) -> EffectIntent {
        let enforcement_digest = if call.is_process() {
            self.process_enforcement_digest
        } else {
            self.enforcement_digest
        };
        EffectIntent {
            id: uuid::Uuid::now_v7(),
            run_id,
            agent_run_id,
            policy_digest: self.config.digest(),
            enforcement_digest,
            workspace_device: self.workspace_identity.0,
            workspace_inode: self.workspace_identity.1,
            call,
            limits: ToolLimits::default(),
            expires_at_ms: now_ms().saturating_add(60_000),
            use_count: 1,
        }
    }

    /// Native preparation and handshake time the Engine adds to a Tool's action deadline.
    pub(crate) fn reservation_overhead(&self, call: &ToolCall) -> Duration {
        match call.facts().site {
            Site::InProcess => Duration::ZERO,
            Site::Guard => guard::RESERVATION_OVERHEAD,
        }
    }

    pub(crate) async fn execute(
        &self,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> ToolObservation {
        if !self.config.allows(&intent.call) {
            return ToolObservation {
                intent,
                disposition: ToolDisposition::Denied,
                output: "Tool operation is outside the pinned grant".into(),
                attestation: None,
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            };
        }
        match intent.call.facts().site {
            Site::InProcess => self.run_in_process(intent, cancellation).await,
            Site::Guard => match guard::execute(self, &intent, cancellation).await {
                Ok(observation) => observation,
                Err(error) => {
                    let cache_update =
                        guard_error_cache_update(&intent.call, self.cache_state.is_some());
                    uncertain_observation(intent, error, cache_update)
                }
            },
        }
    }

    /// The admitted Workspace handle, after the checks the Guard helper applies before Ready.
    fn pinned_root(
        &self,
        intent: &EffectIntent,
    ) -> Result<std::sync::Arc<cap_std::fs::Dir>, ToolError> {
        let root = self
            .workspace_root
            .clone()
            .ok_or(ToolError::ProtectionUnavailable)?;
        if !intent.valid()
            || intent.policy_digest != self.config.digest()
            || intent.expires_at_ms <= now_ms()
        {
            return Err(ToolError::Configuration);
        }
        if fs::identity(&root)? != (intent.workspace_device, intent.workspace_inode) {
            return Err(ToolError::ChangedInput);
        }
        fs::validate_workspace(self)?;
        Ok(root)
    }

    /// Runs a read-only operation on the Workspace handle pinned at admission, with the Guard's
    /// intent, expiry and device/inode checks; nothing it reads can change the Workspace.
    async fn run_in_process(
        &self,
        intent: EffectIntent,
        mut cancellation: Cancellation,
    ) -> ToolObservation {
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(intent.limits.runtime_ms));
        if cancellation.is_cancelled() {
            return cancelled_read(intent);
        }
        let root = match self.pinned_root(&intent) {
            Ok(root) => root,
            Err(error) => return unstarted_observation(intent, error),
        };
        let (call, config) = (intent.call.clone(), self.config.clone());
        let result_bytes = intent.limits.result_bytes as usize;
        let read =
            tokio::task::spawn_blocking(move || fs::inspect(&root, &call, &config, result_bytes));
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return cancelled_read(intent),
            joined = tokio::time::timeout_at(deadline, read) => match joined {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(ToolError::Operation),
                Err(_) => {
                    return ToolObservation {
                        intent,
                        disposition: ToolDisposition::Limit,
                        output: "Tool result or resource limit reached".into(),
                        attestation: None,
                        cache_update: CacheUpdate::NotApplicable,
                    };
                }
            },
        };
        let (disposition, output) = dispatch::outcome(&intent, result);
        let mut observation = ToolObservation {
            attestation: Some(Attestation::InProcess {
                policy_digest: intent.policy_digest,
                workspace_device: intent.workspace_device,
                workspace_inode: intent.workspace_inode,
            }),
            intent,
            disposition,
            output,
            cache_update: CacheUpdate::NotApplicable,
        };
        if !observation.valid() {
            observation.disposition = ToolDisposition::Limit;
            observation.output = "Tool result exceeded its reserved serialized capacity".into();
        }
        observation
    }
}

fn cancelled_read(intent: EffectIntent) -> ToolObservation {
    ToolObservation {
        intent,
        disposition: ToolDisposition::Cancelled,
        output: "Read cancelled; nothing in the Workspace changed".into(),
        attestation: None,
        cache_update: CacheUpdate::NotApplicable,
    }
}

fn guard_error_cache_update(call: &ToolCall, cache_selected: bool) -> CacheUpdate {
    if cache_selected && matches!(call, ToolCall::Command { .. }) {
        CacheUpdate::NotAttempted
    } else {
        CacheUpdate::NotApplicable
    }
}

pub(crate) fn failure_stage(error: &ToolError) -> &'static str {
    match error {
        ToolError::GuardBootstrap(failure) => failure.failure_stage(),
        ToolError::GuardRejected(stage) => stage.failure_stage(),
        ToolError::Configuration => "admission",
        ToolError::ChangedInput | ToolError::Path => "pinned input",
        ToolError::ProtectionUnavailable => "native protection",
        ToolError::Limit | ToolError::QueueFull => "resource limit",
        _ => "execution",
    }
}

fn unstarted_observation(intent: EffectIntent, error: ToolError) -> ToolObservation {
    crate::diagnostics::guard_failure(&error);
    let site = match intent.call.facts().site {
        Site::InProcess => "Tool",
        Site::Guard => "Guard",
    };
    ToolObservation {
        output: match error {
            ToolError::QueueFull => format!("{error}. No operation was dispatched."),
            _ => format!(
                "{site} {} unavailable before dispatch; no operation was dispatched.",
                failure_stage(&error)
            ),
        },
        intent,
        disposition: ToolDisposition::Failed,
        attestation: None,
        cache_update: crate::tools::CacheUpdate::NotApplicable,
    }
}

fn uncertain_observation(
    intent: EffectIntent,
    error: ToolError,
    cache_update: crate::tools::CacheUpdate,
) -> ToolObservation {
    crate::diagnostics::guard_failure(&error);
    let stage = failure_stage(&error);
    ToolObservation {
        intent,
        disposition: ToolDisposition::Uncertain,
        output: format!(
            "Guard {stage} unconfirmed; effects may have occurred. Do not retry automatically."
        ),
        attestation: None,
        cache_update,
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn tool_guard_main() -> std::process::ExitCode {
    guard::helper_main()
}

#[cfg(target_os = "macos")]
pub fn tool_guard_unit_main() -> std::process::ExitCode {
    guard::unit_main()
}
