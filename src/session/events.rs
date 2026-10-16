//! Canonical Event payloads. Never persist terminal focus, panel state, hover, picker selection or
//! native scroll position. Explicit nullable fields reject when missing; a new field bumps the
//! payload version.
use crate::provider::AgentPhase;
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path, str::FromStr};
use uuid::{Uuid, Version};

fn parse_v7(value: &str) -> Result<Uuid, &'static str> {
    let id = Uuid::parse_str(value).map_err(|_| "invalid ID")?;
    if id.get_version() != Some(Version::SortRand) {
        return Err("ID must be UUIDv7");
    }
    Ok(id)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}
impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for SessionId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(Uuid);

impl RunId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}
impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for RunId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GoalId(Uuid);

impl GoalId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    fn valid(self) -> bool {
        self.0.get_version() == Some(Version::SortRand)
    }
}
impl Default for GoalId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for GoalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for GoalId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

pub const MAX_GOAL_PROVIDER_CALLS: u32 = 64;
pub const MAX_GOAL_ACTIVE_MS: u64 = 30 * 60 * 1000;
pub(crate) const EVENT_PAYLOAD_VERSION: i64 = 6;
pub(crate) const MAX_CHILD_TOOL_CALLS: usize = 16;
/// Model Tool requests one Provider step may declare. Engine and adapters emit one until fan-out lands.
pub(crate) const MAX_STEP_TOOL_CALLS: u8 = 4;
const _: () = assert!(MAX_STEP_TOOL_CALLS as u32 <= u8::MAX as u32);
pub(crate) const MAX_GOAL_ACTION_ACTIVE_MS: u64 = 31 * 60 * 1000;
pub const MAX_GOAL_RUNS: u8 = 16;
pub const MAX_GOAL_PLAN_PATHS: usize = 64;
pub const MAX_GOAL_PLAN_COMMANDS: usize = 8;
pub const MAX_GOAL_PLAN_MCP_TOOLS: usize = 64;
pub const MAX_GOAL_PLAN_SKILLS: usize = 64;
pub const MAX_GOAL_PLAN_CONTRACT_BYTES: usize = 8 * 1024;
pub const MAX_GOAL_REPORT_BYTES: usize = 8 * 1024;

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
            max_provider_calls: Some(MAX_GOAL_PROVIDER_CALLS),
            max_tokens: None,
            max_active_ms: Some(MAX_GOAL_ACTIVE_MS),
            max_runs: MAX_GOAL_RUNS,
        }
    }
}

/// Goal usage that decides whether another Run is admissible; `runs` counts linked Runs.
#[derive(Clone, Copy)]
pub(crate) struct GoalUsage {
    pub(crate) runs: usize,
    pub(crate) provider_calls: u32,
    pub(crate) active_ms: u64,
    pub(crate) known_tokens: u64,
    pub(crate) token_usage_unknown: bool,
}

impl GoalBudget {
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

    /// Limits in force once a Plan contract narrows the pinned budget; an absent plan token ceiling keeps the pinned one.
    pub(crate) fn effective(&self, plan: Option<&GoalPlanContract>) -> Self {
        plan.map_or_else(
            || *self,
            |plan| Self {
                max_tokens: plan.budget.max_tokens.or(self.max_tokens),
                ..plan.budget
            },
        )
    }

    /// The one admission rule for another Run: every finite dimension must have headroom.
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
    pub(crate) const fn as_str(self) -> &'static str {
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
                    source_intent_digest,
                    ..
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
                            | ToolCall::ApplyChangeset { .. }
                            | ToolCall::McpCall { .. }
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
                let expected = crate::tools::types::hex_digest(content.as_bytes());
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
            if observed == path =>
        {
            serde_json::from_str::<serde_json::Value>(&observation.output)
                .ok()
                .and_then(|value| value.get("sha256")?.as_str().map(str::to_owned))
        }
        ToolCall::ApplyChangeset { changeset, .. } => changeset
            .files
            .iter()
            .find(|change| change.path == path)
            .and_then(|change| change.after.as_ref().map(|version| version.sha256.clone())),
        ToolCall::McpCall { .. } => return Some(None),
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

#[cfg(test)]
mod goal_success_tests {
    use super::*;
    use crate::tools::{
        EffectIntent, GuardReceipt, ToolCall, ToolDisposition, ToolLimits, ToolObservation,
    };

    fn matches_success<'a>(
        contract: &GoalPlanContract,
        observations: impl IntoIterator<Item = &'a ToolObservation>,
    ) -> bool {
        let mut state = contract.initial_success_state();
        for observation in observations {
            contract.observe_success(&mut state, observation);
        }
        contract.success_state_satisfied(&state)
    }

    fn observation(
        call: ToolCall,
        disposition: ToolDisposition,
        output: String,
    ) -> ToolObservation {
        let intent = EffectIntent {
            id: Uuid::now_v7(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        ToolObservation {
            guard: Some(GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            }),
            intent,
            disposition,
            output,
            cache_update: crate::tools::CacheUpdate::NotApplicable,
        }
    }

    fn criterion(criterion: GoalSuccessCriterion) -> GoalPlanContract {
        GoalPlanContract {
            mission: "Verify the requested result".into(),
            paths: Vec::new(),
            commands: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: criterion,
            budget: GoalBudget::default(),
        }
    }

    #[test]
    fn bounded_macos_build_reservation_fits_the_action_ceiling() {
        let goal_id = GoalId::new();
        let run_id = RunId::new();
        let reserve = Event::GoalActiveTimeReserved {
            goal_id,
            run_id,
            reserved_ms: 1_850_000,
        };
        let settle = Event::GoalActiveTimeSettled {
            goal_id,
            run_id,
            active_ms: 1_850_000,
        };
        assert!(reserve.validate().is_ok());
        assert!(settle.validate().is_ok());
        assert!(GoalBudget::default().validate().is_ok());
    }

    #[test]
    fn later_failed_receipt_invalidates_an_earlier_matching_call() {
        let call = ToolCall::Read {
            path: "src/main.rs".into(),
            offset: 0,
            limit: 1024,
        };
        let contract = criterion(GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(call.clone()),
        });
        let observations = [
            observation(call.clone(), ToolDisposition::Succeeded, "{}".into()),
            observation(call, ToolDisposition::Failed, "".into()),
        ];
        assert!(!matches_success(&contract, &observations));
    }

