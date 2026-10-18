#![cfg_attr(not(test), warn(clippy::too_many_lines))]

mod approval;
mod cancel;
pub(crate) mod catalog;
pub(crate) mod config;
pub(crate) mod definitions;
mod dispatch;
mod executable;
mod fs;
mod git;
mod guard;
mod mcp;
pub mod permissions;
mod process;
mod skills;
pub use definitions::{DefinitionFilePin, DefinitionRegistryPin};
pub(crate) mod toolchains;
pub(crate) mod types;
mod unguarded;
pub(crate) mod unguarded_rules;
pub(crate) mod worker;
mod workspace;

#[cfg(test)]
mod tests;

use crate::session::{AgentRunId, RunId, WorkspaceIdentity};
use crate::store::StateRoot;
pub use approval::{
    ApprovalInbox, ApprovalMode, ApprovalReply, ToolApproval, ToolApprovals, UnguardedPrompt,
};
pub use cancel::Cancellation;
use config::Config;
pub(crate) use skills::parse_json;
pub use skills::{ProjectSkills, project_skills};
pub use workspace::{
    RuntimeProfileOption, TrustRecordError, TrustReset, WorkspacePermissions, mention_paths,
};
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

pub(crate) fn workspace_identity(workspace: &Path) -> Result<WorkspaceIdentity, ToolError> {
    let root = fs::open_directory(workspace)?;
    fs::workspace_identity(&root)
}
use catalog::Site;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
pub use types::{
    Attestation, CacheUpdate, CommandChangeset, EffectIntent, EnforcementCause, FileChange,
    FileVersion, GuardReceipt, MAX_SKILLS, NetworkGrant, ToolCall, ToolContext, ToolDisposition,
    ToolLimits, ToolObservation, ToolPolicyReceipt, ToolRequest, ToolResourceClass,
    ToolResourceProfile, ToolResourceSettings, ToolStep, UnguardedGrant, UnguardedReceipt,
};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tools require a private tools.json; see docs/tools.md")]
    MissingConfiguration,
    #[error("Tool configuration is invalid or unsafe")]
    Configuration,
    #[error("another process is updating private account state; retry when that update finishes")]
    AccountBusy,
    #[error("required native Tool protection is unavailable")]
    ProtectionUnavailable,
    #[error("Guard waiting queue is full; retry when capacity is available")]
    QueueFull,
    #[error("pinned Tool input changed")]
    ChangedInput,
    #[error("Tool path is not admitted")]
    Path,
    #[error("Guard filesystem boundary denied access")]
    FilesystemDenied,
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
pub(crate) struct PermissionSnapshot {
    config: std::sync::Arc<permissions::PermissionConfig>,
    digest: [u8; 32],
}

#[derive(Clone)]
pub(crate) struct ToolRuntime {
    config: Config,
    worker: Option<std::sync::Arc<worker::Action>>,
    handover: Option<std::sync::Arc<[u8]>>,
    pub(crate) definitions: std::sync::Arc<definitions::DefinitionSnapshots>,
    permissions: Option<PermissionSnapshot>,
    unguarded_rules: Option<unguarded_rules::ExactCommandRules>,
    workspace: PathBuf,
    // Set at admission; `None` only in a runtime rebuilt inside a Guard unit, which never reads in process.
    reads: Option<InProcessReads>,
    workspace_identity: WorkspaceIdentity,
    guard_executable: PathBuf,
    enforcement_digest: [u8; 32],
    process_enforcement_digest: [u8; 32],
    context_limit_bytes: usize,
    queue_progress: Option<tokio::sync::watch::Sender<Option<uuid::Uuid>>>,
    queue_wait: Option<std::sync::Arc<std::sync::Mutex<QueueWait>>>,
    active_action_gate: Option<tokio::sync::mpsc::Sender<ActiveActionRequest>>,
    hook_stdin: Option<std::sync::Arc<str>>,
    cache_state: Option<std::sync::Arc<StateRoot>>,
    #[cfg(target_os = "macos")]
    system_programs: Option<std::sync::Arc<guard::SystemPrograms>>,
}

