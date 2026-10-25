use super::*;
use std::path::Path;

pub const MAX_GOAL_PROVIDER_CALLS: u32 = 64;
pub const MAX_GOAL_ACTIVE_MS: u64 = 30 * 60 * 1000;
pub const MAX_GOAL_RUNS: u8 = 16;
pub const MAX_GOAL_PLAN_PATHS: usize = 64;
pub const MAX_GOAL_PLAN_COMMANDS: usize = 8;
pub const MAX_GOAL_PLAN_MCP_TOOLS: usize = 64;
pub const MAX_GOAL_PLAN_SKILLS: usize = 64;
pub const MAX_GOAL_PLAN_CONTRACT_BYTES: usize = 8 * 1024;
pub const MAX_GOAL_REPORT_BYTES: usize = 8 * 1024;
pub(crate) const STORE_ACTION_OVERHEAD_MS: u64 = 5_000;
pub const MAX_BUDGET_REFUSAL_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetRefusalCause {
    Exhausted,
    UnknownUsage,
    InsufficientHeadroom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetAction {
    Provider,
    Tool,
    Delegation,
    Compaction,
    Run,
    Resume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetChargeBasis {
    Observed,
    Conservative,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetRefusal {
    pub dimension: super::super::GoalLimit,
    pub cause: BudgetRefusalCause,
    pub action: BudgetAction,
    #[serde(deserialize_with = "Option::deserialize")]
    pub run_id: Option<RunId>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub agent_run_id: Option<AgentRunId>,
    pub used: u64,
    pub limit: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub requested: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub required_minimum: Option<u64>,
    pub token_usage_unknown: bool,
    pub charge_basis: BudgetChargeBasis,
}

impl BudgetRefusal {
    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        use super::super::GoalLimit;
        let valid_cause = match self.cause {
            BudgetRefusalCause::Exhausted => self.used >= self.limit,
            BudgetRefusalCause::UnknownUsage => {
                self.dimension == GoalLimit::Tokens && self.token_usage_unknown
            }
            BudgetRefusalCause::InsufficientHeadroom => {
                self.dimension == GoalLimit::ActiveTime
                    && self.used < self.limit
                    && self
                        .required_minimum
                        .is_some_and(|minimum| minimum > STORE_ACTION_OVERHEAD_MS)
                    && self
                        .required_minimum
                        .is_some_and(|minimum| minimum > self.limit - self.used)
            }
        };
        let valid_amounts = match self.dimension {
            GoalLimit::Tokens => self.requested.is_none() && self.required_minimum.is_none(),
            GoalLimit::Calls | GoalLimit::Runs => {
                self.requested == Some(1) && self.required_minimum == Some(1)
            }
            GoalLimit::ActiveTime => match (self.requested, self.required_minimum) {
                (None, None) => self.cause == BudgetRefusalCause::Exhausted,
                (Some(requested), Some(minimum)) => {
                    requested >= minimum
                        && (minimum > STORE_ACTION_OVERHEAD_MS
                            || requested.checked_sub(minimum) == Some(STORE_ACTION_OVERHEAD_MS - 1)
                                && matches!(
                                    self.action,
                                    BudgetAction::Tool | BudgetAction::Delegation
                                ))
                }
                (None, Some(minimum)) => minimum == STORE_ACTION_OVERHEAD_MS + 1,
                (Some(_), None) => false,
            },
        };
        if self.limit == 0
            || match self.dimension {
                GoalLimit::Calls => self.limit > u64::from(MAX_GOAL_PROVIDER_CALLS),
                GoalLimit::Runs => self.limit > u64::from(MAX_GOAL_RUNS),
                GoalLimit::ActiveTime => self.limit > MAX_GOAL_ACTIVE_MS,
                GoalLimit::Tokens => false,
            }
            || !valid_cause
            || self.dimension == GoalLimit::Tokens
                && self.token_usage_unknown != (self.cause == BudgetRefusalCause::UnknownUsage)
            || !valid_amounts
            || self.agent_run_id.is_some() && self.run_id.is_none()
            || !serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_BUDGET_REFUSAL_BYTES)
        {
            return Err(ReplayError::InvalidPayload);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalBudget {
    #[serde(deserialize_with = "Option::deserialize")]
    pub max_provider_calls: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub max_tokens: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub max_active_ms: Option<u64>,
    pub max_runs: u8,
}

impl Default for GoalBudget {
    fn default() -> Self {
        Self {
            max_provider_calls: None,
            max_tokens: None,
            max_active_ms: None,
            max_runs: MAX_GOAL_RUNS,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct GoalUsage {
    pub(crate) runs: usize,
    pub(crate) provider_calls: u32,
    pub(crate) active_ms: u64,
    pub(crate) known_tokens: u64,
    pub(crate) token_usage_unknown: bool,
}

impl GoalBudget {
    pub(crate) fn limit_for(&self, dimension: super::super::GoalLimit) -> Option<u64> {
        use super::super::GoalLimit;
        match dimension {
            GoalLimit::Calls => self.max_provider_calls.map(u64::from),
            GoalLimit::Runs => Some(u64::from(self.max_runs)),
            GoalLimit::ActiveTime => self.max_active_ms,
            GoalLimit::Tokens => self.max_tokens,
        }
    }

    pub(crate) fn exhausted_dimension(&self, usage: GoalUsage) -> Option<super::super::GoalLimit> {
        use super::super::GoalLimit;
        if self
            .max_tokens
            .is_some_and(|limit| usage.token_usage_unknown || usage.known_tokens >= limit)
        {
            Some(GoalLimit::Tokens)
        } else if self
            .max_provider_calls
            .is_some_and(|limit| usage.provider_calls >= limit)
        {
            Some(GoalLimit::Calls)
        } else if self
            .max_active_ms
            .is_some_and(|limit| limit.saturating_sub(usage.active_ms) <= STORE_ACTION_OVERHEAD_MS)
        {
            Some(GoalLimit::ActiveTime)
        } else if usage.runs >= usize::from(self.max_runs) {
            Some(GoalLimit::Runs)
        } else {
            None
        }
    }

    pub(crate) fn refusal(&self, usage: GoalUsage, action: BudgetAction) -> Option<BudgetRefusal> {
        use super::super::GoalLimit;
        let dimension = self.exhausted_dimension(usage)?;
        let limit = self.limit_for(dimension)?;
        let used = match dimension {
            GoalLimit::Tokens => usage.known_tokens,
            GoalLimit::Calls => u64::from(usage.provider_calls),
            GoalLimit::ActiveTime => usage.active_ms,
            GoalLimit::Runs => usage.runs as u64,
        };
        let (cause, amount) = if dimension == GoalLimit::Tokens && usage.token_usage_unknown {
            (BudgetRefusalCause::UnknownUsage, None)
        } else if dimension == GoalLimit::ActiveTime && used < limit {
            (
                BudgetRefusalCause::InsufficientHeadroom,
                Some(STORE_ACTION_OVERHEAD_MS + 1),
            )
        } else {
            (
                BudgetRefusalCause::Exhausted,
                match dimension {
                    GoalLimit::Tokens | GoalLimit::ActiveTime => None,
                    GoalLimit::Calls | GoalLimit::Runs => Some(1),
                },
            )
        };
        Some(BudgetRefusal {
            dimension,
            cause,
            action,
            run_id: None,
            agent_run_id: None,
            used,
            limit,
            requested: if cause == BudgetRefusalCause::InsufficientHeadroom {
                None
            } else {
                amount
            },
            required_minimum: amount,
            token_usage_unknown: usage.token_usage_unknown,
            charge_basis: BudgetChargeBasis::Observed,
        })
    }

    pub fn validate(&self) -> Result<(), ReplayError> {
        if self
            .max_provider_calls
            .is_some_and(|calls| calls == 0 || calls > MAX_GOAL_PROVIDER_CALLS)
            || self.max_tokens.is_some_and(|tokens| tokens == 0)
            || self
                .max_active_ms
                .is_some_and(|ms| ms == 0 || ms > MAX_GOAL_ACTIVE_MS)
            || self.max_runs == 0
            || self.max_runs > MAX_GOAL_RUNS
        {
            return Err(ReplayError::InvalidPayload);
        }
        Ok(())
    }

    pub(crate) fn effective(&self, plan: Option<&GoalPlanContract>) -> Self {
        plan.map_or_else(
            || *self,
            |plan| Self {
                max_tokens: plan.budget.max_tokens.or(self.max_tokens),
                ..plan.budget
            },
        )
    }

    pub(crate) fn admits_next_run(&self, usage: GoalUsage) -> bool {
        usize::from(self.max_runs) > usage.runs
            && self
                .max_provider_calls
                .is_none_or(|limit| limit > usage.provider_calls)
            && self
                .max_active_ms
                .is_none_or(|limit| limit > usage.active_ms)
            && self
                .max_tokens
                .is_none_or(|limit| !usage.token_usage_unknown && usage.known_tokens < limit)
    }

    pub(crate) fn spent(&self, usage: GoalUsage) -> bool {
        !self.admits_next_run(usage)
            || self.max_active_ms.is_some_and(|limit| {
                limit.saturating_sub(usage.active_ms) <= STORE_ACTION_OVERHEAD_MS
            })
    }

    pub(crate) fn is_within(&self, pinned: &Self) -> bool {
        limit_is_within(self.max_provider_calls, pinned.max_provider_calls)
            && limit_is_within(self.max_active_ms, pinned.max_active_ms)
            && self.max_runs <= pinned.max_runs
            && match (self.max_tokens, pinned.max_tokens) {
                (None, None) | (Some(_), None) => true,
                (Some(candidate), Some(pinned)) => candidate <= pinned,
                (None, Some(_)) => false,
            }
    }
}

fn limit_is_within<T: Copy + Ord>(candidate: Option<T>, pinned: Option<T>) -> bool {
    match (candidate, pinned) {
        (Some(candidate), Some(pinned)) => candidate <= pinned,
        (Some(_), None) | (None, None) => true,
        (None, Some(_)) => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    Research,
    Plan,
    Implementation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Running,
    Interrupted,
    Completed,
    Failed,
}

impl GoalStatus {
    /// Stable name, identical to the serde snake_case representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Interrupted => "interrupted",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalPlanContract {
    pub mission: String,
    pub paths: Vec<String>,
    pub commands: Vec<String>,
    pub resource_class: crate::tools::ToolResourceClass,
    pub command_timeout_ms: Option<u32>,
    pub skills: Vec<GoalSkillBinding>,
    pub mcp_tools: Vec<GoalMcpTool>,
    pub success_criterion: GoalSuccessCriterion,
    pub budget: GoalBudget,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalSuccessCriterion {
    ToolCallSucceeded { call: Box<crate::tools::ToolCall> },
    WorkspaceFileDigest { path: String, sha256: String },
    WorkspaceFileContent { path: String, content: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GoalSuccessState {
    NoContract,
    ToolCall {
        satisfied: bool,
        pending_integration: Option<[u8; 32]>,
    },
    WorkspaceFile {
        sha256: Option<String>,
    },
}

impl GoalPlanContract {
    pub(crate) fn initial_success_state(&self) -> GoalSuccessState {
        match &self.success_criterion {
            GoalSuccessCriterion::ToolCallSucceeded { .. } => GoalSuccessState::ToolCall {
                satisfied: false,
                pending_integration: None,
            },
            GoalSuccessCriterion::WorkspaceFileDigest { .. }
            | GoalSuccessCriterion::WorkspaceFileContent { .. } => {
                GoalSuccessState::WorkspaceFile { sha256: None }
            }
        }
    }

    pub(crate) fn observe_success(
        &self,
        state: &mut GoalSuccessState,
        observation: &crate::tools::ToolObservation,
    ) {
        use crate::tools::{ToolCall, ToolDisposition};

        if observation.intent.target != crate::session::worker::EffectTarget::Workspace {
            return;
        }
        if matches!(state, GoalSuccessState::NoContract)
            || matches!(
                (&self.success_criterion, &*state),
                (
                    GoalSuccessCriterion::ToolCallSucceeded { .. },
                    GoalSuccessState::WorkspaceFile { .. }
                ) | (
                    GoalSuccessCriterion::WorkspaceFileDigest { .. },
                    GoalSuccessState::ToolCall { .. }
                ) | (
                    GoalSuccessCriterion::WorkspaceFileContent { .. },
                    GoalSuccessState::ToolCall { .. }
                )
            )
        {
            *state = self.initial_success_state();
        }

        match (&self.success_criterion, state) {
            (
                GoalSuccessCriterion::ToolCallSucceeded { call },
                GoalSuccessState::ToolCall {
                    satisfied,
                    pending_integration,
                },
            ) => {
                if let ToolCall::ApplyChangeset {
                    source:
                        crate::session::worker::ChangesetSource::CommandReceipt {
                            source_intent_digest,
                            ..
                        },
                } = &observation.intent.call
                    && *pending_integration == Some(*source_intent_digest)
                {
                    *satisfied = observation.disposition == ToolDisposition::Succeeded
                        && observation.attested_for(&observation.intent);
                    *pending_integration = None;
                    return;
                }
                if observation.intent.call == **call {
                    if observation.disposition != ToolDisposition::Succeeded
                        || !observation.attested_for(&observation.intent)
                    {
                        *satisfied = false;
                        *pending_integration = None;
                        return;
                    }
                    if matches!(&**call, ToolCall::Command { .. }) {
                        let Ok(result) = serde_json::from_str::<crate::tools::types::CommandResult>(
                            &observation.output,
                        ) else {
                            *satisfied = false;
                            *pending_integration = None;
                            return;
                        };
                        let command_succeeded = result.valid() && result.exit_code == Some(0);
                        *satisfied = command_succeeded && result.changeset.files.is_empty();
                        *pending_integration = (command_succeeded
                            && !result.changeset.files.is_empty())
                        .then(|| observation.intent.digest());
                    } else {
                        *satisfied = true;
                        *pending_integration = None;
                    }
                    return;
                }
                if matches!(&**call, ToolCall::Command { .. })
                    && *satisfied
                    && matches!(
                        &observation.intent.call,
                        ToolCall::Write { .. }
                            | ToolCall::Edit { .. }
                            | ToolCall::Delete { .. }
                            | ToolCall::Move { .. }
                            | ToolCall::MultiEdit { .. }
                            | ToolCall::ApplyChangeset { .. }
                            | ToolCall::McpCall { .. }
                            | ToolCall::GitSwitch { .. }
                    )
                {
                    *satisfied = false;
                }
            }
            (
                GoalSuccessCriterion::WorkspaceFileDigest { path, .. },
                GoalSuccessState::WorkspaceFile { sha256 },
            )
            | (
                GoalSuccessCriterion::WorkspaceFileContent { path, .. },
                GoalSuccessState::WorkspaceFile { sha256 },
            ) => {
                if let Some(digest) = file_observation_digest(observation, path) {
                    *sha256 = digest;
                }
            }
            (_, GoalSuccessState::NoContract) => {}
            _ => unreachable!("criterion state was normalized before matching"),
        }
    }

    pub(crate) fn success_state_satisfied(&self, state: &GoalSuccessState) -> bool {
        match (&self.success_criterion, state) {
            (
                GoalSuccessCriterion::ToolCallSucceeded { .. },
                GoalSuccessState::ToolCall { satisfied, .. },
            ) => *satisfied,
            (
                GoalSuccessCriterion::WorkspaceFileDigest {
                    sha256: expected, ..
                },
                GoalSuccessState::WorkspaceFile { sha256 },
            ) => sha256.as_deref() == Some(expected),
            (
                GoalSuccessCriterion::WorkspaceFileContent { content, .. },
                GoalSuccessState::WorkspaceFile { sha256 },
            ) => {
                let expected = crate::validate::hex_digest(content.as_bytes());
                sha256.as_deref() == Some(&expected)
            }
            _ => false,
        }
    }
}

fn file_observation_digest(
    observation: &crate::tools::ToolObservation,
    path: &str,
) -> Option<Option<String>> {
    use crate::tools::{ToolCall, ToolDisposition};

    let digest = match &observation.intent.call {
        ToolCall::Read { path: observed, .. }
        | ToolCall::Write { path: observed, .. }
        | ToolCall::Edit { path: observed, .. }
        | ToolCall::MultiEdit {
            file_path: observed,
            ..
        } if observed == path => serde_json::from_str::<serde_json::Value>(&observation.output)
            .ok()
            .and_then(|value| value.get("sha256")?.as_str().map(str::to_owned)),
        ToolCall::Delete { path: source, .. } if source == path => None,
        ToolCall::Move {
            source,
            destination,
            expected_digest,
        } => {
            if source == path {
                None
            } else if destination == path {
                Some(expected_digest.clone())
            } else {
                return None;
            }
        }
        ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } => changeset
            .files
            .iter()
            .find(|change| change.path == path)
            .and_then(|change| change.after.as_ref().map(|version| version.sha256.clone())),
        ToolCall::McpCall { .. } | ToolCall::GitSwitch { .. } => return Some(None),
        _ => return None,
    };
    Some(digest.filter(|_| {
        observation.disposition == ToolDisposition::Succeeded
            && observation.attested_for(&observation.intent)
    }))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalMcpTool {
    pub server: String,
    pub tool: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSkillBinding {
    pub name: String,
    #[serde(deserialize_with = "required_nullable")]
    pub resource: Option<String>,
}

fn required_nullable<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

impl GoalPlanContract {
    pub(crate) fn allows_call(&self, call: &crate::tools::ToolCall) -> bool {
        use crate::tools::ToolCall;
        match call {
            ToolCall::ParseAgentDefinition { .. }
            | ToolCall::PrepareWorker { .. }
            | ToolCall::ApplyChangeset {
                source:
                    crate::session::worker::ChangesetSource::WorkerHandover { .. }
                    | crate::session::worker::ChangesetSource::Rewind { .. },
            } => false,
            ToolCall::List { path }
            | ToolCall::Read { path, .. }
            | ToolCall::Search { path, .. }
            | ToolCall::Glob { path, .. }
            | ToolCall::Mkdir { path }
            | ToolCall::Write { path, .. }
            | ToolCall::Edit { path, .. }
            | ToolCall::Delete { path, .. }
            | ToolCall::MultiEdit {
                file_path: path, ..
            }
            | ToolCall::GitStatus { path }
            | ToolCall::GitDiff { path, .. }
            | ToolCall::GitLog { path, .. } => self.allows_path(path),
            ToolCall::Move {
                source,
                destination,
                ..
            } => self.allows_path(source) && self.allows_path(destination),
            ToolCall::Command { program, .. } | ToolCall::CommandUnguarded { program, .. } => {
                self.commands.contains(program)
            }
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::CommandReceipt { changeset, .. },
            } => changeset
                .files
                .iter()
                .all(|change| self.allows_path(&change.path)),
            ToolCall::McpCall { server, tool, .. } => self
                .mcp_tools
                .iter()
                .any(|binding| binding.server == *server && binding.tool == *tool),
            ToolCall::Skill { name, resource } => self.skills.iter().any(|binding| {
                binding.name == *name
                    && binding.resource.as_deref().unwrap_or("SKILL.md")
                        == resource.as_deref().unwrap_or("SKILL.md")
            }),
            ToolCall::McpList { .. } => true,
            // Commits, branches and switches act on the whole repository.
            ToolCall::GitCommit { .. }
            | ToolCall::GitBranch { .. }
            | ToolCall::GitSwitch { .. }
            | ToolCall::GitPush { .. }
            | ToolCall::GhPrCreate { .. } => self.allows_path("."),
            ToolCall::GhPrList { .. } => self.allows_path("."),
        }
    }

    fn allows_path(&self, path: &str) -> bool {
        self.paths.iter().any(|scope| {
            scope == "."
                || Path::new(path) == Path::new(scope)
                || Path::new(path).starts_with(scope)
        })
    }

    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        if self.paths.is_empty()
            && self.commands.is_empty()
            && self.skills.is_empty()
            && self.mcp_tools.is_empty()
            || self.paths.len() > MAX_GOAL_PLAN_PATHS
            || self.commands.len() > MAX_GOAL_PLAN_COMMANDS
            || self.command_timeout_ms.is_some_and(|timeout| {
                !(1_000..=crate::tools::types::MAX_COMMAND_TIMEOUT_MS).contains(&timeout)
            })
            || self.resource_class == crate::tools::ToolResourceClass::Build
                && self.commands.is_empty()
            || self.command_timeout_ms.is_some() && self.commands.is_empty()
            || self.paths.windows(2).any(|pair| pair[0] >= pair[1])
            || self.commands.windows(2).any(|pair| pair[0] >= pair[1])
            || self.mcp_tools.len() > MAX_GOAL_PLAN_MCP_TOOLS
            || self.skills.len() > MAX_GOAL_PLAN_SKILLS
            || self.skills.windows(2).any(|pair| {
                (
                    pair[0].name.as_str(),
                    pair[0].resource.as_deref().unwrap_or("SKILL.md"),
                ) >= (
                    pair[1].name.as_str(),
                    pair[1].resource.as_deref().unwrap_or("SKILL.md"),
                )
            })
            || self
                .mcp_tools
                .windows(2)
                .any(|pair| (&pair[0].server, &pair[0].tool) >= (&pair[1].server, &pair[1].tool))
        {
            return Err(ReplayError::InvalidPayload);
        }
        if !serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_GOAL_PLAN_CONTRACT_BYTES)
        {
            return Err(ReplayError::InvalidPayload);
        }
        bounded(&self.mission, 2048)?;
        match &self.success_criterion {
            GoalSuccessCriterion::ToolCallSucceeded { call } => {
                if !call.valid() || !call.facts().goal_success {
                    return Err(ReplayError::InvalidPayload);
                }
            }
            GoalSuccessCriterion::WorkspaceFileDigest { path, sha256 } => {
                if !crate::validate::valid_relative(path, false)
                    || !crate::validate::valid_digest(sha256)
                {
                    return Err(ReplayError::InvalidPayload);
                }
            }
            GoalSuccessCriterion::WorkspaceFileContent { path, content } => {
                if !crate::validate::valid_relative(path, false) || content.len() > 8 * 1024 {
                    return Err(ReplayError::InvalidPayload);
                }
            }
        }
        if self.paths.iter().any(|path| {
            path != "." && !crate::validate::valid_relative(path, false)
                || path.len() > 512
                || self
                    .paths
                    .iter()
                    .any(|other| other != path && Path::new(path).starts_with(other))
        }) || self.paths.contains(&".".to_owned()) && self.paths.len() != 1
            || self
                .commands
                .iter()
                .any(|command| !crate::validate::valid_name(command) || command.len() > 64)
            || self.skills.iter().any(|binding| {
                !crate::validate::valid_name(&binding.name)
                    || binding.name.len() > 64
                    || binding
                        .resource
                        .as_ref()
                        .is_some_and(|resource| !crate::validate::valid_relative(resource, false))
            })
            || self.mcp_tools.iter().any(|binding| {
                !crate::validate::valid_name(&binding.server)
                    || binding.server.len() > 64
                    || !crate::validate::valid_name(&binding.tool)
                    || binding.tool.len() > 128
            })
        {
            return Err(ReplayError::InvalidPayload);
        }
        match &self.success_criterion {
            GoalSuccessCriterion::ToolCallSucceeded { call } if !self.allows_call(call) => {
                return Err(ReplayError::InvalidPayload);
            }
            GoalSuccessCriterion::WorkspaceFileDigest { path, .. }
            | GoalSuccessCriterion::WorkspaceFileContent { path, .. }
                if !self.allows_path(path) =>
            {
                return Err(ReplayError::InvalidPayload);
            }
            _ => {}
        }
        self.budget.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalReport {
    pub summary: String,
    pub changes: Vec<String>,
    pub evidence: Vec<String>,
    pub unresolved: Vec<String>,
    /// Information only: total cache-read input tokens across the Goal's Provider calls, never a budget.
    pub cached_input_tokens: u64,
}

impl GoalReport {
    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        for item in [&self.summary]
            .into_iter()
            .chain(self.changes.iter())
            .chain(self.evidence.iter())
            .chain(self.unresolved.iter())
        {
            bounded(item, 2048)?;
        }
        if self.changes.len() > 32
            || self.evidence.len() > 32
            || self.unresolved.len() > 32
            || serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > MAX_GOAL_REPORT_BYTES)
        {
            return Err(ReplayError::InvalidPayload);
        }
        Ok(())
    }
}