    #[test]
    fn skill_only_plan_requires_an_exact_binding_and_successful_receipt() {
        let call = ToolCall::Skill {
            name: "review".into(),
            resource: None,
        };
        let contract = GoalPlanContract {
            mission: "Read the requested Skill guidance".into(),
            paths: Vec::new(),
            commands: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: vec![GoalSkillBinding {
                name: "review".into(),
                resource: None,
            }],
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(call.clone()),
            },
            budget: GoalBudget::default(),
        };
        assert!(contract.validate().is_ok());
        assert!(contract.allows_call(&call));
        assert!(contract.allows_call(&ToolCall::Skill {
            name: "review".into(),
            resource: Some("SKILL.md".into()),
        }));
        let success = observation(
            call.clone(),
            ToolDisposition::Succeeded,
            "review guidance".into(),
        );
        assert!(matches_success(&contract, std::slice::from_ref(&success)));
        assert!(!matches_success(
            &contract,
            &[
                success,
                observation(call.clone(), ToolDisposition::Failed, "failed".into()),
            ]
        ));
        let resource_call = ToolCall::Skill {
            name: "review".into(),
            resource: Some("references/checklist.md".into()),
        };
        let mut resource_contract = contract.clone();
        resource_contract.skills[0].resource = Some("references/checklist.md".into());
        resource_contract.success_criterion = GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(resource_call.clone()),
        };
        assert!(resource_contract.validate().is_ok());
        assert!(matches_success(
            &resource_contract,
            &[observation(
                resource_call,
                ToolDisposition::Succeeded,
                "checklist".into(),
            )]
        ));
        let mut missing_resource = serde_json::to_value(&contract).unwrap();
        missing_resource["skills"][0]
            .as_object_mut()
            .unwrap()
            .remove("resource");
        assert!(serde_json::from_value::<GoalPlanContract>(missing_resource).is_err());

        let wrong_resource = ToolCall::Skill {
            name: "review".into(),
            resource: Some("references/other.md".into()),
        };
        let undeclared_skill = ToolCall::Skill {
            name: "other".into(),
            resource: None,
        };
        assert!(!contract.allows_call(&wrong_resource));
        assert!(!contract.allows_call(&undeclared_skill));
        let mut invalid = contract;
        invalid.success_criterion = GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(wrong_resource),
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn success_criterion_rejects_inspection_and_internal_tools() {
        let mut contract = GoalPlanContract {
            mission: "Verify a declared result".into(),
            paths: vec!["src".into()],
            commands: vec!["shell".into()],
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: vec![GoalMcpTool {
                server: "local".into(),
                tool: "inspect".into(),
            }],
            success_criterion: GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(ToolCall::Command {
                    program: "shell".into(),
                    args: Vec::new(),
                    cwd: String::new(),
                }),
            },
            budget: GoalBudget::default(),
        };
        for call in [
            ToolCall::List { path: "src".into() },
            ToolCall::Read {
                path: "src/main.rs".into(),
                offset: 0,
                limit: 1024,
            },
            ToolCall::Search {
                path: "src".into(),
                query: "main".into(),
            },
            ToolCall::McpList {
                server: "local".into(),
            },
            ToolCall::GitStatus { path: "src".into() },
            ToolCall::GitDiff {
                path: "src".into(),
                base: None,
                head: None,
                staged: false,
            },
            ToolCall::GitLog {
                path: "src".into(),
                revision: None,
                max_count: 1,
            },
        ] {
            contract.success_criterion = GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(call),
            };
            assert!(contract.validate().is_err());
        }
    }

    #[test]
    fn plan_contract_scopes_git_reads_by_path() {
        use crate::tools::ToolCall;
        let contract = |paths: Vec<&str>| GoalPlanContract {
            mission: "Inspect Git state".into(),
            paths: paths.into_iter().map(str::to_owned).collect(),
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/main.rs".into(),
                content: "ok".into(),
            },
            budget: GoalBudget::default(),
        };
        let calls = |path: &str| {
            [
                ToolCall::GitStatus { path: path.into() },
                ToolCall::GitDiff {
                    path: path.into(),
                    base: None,
                    head: None,
                    staged: false,
                },
                ToolCall::GitLog {
                    path: path.into(),
                    revision: None,
                    max_count: 1,
                },
            ]
        };
        let scoped = contract(vec!["src"]);
        for path in ["src", "src/lib.rs"] {
            for call in calls(path) {
                assert!(scoped.allows_call(&call), "{call:?}");
            }
        }
        for path in [".", "docs", ""] {
            for call in calls(path) {
                assert!(!scoped.allows_call(&call), "{call:?}");
            }
        }
        for call in calls(".") {
            assert!(contract(vec!["."]).allows_call(&call), "{call:?}");
        }
    }

    #[test]
    fn later_file_edit_invalidates_an_earlier_matching_read() {
        let expected = crate::tools::types::hex_digest(b"before");
        let contract = criterion(GoalSuccessCriterion::WorkspaceFileDigest {
            path: "src/main.rs".into(),
            sha256: expected,
        });
        let observations = [
            observation(
                ToolCall::Read {
                    path: "src/main.rs".into(),
                    offset: 0,
                    limit: 1024,
                },
                ToolDisposition::Succeeded,
                serde_json::json!({
                    "sha256": crate::tools::types::hex_digest(b"before")
                })
                .to_string(),
            ),
            observation(
                ToolCall::Write {
                    path: "src/main.rs".into(),
                    expected_digest: None,
                    content: "after".into(),
                },
                ToolDisposition::Succeeded,
                serde_json::json!({
                    "sha256": crate::tools::types::hex_digest(b"after")
                })
                .to_string(),
            ),
        ];
        assert!(!matches_success(&contract, &observations));
    }

    #[test]
    fn active_time_segments_are_bounded_per_run() {
        let (goal_id, run_id) = (GoalId::new(), RunId::new());
        let reserve = |reserved_ms| Event::GoalActiveTimeReserved {
            goal_id,
            run_id,
            reserved_ms,
        };
        assert!(reserve(MAX_GOAL_ACTION_ACTIVE_MS).validate().is_ok());
        assert!(reserve(MAX_GOAL_ACTION_ACTIVE_MS + 1).validate().is_err());
    }

    #[test]
    fn goal_steering_note_round_trips_and_rejects_bad_scope_and_bounds() {
        let (goal_id, run_id) = (GoalId::new(), RunId::new());
        let note = |text: &str| Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: text.into(),
        };
        let event = note("use the smaller fixture");
        assert_eq!(event.scope(), (Some(run_id), None));
        let payload = event.payload().unwrap();
        assert_eq!(
            Event::decode(event.kind(), event.version(), &payload, Some(run_id), None).unwrap(),
            event
        );

        assert!(note("").validate().is_err());
        assert!(note(" \n\t").validate().is_err());
        let limit = crate::session::MAX_STEERING_NOTE_BYTES;
        assert!(note(&"x".repeat(limit)).validate().is_ok());
        assert!(note(&"x".repeat(limit + 1)).validate().is_err());

        let kind = event.kind();
        let agent_run_id = Some(AgentRunId::new());
        assert!(
            Event::decode(
                kind,
                EVENT_PAYLOAD_VERSION - 1,
                &payload,
                Some(run_id),
                agent_run_id
            )
            .is_err()
        );
        assert!(Event::decode(kind, EVENT_PAYLOAD_VERSION, &payload, None, None).is_err());
        assert!(Event::decode(kind, 3, &payload, Some(run_id), None).is_err());

        let mut extra: serde_json::Value = serde_json::from_str(&payload).unwrap();
        extra["unexpected"] = serde_json::Value::Bool(true);
        assert!(
            Event::decode(
                kind,
                EVENT_PAYLOAD_VERSION,
                &extra.to_string(),
                Some(run_id),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn plan_contract_bounds_are_enforced_without_provider_schema_keywords() {
        let base = || GoalPlanContract {
            mission: "Verify".into(),
            paths: vec!["src".into()],
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/main.rs".into(),
                content: "ok".into(),
            },
            budget: GoalBudget::default(),
        };
        let long = |byte: char, len: usize| byte.to_string().repeat(len);
        let mut cases: Vec<(&str, GoalPlanContract, bool)> = vec![
            (
                "mission at limit",
                GoalPlanContract {
                    mission: long('m', 2048),
                    ..base()
                },
                true,
            ),
            (
                "mission over limit",
                GoalPlanContract {
                    mission: long('m', 2049),
                    ..base()
                },
                false,
            ),
            (
                "empty mission",
                GoalPlanContract {
                    mission: String::new(),
                    ..base()
                },
                false,
            ),
            (
                "path count at limit",
                GoalPlanContract {
                    paths: (0..63)
                        .map(|i| format!("p{i:02}"))
                        .chain(["src".into()])
                        .collect(),
                    ..base()
                },
                true,
            ),
            (
                "path count over limit",
                GoalPlanContract {
                    paths: (0..64)
                        .map(|i| format!("p{i:02}"))
                        .chain(["src".into()])
                        .collect(),
                    ..base()
                },
                false,
            ),
            (
                "path length at limit",
                GoalPlanContract {
                    paths: vec![long('a', 512)],
                    success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                        path: long('a', 512),
                        content: "ok".into(),
                    },
                    ..base()
                },
                true,
            ),
            (
                "path length over limit",
                GoalPlanContract {
                    paths: vec![long('a', 513)],
                    success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                        path: long('a', 513),
                        content: "ok".into(),
                    },
                    ..base()
                },
                false,
            ),
            (
                "command count over limit",
                GoalPlanContract {
                    commands: (0..9).map(|i| format!("c{i}")).collect(),
                    ..base()
                },
                false,
            ),
            (
                "command length over limit",
                GoalPlanContract {
                    commands: vec![long('a', 65)],
                    ..base()
                },
                false,
            ),
            (
                "skill count over limit",
                GoalPlanContract {
                    skills: (0..65)
                        .map(|i| GoalSkillBinding {
                            name: format!("s{i:02}"),
                            resource: None,
                        })
                        .collect(),
                    ..base()
                },
                false,
            ),
            (
                "skill name over limit",
                GoalPlanContract {
                    skills: vec![GoalSkillBinding {
                        name: long('a', 65),
                        resource: None,
                    }],
                    ..base()
                },
                false,
            ),
            (
                "skill resource over limit",
                GoalPlanContract {
                    skills: vec![GoalSkillBinding {
                        name: "review".into(),
                        resource: Some(long('r', 513)),
                    }],
                    ..base()
                },
                false,
            ),
            (
                "mcp count over limit",
                GoalPlanContract {
                    mcp_tools: (0..65)
                        .map(|i| GoalMcpTool {
                            server: format!("m{i:02}"),
                            tool: "t".into(),
                        })
                        .collect(),
                    ..base()
                },
                false,
            ),
            (
                "mcp server over limit",
                GoalPlanContract {
                    mcp_tools: vec![GoalMcpTool {
                        server: long('a', 65),
                        tool: "t".into(),
                    }],
                    ..base()
                },
                false,
            ),
            (
                "mcp tool over limit",
                GoalPlanContract {
                    mcp_tools: vec![GoalMcpTool {
                        server: "local".into(),
                        tool: long('a', 129),
                    }],
                    ..base()
                },
                false,
            ),
            (
                "content over limit",
                GoalPlanContract {
                    success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                        path: "src/main.rs".into(),
                        content: long('x', 8193),
                    },
                    ..base()
                },
                false,
            ),
            (
                "digest length mismatch",
                GoalPlanContract {
                    success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
                        path: "src/main.rs".into(),
                        sha256: "a".repeat(63),
                    },
                    ..base()
                },
                false,
            ),
        ];
        cases.push(("base", base(), true));
        for (name, contract, admitted) in cases {
            assert_eq!(contract.validate().is_ok(), admitted, "{name}");
        }
    }
}

