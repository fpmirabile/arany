mod approval;
pub(crate) mod config;
mod fs;
mod guard;
mod mcp;
mod skills;
pub(crate) mod types;
mod workspace;

#[cfg(test)]
mod tests;

use crate::engine::RunCancellation;
use crate::session::{AgentRunId, RunId};
use crate::store::StateRoot;
pub use approval::{ApprovalInbox, ApprovalMode, ToolApproval, ToolApprovals};
use config::Config;
pub use workspace::{WorkspacePermissions, mention_paths};
pub fn list_workspace_entries(
    workspace: &Path,
    folder: &str,
    prefix: &str,
) -> Result<Vec<String>, ToolError> {
    fs::workspace_entries(workspace, folder, prefix)
}
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
pub use types::{
    EffectIntent, GuardReceipt, NetworkGrant, ToolCall, ToolContext, ToolDisposition, ToolLimits,
    ToolObservation, ToolPolicyReceipt,
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
    GuardRejected(&'static str),
}

pub(crate) struct ToolRuntime {
    config: Config,
    workspace: PathBuf,
    workspace_identity: (u64, u64),
    guard_executable: PathBuf,
    enforcement_digest: [u8; 32],
}

impl ToolRuntime {
    pub(crate) fn workspace_identity(&self) -> (u64, u64) {
        self.workspace_identity
    }
    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        self.config.allows(call)
    }
    pub(crate) fn admit(
        config: Config,
        workspace: &Path,
        state: &StateRoot,
        guard_executable: PathBuf,
    ) -> Result<Self, ToolError> {
        guard::check_available()?;
        let enforcement_digest = guard::enforcement_digest()?;
        let root = fs::open_directory(workspace)?;
        state
            .ensure_outside_workspace(&root)
            .map_err(|_| ToolError::Path)?;
        fs::reject_overlap(workspace, state.path())?;
        let account = StateRoot::account_path().map_err(|_| ToolError::Path)?;
        fs::reject_overlap(workspace, &account)?;
        for protected in [workspace, state.path(), account.as_path()] {
            fs::reject_overlap(Path::new(guard::RUNTIME_ROOT), protected)?;
        }
        let workspace_identity = fs::identity(&root)?;
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
        Ok(Self {
            config,
            workspace: workspace.to_owned(),
            workspace_identity,
            guard_executable,
            enforcement_digest,
        })
    }

    pub(crate) fn receipt(&self) -> ToolPolicyReceipt {
        self.config.receipt()
    }

    pub(crate) fn context(&self) -> Result<ToolContext, ToolError> {
        let skill_rows: Vec<_> = self.config.skills.iter().map(|skill| serde_json::json!({"name":skill.name,"description":skill.description,"sha256":skill.files["SKILL.md"]})).collect();
        let catalog = serde_json::json!({
            "host_clock": {"unix_ms": now_ms(), "timezone":"UTC"},
            "workspace_paths": self.config.workspace_paths,
            "write": self.config.write,
            "file_tools": "list (directory entries), read (UTF-8, zero-based byte offset, limit 1..4096, whole-file sha256), search (literal UTF-8 content in a file or directory; use Read or an include for edit sha256), mkdir (new directory; parent must exist), write (null digest only creates), edit (unique literal match, expected sha256)",
            "commands": self.config.commands.iter().map(|program| serde_json::json!({"name":program.name,"interpreter":program.interpreter,"inputs":program.inputs.iter().map(|input| format!("/inputs/{}/{}", program.name, input.destination)).collect::<Vec<_>>()})).collect::<Vec<_>>(),
            "command_contract": "Explicit argv only. Private writable selected-project snapshot at /workspace; /scratch is bounded. No network, host home, credentials or state. Subprocess file changes are discarded, not applied to host. Use typed write/edit to integrate source. Skills available at /skills/NAME.",
            "skills": skill_rows,
            "mcp_servers": self.config.mcp.iter().map(|server| serde_json::json!({"name":server.name,"tools":server.tools,"protocol":"2025-11-25 stdio; use mcp_list before mcp_call"})).collect::<Vec<_>>(),
        }).to_string();
        if catalog.len() > 8 * 1024 {
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
        EffectIntent {
            id: uuid::Uuid::now_v7(),
            run_id,
            agent_run_id,
            policy_digest: self.config.digest(),
            enforcement_digest: self.enforcement_digest,
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
            };
        }
        match guard::execute(self, &intent, cancellation).await {
            Ok(observation) => observation,
            Err(error) => uncertain_observation(intent, error),
        }
    }
}

pub(crate) fn failure_stage(error: &ToolError) -> &'static str {
    match error {
        ToolError::GuardRejected("admission") | ToolError::Configuration => "admission",
        ToolError::GuardRejected("Workspace identity")
        | ToolError::ChangedInput
        | ToolError::Path => "pinned input",
        ToolError::GuardRejected("kernel capability") | ToolError::ProtectionUnavailable => {
            "native protection"
        }
        ToolError::GuardRejected("handshake") => "handshake",
        ToolError::GuardRejected("receipt") => "receipt",
        ToolError::GuardRejected("resource attestation") => "resource attestation",
        ToolError::GuardRejected("process-unit cleanup") => "process cleanup",
        ToolError::GuardRejected("launcher reap") => "launcher reap",
        ToolError::GuardRejected("stderr EOF") => "output EOF",
        ToolError::GuardRejected("namespace bootstrap") => "namespace bootstrap",
        ToolError::GuardRejected("native manager") => "native manager",
        ToolError::GuardRejected("bounded control protocol") => "control protocol",
        ToolError::Limit => "resource limit",
        _ => "execution",
    }
}

fn unstarted_observation(intent: EffectIntent, error: ToolError) -> ToolObservation {
    crate::diagnostics::guard_failure(&error);
    ToolObservation {
        intent,
        disposition: ToolDisposition::Failed,
        output: format!(
            "Guard {} unavailable before dispatch; no operation was dispatched.",
            failure_stage(&error)
        ),
        guard: None,
    }
}

fn uncertain_observation(intent: EffectIntent, error: ToolError) -> ToolObservation {
    crate::diagnostics::guard_failure(&error);
    let stage = failure_stage(&error);
    ToolObservation {
        intent,
        disposition: ToolDisposition::Uncertain,
        output: format!(
            "Guard {stage} unconfirmed; effects may have occurred. Do not retry automatically."
        ),
        guard: None,
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
