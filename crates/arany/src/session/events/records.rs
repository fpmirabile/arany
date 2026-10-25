use super::*;
use crate::provider::{AgentPhase, ProviderKind};
use crate::tools::types::HostCommandPin;
use crate::tools::{EffectIntent, HostIntent, ToolCall, ToolLimits};
use crate::validate::MAX_PATH_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectTarget {
    Workspace,
    WorkerView {
        assignment_id: AgentRunId,
        base_digest: [u8; 32],
        revision: u32,
        #[serde(deserialize_with = "Option::deserialize")]
        cache_generation: Option<[u8; 32]>,
    },
}
impl EffectTarget {
    pub(crate) fn valid(&self) -> bool {
        match self {
            Self::Workspace => true,
            Self::WorkerView {
                assignment_id,
                base_digest,
                revision,
                cache_generation,
            } => {
                assignment_id.valid()
                    && *base_digest != [0; 32]
                    && *revision <= 16
                    && *cache_generation != Some([0; 32])
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolIntentRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_command: Option<HostCommandPin>,
    pub target: EffectTarget,
    pub id: Uuid,
    pub run_id: RunId,
    pub agent_run_id: AgentRunId,
    pub policy_digest: [u8; 32],
    pub enforcement_digest: [u8; 32],
    pub workspace_device: u64,
    pub workspace_inode: u64,
    pub call: ToolCall,
    pub limits: ToolLimits,
    pub use_count: u8,
}

impl From<EffectIntent> for ToolIntentRecord {
    fn from(intent: EffectIntent) -> Self {
        Self {
            host: intent.host,
            host_command: intent.host_command,
            target: intent.target,
            id: intent.id,
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            policy_digest: intent.policy_digest,
            enforcement_digest: intent.enforcement_digest,
            workspace_device: intent.workspace_device,
            workspace_inode: intent.workspace_inode,
            call: intent.call,
            limits: intent.limits,
            use_count: intent.use_count,
        }
    }
}

impl From<ToolIntentRecord> for EffectIntent {
    fn from(intent: ToolIntentRecord) -> Self {
        Self {
            host: intent.host,
            host_command: intent.host_command,
            target: intent.target,
            id: intent.id,
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            policy_digest: intent.policy_digest,
            enforcement_digest: intent.enforcement_digest,
            workspace_device: intent.workspace_device,
            workspace_inode: intent.workspace_inode,
            call: intent.call,
            limits: intent.limits,
            use_count: intent.use_count,
        }
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
        match self.provider.as_deref().and_then(ProviderKind::parse) {
            Some(ProviderKind::OpenAi | ProviderKind::Anthropic) => {
                if self.account_id.is_some() && self.model.is_none() {
                    return Err(ReplayError::InvalidPayload);
                }
            }
            Some(ProviderKind::ChatGpt) => {
                if self.account_id.is_none() {
                    return Err(ReplayError::InvalidPayload);
                }
            }
            Some(ProviderKind::Custom) | None => {
                if self.account_id.is_some() {
                    return Err(ReplayError::InvalidPayload);
                }
            }
        }
        if self.model.is_some() && self.provider.is_none()
            || self.effort.is_some() && self.model.is_none()
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
            && self.budget_bytes <= super::MAX_WINDOW_CONTENT_BYTES as u32
            && self.used_bytes <= self.budget_bytes
            && self.compactable_bytes <= self.used_bytes
            && self.tool_history_bytes <= self.compactable_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    #[serde(deserialize_with = "Option::deserialize")]
    pub supervisor: Option<crate::supervisor::SupervisorPolicy>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub worker_policy: Option<super::WorkerPolicy>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub agent_definitions: Option<crate::tools::DefinitionRegistryPin>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub roster: Option<Box<crate::provider::RosterSnapshot>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<crate::provider::ProviderRoutes>,
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
    #[serde(deserialize_with = "Option::deserialize")]
    pub context_window_tokens: Option<u32>,
    pub policy: CollaborationPolicy,
    pub output_token_cap: u32,
    pub provider_concurrency: u8,
    pub workspace_device: u64,
    pub workspace_inode: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub user_instruction_digest: Option<[u8; 32]>,
    pub instruction_sources: Vec<InstructionSource>,
    pub launch_directory: String,
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
    pub tool_policy: Option<Box<crate::tools::ToolPolicyReceipt>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionSource {
    pub directory: String,
    // Null only for a source migrated from version 18, which journaled the digest alone.
    #[serde(deserialize_with = "Option::deserialize")]
    pub file: Option<InstructionFile>,
    pub digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InstructionFile {
    #[serde(rename = "AGENTS.override.md")]
    AgentsOverride,
    #[serde(rename = "AGENTS.md")]
    Agents,
    #[serde(rename = "CLAUDE.md")]
    Claude,
}

impl InstructionFile {
    pub const DISCOVERY: [Self; 3] = [Self::AgentsOverride, Self::Agents, Self::Claude];

    pub const fn name(self) -> &'static str {
        match self {
            Self::AgentsOverride => "AGENTS.override.md",
            Self::Agents => "AGENTS.md",
            Self::Claude => "CLAUDE.md",
        }
    }
}

impl RunConfig {
    // Sources name strictly deeper directories on the path from the root to the launch directory.
    fn instruction_scope_valid(&self) -> bool {
        let components = |path: &str| -> Vec<String> {
            path.split('/')
                .filter(|_| !path.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let launch = components(&self.launch_directory);
        let mut depths = self.instruction_sources.iter().map(|source| {
            let directory = components(&source.directory);
            launch.starts_with(&directory).then_some(directory.len())
        });
        self.launch_directory.len() <= MAX_PATH_BYTES
            && launch.len() < crate::session::input::MAX_INSTRUCTION_LEVELS
            && launch
                .iter()
                .all(|name| !name.is_empty() && *name != "." && *name != "..")
            && depths
                .try_fold(None, |previous: Option<usize>, depth| {
                    depth
                        .filter(|depth| previous.is_none_or(|previous| *depth > previous))
                        .map(Some)
                })
                .is_some()
    }

    pub fn auto_compaction_due(&self) -> bool {
        self.context_usage.as_ref().is_some_and(|usage| {
            usage.near_limit()
                || usage.compactable_bytes as usize
                    >= super::compaction::AUTO_COMPACTION_HISTORY_BYTES
        }) || self.history_run_ids.len() + 1 >= super::MAX_HISTORY_RUNS * 3 / 4
    }

    fn context_window_valid(&self) -> bool {
        let ceiling = super::content_ceiling(self.context_window_tokens);
        self.context_window_tokens.is_none_or(|tokens| {
            crate::provider::admitted_context_window(u64::from(tokens)).is_some()
                && self.custom_profile_provenance.is_none()
                && self
                    .identity()
                    .is_some_and(|identity| supports_context_window_evidence(&identity))
                && self.providers.as_ref().is_none_or(|providers| {
                    providers
                        .primary
                        .iter()
                        .all(supports_context_window_evidence)
                })
        }) && self
            .context_usage
            .as_ref()
            .is_none_or(|usage| usage.budget_bytes as usize <= ceiling)
            && self
                .tool_policy
                .as_ref()
                .is_none_or(|policy| policy.max_context_bytes as usize <= ceiling)
    }

    pub(crate) fn identity(&self) -> Option<crate::provider::ProviderIdentity> {
        use crate::provider::Provenance;
        let provenance = match (
            &self.custom_profile_provenance,
            self.saved_api_account_id,
            &self.chatgpt_provenance,
        ) {
            (Some(custom), None, None) => Provenance::Custom(custom.clone()),
            (None, saved_api_account_id, None) => Provenance::Native {
                saved_api_account_id,
            },
            (None, None, Some(chatgpt)) => Provenance::ChatGpt(chatgpt.clone()),
            _ => return None,
        };
        Some(crate::provider::ProviderIdentity {
            profile: self.provider.clone(),
            model: self.model.clone(),
            effort: self.effort,
            provenance,
            limits: crate::provider::CallLimits {
                concurrency: self.provider_concurrency,
                max_output_tokens: self.output_token_cap,
                output_token_bound: self.output_token_bound,
                ..Default::default()
            },
        })
    }

    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        if self.supervisor.is_some() != self.tool_policy.is_some()
            || self.supervisor.as_ref().is_some_and(|p| !p.valid())
            || self.worker_policy.as_ref().is_some_and(|p| {
                !p.valid() || self.agent_definitions.is_none() || self.tool_policy.is_none()
            })
            || self
                .agent_definitions
                .as_ref()
                .is_some_and(|pin| !pin.valid() || self.tool_policy.is_none())
            || self.roster.as_ref().is_some_and(|roster| {
                !roster.valid()
                    || std::iter::once(&roster.providers.primary)
                        .chain(roster.child_profiles.iter())
                        .any(|name| {
                            roster.chain(name).is_none_or(|chain| {
                                chain.iter().any(|identity| {
                                    self.provider_concurrency > identity.limits.concurrency
                                })
                            })
                        })
                    || roster
                        .role_routes()
                        .is_none_or(|routes| match &self.providers {
                            Some(pinned) => *pinned != routes,
                            None => self.identity().is_none_or(|identity| {
                                routes.values().iter().any(|chain| {
                                    chain.len() != 1
                                        || chain[0].profile != identity.profile
                                        || chain[0].model != identity.model
                                        || chain[0].effort != identity.effort
                                        || chain[0].provenance != identity.provenance
                                        || chain[0].limits.max_output_tokens
                                            != identity.limits.max_output_tokens
                                        || chain[0].limits.output_token_bound
                                            != identity.limits.output_token_bound
                                        || identity.limits.concurrency > chain[0].limits.concurrency
                                })
                            }),
                        })
                    || self.tool_policy.is_some()
                        && roster.child_profiles.iter().any(|name| {
                            roster.chain(name).is_none_or(|chain| {
                                chain.iter().any(|identity| {
                                    identity.custom_provenance().is_some()
                                        || !identity.shares_account(&chain[0])
                                })
                            })
                        })
            })
            || self.providers.as_ref().is_some_and(|providers| {
                !providers.valid()
                    || providers
                        .values()
                        .iter()
                        .all(|chain| chain.len() == 1 && chain[0] == providers.primary[0])
                    || self.identity().is_none_or(|identity| {
                        let primary = &providers.primary[0];
                        identity.profile != primary.profile
                            || identity.model != primary.model
                            || identity.effort != primary.effort
                            || identity.provenance != primary.provenance
                            || identity.limits.max_output_tokens != primary.limits.max_output_tokens
                            || identity.limits.output_token_bound
                                != primary.limits.output_token_bound
                            || self.provider_concurrency > primary.limits.concurrency
                    })
                    || self.tool_policy.is_some()
                        && providers.values().iter().take(2).any(|chain| {
                            chain.iter().any(|identity| {
                                identity.custom_provenance().is_some()
                                    || !identity.shares_account(&chain[0])
                            })
                        })
            })
            || !self.identity().is_some_and(|identity| identity.valid())
            || self
                .tool_policy
                .as_ref()
                .is_some_and(|policy| !policy.valid())
            || ProviderKind::parse(&self.provider) == Some(ProviderKind::Custom)
                && self.tool_policy.is_some()
            || self.include_digests.len() > 16
            || !self.instruction_scope_valid()
            || self.history_run_ids.len() > super::MAX_HISTORY_RUNS
            || self.compaction_event_sequence.is_some() != self.compaction_content_digest.is_some()
            || self
                .context_usage
                .as_ref()
                .is_some_and(|usage| !usage.valid())
            || !self.context_window_valid()
        {
            return Err(ReplayError::InvalidPayload);
        }
        self.policy.validate()
    }
}

fn supports_context_window_evidence(identity: &crate::provider::ProviderIdentity) -> bool {
    identity.chatgpt_provenance().is_some()
        || ProviderKind::parse(&identity.profile) == Some(ProviderKind::Anthropic)
            && matches!(
                identity.provenance,
                crate::provider::Provenance::Native { .. }
            )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Primary,
    Child,
}

/// Authority a child AgentRun receives at spawn; `ReadOnly` children may only research the Workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum ChildGrant {
    NoTools,
    ReadOnly {
        unguarded: crate::tools::UnguardedGrant,
    },
    EditOnly {
        unguarded: crate::tools::UnguardedGrant,
    },
}

impl ChildGrant {
    pub const fn unguarded(self) -> crate::tools::UnguardedGrant {
        match self {
            Self::NoTools => crate::tools::UnguardedGrant::Denied,
            Self::ReadOnly { unguarded } | Self::EditOnly { unguarded } => unguarded,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDisposition {
    StoppedByHook,
    Finished,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunDisposition {
    StoppedByHook,
    Finished,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunFailure {
    ProviderCall,
    ProposalRefused,
    BudgetExhausted,
    ToolStopped,
    DefinitionInvalid,
    PlanAdmission,
    ToolContextFull,
    WorkersUnverified,
    CriterionUnmet,
    RefusalLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalRefusalCause {
    InvalidArguments,
    InvalidPath,
    Phase,
    PlanInvalid,
    PlanBudgetOutsideGoal,
    PlanBudgetSpent,
    PlanScopeExcludesCall,
    PlanOutsideGrant,
    ArgumentBytes,
    LeaseOverlap,
    NotOffered,
    AnswerInvalid,
    DelegateInvalid,
    EditorDefinition,
    EditorDigest,
    EditorCommands,
    EditorTurns,
    EditorLease,
    EditorCriterion,
    EditorResume,
    EditorGrant,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalRefusal {
    pub operation: String,
    pub cause: ProposalRefusalCause,
}

impl ProposalRefusal {
    fn valid(&self) -> bool {
        matches!(
            self.operation.as_str(),
            "answer" | "delegate" | "arany_plan"
        ) || self.operation.strip_prefix("arany_").is_some_and(|name| {
            crate::tools::catalog::ALL
                .iter()
                .any(|facts| facts.name == name)
        })
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<crate::provider::RateLimits>,
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
        let chatgpt = ProviderKind::parse(&self.provider) == Some(ProviderKind::ChatGpt);
        if self
            .rate_limits
            .as_ref()
            .is_some_and(|limits| !limits.valid())
            || self.covered_sequence == 0
            || self.prompt_version != 1
            || self.schema_version != 1
            || !(crate::session::MIN_RECORDED_COMPACTION_COMPILER_VERSION
                ..=crate::session::COMPACTION_COMPILER_VERSION)
                .contains(&self.compiler_version)
            || self.source_bytes == 0
            || self.source_bytes > 256 * 1024
            || self.source_tokens_estimate != self.source_bytes.div_ceil(4)
            || self.input_tokens.is_some_and(|value| value > 1_000_000)
            || self
                .output_tokens
                .is_some_and(|value| value > crate::provider::MAX_REPORTED_OUTPUT_TOKENS)
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
            || chatgpt != self.chatgpt_provenance.is_some()
            || chatgpt
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
                || !self
                    .output_token_bound
                    .admits_reported_output(self.output_tokens, super::MAX_COMPACTION_OUTPUT_TOKENS)
            {
                return Err(ReplayError::InvalidSnapshot);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCallDisposition {
    Finished,
    Delegated,
    ToolRequested,
    PlanRecorded,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<crate::provider::RateLimits>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub delegation: Option<Vec<super::ChildAssignment>>,
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
    #[serde(deserialize_with = "Option::deserialize")]
    pub refusal: Option<ProposalRefusal>,
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

    pub(super) fn validate(&self) -> Result<(), ReplayError> {
        if self
            .rate_limits
            .as_ref()
            .is_some_and(|limits| !limits.valid())
            || !serde_json::to_vec(self).is_ok_and(|b| b.len() <= crate::store::MAX_EVENT_BYTES)
            || (self.disposition == ProviderCallDisposition::Delegated) != self.delegation.is_some()
            || self.delegation.as_ref().is_some_and(|plan| {
                plan.is_empty() || plan.len() > 8 || plan.iter().any(|a| !a.valid())
            })
            || (self.failure_reason
                == Some(crate::provider::ProviderFailureReason::ProposalRefused))
                != self.refusal.is_some()
            || self
                .refusal
                .as_ref()
                .is_some_and(|refusal| !refusal.valid())
            || self.disposition == ProviderCallDisposition::PlanRecorded
                && !matches!(
                    self.phase,
                    crate::provider::AgentPhase::RootPlan
                        | crate::provider::AgentPhase::RootSynthesis
                )
            || self.requested_tools > MAX_STEP_TOOL_CALLS
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
                .is_some_and(|count| count > crate::provider::MAX_REPORTED_OUTPUT_TOKENS)
            || !self.usage().cache_consistent()
            || self.wire_provenance.is_some_and(|provenance| {
                !provenance.valid_for(
                    self.response_id.as_deref(),
                    self.input_tokens,
                    self.output_tokens,
                )
            })
            || self.failure_reason.is_some_and(|reason| {
                if matches!(
                    reason,
                    crate::provider::ProviderFailureReason::WorkerAnswerDeferred
                        | crate::provider::ProviderFailureReason::WorkerDelegateRefused
                        | crate::provider::ProviderFailureReason::WorkerToolRefused
                ) && self.phase != crate::provider::AgentPhase::RootSynthesis
                {
                    return true;
                }
                if reason == crate::provider::ProviderFailureReason::ProposalRefused
                    && !matches!(
                        self.phase,
                        crate::provider::AgentPhase::RootPlan
                            | crate::provider::AgentPhase::RootSynthesis
                    )
                {
                    return true;
                }
                if reason == crate::provider::ProviderFailureReason::CriterionNotMetInView
                    && self.phase != crate::provider::AgentPhase::ChildWork
                {
                    return true;
                }

                use crate::provider::ProviderFailureReason;
                !matches!(
                    (self.disposition, reason),
                    (
                        ProviderCallDisposition::Rejected,
                        ProviderFailureReason::AccountAccess
                            | ProviderFailureReason::UsageLimit
                            | ProviderFailureReason::RateLimited
                            | ProviderFailureReason::ProposalRefused
                    ) | (
                        ProviderCallDisposition::Unavailable,
                        ProviderFailureReason::UsageTemporarilyUnavailable
                            | ProviderFailureReason::ServiceUnavailable
                    ) | (
                        ProviderCallDisposition::InvalidResponse,
                        ProviderFailureReason::StreamProtocol
                            | ProviderFailureReason::OutcomeDecode
                            | ProviderFailureReason::ResponseContract
                            | ProviderFailureReason::OutcomeContract
                            | ProviderFailureReason::LocalOutputLimit
                            | ProviderFailureReason::WorkerAnswerDeferred
                            | ProviderFailureReason::WorkerDelegateRefused
                            | ProviderFailureReason::WorkerToolRefused
                            | ProviderFailureReason::CriterionNotMetInView
                    )
                )
            })
            || (!matches!(
                self.disposition,
                ProviderCallDisposition::Finished
                    | ProviderCallDisposition::Delegated
                    | ProviderCallDisposition::ToolRequested
                    | ProviderCallDisposition::PlanRecorded
                    | ProviderCallDisposition::InvalidResponse
            ) && self.refusal.is_none()
                && (self.response_id.is_some()
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
    pub(super) fn validate(&self) -> Result<(), ReplayError> {
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