impl GoalPlanContract {
    pub(crate) fn allows_call(&self, call: &crate::tools::ToolCall) -> bool {
        use crate::tools::ToolCall;
        match call {
            ToolCall::List { path }
            | ToolCall::Read { path, .. }
            | ToolCall::Search { path, .. }
            | ToolCall::Mkdir { path }
            | ToolCall::Write { path, .. }
            | ToolCall::Edit { path, .. }
            | ToolCall::GitStatus { path }
            | ToolCall::GitDiff { path, .. }
            | ToolCall::GitLog { path, .. } => self.allows_path(path),
            ToolCall::Command { program, .. } => self.commands.contains(program),
            ToolCall::ApplyChangeset { changeset, .. } => changeset
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
                if !call.valid()
                    || matches!(
                        call.as_ref(),
                        crate::tools::ToolCall::List { .. }
                            | crate::tools::ToolCall::Read { .. }
                            | crate::tools::ToolCall::Search { .. }
                            | crate::tools::ToolCall::ApplyChangeset { .. }
                            | crate::tools::ToolCall::McpList { .. }
                            | crate::tools::ToolCall::GitStatus { .. }
                            | crate::tools::ToolCall::GitDiff { .. }
                            | crate::tools::ToolCall::GitLog { .. }
                    )
                {
                    return Err(ReplayError::InvalidPayload);
                }
            }
            GoalSuccessCriterion::WorkspaceFileDigest { path, sha256 } => {
                if !crate::tools::types::valid_relative(path, false)
                    || !crate::tools::types::valid_digest(sha256)
                {
                    return Err(ReplayError::InvalidPayload);
                }
            }
            GoalSuccessCriterion::WorkspaceFileContent { path, content } => {
                if !crate::tools::types::valid_relative(path, false) || content.len() > 8 * 1024 {
                    return Err(ReplayError::InvalidPayload);
                }
            }
        }
        if self.paths.iter().any(|path| {
            path != "." && !crate::tools::types::valid_relative(path, false)
                || path.len() > 512
                || self
                    .paths
                    .iter()
                    .any(|other| other != path && Path::new(path).starts_with(other))
        }) || self.paths.contains(&".".to_owned()) && self.paths.len() != 1
            || self
                .commands
                .iter()
                .any(|command| !crate::tools::types::valid_name(command) || command.len() > 64)
            || self.skills.iter().any(|binding| {
                !crate::tools::types::valid_name(&binding.name)
                    || binding.name.len() > 64
                    || binding.resource.as_ref().is_some_and(|resource| {
                        !crate::tools::types::valid_relative(resource, false)
                    })
            })
            || self.mcp_tools.iter().any(|binding| {
                !crate::tools::types::valid_name(&binding.server)
                    || binding.server.len() > 64
                    || !crate::tools::types::valid_name(&binding.tool)
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentRunId(Uuid);

impl AgentRunId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}
impl Default for AgentRunId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for AgentRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for AgentRunId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CollaborationPolicy {
    Single,
    Auto { max_active_children: u8 },
    Team { max_active_children: u8 },
}

impl Default for CollaborationPolicy {
    fn default() -> Self {
        Self::Auto {
            max_active_children: 3,
        }
    }
}
impl CollaborationPolicy {
    pub fn max_children(self) -> u8 {
        match self {
            Self::Single => 0,
            Self::Auto {
                max_active_children,
            }
            | Self::Team {
                max_active_children,
            } => max_active_children,
        }
    }
    pub(crate) fn validate(self) -> Result<(), ReplayError> {
        match self {
            Self::Single => Ok(()),
            Self::Auto {
                max_active_children,
            } if max_active_children <= 8 => Ok(()),
            Self::Team {
                max_active_children,
            } if (1..=8).contains(&max_active_children) => Ok(()),
            _ => Err(ReplayError::InvalidPayload),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDefaults {
    #[serde(deserialize_with = "Option::deserialize")]
    pub provider: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub model: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub effort: Option<crate::provider::Effort>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub account_id: Option<Uuid>,
    pub policy: CollaborationPolicy,
}

impl SessionDefaults {
    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        if self.model.is_some() && self.provider.is_none()
            || self.effort.is_some() && self.model.is_none()
            || self.account_id.is_some()
                && (!matches!(
                    self.provider.as_deref(),
                    Some("openai" | "anthropic" | "chatgpt")
                ) || self.provider.as_deref() != Some("chatgpt") && self.model.is_none())
            || self.provider.as_deref() == Some("chatgpt") && self.account_id.is_none()
            || self
                .account_id
                .is_some_and(|id| id.get_version() != Some(Version::SortRand))
        {
            return Err(ReplayError::InvalidPayload);
        }
        if let Some(provider) = &self.provider {
            bounded(provider, 128)?;
        }
        if let Some(model) = &self.model {
            bounded(model, 128)?;
        }
        self.policy.validate()
    }
}

#[cfg(test)]
mod account_default_tests {
    use super::*;

    #[test]
    fn saved_account_binding_requires_a_native_selected_provider() {
        let id = Uuid::now_v7();
        let mut defaults = SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: None,
            account_id: Some(id),
            policy: CollaborationPolicy::Single,
        };
        assert!(defaults.validate().is_ok());
        defaults.provider = Some("custom:local".into());
        assert!(defaults.validate().is_err());
        defaults.provider = Some("openai".into());
        defaults.model = None;
        assert!(defaults.validate().is_err());
        defaults.model = Some("gpt-5.4".into());
        defaults.account_id = Some(Uuid::nil());
        assert!(defaults.validate().is_err());
        defaults.account_id = Some(id);
        defaults.provider = Some("chatgpt".into());
        defaults.model = None;
        assert!(defaults.validate().is_ok());
        defaults.account_id = None;
        assert!(defaults.validate().is_err());
    }

    #[test]
    fn current_defaults_require_explicit_nullable_fields() {
        let defaults = SessionDefaults::default();
        let encoded = serde_json::to_value(&defaults).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionDefaults>(encoded.clone()).unwrap(),
            defaults
        );
        for field in ["provider", "model", "effort", "account_id"] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<SessionDefaults>(missing).is_err());
        }
    }

