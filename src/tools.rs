mod approval;
pub(crate) mod config;
mod fs;
mod guard;
mod mcp;
mod skills;
pub(crate) mod toolchains;
pub(crate) mod types;
mod workspace;

#[cfg(test)]
mod tests;

use crate::engine::RunCancellation;
use crate::session::{AgentRunId, RunId};
use crate::store::StateRoot;
pub use approval::{ApprovalInbox, ApprovalMode, ToolApproval, ToolApprovals};
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
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
pub use types::{
    CacheUpdate, CommandChangeset, EffectIntent, FileChange, FileVersion, GuardReceipt, MAX_SKILLS,
    NetworkGrant, ToolCall, ToolContext, ToolDisposition, ToolLimits, ToolObservation,
    ToolPolicyReceipt, ToolResourceProfile,
};
pub(crate) use types::{MAX_MODEL_STEPS, MAX_TOOL_CALLS, MAX_TOOL_CONTEXT_BYTES};

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
    workspace_identity: (u64, u64),
    guard_executable: PathBuf,
    enforcement_digest: [u8; 32],
    process_enforcement_digest: [u8; 32],
    queue_progress: Option<tokio::sync::watch::Sender<Option<uuid::Uuid>>>,
    cache_state: Option<std::sync::Arc<StateRoot>>,
    #[cfg(target_os = "macos")]
    system_programs: Option<std::sync::Arc<guard::SystemPrograms>>,
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

    pub(crate) fn queue_changed(&self, id: uuid::Uuid, queued: bool) {
        if let Some(sender) = &self.queue_progress {
            sender.send_replace(queued.then_some(id));
        }
    }
    pub(crate) fn workspace_identity(&self) -> (u64, u64) {
        self.workspace_identity
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
                self.config.workspace_paths == ["."]
                    || self
                        .config
                        .workspace_paths
                        .iter()
                        .any(|root| Path::new(path).starts_with(root))
            }
        };
        criterion_allowed
            && contract.paths.iter().all(|path| {
                self.config.workspace_paths == ["."]
                    || self
                        .config
                        .workspace_paths
                        .iter()
                        .any(|root| Path::new(path).starts_with(root))
            })
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
            grant.verify_host()?;
            for protected in [workspace, state.path(), account.as_path()] {
                fs::reject_overlap(&grant.path, protected)?;
            }
            for runtime in guard::runtime_roots() {
                fs::reject_overlap(&grant.path, Path::new(runtime))?;
            }
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
            cache_state: Some(std::sync::Arc::new(
                state
                    .try_clone()
                    .map_err(|_| ToolError::ProtectionUnavailable)?,
            )),
            workspace: workspace.to_owned(),
            workspace_identity,
            guard_executable,
            enforcement_digest,
            process_enforcement_digest,
            #[cfg(target_os = "macos")]
            system_programs,
        })
    }

    pub(crate) fn receipt(&self) -> ToolPolicyReceipt {
        self.config.receipt()
    }

    pub(crate) fn context(&self) -> Result<ToolContext, ToolError> {
        let skill_rows: Vec<_> = self.config.skills.iter().map(|skill| serde_json::json!({"name":skill.name,"description":skill.description.chars().take(96).collect::<String>()})).collect();
        let catalog = serde_json::json!({
            "host_clock": {"unix_ms": now_ms(), "timezone":"UTC"},
            "workspace_paths": self.config.workspace_paths,
            "write": self.config.write,
            "file_tools": "list (directory entries), read (UTF-8, zero-based byte offset, limit 1..4096, whole-file sha256), search (literal UTF-8 content in a file or directory; use Read or an include for edit sha256), mkdir (new directory; parent must exist), write (null digest only creates), edit (unique literal match, expected sha256)",
            "commands": self.config.commands.iter().map(|program| serde_json::json!({"name":program.name,"interpreter":program.interpreter,"inputs":program.inputs.iter().map(|input| format!("/inputs/{}/{}", program.name, input.destination)).collect::<Vec<_>>()})).collect::<Vec<_>>(),
            "command_contract": if cfg!(target_os = "macos") {
                "Explicit argv only. Private selected-project cwd; use ARANY_TOOL_WORKSPACE, ARANY_TOOL_INPUTS and ARANY_TOOL_SKILLS for paths inside shell strings. Standalone /workspace, /scratch, /inputs, /skills and /programs argv paths map to private native paths; checked system programs execute at their original paths. No network, host home, credentials or state. Bounded regular-file changes return as a changeset; a separate applied integration receipt confirms Workspace publication."
            } else {
                "Explicit argv only. Private writable selected-project snapshot at /workspace; /scratch is bounded. No network, host home, credentials or state. Bounded regular-file changes return as a changeset; a separate applied integration receipt confirms Workspace publication. Skills available at /skills/NAME."
            },
            "skills": skill_rows,
            "mcp_servers": self.config.mcp.iter().map(|server| serde_json::json!({"name":server.name,"tools":server.tools,"protocol":"2025-11-25 stdio; use mcp_list before mcp_call"})).collect::<Vec<_>>(),
        }).to_string();
        if catalog.len() > types::MAX_TOOL_CATALOG_BYTES {
            return Err(ToolError::Limit);
        }
        Ok(ToolContext {
            catalog,
            observations: Vec::new(),
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

    pub(crate) async fn execute(
        &self,
        intent: EffectIntent,
        cancellation: RunCancellation,
    ) -> ToolObservation {
        if !self.config.allows(&intent.call) {
            return ToolObservation {
                intent,
                disposition: ToolDisposition::Denied,
                output: "Tool operation is outside the pinned grant".into(),
                guard: None,
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            };
        }
        let cache_selected =
            matches!(&intent.call, ToolCall::Command { .. }) && self.cache_state.is_some();
        match guard::execute(self, &intent, cancellation).await {
            Ok(observation) => observation,
            Err(error) => uncertain_observation(
                intent,
                error,
                if cache_selected {
                    crate::tools::CacheUpdate::Uncertain
                } else {
                    crate::tools::CacheUpdate::NotApplicable
                },
            ),
        }
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
    ToolObservation {
        intent,
        disposition: ToolDisposition::Failed,
        output: match error {
            ToolError::QueueFull => format!("{error}. No operation was dispatched."),
            _ => format!(
                "Guard {} unavailable before dispatch; no operation was dispatched.",
                failure_stage(&error)
            ),
        },
        guard: None,
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
        guard: None,
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