#[derive(Default)]
struct QueueWait {
    waiting: Option<std::time::Instant>,
    elapsed: Duration,
}

pub(crate) struct ActiveActionRequest {
    pub(crate) admitted_at: tokio::time::Instant,
    pub(crate) reply: tokio::sync::oneshot::Sender<Result<ActiveToolWindow, ToolError>>,
}

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

    pub(crate) fn with_queue_accounting(&self) -> Self {
        let mut runtime = self.clone();
        runtime.queue_wait = Some(Default::default());
        runtime
    }

    pub(crate) fn queued_duration(&self) -> Result<Duration, ToolError> {
        let timing = self
            .queue_wait
            .as_ref()
            .ok_or(ToolError::Configuration)?
            .lock()
            .map_err(|_| ToolError::Operation)?;
        Ok(timing.elapsed.saturating_add(
            timing
                .waiting
                .map_or(Duration::ZERO, |started| started.elapsed()),
        ))
    }

    pub(crate) fn with_active_action_gate(
        &self,
        sender: tokio::sync::mpsc::Sender<ActiveActionRequest>,
    ) -> Self {
        let mut runtime = self.clone();
        runtime.active_action_gate = Some(sender);
        runtime
    }

    pub(crate) fn with_hook_stdin(&self, stdin: &str) -> Self {
        let mut runtime = self.clone();
        runtime.hook_stdin = Some(stdin.into());
        runtime
    }

    pub(crate) fn hook_task_fits(&self, intent: &EffectIntent) -> bool {
        guard::task_fits(self, intent)
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
            .send(ActiveActionRequest { admitted_at, reply })
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
        if let Some(timing) = &self.queue_wait
            && let Ok(mut timing) = timing.lock()
        {
            if queued {
                timing.waiting.get_or_insert_with(std::time::Instant::now);
            } else if let Some(started) = timing.waiting.take() {
                timing.elapsed = timing.elapsed.saturating_add(started.elapsed());
            }
        }
        if let Some(sender) = &self.queue_progress {
            sender.send_replace(queued.then_some(id));
        }
    }
    pub(crate) fn workspace_identity(&self) -> WorkspaceIdentity {
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

    pub(crate) fn result_capacity(&self, overhead: usize, reserved: usize) -> Option<usize> {
        let capacity = self
            .context_limit_bytes
            .saturating_sub(overhead.saturating_add(512 + reserved))
            / 6;
        (capacity >= 128).then_some(capacity)
    }
    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        match call {
            ToolCall::PrepareWorker { .. } => self.worker_policy().is_some(),
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::WorkerHandover { .. },
            } => self.config.write,
            _ => self.config.allows(call),
        }
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
            self.workspace_identity.device,
            self.workspace_identity.inode,
        )
        .map_err(|_| ToolError::Operation)?;
        match state.effective_build_cache_bytes(&identity, requested) {
            Ok(limits) => Ok(limits),
            Err(crate::store::StoreError::StorageFull) => Ok((0, 0)),
            Err(_) => Err(ToolError::Operation),
        }
    }

    pub(crate) fn load_permissions(
        state: &StateRoot,
    ) -> Result<Option<PermissionSnapshot>, ToolError> {
        state
            .record(crate::store::RecordKind::Permissions)
            .read()
            .map_err(|_| ToolError::Configuration)?
            .map(|bytes| {
                permissions::PermissionConfig::parse(&bytes)
                    .map(|config| PermissionSnapshot {
                        config: std::sync::Arc::new(config),
                        digest: types::digest(&bytes),
                    })
                    .map_err(|_| ToolError::Configuration)
            })
            .transpose()
    }

    pub(crate) fn readmit_compaction_hooks(
        state: &StateRoot,
        workspace: &Path,
        guard: Option<PathBuf>,
        pin: &ToolPolicyReceipt,
    ) -> Result<Option<Self>, ToolError> {
        let Some(digest) = pin.permissions_digest else {
            return Ok(None);
        };
        let snapshot = Self::load_permissions(state)?.ok_or(ToolError::ChangedInput)?;
        if snapshot.digest != digest {
            return Err(ToolError::ChangedInput);
        }
        if snapshot
            .config
            .hooks()
            .groups(permissions::HookEvent::PreCompact)
            .is_empty()
        {
            return Ok(None);
        }
        let mut config = match Config::load(state) {
            Ok(mut config) => {
                config.discover_skills(workspace)?;
                if config.digest() == pin.config_digest {
                    config
                } else {
                    WorkspacePermissions::open(workspace)
                        .map_err(|_| ToolError::ChangedInput)?
                        .config(workspace)?
                }
            }
            Err(ToolError::MissingConfiguration) => WorkspacePermissions::open(workspace)
                .map_err(|_| ToolError::ChangedInput)?
                .config(workspace)?,
            Err(error) => return Err(error),
        };
        config.discover_skills(workspace)?;
        if config.digest() != pin.config_digest {
            return Err(ToolError::ChangedInput);
        }
        let runtime = Self::admit(
            config,
            Some(snapshot),
            Self::load_exact_command_rules(state)?,
            workspace,
            state,
            guard.ok_or(ToolError::ProtectionUnavailable)?,
        )?;
        if runtime.receipt().config_digest != pin.config_digest {
            return Err(ToolError::ChangedInput);
        }
        Ok(Some(runtime))
    }

    pub(crate) fn load_exact_command_rules(
        state: &StateRoot,
    ) -> Result<Option<unguarded_rules::ExactCommandRules>, ToolError> {
        unguarded_rules::ExactCommandRules::load(state)
    }

    pub(crate) fn admit(
        mut config: Config,
        permissions: Option<PermissionSnapshot>,
        unguarded_rules: Option<unguarded_rules::ExactCommandRules>,
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
        let workspace_identity = fs::workspace_identity(&root)?;
        config.discover_skills(workspace)?;
        let definitions = definitions::snapshot(&root, state, &config)?;
        if fs::workspace_identity(&fs::open_directory(workspace)?)? != workspace_identity {
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
            definitions: std::sync::Arc::new(definitions),
            config,
            worker: None,
            handover: None,
            permissions,
            unguarded_rules,
            queue_progress: None,
            queue_wait: None,
            active_action_gate: None,
            hook_stdin: None,
            cache_state: Some(std::sync::Arc::new(
                state
                    .try_clone()
                    .map_err(|_| ToolError::ProtectionUnavailable)?,
            )),
            workspace: workspace.to_owned(),
            reads: Some(InProcessReads {
                root: std::sync::Arc::new(root),
                slots: std::sync::Arc::clone(&IN_PROCESS_READS),
            }),
            workspace_identity,
            guard_executable,
            enforcement_digest,
            process_enforcement_digest,
            context_limit_bytes: types::MAX_TOOL_CONTEXT_BYTES,
            #[cfg(target_os = "macos")]
            system_programs,
        })
    }

    pub(crate) fn stop_feedback_reserve(&self) -> usize {
        if self.permissions().is_some_and(|config| {
            !config
                .hooks()
                .groups(permissions::HookEvent::Stop)
                .is_empty()
        }) {
            2 * permissions::MAX_STOP_FEEDBACK_BYTES + 256
        } else {
            0
        }
    }

    pub(crate) fn receipt(&self) -> ToolPolicyReceipt {
        let mut receipt = self.config.receipt();
        receipt.max_context_bytes = self.context_limit_bytes as u32;
        receipt.permissions_digest = self.permissions.as_ref().map(|snapshot| snapshot.digest);
        receipt.unguarded_rules_digest = self.exact_command_rules().map(|rules| rules.digest);
        receipt
    }

    pub(crate) fn permissions(&self) -> Option<&permissions::PermissionConfig> {
        self.permissions
            .as_ref()
            .map(|snapshot| snapshot.config.as_ref())
    }

    fn exact_command_rules(&self) -> Option<&unguarded_rules::ExactCommandRules> {
        self.unguarded_rules.as_ref()
    }

    pub(crate) fn command_executable(&self, name: &str) -> Option<&str> {
        self.config
            .commands
            .iter()
            .find(|program| program.name == name)
            .map(|program| program.executable.as_str())
    }

    pub(crate) fn matches_exact_command_rule(&self, call: &ToolCall) -> bool {
        self.exact_command_rules()
            .is_some_and(|rules| rules.matches(call, self.workspace_identity, &self.workspace))
    }

    pub(crate) fn record_exact_command_rule(&self, call: &ToolCall) -> Result<(), ToolError> {
        fs::validate_workspace(self)?;
        let state = self
            .cache_state
            .as_deref()
            .ok_or(ToolError::Configuration)?;
        unguarded_rules::ExactCommandRules::record(
            state,
            &self.workspace,
            self.workspace_identity,
            call,
        )
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
            stop_bytes: 0,
            catalog,
            history_summary: None,
            steps: Vec::new(),
        })
    }

    pub(crate) fn worker_policy(&self) -> Option<crate::session::worker::WorkerPolicy> {
        use crate::session::worker::*;
        (self.config.write && self.definitions.pin().is_some())
            .then(|| WorkerPolicy {
                parent_grant: WorkerGrant {
                    read_paths: self.config.workspace_paths.clone(),
                    write_paths: self.config.workspace_paths.clone(),
                    tools: vec![
                        WorkerTool::List,
                        WorkerTool::Read,
                        WorkerTool::Search,
                        WorkerTool::Mkdir,
                        WorkerTool::Write,
                        WorkerTool::Edit,
                        WorkerTool::Skill,
                        WorkerTool::McpList,
                    ],
                    skills: self.config.skills.iter().map(|s| s.name.clone()).collect(),
                    mcp_servers: self.config.mcp.iter().map(|s| s.name.clone()).collect(),
                    permission_mode: self.permissions().map(permissions::PermissionConfig::mode),
                },
                concurrency: 4,
                max_turns: 17,
                max_tools: 16,
                data_bytes: WORKER_DATA_BYTES as u32,
            })
            .filter(WorkerPolicy::valid)
    }

    pub(crate) fn with_handover(&self, bytes: std::sync::Arc<[u8]>) -> Self {
        let mut runtime = self.clone();
        runtime.handover = Some(bytes);
        runtime
    }

    pub(crate) fn editor_context(
        &self,
        grant: &crate::session::worker::WorkerGrant,
        remaining: usize,
    ) -> Result<ToolContext, ToolError> {
        let mut context = self.research_context(remaining)?;
        let mut catalog: serde_json::Value =
            serde_json::from_str(&context.catalog).map_err(|_| ToolError::Configuration)?;
        catalog["workspace_paths"] = serde_json::json!(grant.read_paths);
        catalog["write"] = serde_json::json!(true);
        catalog["skills"] = serde_json::json!([]);
        catalog["mcp_servers"] = serde_json::json!([]);
        catalog["file_tools"] = serde_json::json!(catalog::FILE_TOOLS);
        catalog["operations"] = serde_json::json!(
            grant
                .tools
                .iter()
                .map(|tool| tool.label())
                .collect::<Vec<_>>()
        );
        context.catalog = catalog.to_string();
        Ok(context)
    }

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
            stop_bytes: 0,
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
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id,
            agent_run_id,
            policy_digest: self.config.digest(),
            enforcement_digest,
            workspace_device: self.workspace_identity.device,
            workspace_inode: self.workspace_identity.inode,
            call,
            limits: ToolLimits::default(),
            expires_at_ms: now_ms().saturating_add(60_000),
            use_count: 1,
        }
    }

    pub(crate) fn reservation_overhead(&self, call: &ToolCall) -> Duration {
        match call.facts().site {
            Site::InProcess | Site::Unguarded => Duration::ZERO,
            Site::Guard => guard::RESERVATION_OVERHEAD,
        }
    }

    pub(crate) async fn apply_worker_handover(
        &self,
        intent: EffectIntent,
        bytes: std::sync::Arc<[u8]>,
        cancellation: Cancellation,
    ) -> ToolObservation {
        if let Err(error) = worker::decode_handover(&intent, &bytes, &self.config) {
            return unstarted_observation(intent, error);
        }
        let mut runtime = self.clone();
        runtime.handover = Some(bytes);
        match guard::execute(&runtime, &intent, cancellation).await {
            Ok(observation) => observation,
            Err(error) => uncertain_observation(intent, error, CacheUpdate::NotApplicable),
        }
    }

    pub(crate) async fn execute(
        &self,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> ToolObservation {
        if matches!(
            intent.call,
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::WorkerHandover { .. }
            }
        ) && let Some(bytes) = &self.handover
        {
            return self
                .apply_worker_handover(intent, bytes.clone(), cancellation)
                .await;
        }
        if intent.target != crate::session::worker::EffectTarget::Workspace
            || !self.config.allows(&intent.call)
        {
            return ToolObservation {
                enforcement_cause: None,
                intent,
                disposition: ToolDisposition::Denied,
                output: "Tool operation is outside the pinned grant".into(),
                attestation: None,
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            };
        }
        match intent.call.facts().site {
            Site::InProcess => self.run_in_process(intent, cancellation).await,
            Site::Unguarded => self.run_unguarded(intent, cancellation).await,
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

    async fn run_unguarded(
        &self,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> ToolObservation {
        let ToolCall::CommandUnguarded {
            program, args, cwd, ..
        } = &intent.call
        else {
            return unstarted_observation(intent, ToolError::Configuration);
        };
        if let Err(error) = self.pinned_root(&intent) {
            return unstarted_observation(intent, error);
        }
        let Some(pinned) = self.config.commands.iter().find(|row| &row.name == program) else {
            return unstarted_observation(intent, ToolError::Configuration);
        };
        let outcome = unguarded::run(
            unguarded::Request {
                program: pinned,
                args,
                cwd,
                workspace: &self.workspace,
                workspace_identity: self.workspace_identity,
                runtime_grants: &self.config.runtime_grants,
                result_bytes: intent.limits.result_bytes,
                runtime_ms: intent.limits.runtime_ms,
            },
            cancellation,
        )
        .await;
        let attestation = outcome.spawned.then(|| {
            Attestation::Unguarded(types::UnguardedReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                executable_sha256: pinned.sha256.clone(),
                pid: outcome.pid,
                exit_code: outcome.exit_code,
                descendants_confirmed_stopped: outcome.descendants_stopped,
            })
        });
        let mut observation = ToolObservation {
            enforcement_cause: None,
            attestation,
            intent,
            disposition: outcome.disposition,
            output: outcome.output,
            cache_update: CacheUpdate::NotApplicable,
        };
        if !observation.valid() {
            observation.disposition = ToolDisposition::Limit;
            observation.output = "Tool result exceeded its reserved serialized capacity".into();
        }
        observation
    }

    fn pinned_root(
        &self,
        intent: &EffectIntent,
    ) -> Result<std::sync::Arc<cap_std::fs::Dir>, ToolError> {
        let root = self
            .reads
            .as_ref()
            .map(|reads| std::sync::Arc::clone(&reads.root))
            .ok_or(ToolError::ProtectionUnavailable)?;
        if intent.target != crate::session::worker::EffectTarget::Workspace
            || !intent.valid()
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
        // The owned permit moves into the blocking task, so a timed-out read keeps its slot.
        let Some(Ok(permit)) = self
            .reads
            .as_ref()
            .map(|reads| std::sync::Arc::clone(&reads.slots).try_acquire_owned())
        else {
            return unattested_limit(intent, "In-process read capacity is full; nothing was read");
        };
        let (runtime, read_intent) = (self.clone(), intent.clone());
        let read = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let root = runtime.pinned_root(&read_intent)?;
            Ok(fs::inspect(
                &root,
                &read_intent.call,
                &runtime.config,
                read_intent.limits.result_bytes as usize,
            ))
        });
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return cancelled_read(intent),
            joined = tokio::time::timeout_at(deadline, read) => match joined {
                Ok(Ok(Ok(result))) => result,
                Ok(Ok(Err(error))) => return unstarted_observation(intent, error),
                Ok(Err(_)) => Err(ToolError::Operation),
                Err(_) => return unattested_limit(intent, "Tool result or resource limit reached"),
            },
        };
        let (disposition, output, enforcement_cause) = dispatch::outcome(&intent, result);
        let mut observation = ToolObservation {
            enforcement_cause,
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
            observation.enforcement_cause = None;
            observation.output = "Tool result exceeded its reserved serialized capacity".into();
        }
        observation
    }
}