    #[test]
    fn goal_budget_requires_explicit_nullable_token_limit() {
        let budget = GoalBudget::default();
        let encoded = serde_json::to_value(budget).unwrap();
        assert!(encoded["max_tokens"].is_null());
        assert_eq!(
            serde_json::from_value::<GoalBudget>(encoded.clone()).unwrap(),
            budget
        );
        for field in [
            "max_tokens",
            "max_provider_calls",
            "max_active_ms",
            "max_runs",
        ] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<GoalBudget>(missing).is_err());
        }
        let mut unlimited = encoded;
        unlimited["max_provider_calls"] = serde_json::Value::Null;
        unlimited["max_active_ms"] = serde_json::Value::Null;
        let decoded = serde_json::from_value::<GoalBudget>(unlimited).unwrap();
        assert_eq!(decoded.max_provider_calls, None);
        assert_eq!(decoded.max_active_ms, None);
    }

    #[test]
    fn run_account_provenance_separates_api_keys_and_subscription_bounds() {
        let mut config = RunConfig {
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            effort: None,
            custom_profile_provenance: None,
            saved_api_account_id: Some(Uuid::now_v7()),
            chatgpt_provenance: None,
            output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
            policy: CollaborationPolicy::Single,
            output_token_cap: 4096,
            provider_concurrency: 1,
            workspace_device: 1,
            workspace_inode: 1,
            instruction_digest: None,
            include_digests: Vec::new(),
            history_run_ids: Vec::new(),
            excluded_history_runs: 0,
            context_usage: None,
            compaction_event_sequence: None,
            compaction_content_digest: None,
            tool_policy: None,
        };
        assert!(config.validate().is_ok());
        config.output_token_cap = crate::provider::MAX_OUTPUT_TOKEN_CAP;
        assert!(config.validate().is_ok());
        for (cap, valid) in [
            (0, false),
            (4096, true),
            (crate::provider::MAX_OUTPUT_TOKEN_CAP, true),
            (crate::provider::MAX_OUTPUT_TOKEN_CAP + 1, false),
        ] {
            config.output_token_cap = cap;
            assert_eq!(config.validate().is_ok(), valid, "output cap {cap}");
        }
        config.output_token_cap = 4096;
        config.saved_api_account_id = Some(Uuid::nil());
        assert!(matches!(
            config.validate(),
            Err(ReplayError::InvalidPayload)
        ));
        config.saved_api_account_id = Some(Uuid::now_v7());
        config.provider = "scripted".into();
        assert!(matches!(
            config.validate(),
            Err(ReplayError::InvalidPayload)
        ));
        config.saved_api_account_id = None;
        assert!(config.validate().is_ok());
        let decoded: RunConfig =
            serde_json::from_value(serde_json::to_value(&config).expect("current JSON"))
                .expect("current Run config");
        assert_eq!(decoded.saved_api_account_id, None);
        assert_eq!(decoded.chatgpt_provenance, None);
        assert_eq!(
            decoded.output_token_bound,
            crate::provider::OutputTokenBound::ProviderEnforced
        );

        config.provider = "chatgpt".into();
        config.model = "visible-model".into();
        config.effort = Some(crate::provider::Effort::High);
        config.chatgpt_provenance = Some(crate::provider::ChatGptProvenance {
            account_id: Uuid::now_v7(),
            evidence_fingerprint: [7; 32],
            admission: crate::provider::ChatGptAdmission::AccountConsent,
        });
        assert!(config.validate().is_err());
        config.output_token_bound = crate::provider::OutputTokenBound::LocalAcceptanceOnly;
        assert!(config.validate().is_ok());
        config.saved_api_account_id = Some(Uuid::now_v7());
        assert!(config.validate().is_err());
        config.saved_api_account_id = None;
        config.chatgpt_provenance.as_mut().unwrap().account_id = Uuid::nil();
        assert!(config.validate().is_err());
        config.chatgpt_provenance.as_mut().unwrap().account_id = Uuid::now_v7();
        config
            .chatgpt_provenance
            .as_mut()
            .unwrap()
            .evidence_fingerprint = [0; 32];
        assert!(config.validate().is_err());
        config
            .chatgpt_provenance
            .as_mut()
            .unwrap()
            .evidence_fingerprint = [7; 32];
        config.provider_concurrency = 2;
        assert!(config.validate().is_err());
        config.provider_concurrency = 1;
        assert!(config.validate().is_ok());
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(encoded["output_token_bound"], "local_acceptance_only");
        assert_eq!(encoded["chatgpt_provenance"]["evidence_fingerprint"][0], 7);
        assert_eq!(
            encoded["chatgpt_provenance"]["admission"],
            "account_consent"
        );
        assert_eq!(
            serde_json::from_value::<RunConfig>(encoded.clone()).unwrap(),
            config
        );
        for field in [
            "effort",
            "custom_profile_provenance",
            "saved_api_account_id",
            "chatgpt_provenance",
            "output_token_bound",
            "provider_concurrency",
            "context_usage",
            "compaction_event_sequence",
            "compaction_content_digest",
            "tool_policy",
        ] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<RunConfig>(missing).is_err());
        }
        let mut missing_admission = encoded.clone();
        missing_admission["chatgpt_provenance"]
            .as_object_mut()
            .unwrap()
            .remove("admission");
        assert!(serde_json::from_value::<RunConfig>(missing_admission).is_err());
        for (field, value) in [
            ("admission", serde_json::json!("unchecked")),
            ("admission", serde_json::Value::Null),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut invalid = encoded.clone();
            invalid["chatgpt_provenance"][field] = value;
            assert!(serde_json::from_value::<RunConfig>(invalid).is_err());
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextUsage {
    pub used_bytes: u32,
    pub budget_bytes: u32,
    pub compactable_bytes: u32,
    pub tool_history_bytes: u32,
}

impl ContextUsage {
    pub fn utilization_percent(&self) -> u32 {
        let budget = u64::from(self.budget_bytes).max(1);
        ((u64::from(self.used_bytes) * 100 + budget / 2) / budget).min(100) as u32
    }

    pub fn near_limit(&self) -> bool {
        u64::from(self.used_bytes) * 5 >= u64::from(self.budget_bytes) * 4
            && u64::from(self.compactable_bytes) * 10 >= u64::from(self.budget_bytes)
    }

    fn valid(&self) -> bool {
        self.budget_bytes > 0
            && self.budget_bytes <= super::MAX_CONTEXT_CONTENT_BYTES as u32
            && self.used_bytes <= self.budget_bytes
            && self.compactable_bytes <= self.used_bytes
            && self.tool_history_bytes <= self.compactable_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub provider: String,
    pub model: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub effort: Option<crate::provider::Effort>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub custom_profile_provenance: Option<crate::provider::CustomProfileProvenance>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub saved_api_account_id: Option<Uuid>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub chatgpt_provenance: Option<crate::provider::ChatGptProvenance>,
    pub output_token_bound: crate::provider::OutputTokenBound,
    pub policy: CollaborationPolicy,
    pub output_token_cap: u32,
    pub provider_concurrency: u8,
    pub workspace_device: u64,
    pub workspace_inode: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub instruction_digest: Option<[u8; 32]>,
    pub include_digests: Vec<[u8; 32]>,
    pub history_run_ids: Vec<RunId>,
    pub excluded_history_runs: u32,
    #[serde(deserialize_with = "Option::deserialize")]
    pub context_usage: Option<ContextUsage>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub compaction_event_sequence: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub compaction_content_digest: Option<[u8; 32]>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub tool_policy: Option<crate::tools::ToolPolicyReceipt>,
}

impl RunConfig {
    pub fn auto_compaction_due(&self) -> bool {
        self.context_usage.as_ref().is_some_and(|usage| {
            usage.near_limit()
                || usage.compactable_bytes as usize
                    >= super::compaction::AUTO_COMPACTION_HISTORY_BYTES
        }) || self.history_run_ids.len() + 1 >= super::MAX_HISTORY_RUNS * 3 / 4
    }

    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        if !(crate::provider::ProviderIdentity {
            profile: &self.provider,
            model: &self.model,
            effort: self.effort,
            concurrency: self.provider_concurrency,
            custom_profile_provenance: self.custom_profile_provenance.as_ref(),
            saved_api_account_id: self.saved_api_account_id,
            chatgpt_provenance: self.chatgpt_provenance.as_ref(),
            output_token_bound: self.output_token_bound,
        })
        .valid()
            || !(1..=crate::provider::MAX_OUTPUT_TOKEN_CAP).contains(&self.output_token_cap)
            || self
                .tool_policy
                .as_ref()
                .is_some_and(|policy| !policy.valid())
            || self.provider.starts_with("custom:") && self.tool_policy.is_some()
            || self.include_digests.len() > 16
            || self.history_run_ids.len() > super::MAX_HISTORY_RUNS
            || self.compaction_event_sequence.is_some() != self.compaction_content_digest.is_some()
            || self
                .context_usage
                .as_ref()
                .is_some_and(|usage| !usage.valid())
        {
            return Err(ReplayError::InvalidPayload);
        }
        self.policy.validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Primary,
    Child,
}

/// Authority a child AgentRun receives at spawn; `ReadOnly` children may only research the Workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildGrant {
    NoTools,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDisposition {
    Finished,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunDisposition {
    Finished,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionFailure {
    ProviderUnavailable,
    ProviderRejected,
    InvalidOutcome,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompactionStatus {
    Succeeded {
        summary: String,
        content_digest: [u8; 32],
        summary_bytes: u32,
        summary_tokens_estimate: u32,
    },
    Failed {
        reason: CompactionFailure,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionRecord {
    pub covered_run_id: RunId,
    pub covered_sequence: u64,
    pub source_digest: [u8; 32],
    pub provider: String,
    pub model: String,
    pub prompt_version: u16,
    pub schema_version: u16,
    pub compiler_version: u16,
    pub source_bytes: u32,
    pub source_tokens_estimate: u32,
    #[serde(deserialize_with = "Option::deserialize")]
    pub input_tokens: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub output_tokens: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub response_id: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub wire_provenance: Option<crate::provider::ProviderWireProvenance>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub chatgpt_provenance: Option<crate::provider::ChatGptProvenance>,
    pub output_token_bound: crate::provider::OutputTokenBound,
    pub status: CompactionStatus,
}

impl CompactionRecord {
    fn usage(&self) -> crate::provider::Usage {
        crate::provider::Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        if self.covered_sequence == 0
            || self.prompt_version != 1
            || self.schema_version != 1
            || self.compiler_version != 3
            || self.source_bytes == 0
            || self.source_bytes > 256 * 1024
            || self.source_tokens_estimate != self.source_bytes.div_ceil(4)
            || self.input_tokens.is_some_and(|value| value > 1_000_000)
            || self.output_tokens.is_some_and(|value| value > 1_000_000)
            || !self.usage().cache_consistent()
            || self.response_id.as_ref().is_some_and(|value| {
                value.is_empty()
                    || value.len() > 128
                    || !value.bytes().all(|byte| byte.is_ascii_graphic())
            })
            || self.wire_provenance.is_some_and(|provenance| {
                !provenance.valid_for(
                    self.response_id.as_deref(),
                    self.input_tokens,
                    self.output_tokens,
                )
            })
            || (self.provider == "chatgpt") != self.chatgpt_provenance.is_some()
            || (self.provider == "chatgpt")
                != (self.output_token_bound
                    == crate::provider::OutputTokenBound::LocalAcceptanceOnly)
            || self
                .chatgpt_provenance
                .as_ref()
                .is_some_and(|provenance| !provenance.valid())
        {
            return Err(ReplayError::InvalidSnapshot);
        }
        bounded(&self.provider, 128).map_err(|_| ReplayError::InvalidSnapshot)?;
        bounded(&self.model, 128).map_err(|_| ReplayError::InvalidSnapshot)?;
        if let CompactionStatus::Succeeded {
            summary,
            summary_bytes,
            summary_tokens_estimate,
            ..
        } = &self.status
        {
            bounded(summary, super::MAX_COMPACTION_SUMMARY_BYTES)
                .map_err(|_| ReplayError::InvalidSnapshot)?;
            if *summary_bytes != summary.len() as u32
                || *summary_tokens_estimate != summary_bytes.div_ceil(4)
                || self
                    .output_tokens
                    .is_some_and(|count| count > super::MAX_COMPACTION_OUTPUT_TOKENS)
            {
                return Err(ReplayError::InvalidSnapshot);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod subscription_compaction_tests {
    use super::*;

    #[test]
    fn subscription_compaction_pins_account_and_weaker_bound() {
        let mut record = CompactionRecord {
            covered_run_id: RunId::new(),
            covered_sequence: 1,
            source_digest: [1; 32],
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            prompt_version: 1,
            schema_version: 1,
            compiler_version: 3,
            source_bytes: 4,
            source_tokens_estimate: 1,
            input_tokens: None,
            output_tokens: None,
            response_id: None,
            wire_provenance: None,
            chatgpt_provenance: None,
            output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
            status: CompactionStatus::Failed {
                reason: CompactionFailure::ProviderUnavailable,
            },
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert!(record.validate().is_ok());
        let encoded = serde_json::to_value(&record).unwrap();
        assert!(encoded["chatgpt_provenance"].is_null());
        assert_eq!(encoded["output_token_bound"], "provider_enforced");
        for field in [
            "wire_provenance",
            "chatgpt_provenance",
            "output_token_bound",
        ] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<CompactionRecord>(missing).is_err());
        }
        for compiler_version in [1, 2, 4] {
            let mut invalid = record.clone();
            invalid.compiler_version = compiler_version;
            assert!(invalid.validate().is_err());
        }
        record.provider = "chatgpt".into();
        record.chatgpt_provenance = Some(crate::provider::ChatGptProvenance {
            account_id: Uuid::now_v7(),
            evidence_fingerprint: [7; 32],
            admission: crate::provider::ChatGptAdmission::AccountConsent,
        });
        assert!(record.validate().is_err());
        record.output_token_bound = crate::provider::OutputTokenBound::LocalAcceptanceOnly;
        assert!(record.validate().is_ok());
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(
            encoded["chatgpt_provenance"]["admission"],
            "account_consent"
        );
        assert_eq!(
            serde_json::from_value::<CompactionRecord>(encoded.clone()).unwrap(),
            record
        );
        for (field, value) in [
            ("admission", serde_json::json!("unchecked")),
            ("admission", serde_json::Value::Null),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut invalid = encoded.clone();
            invalid["chatgpt_provenance"][field] = value;
            assert!(serde_json::from_value::<CompactionRecord>(invalid).is_err());
        }
        record
            .chatgpt_provenance
            .as_mut()
            .unwrap()
            .evidence_fingerprint = [0; 32];
        assert!(record.validate().is_err());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCallDisposition {
    Finished,
    Delegated,
    ToolRequested,
    Unavailable,
    Rejected,
    InvalidResponse,
    OutputLimit,
    TimedOut,
    Cancelled,
    TaskPanic,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallRecord {
    pub phase: crate::provider::AgentPhase,
    pub disposition: ProviderCallDisposition,
    pub requested_tools: u8,
    #[serde(deserialize_with = "Option::deserialize")]
    pub response_id: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub input_tokens: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub output_tokens: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub wire_provenance: Option<crate::provider::ProviderWireProvenance>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub failure_reason: Option<crate::provider::ProviderFailureReason>,
}

impl ProviderCallRecord {
    fn usage(&self) -> crate::provider::Usage {
        crate::provider::Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
        }
    }

    fn validate(&self) -> Result<(), ReplayError> {
        if self.requested_tools > MAX_STEP_TOOL_CALLS
            || (self.disposition == ProviderCallDisposition::ToolRequested)
                != (self.requested_tools > 0)
            || self.response_id.as_ref().is_some_and(|id| {
                id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic())
            })
            || self
                .input_tokens
                .is_some_and(|count| count > crate::provider::MAX_REPORTED_INPUT_TOKENS)
            || self
                .output_tokens
                .is_some_and(|count| count > crate::provider::MAX_OUTPUT_TOKEN_CAP)
            || !self.usage().cache_consistent()
            || self.wire_provenance.is_some_and(|provenance| {
                !provenance.valid_for(
                    self.response_id.as_deref(),
                    self.input_tokens,
                    self.output_tokens,
                )
            })
            || self.failure_reason.is_some_and(|reason| {
                use crate::provider::ProviderFailureReason;
                !matches!(
                    (self.disposition, reason),
                    (
                        ProviderCallDisposition::Rejected,
                        ProviderFailureReason::AccountAccess | ProviderFailureReason::UsageLimit
                    ) | (
                        ProviderCallDisposition::Unavailable,
                        ProviderFailureReason::UsageTemporarilyUnavailable
                            | ProviderFailureReason::ServiceUnavailable
                    ) | (
                        ProviderCallDisposition::InvalidResponse,
                        ProviderFailureReason::StreamProtocol
                            | ProviderFailureReason::ResponseContract
                            | ProviderFailureReason::OutcomeContract
                            | ProviderFailureReason::LocalOutputLimit
                    )
                )
            })
            || (!matches!(
                self.disposition,
                ProviderCallDisposition::Finished
                    | ProviderCallDisposition::Delegated
                    | ProviderCallDisposition::ToolRequested
                    | ProviderCallDisposition::InvalidResponse
            ) && (self.response_id.is_some()
                || self.input_tokens.is_some()
                || self.output_tokens.is_some()
                || self.cache_read_input_tokens.is_some()
                || self.cache_creation_input_tokens.is_some()
                || self.wire_provenance.is_some()))
        {
            return Err(ReplayError::InvalidPayload);
        }
        Ok(())
    }
}

#[cfg(test)]
mod provider_call_tests {
    use super::*;

    #[test]
    fn provider_call_record_v6_rejects_v5_and_inconsistent_cache_fields() {
        let run = RunId::new();
        let agent = AgentRunId::new();
        let mut record = ProviderCallRecord {
            phase: crate::provider::AgentPhase::RootPlan,
            disposition: ProviderCallDisposition::Finished,
            requested_tools: 0,
            response_id: Some("response-1".into()),
            input_tokens: Some(100),
            output_tokens: Some(10),
            cache_read_input_tokens: Some(40),
            cache_creation_input_tokens: Some(20),
            wire_provenance: None,
            failure_reason: None,
        };
        assert!(record.validate().is_ok());
        record.cache_read_input_tokens = Some(81);
        assert!(
            record.validate().is_err(),
            "cache parts exceed the input total"
        );
        record.cache_read_input_tokens = Some(40);
        record.input_tokens = None;
        assert!(
            record.validate().is_err(),
            "cache without a reported input total"
        );
        record.input_tokens = Some(100);
        record.requested_tools = 1;
        assert!(
            record.validate().is_err(),
            "a finished call declares no Tool"
        );
        record.disposition = ProviderCallDisposition::ToolRequested;
        record.requested_tools = 0;
        assert!(
            record.validate().is_err(),
            "a Tool request declares at least one Tool"
        );
        record.requested_tools = MAX_STEP_TOOL_CALLS + 1;
        assert!(record.validate().is_err(), "above the step ceiling");
        record.requested_tools = MAX_STEP_TOOL_CALLS;
        assert!(record.validate().is_ok());

        let event = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: agent,
            record: record.clone(),
        };
        let payload = event.payload().expect("current payload");
        assert_eq!(
            Event::decode(
                "ProviderCallRecorded",
                EVENT_PAYLOAD_VERSION,
                &payload,
                Some(run),
                Some(agent),
            )
            .expect("current replay"),
            event
        );
        assert!(
            Event::decode(
                "ProviderCallRecorded",
                EVENT_PAYLOAD_VERSION - 1,
                &payload,
                Some(run),
                Some(agent),
            )
            .is_err(),
            "payload version 5 is not current"
        );
    }

    #[test]
    fn rejects_unbounded_or_unearned_provider_metadata() {
        let mut record = ProviderCallRecord {
            phase: crate::provider::AgentPhase::RootPlan,
            disposition: ProviderCallDisposition::Finished,
            response_id: Some("response-1".into()),
            input_tokens: Some(100),
            output_tokens: Some(10),
            wire_provenance: None,
            failure_reason: None,
            requested_tools: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert!(record.validate().is_ok());
        record.response_id = Some("bad\nidentifier".into());
        assert!(record.validate().is_err());
        record.response_id = Some("r".repeat(129));
        assert!(record.validate().is_err());
        record.response_id = None;
        record.input_tokens = Some(crate::provider::MAX_REPORTED_INPUT_TOKENS + 1);
        assert!(record.validate().is_err());
        record.input_tokens = Some(100);
        record.output_tokens = Some(crate::provider::MAX_OUTPUT_TOKEN_CAP + 1);
        assert!(record.validate().is_err());
        record.output_tokens = Some(crate::provider::MAX_OUTPUT_TOKEN_CAP);
        assert!(record.validate().is_ok());
        record.output_tokens = None;
        record.disposition = ProviderCallDisposition::Unavailable;
        record.input_tokens = Some(100);
        assert!(record.validate().is_err());
        record.input_tokens = None;
        assert!(record.validate().is_ok());
        record.disposition = ProviderCallDisposition::Finished;
        record.response_id = Some("response-1".into());
        record.wire_provenance =
            Some(crate::provider::ProviderWireProvenance::MessagesEndTurnStorageUnspecified);
        assert!(record.validate().is_err());
        record.input_tokens = Some(100);
        record.output_tokens = Some(10);
        assert!(record.validate().is_ok());
        record.disposition = ProviderCallDisposition::TimedOut;
        assert!(record.validate().is_err());
        record.disposition = ProviderCallDisposition::Finished;
        record.wire_provenance = None;
        let encoded = serde_json::to_value(&record).expect("current payload");
        assert!(encoded["wire_provenance"].is_null());
        assert!(encoded["failure_reason"].is_null());
        let decoded: ProviderCallRecord = serde_json::from_value(encoded).expect("current replay");
        assert_eq!(decoded, record);

        use crate::provider::ProviderFailureReason;
        let run = RunId::new();
        let agent = AgentRunId::new();
        let run_id = Some(run);
        let agent_run_id = Some(agent);
        let event = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: agent,
            record: decoded,
        };
        let payload = event.payload().expect("current payload");
        assert_eq!(event.version(), EVENT_PAYLOAD_VERSION);
        assert_eq!(
            Event::decode(
                "ProviderCallRecorded",
                EVENT_PAYLOAD_VERSION,
                &payload,
                run_id,
                agent_run_id
            )
            .unwrap(),
            event
        );
        for version in [0, 1, 2, 3, 4, 5, EVENT_PAYLOAD_VERSION + 1] {
            assert!(
                Event::decode(
                    "ProviderCallRecorded",
                    version,
                    &payload,
                    run_id,
                    agent_run_id
                )
                .is_err()
            );
        }
        for (reason, expected) in [
            (
                ProviderFailureReason::AccountAccess,
                ProviderCallDisposition::Rejected,
            ),
            (
                ProviderFailureReason::UsageLimit,
                ProviderCallDisposition::Rejected,
            ),
            (
                ProviderFailureReason::UsageTemporarilyUnavailable,
                ProviderCallDisposition::Unavailable,
            ),
            (
                ProviderFailureReason::ServiceUnavailable,
                ProviderCallDisposition::Unavailable,
            ),
            (
                ProviderFailureReason::StreamProtocol,
                ProviderCallDisposition::InvalidResponse,
            ),
            (
                ProviderFailureReason::ResponseContract,
                ProviderCallDisposition::InvalidResponse,
            ),
            (
                ProviderFailureReason::OutcomeContract,
                ProviderCallDisposition::InvalidResponse,
            ),
            (
                ProviderFailureReason::LocalOutputLimit,
                ProviderCallDisposition::InvalidResponse,
            ),
        ] {
            for disposition in [
                ProviderCallDisposition::Finished,
                ProviderCallDisposition::Delegated,
                ProviderCallDisposition::Unavailable,
                ProviderCallDisposition::Rejected,
                ProviderCallDisposition::InvalidResponse,
                ProviderCallDisposition::OutputLimit,
                ProviderCallDisposition::TimedOut,
                ProviderCallDisposition::Cancelled,
                ProviderCallDisposition::TaskPanic,
            ] {
                let record = ProviderCallRecord {
                    phase: crate::provider::AgentPhase::RootPlan,
                    disposition,
                    response_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    wire_provenance: None,
                    failure_reason: Some(reason),
                    requested_tools: 0,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                };
                assert_eq!(
                    record.validate().is_ok(),
                    disposition == expected,
                    "{disposition:?}/{reason:?}"
                );
                if disposition != expected {
                    continue;
                }
                let event = Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: agent,
                    record,
                };
                let payload = event.payload().expect("current payload");
                assert_eq!(event.version(), EVENT_PAYLOAD_VERSION);
                assert_eq!(
                    Event::decode(
                        "ProviderCallRecorded",
                        EVENT_PAYLOAD_VERSION,
                        &payload,
                        run_id,
                        agent_run_id,
                    )
                    .expect("current replay"),
                    event
                );
                for version in [0, 1, 2, 3, 4, 5, EVENT_PAYLOAD_VERSION + 1] {
                    assert!(
                        Event::decode(
                            "ProviderCallRecorded",
                            version,
                            &payload,
                            run_id,
                            agent_run_id
                        )
                        .is_err(),
                        "version {version}"
                    );
                }
                for scope in [(None, agent_run_id), (run_id, None)] {
                    assert!(
                        Event::decode(
                            "ProviderCallRecorded",
                            EVENT_PAYLOAD_VERSION,
                            &payload,
                            scope.0,
                            scope.1
                        )
                        .is_err()
                    );
                }
                let mut invalid: serde_json::Value =
                    serde_json::from_str(&payload).expect("v4 JSON");
                invalid["failure_reason"] = "unknown_reason".into();
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        EVENT_PAYLOAD_VERSION,
                        &invalid.to_string(),
                        run_id,
                        agent_run_id
                    )
                    .is_err()
                );
                invalid["failure_reason"] = serde_json::Value::Null;
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        3,
                        &invalid.to_string(),
                        run_id,
                        agent_run_id
                    )
                    .is_err()
                );
                invalid.as_object_mut().unwrap().remove("failure_reason");
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        3,
                        &invalid.to_string(),
                        run_id,
                        agent_run_id
                    )
                    .is_err()
                );
            }
        }
    }
}

pub const MAX_DISPLAY_CHUNKS: usize = 64;
pub const MAX_DISPLAY_CHUNK_BYTES: usize = 2 * 1024;
pub const MAX_DISPLAY_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_DISPLAY_COMMENTARY_ITEMS: usize = 32;
pub const DISPLAY_LIMIT_TEXT: &str = "Some public commentary was omitted.";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayCategory {
    Answer,
    Commentary,
    Limit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayChunk {
    pub call_index: u32,
    pub phase: AgentPhase,
    pub category: DisplayCategory,
    pub item: u8,
    pub offset: u32,
    pub text: String,
}

impl DisplayChunk {
    fn validate(&self) -> Result<(), ReplayError> {
        if !matches!(self.phase, AgentPhase::RootPlan | AgentPhase::RootSynthesis)
            || self.item as usize >= MAX_DISPLAY_COMMENTARY_ITEMS
            || self.category == DisplayCategory::Answer && self.item != 0
            || self.category == DisplayCategory::Limit
                && (self.item != 0 || self.offset != 0 || self.text != DISPLAY_LIMIT_TEXT)
            || self.text.is_empty()
            || self.text.len() > MAX_DISPLAY_CHUNK_BYTES
            || self.offset as usize + self.text.len() > MAX_DISPLAY_TEXT_BYTES
        {
            return Err(ReplayError::InvalidPayload);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    SessionStarted {
        title: String,
        workspace_identity: (u64, u64),
    },
    SessionDefaultChanged {
        defaults: SessionDefaults,
    },
    SessionForked {
        title: String,
        source_session_id: SessionId,
        source_run_id: RunId,
        source_sequence: u64,
        prefix_digest: [u8; 32],
    },
    ContextCompacted {
        record: CompactionRecord,
    },
    SessionRenamed {
        title: String,
    },
    GoalStarted {
        goal_id: GoalId,
        objective: String,
        workspace_identity: (u64, u64),
        budget: GoalBudget,
        phase: GoalPhase,
        /// Engine iterates Runs on the pinned objective until a Goal stop condition.
        autonomous: bool,
        resource_settings: crate::tools::ToolResourceSettings,
    },
    GoalRunLinked {
        goal_id: GoalId,
        run_id: RunId,
        phase: GoalPhase,
    },
    GoalResumed {
        goal_id: GoalId,
    },
    GoalActiveTimeReserved {
        goal_id: GoalId,
        run_id: RunId,
        reserved_ms: u64,
    },
    GoalActiveTimeSettled {
        goal_id: GoalId,
        run_id: RunId,
        active_ms: u64,
    },
    GoalPlanContractRecorded {
        goal_id: GoalId,
        run_id: RunId,
        contract: GoalPlanContract,
    },
    GoalFinished {
        goal_id: GoalId,
        status: GoalStatus,
        active_ms: u64,
        report: GoalReport,
    },
    MessageAccepted {
        run_id: RunId,
        text: String,
        images: Vec<crate::provider::ImageAttachment>,
    },
    RunStarted {
        run_id: RunId,
        config: RunConfig,
    },
    AgentSpawned {
        run_id: RunId,
        agent_run_id: AgentRunId,
        role: AgentRole,
        ordinal: u8,
        objective: Option<String>,
        grant: Option<ChildGrant>,
    },
    ProviderCallRecorded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: ProviderCallRecord,
    },
    GoalProviderCallReserved {
        goal_id: GoalId,
        run_id: RunId,
        agent_run_id: AgentRunId,
    },
    GoalCompactionCallReserved {
        goal_id: GoalId,
        run_id: RunId,
    },
    GoalContextCompacted {
        goal_id: GoalId,
        run_id: RunId,
        covered_tool_count: u32,
        record: CompactionRecord,
    },
    GoalSteeringNoteRecorded {
        goal_id: GoalId,
        run_id: RunId,
        text: String,
    },
    AssistantDisplayRecorded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        chunk: DisplayChunk,
    },
    ToolStarted {
        run_id: RunId,
        agent_run_id: AgentRunId,
        intent: crate::tools::EffectIntent,
    },
    ToolFinished {
        run_id: RunId,
        agent_run_id: AgentRunId,
        observation: crate::tools::ToolObservation,
    },
    AgentFinished {
        run_id: RunId,
        agent_run_id: AgentRunId,
        disposition: AgentDisposition,
        summary: Option<String>,
        result: Option<String>,
    },
    MessageCommitted {
        run_id: RunId,
        text: String,
    },
    RunFinished {
        run_id: RunId,
        disposition: RunDisposition,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventEnvelope {
    pub sequence: u64,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub agent_run_id: Option<AgentRunId>,
    pub event: Event,
    pub created_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForkLineage {
    pub source_session_id: SessionId,
    pub source_run_id: RunId,
    pub source_sequence: u64,
    pub prefix_digest: [u8; 32],
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("invalid Event payload")]
    InvalidPayload,
    #[error("unknown Event kind or version")]
    UnknownEvent,
    #[error("invalid Session transition")]
    InvalidTransition,
    #[error("invalid compaction snapshot")]
    InvalidSnapshot,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TitlePayload {
    title: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartedPayload {
    title: String,
    workspace_device: u64,
    workspace_inode: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkPayload {
    title: String,
    source_session_id: SessionId,
    source_run_id: RunId,
    source_sequence: u64,
    prefix_digest: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextPayload {
    text: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagePayload {
    text: String,
    #[serde(deserialize_with = "crate::provider::deserialize_images")]
    images: Vec<crate::provider::ImageAttachment>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalStartedPayload {
    goal_id: GoalId,
    objective: String,
    workspace_device: u64,
    workspace_inode: u64,
    budget: GoalBudget,
    phase: GoalPhase,
    autonomous: bool,
    resource_settings: crate::tools::ToolResourceSettings,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalRunPayload {
    goal_id: GoalId,
    phase: GoalPhase,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalActiveTimePayload {
    goal_id: GoalId,
    active_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalIdPayload {
    goal_id: GoalId,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalContractPayload {
    goal_id: GoalId,
    contract: GoalPlanContract,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalFinishedPayload {
    goal_id: GoalId,
    status: GoalStatus,
    active_ms: u64,
    report: GoalReport,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalProviderCallPayload {
    goal_id: GoalId,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalContextCompactionPayload {
    goal_id: GoalId,
    covered_tool_count: u32,
    record: CompactionRecord,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalSteeringNotePayload {
    goal_id: GoalId,
    text: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnPayload {
    role: AgentRole,
    ordinal: u8,
    #[serde(deserialize_with = "Option::deserialize")]
    objective: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    grant: Option<ChildGrant>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFinishedPayload {
    disposition: AgentDisposition,
    #[serde(deserialize_with = "Option::deserialize")]
    summary: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    result: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunFinishedPayload {
    disposition: RunDisposition,
}

impl Event {
    pub(crate) fn finished_payload_size(summary: &str, result: &str) -> Result<usize, ReplayError> {
        serialize(&AgentFinishedPayload {
            disposition: AgentDisposition::Finished,
            summary: Some(summary.into()),
            result: Some(result.into()),
        })
        .map(|payload| payload.len())
    }

    pub(crate) fn version(&self) -> i64 {
        EVENT_PAYLOAD_VERSION
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "SessionStarted",
            Self::SessionDefaultChanged { .. } => "SessionDefaultChanged",
            Self::SessionForked { .. } => "SessionForked",
            Self::ContextCompacted { .. } => "ContextCompacted",
            Self::SessionRenamed { .. } => "SessionRenamed",
            Self::GoalStarted { .. } => "GoalStarted",
            Self::GoalRunLinked { .. } => "GoalRunLinked",
            Self::GoalResumed { .. } => "GoalResumed",
            Self::GoalActiveTimeReserved { .. } => "GoalActiveTimeReserved",
            Self::GoalActiveTimeSettled { .. } => "GoalActiveTimeSettled",
            Self::GoalPlanContractRecorded { .. } => "GoalPlanContractRecorded",
            Self::GoalFinished { .. } => "GoalFinished",
            Self::MessageAccepted { .. } => "MessageAccepted",
            Self::RunStarted { .. } => "RunStarted",
            Self::AgentSpawned { .. } => "AgentSpawned",
            Self::ProviderCallRecorded { .. } => "ProviderCallRecorded",
            Self::GoalProviderCallReserved { .. } => "GoalProviderCallReserved",
            Self::GoalCompactionCallReserved { .. } => "GoalCompactionCallReserved",
            Self::GoalContextCompacted { .. } => "GoalContextCompacted",
            Self::GoalSteeringNoteRecorded { .. } => "GoalSteeringNoteRecorded",
            Self::AssistantDisplayRecorded { .. } => "AssistantDisplayRecorded",
            Self::ToolStarted { .. } => "ToolStarted",
            Self::ToolFinished { .. } => "ToolFinished",
            Self::AgentFinished { .. } => "AgentFinished",
            Self::MessageCommitted { .. } => "MessageCommitted",
            Self::RunFinished { .. } => "RunFinished",
        }
    }

    pub(crate) fn scope(&self) -> (Option<RunId>, Option<AgentRunId>) {
        match self {
            Self::SessionStarted { .. }
            | Self::SessionDefaultChanged { .. }
            | Self::SessionForked { .. }
            | Self::ContextCompacted { .. }
            | Self::SessionRenamed { .. }
            | Self::GoalStarted { .. }
            | Self::GoalResumed { .. }
            | Self::GoalFinished { .. } => (None, None),
            Self::GoalRunLinked { run_id, .. }
            | Self::GoalActiveTimeReserved { run_id, .. }
            | Self::GoalActiveTimeSettled { run_id, .. }
            | Self::GoalPlanContractRecorded { run_id, .. }
            | Self::GoalCompactionCallReserved { run_id, .. }
            | Self::GoalContextCompacted { run_id, .. }
            | Self::GoalSteeringNoteRecorded { run_id, .. }
            | Self::MessageAccepted { run_id, .. }
            | Self::RunStarted { run_id, .. }
            | Self::MessageCommitted { run_id, .. }
            | Self::RunFinished { run_id, .. } => (Some(*run_id), None),
            Self::AgentSpawned {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ProviderCallRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::GoalProviderCallReserved {
                run_id,
                agent_run_id,
                ..
            }
            | Self::AssistantDisplayRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolStarted {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolFinished {
                run_id,
                agent_run_id,
                ..
            }
            | Self::AgentFinished {
                run_id,
                agent_run_id,
                ..
            } => (Some(*run_id), Some(*agent_run_id)),
        }
    }

    pub(crate) fn payload(&self) -> Result<String, ReplayError> {
        self.validate()?;
        match self {
            Self::SessionStarted {
                title,
                workspace_identity: (workspace_device, workspace_inode),
            } => serialize(&StartedPayload {
                title: title.clone(),
                workspace_device: *workspace_device,
                workspace_inode: *workspace_inode,
            }),
            Self::SessionRenamed { title } => serialize(&TitlePayload {
                title: title.clone(),
            }),
            Self::SessionDefaultChanged { defaults } => serialize(defaults),
            Self::SessionForked {
                title,
                source_session_id,
                source_run_id,
                source_sequence,
                prefix_digest,
            } => serialize(&ForkPayload {
                title: title.clone(),
                source_session_id: *source_session_id,
                source_run_id: *source_run_id,
                source_sequence: *source_sequence,
                prefix_digest: *prefix_digest,
            }),
            Self::ContextCompacted { record } => serialize(record),
            Self::GoalStarted {
                goal_id,
                objective,
                workspace_identity: (workspace_device, workspace_inode),
                budget,
                phase,
                autonomous,
                resource_settings,
            } => serialize(&GoalStartedPayload {
                goal_id: *goal_id,
                objective: objective.clone(),
                workspace_device: *workspace_device,
                workspace_inode: *workspace_inode,
                budget: *budget,
                phase: *phase,
                autonomous: *autonomous,
                resource_settings: *resource_settings,
            }),
            Self::GoalRunLinked { goal_id, phase, .. } => serialize(&GoalRunPayload {
                goal_id: *goal_id,
                phase: *phase,
            }),
            Self::GoalResumed { goal_id } => serialize(&GoalIdPayload { goal_id: *goal_id }),
            Self::GoalActiveTimeReserved {
                goal_id,
                reserved_ms,
                ..
            } => serialize(&GoalActiveTimePayload {
                goal_id: *goal_id,
                active_ms: *reserved_ms,
            }),
            Self::GoalActiveTimeSettled {
                goal_id, active_ms, ..
            } => serialize(&GoalActiveTimePayload {
                goal_id: *goal_id,
                active_ms: *active_ms,
            }),
            Self::GoalPlanContractRecorded {
                goal_id, contract, ..
            } => serialize(&GoalContractPayload {
                goal_id: *goal_id,
                contract: contract.clone(),
            }),
            Self::GoalFinished {
                goal_id,
                status,
                active_ms,
                report,
            } => serialize(&GoalFinishedPayload {
                goal_id: *goal_id,
                status: *status,
                active_ms: *active_ms,
                report: report.clone(),
            }),
            Self::MessageAccepted { text, images, .. } => serialize(&MessagePayload {
                text: text.clone(),
                images: images.clone(),
            }),
            Self::MessageCommitted { text, .. } => serialize(&TextPayload { text: text.clone() }),
            Self::RunStarted { config, .. } => serialize(config),
            Self::AgentSpawned {
                role,
                ordinal,
                objective,
                grant,
                ..
            } => serialize(&SpawnPayload {
                role: *role,
                ordinal: *ordinal,
                objective: objective.clone(),
                grant: *grant,
            }),
            Self::ProviderCallRecorded { record, .. } => serialize(record),
            Self::GoalProviderCallReserved { goal_id, .. } => {
                serialize(&GoalProviderCallPayload { goal_id: *goal_id })
            }
            Self::GoalCompactionCallReserved { goal_id, .. } => {
                serialize(&GoalIdPayload { goal_id: *goal_id })
            }
            Self::GoalContextCompacted {
                goal_id,
                covered_tool_count,
                record,
                ..
            } => serialize(&GoalContextCompactionPayload {
                goal_id: *goal_id,
                covered_tool_count: *covered_tool_count,
                record: record.clone(),
            }),
            Self::GoalSteeringNoteRecorded { goal_id, text, .. } => {
                serialize(&GoalSteeringNotePayload {
                    goal_id: *goal_id,
                    text: text.clone(),
                })
            }
            Self::AssistantDisplayRecorded { chunk, .. } => serialize(chunk),
            Self::ToolStarted { intent, .. } => serialize(intent),
            Self::ToolFinished { observation, .. } => serialize(observation),
            Self::AgentFinished {
                disposition,
                summary,
                result,
                ..
            } => serialize(&AgentFinishedPayload {
                disposition: *disposition,
                summary: summary.clone(),
                result: result.clone(),
            }),
            Self::RunFinished { disposition, .. } => serialize(&RunFinishedPayload {
                disposition: *disposition,
            }),
        }
    }

    pub(crate) fn decode(
        kind: &str,
        version: i64,
        payload: &str,
        run_id: Option<RunId>,
        agent_run_id: Option<AgentRunId>,
    ) -> Result<Self, ReplayError> {
        if version != EVENT_PAYLOAD_VERSION {
            return Err(ReplayError::UnknownEvent);
        }
        let event = match kind {
            "SessionStarted" if run_id.is_none() && agent_run_id.is_none() => {
                let data: StartedPayload = parse(payload)?;
                Self::SessionStarted {
                    title: data.title,
                    workspace_identity: (data.workspace_device, data.workspace_inode),
                }
            }
            "SessionDefaultChanged" if run_id.is_none() && agent_run_id.is_none() => {
                Self::SessionDefaultChanged {
                    defaults: parse(payload)?,
                }
            }
            "SessionForked" if run_id.is_none() && agent_run_id.is_none() => {
                let data: ForkPayload = parse(payload)?;
                Self::SessionForked {
                    title: data.title,
                    source_session_id: data.source_session_id,
                    source_run_id: data.source_run_id,
                    source_sequence: data.source_sequence,
                    prefix_digest: data.prefix_digest,
                }
            }
            "ContextCompacted" if run_id.is_none() && agent_run_id.is_none() => {
                Self::ContextCompacted {
                    record: parse(payload)?,
                }
            }
            "SessionRenamed" if run_id.is_none() && agent_run_id.is_none() => {
                Self::SessionRenamed {
                    title: parse::<TitlePayload>(payload)?.title,
                }
            }
            "GoalStarted" if run_id.is_none() && agent_run_id.is_none() => {
                let data: GoalStartedPayload = parse(payload)?;
                Self::GoalStarted {
                    goal_id: data.goal_id,
                    objective: data.objective,
                    workspace_identity: (data.workspace_device, data.workspace_inode),
                    budget: data.budget,
                    phase: data.phase,
                    autonomous: data.autonomous,
                    resource_settings: data.resource_settings,
                }
            }
            "GoalRunLinked" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalRunPayload = parse(payload)?;
                Self::GoalRunLinked {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    phase: data.phase,
                }
            }
            "GoalResumed" if run_id.is_none() && agent_run_id.is_none() => Self::GoalResumed {
                goal_id: parse::<GoalIdPayload>(payload)?.goal_id,
            },
            "GoalActiveTimeReserved" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalActiveTimePayload = parse(payload)?;
                Self::GoalActiveTimeReserved {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    reserved_ms: data.active_ms,
                }
            }
            "GoalActiveTimeSettled" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalActiveTimePayload = parse(payload)?;
                Self::GoalActiveTimeSettled {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    active_ms: data.active_ms,
                }
            }
            "GoalPlanContractRecorded" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalContractPayload = parse(payload)?;
                Self::GoalPlanContractRecorded {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    contract: data.contract,
                }
            }
            "GoalFinished" if run_id.is_none() && agent_run_id.is_none() => {
                let data: GoalFinishedPayload = parse(payload)?;
                Self::GoalFinished {
                    goal_id: data.goal_id,
                    status: data.status,
                    active_ms: data.active_ms,
                    report: data.report,
                }
            }
            "MessageAccepted" if run_id.is_some() && agent_run_id.is_none() => {
                let data: MessagePayload = parse(payload)?;
                Self::MessageAccepted {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    text: data.text,
                    images: data.images,
                }
            }
            "RunStarted" if run_id.is_some() && agent_run_id.is_none() => Self::RunStarted {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                config: parse(payload)?,
            },
            "AgentSpawned" if run_id.is_some() && agent_run_id.is_some() => {
                let data: SpawnPayload = parse(payload)?;
                Self::AgentSpawned {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    role: data.role,
                    ordinal: data.ordinal,
                    objective: data.objective,
                    grant: data.grant,
                }
            }
            "AgentFinished" if run_id.is_some() && agent_run_id.is_some() => {
                let data: AgentFinishedPayload = parse(payload)?;
                Self::AgentFinished {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    disposition: data.disposition,
                    summary: data.summary,
                    result: data.result,
                }
            }
            "ProviderCallRecorded" if run_id.is_some() && agent_run_id.is_some() => {
                Self::ProviderCallRecorded {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    record: parse(payload)?,
                }
            }
            "GoalProviderCallReserved" if run_id.is_some() && agent_run_id.is_some() => {
                Self::GoalProviderCallReserved {
                    goal_id: parse::<GoalProviderCallPayload>(payload)?.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                }
            }
            "GoalCompactionCallReserved" if run_id.is_some() && agent_run_id.is_none() => {
                Self::GoalCompactionCallReserved {
                    goal_id: parse::<GoalIdPayload>(payload)?.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                }
            }
            "GoalContextCompacted" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalContextCompactionPayload = parse(payload)?;
                Self::GoalContextCompacted {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    covered_tool_count: data.covered_tool_count,
                    record: data.record,
                }
            }
            "GoalSteeringNoteRecorded" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalSteeringNotePayload = parse(payload)?;
                Self::GoalSteeringNoteRecorded {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    text: data.text,
                }
            }
            "ToolStarted" if run_id.is_some() && agent_run_id.is_some() => Self::ToolStarted {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                intent: parse(payload)?,
            },
            "AssistantDisplayRecorded" if run_id.is_some() && agent_run_id.is_some() => {
                Self::AssistantDisplayRecorded {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    chunk: parse(payload)?,
                }
            }
            "ToolFinished" if run_id.is_some() && agent_run_id.is_some() => Self::ToolFinished {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                observation: parse(payload)?,
            },
            "MessageCommitted" if run_id.is_some() && agent_run_id.is_none() => {
                Self::MessageCommitted {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    text: parse::<TextPayload>(payload)?.text,
                }
            }
            "RunFinished" if run_id.is_some() && agent_run_id.is_none() => Self::RunFinished {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                disposition: parse::<RunFinishedPayload>(payload)?.disposition,
            },
            "SessionStarted"
            | "SessionDefaultChanged"
            | "SessionForked"
            | "ContextCompacted"
            | "SessionRenamed"
            | "GoalStarted"
            | "GoalRunLinked"
            | "GoalResumed"
            | "GoalActiveTimeReserved"
            | "GoalActiveTimeSettled"
            | "GoalPlanContractRecorded"
            | "GoalFinished"
            | "MessageAccepted"
            | "RunStarted"
            | "AgentSpawned"
            | "ProviderCallRecorded"
            | "GoalCompactionCallReserved"
            | "GoalContextCompacted"
            | "GoalSteeringNoteRecorded"
            | "AssistantDisplayRecorded"
            | "ToolStarted"
            | "ToolFinished"
            | "AgentFinished"
            | "MessageCommitted"
            | "RunFinished" => {
                return Err(ReplayError::InvalidPayload);
            }
            _ => return Err(ReplayError::UnknownEvent),
        };
        event.validate()?;
        Ok(event)
    }

    pub(super) fn validate(&self) -> Result<(), ReplayError> {
        let valid_goal_ids = match self {
            Self::GoalStarted { goal_id, .. }
            | Self::GoalRunLinked { goal_id, .. }
            | Self::GoalResumed { goal_id }
            | Self::GoalActiveTimeReserved { goal_id, .. }
            | Self::GoalActiveTimeSettled { goal_id, .. }
            | Self::GoalPlanContractRecorded { goal_id, .. }
            | Self::GoalCompactionCallReserved { goal_id, .. }
            | Self::GoalContextCompacted { goal_id, .. }
            | Self::GoalSteeringNoteRecorded { goal_id, .. }
            | Self::GoalFinished { goal_id, .. } => goal_id.valid(),
            _ => true,
        };
        if !valid_goal_ids {
            return Err(ReplayError::InvalidPayload);
        }
        match self {
            Self::SessionStarted { title, .. } | Self::SessionRenamed { title } => {
                bounded(title, 128)
            }
            Self::SessionDefaultChanged { defaults } => defaults.validate(),
            Self::SessionForked {
                title,
                source_sequence,
                ..
            } if *source_sequence > 0 => bounded(title, 128),
            Self::SessionForked { .. } => Err(ReplayError::InvalidPayload),
            Self::ContextCompacted { record } => record.validate(),
            Self::GoalStarted {
                objective,
                workspace_identity: (_, inode),
                budget,
                resource_settings,
                ..
            } => {
                if objective.len() > super::MAX_USER_MESSAGE_BYTES || *inode == 0 {
                    return Err(ReplayError::InvalidPayload);
                }
                budget.validate()?;
                if !resource_settings.valid() {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalRunLinked { .. } | Self::GoalResumed { .. } => Ok(()),
            Self::GoalActiveTimeReserved { reserved_ms, .. } => {
                if *reserved_ms == 0 || *reserved_ms > MAX_GOAL_ACTION_ACTIVE_MS {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalActiveTimeSettled { active_ms, .. } => {
                if *active_ms > MAX_GOAL_ACTION_ACTIVE_MS {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalCompactionCallReserved { .. } => Ok(()),
            Self::GoalContextCompacted {
                covered_tool_count,
                record,
                ..
            } => {
                if *covered_tool_count == 0 || record.validate().is_err() {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalPlanContractRecorded { contract, .. } => {
                contract.validate()?;
                Ok(())
            }
            Self::GoalSteeringNoteRecorded { text, .. } => {
                if text.trim().is_empty() || text.len() > super::MAX_STEERING_NOTE_BYTES {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalFinished { status, report, .. } => {
                if !matches!(status, GoalStatus::Completed | GoalStatus::Failed) {
                    return Err(ReplayError::InvalidPayload);
                }
                report.validate()
            }
            Self::MessageAccepted { text, images, .. } => {
                if text.len() > super::MAX_USER_MESSAGE_BYTES
                    || text.is_empty() && images.is_empty()
                    || !crate::provider::valid_image_collection(images)
                {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::MessageCommitted { text, .. } => {
                bounded(text, super::MAX_ASSISTANT_MESSAGE_BYTES)
            }
            Self::RunStarted { config, .. } => config.validate(),
            Self::AgentSpawned {
                role,
                ordinal,
                objective,
                grant,
                ..
            } => match (role, grant) {
                (AgentRole::Primary, None) if *ordinal == 0 && objective.is_none() => Ok(()),
                (AgentRole::Child, Some(_)) if (1..=8).contains(ordinal) => bounded(
                    objective.as_deref().ok_or(ReplayError::InvalidPayload)?,
                    2 * 1024,
                ),
                _ => Err(ReplayError::InvalidPayload),
            },
            Self::ProviderCallRecorded { record, .. } => record.validate(),
            Self::GoalProviderCallReserved { goal_id, .. } => {
                if goal_id.valid() {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::AssistantDisplayRecorded { chunk, .. } => chunk.validate(),
            Self::ToolStarted {
                run_id,
                agent_run_id,
                intent,
            } => {
                if intent.valid()
                    && intent.run_id == *run_id
                    && intent.agent_run_id == *agent_run_id
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ToolFinished {
                run_id,
                agent_run_id,
                observation,
            } => {
                if observation.valid()
                    && !(observation.cache_update == crate::tools::CacheUpdate::NotAttempted
                        && observation.disposition == crate::tools::ToolDisposition::Succeeded)
                    && observation.intent.run_id == *run_id
                    && observation.intent.agent_run_id == *agent_run_id
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::AgentFinished {
                disposition,
                summary,
                result,
                ..
            } => match disposition {
                AgentDisposition::Finished => {
                    bounded(
                        summary.as_deref().ok_or(ReplayError::InvalidPayload)?,
                        2 * 1024,
                    )?;
                    bounded(
                        result.as_deref().ok_or(ReplayError::InvalidPayload)?,
                        super::MAX_ASSISTANT_MESSAGE_BYTES,
                    )
                }
                AgentDisposition::Failed | AgentDisposition::Cancelled
                    if summary.is_none() && result.is_none() =>
                {
                    Ok(())
                }
                _ => Err(ReplayError::InvalidPayload),
            },
            Self::RunFinished { .. } => Ok(()),
        }
    }
}

fn bounded(value: &str, max: usize) -> Result<(), ReplayError> {
    if value.is_empty() || value.len() > max {
        return Err(ReplayError::InvalidPayload);
    }
    Ok(())
}

fn serialize<T: Serialize>(value: &T) -> Result<String, ReplayError> {
    serde_json::to_string(value).map_err(|_| ReplayError::InvalidPayload)
}

fn parse<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, ReplayError> {
    serde_json::from_str(value).map_err(|_| ReplayError::InvalidPayload)
}

#[cfg(test)]
mod child_grant_tests {
    use super::*;

    #[test]
    fn child_grant_round_trips_and_rejects_missing_or_misplaced_pins() {
        let (run_id, agent_run_id) = (RunId::new(), AgentRunId::new());
        let spawned = |role, ordinal, objective: Option<&str>, grant| Event::AgentSpawned {
            run_id,
            agent_run_id,
            role,
            ordinal,
            objective: objective.map(String::from),
            grant,
        };
        let decode = |event: &Event, payload: &str| {
            Event::decode(
                event.kind(),
                EVENT_PAYLOAD_VERSION,
                payload,
                Some(run_id),
                Some(agent_run_id),
            )
        };
        for grant in [ChildGrant::NoTools, ChildGrant::ReadOnly] {
            let event = spawned(AgentRole::Child, 1, Some("inspect"), Some(grant));
            let payload = event.payload().expect("child payload");
            assert_eq!(decode(&event, &payload).expect("child replay"), event);
        }
        let primary = spawned(AgentRole::Primary, 0, None, None);
        let payload = primary.payload().expect("primary payload");
        assert_eq!(decode(&primary, &payload).expect("primary replay"), primary);

        let child = spawned(
            AgentRole::Child,
            1,
            Some("inspect"),
            Some(ChildGrant::ReadOnly),
        );
        let payload = child.payload().expect("child payload");
        let mut value: serde_json::Value = serde_json::from_str(&payload).expect("JSON");
        assert!(
            Event::decode(
                child.kind(),
                EVENT_PAYLOAD_VERSION - 1,
                &payload,
                Some(run_id),
                Some(agent_run_id)
            )
            .is_err_and(|error| matches!(error, ReplayError::UnknownEvent))
        );
        let mut missing = value.clone();
        missing.as_object_mut().expect("object").remove("grant");
        assert!(decode(&child, &missing.to_string()).is_err());
        value["grant"] = serde_json::Value::Null;
        assert!(decode(&child, &value.to_string()).is_err());
        value["grant"] = "write".into();
        assert!(decode(&child, &value.to_string()).is_err());

        let primary_payload = primary.payload().expect("primary payload");
        let mut primary_value: serde_json::Value =
            serde_json::from_str(&primary_payload).expect("JSON");
        primary_value["grant"] = "no_tools".into();
        assert!(decode(&primary, &primary_value.to_string()).is_err());
    }
}