#[derive(Clone)]
struct InProcessReads {
    root: std::sync::Arc<cap_std::fs::Dir>,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
}

const MAX_IN_PROCESS_READS: usize = 16;
static IN_PROCESS_READS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| {
        std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_IN_PROCESS_READS))
    });

fn unattested_limit(intent: EffectIntent, output: &str) -> ToolObservation {
    ToolObservation {
        enforcement_cause: None,
        intent,
        disposition: ToolDisposition::Limit,
        output: output.into(),
        attestation: None,
        cache_update: CacheUpdate::NotApplicable,
    }
}

fn cancelled_read(intent: EffectIntent) -> ToolObservation {
    ToolObservation {
        enforcement_cause: None,
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
        ToolError::ChangedInput | ToolError::Path | ToolError::FilesystemDenied => "pinned input",
        ToolError::ProtectionUnavailable => "native protection",
        ToolError::Limit | ToolError::QueueFull => "resource limit",
        _ => "execution",
    }
}

pub(crate) fn failure_check(error: &ToolError) -> Option<&'static str> {
    match error {
        ToolError::GuardBootstrap(failure) => failure.check(),
        _ => None,
    }
}

pub(crate) fn failure_detail(error: &ToolError) -> Option<&str> {
    match error {
        ToolError::GuardBootstrap(failure) => failure.detail(),
        _ => None,
    }
}

fn record_failure(site: &'static str, intent: &EffectIntent, error: &ToolError) {
    crate::diagnostics::tool_failure(site, error);
    #[cfg(any(test, feature = "test-support"))]
    crate::test_support::record_tool_failure(
        intent.id,
        site,
        failure_stage(error),
        failure_check(error),
        failure_detail(error).map(str::to_owned),
    );
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = intent;
}

fn unstarted_observation(intent: EffectIntent, error: ToolError) -> ToolObservation {
    let enforcement_cause =
        matches!(&error, ToolError::GuardBootstrap(_)).then_some(EnforcementCause::GuardBootstrap);
    let site = match intent.call.facts().site {
        Site::InProcess => "Read",
        Site::Guard => "Guard",
        Site::Unguarded => "Unguarded",
    };
    record_failure(site, &intent, &error);
    ToolObservation {
        enforcement_cause,
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
    record_failure("Guard", &intent, &error);
    let stage = failure_stage(&error);
    ToolObservation {
        enforcement_cause: None,
        intent,
        disposition: ToolDisposition::Uncertain,
        output: format!(
            "Guard {stage} unconfirmed; effects may have occurred. Do not retry automatically."
        ),
        attestation: None,
        cache_update,
    }
}

pub fn now_ms() -> u64 {
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
