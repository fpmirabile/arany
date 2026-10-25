use super::dispatch::{DispatchSite, NotDispatched, Verdict, refuse};
use super::hooks::HookFold;
use super::{Grant, RunLoop};
use crate::engine::EngineError;
use crate::engine::ledger::Action;
use crate::tools::permissions::{Decision, PermissionSource};
use crate::tools::{ApprovalMode, EffectIntent, ToolCall, ToolError, ToolRuntime};

pub(super) struct HostPermission {
    pub(super) permission: Permission,
    epoch: Option<uuid::Uuid>,
    source: Option<[u8; 32]>,
    pub(super) remember: bool,
    pub(super) prefix: bool,
    rule: Option<crate::tools::permissions::HostPrefixRule>,
}

impl HostPermission {
    fn decide(
        facts: &HostReuseFacts,
        rule: Option<crate::tools::permissions::HostPrefixRule>,
        intent: &EffectIntent,
        attached: bool,
    ) -> Self {
        let candidate = match &intent.call {
            ToolCall::CommandUnguarded {
                prefix_rule: Some(prefix),
                ..
            } => crate::tools::permissions::HostPrefixRule::from_intent(intent, prefix.len()).ok(),
            _ => None,
        };
        let prefix = decide_host_prefix(
            facts,
            rule.as_ref().map(|rule| rule.decision),
            rule.as_ref()
                .map_or(candidate.is_some(), |rule| rule.dispatch_eligible(intent)),
            attached,
        );
        let prefix = match prefix {
            HostPrefixDecision::Reuse => {
                return HostPermission {
                    permission: Permission {
                        decision: Decision::Allow,
                        basis: PermissionSource::HostPrefixRule,
                        approval: None,
                    },
                    epoch: facts.epoch,
                    source: None,
                    remember: false,
                    prefix: false,
                    rule,
                };
            }
            HostPrefixDecision::Offer => true,
            HostPrefixDecision::NoReuse => false,
        };
        let reuse = if rule.is_some() {
            HostReuse::OnceOnly
        } else {
            decide_host_reuse(facts)
        };
        let (permission, source, remember) = match reuse {
            HostReuse::OnceOnly => (facts.permission, None, false),
            HostReuse::OfferSession => (facts.permission, None, true),
            HostReuse::Reuse(source) => (
                Permission {
                    decision: Decision::Allow,
                    basis: PermissionSource::SessionApproval,
                    approval: None,
                },
                Some(source),
                false,
            ),
        };
        HostPermission {
            permission,
            epoch: facts.epoch,
            source,
            remember,
            prefix,
            rule: candidate,
        }
    }

    pub(super) fn verdict(
        &self,
        approved: bool,
        host: bool,
        save: Option<RuleSave>,
    ) -> Option<crate::tools::permissions::HostApproval> {
        (host && (approved || self.source.is_some()))
            .then_some(self.epoch)
            .flatten()
            .map(|epoch| crate::tools::permissions::HostApproval {
                rule: self.rule.clone().filter(|_| {
                    self.permission.basis == PermissionSource::HostPrefixRule
                        && self.permission.decision == Decision::Allow
                        || matches!(save, Some(RuleSave::HostPrefix))
                }),
                epoch,
                session: approved && matches!(save, Some(RuleSave::Session)),
                source_intent: self.source,
            })
    }
}

#[derive(Clone, Copy)]
pub(super) struct HostReuseFacts {
    pub(super) host: bool,
    pub(super) settings_admitted: bool,
    pub(super) mode: Option<crate::tools::permissions::PermissionMode>,
    pub(super) terminal_mode: Option<ApprovalMode>,
    pub(super) session_scope: bool,
    pub(super) permission: Permission,
    pub(super) hook_restriction: Option<Decision>,
    pub(super) state_target: Option<bool>,
    pub(super) epoch: Option<uuid::Uuid>,
    pub(super) source: Option<[u8; 32]>,
    pub(super) grants: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum HostReuse {
    OnceOnly,
    OfferSession,
    Reuse([u8; 32]),
}

pub(super) fn decide_host_reuse(facts: &HostReuseFacts) -> HostReuse {
    use crate::tools::permissions::PermissionMode;
    if !facts.host
        || !facts.settings_admitted
        || !matches!(
            facts.mode,
            None | Some(PermissionMode::Default | PermissionMode::DontAsk)
        )
        || facts.terminal_mode != Some(ApprovalMode::Request)
        || !facts.session_scope
        || facts.permission.basis == PermissionSource::Rule
        || facts.hook_restriction.is_some()
        || facts.state_target != Some(false)
        || facts.epoch.is_none()
        || facts.permission.decision == Decision::Deny
            && !(facts.permission.basis == PermissionSource::Mode
                && facts.mode == Some(PermissionMode::DontAsk))
    {
        return HostReuse::OnceOnly;
    }
    match facts.source {
        Some(source) => HostReuse::Reuse(source),
        None if facts.grants < crate::session::MAX_HOST_SESSION_GRANTS
            && matches!(facts.mode, None | Some(PermissionMode::Default)) =>
        {
            HostReuse::OfferSession
        }
        None => HostReuse::OnceOnly,
    }
}

#[derive(Debug, Eq, PartialEq)]
enum HostPrefixDecision {
    Reuse,
    Offer,
    NoReuse,
}

fn decide_host_prefix(
    facts: &HostReuseFacts,
    rule: Option<Decision>,
    eligible: bool,
    attached: bool,
) -> HostPrefixDecision {
    use crate::tools::permissions::PermissionMode;
    if !eligible
        || !attached
        || !facts.host
        || !facts.settings_admitted
        || facts.epoch.is_none()
        || facts.state_target != Some(false)
        || facts.terminal_mode != Some(ApprovalMode::Request)
        || facts.hook_restriction.is_some()
        || facts.permission.basis == PermissionSource::Rule
    {
        return HostPrefixDecision::NoReuse;
    }
    match rule {
        Some(Decision::Allow)
            if matches!(
                facts.mode,
                Some(PermissionMode::Default | PermissionMode::DontAsk)
            ) && (facts.permission.decision != Decision::Deny
                || facts.mode == Some(PermissionMode::DontAsk)
                    && facts.permission.basis == PermissionSource::Mode) =>
        {
            HostPrefixDecision::Reuse
        }
        None if matches!(facts.mode, None | Some(PermissionMode::Default))
            && facts.permission.decision == Decision::Ask
            && facts.permission.basis == PermissionSource::Mode =>
        {
            HostPrefixDecision::Offer
        }
        Some(Decision::Allow | Decision::Ask | Decision::Deny) | None => {
            HostPrefixDecision::NoReuse
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum RuleSave {
    Prefix(crate::tools::permissions::TypedGitPrefix),
    Session,
    HostPrefix,
}

#[derive(Clone, Copy)]
pub(super) struct Permission {
    pub(super) decision: Decision,
    pub(super) basis: PermissionSource,
    pub(super) approval: Option<ApprovalMode>,
}

impl Permission {
    fn narrow_host_rule(self, rule: Option<Decision>) -> Self {
        match rule {
            Some(Decision::Deny) => Self {
                decision: Decision::Deny,
                basis: PermissionSource::HostPrefixRule,
                approval: None,
            },
            Some(Decision::Ask) if self.decision != Decision::Deny => Self {
                decision: Decision::Ask,
                basis: PermissionSource::HostPrefixRule,
                approval: Some(ApprovalMode::Request),
            },
            Some(Decision::Allow | Decision::Ask) | None => self,
        }
    }

    pub(super) fn narrow_hooks(
        mut self,
        restriction: Option<Decision>,
        mode: Option<crate::tools::permissions::PermissionMode>,
    ) -> Self {
        match restriction {
            Some(Decision::Deny) => {
                self.decision = Decision::Deny;
                self.basis = PermissionSource::Hook;
                self.approval = None;
            }
            // dontAsk has no human to ask, so a hook ask is the narrower deny.
            Some(Decision::Ask)
                if self.decision != Decision::Deny
                    && mode == Some(crate::tools::permissions::PermissionMode::DontAsk) =>
            {
                self.decision = Decision::Deny;
                self.basis = PermissionSource::Mode;
                self.approval = None;
            }
            Some(Decision::Ask) if self.decision != Decision::Deny => {
                if self.decision == Decision::Allow {
                    self.basis = PermissionSource::Hook;
                }
                self.decision = Decision::Ask;
                self.approval = Some(ApprovalMode::Request);
            }
            _ => {}
        }
        self
    }

    pub(super) fn narrow_definition(
        mut self,
        definition: &crate::tools::definitions::AgentDefinition,
        call: &ToolCall,
    ) -> Self {
        self.decision = self.decision.narrow(definition.decision(call));
        if let Some(mode) = definition.permission_mode {
            let (restriction, _, required_approval) =
                crate::tools::permissions::PermissionConfig::evaluate_mode(mode, call);
            if restriction == Decision::Deny && self.decision != Decision::Deny
                || restriction == Decision::Ask && self.decision == Decision::Allow
            {
                self.basis = PermissionSource::Mode;
            }
            self.decision = self.decision.narrow(restriction);
            self.approval = match (self.approval, required_approval) {
                (Some(ApprovalMode::Request), _) | (_, Some(ApprovalMode::Request)) => {
                    Some(ApprovalMode::Request)
                }
                (existing, required) => existing.or(required),
            };
            if mode == crate::tools::permissions::PermissionMode::DontAsk
                && self.decision == Decision::Ask
            {
                self.decision = Decision::Deny;
                self.basis = PermissionSource::Mode;
            }
        }
        if self.decision == Decision::Deny {
            self.approval = None;
        }
        self
    }

    pub(super) fn narrow_for_intent(
        self,
        mode: Option<ApprovalMode>,
        intent: &EffectIntent,
    ) -> Self {
        if intent.call.is_host() && intent.host.is_none() {
            Self {
                decision: Decision::Deny,
                basis: PermissionSource::PinnedGrant,
                approval: None,
            }
        } else {
            self.narrow_terminal(mode, &intent.call)
        }
    }

    pub(super) fn narrow_terminal(mut self, mode: Option<ApprovalMode>, call: &ToolCall) -> Self {
        if self.decision != Decision::Deny
            && !matches!(
                self.basis,
                PermissionSource::SessionApproval | PermissionSource::HostPrefixRule
            )
            && let Some(mode) = mode.filter(|mode| {
                // A user-owned typed allow is the prompt-free path in Request and Auto-edits; Auto keeps its review.
                mode.asks(call)
                    && !(self.basis == PermissionSource::TypedPrefixRule
                        && self.decision == Decision::Allow
                        && (matches!(call, ToolCall::GhPrList { .. })
                            || *mode != ApprovalMode::Auto))
            })
        {
            if self.decision == Decision::Allow {
                self.basis = PermissionSource::Mode;
            }
            self.decision = self.decision.narrow(Decision::Ask);
            self.approval = Some(
                if self.approval == Some(ApprovalMode::Request) || mode != ApprovalMode::Auto {
                    ApprovalMode::Request
                } else {
                    ApprovalMode::Auto
                },
            );
        }
        self
    }
}

impl RunLoop<'_> {
    pub(super) fn plan_allows(&self, call: &ToolCall) -> Result<bool, EngineError> {
        Ok(self.primary_lease_allows(call)?.is_ok()
            && (matches!(
                call,
                ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::WorkerHandover { .. }
                }
            ) || self.ledger().allows_tool(call)))
    }

    pub(in crate::engine::run_loop) fn primary_lease_allows(
        &self,
        call: &ToolCall,
    ) -> Result<Result<(), super::super::ProposalRefusalCause>, EngineError> {
        let allowed = self.ledger().view(|v| {
            v.runs.last().is_some_and(|r| {
                let leases = r
                    .agents
                    .iter()
                    .filter_map(|a| a.assignment.as_ref())
                    .filter(|w| w.released.is_none())
                    .flat_map(|w| &w.pins.request.lease)
                    .collect::<Vec<_>>();
                match call {
                    ToolCall::ApplyChangeset {
                        source: crate::worker::ChangesetSource::WorkerHandover { .. },
                    } => true,
                    ToolCall::Write { path, .. }
                    | ToolCall::Edit { path, .. }
                    | ToolCall::Delete { path, .. }
                    | ToolCall::MultiEdit {
                        file_path: path, ..
                    }
                    | ToolCall::Mkdir { path } => {
                        !leases.iter().any(|l| crate::worker::overlaps(l, path))
                    }
                    ToolCall::Move {
                        source,
                        destination,
                        ..
                    } => ![source, destination]
                        .iter()
                        .any(|p| leases.iter().any(|l| crate::worker::overlaps(l, p))),
                    ToolCall::ParseAgentDefinition { .. }
                    | ToolCall::List { .. }
                    | ToolCall::Read { .. }
                    | ToolCall::Search { .. }
                    | ToolCall::Command { .. }
                    | ToolCall::CommandUnguarded { .. }
                    | ToolCall::ApplyChangeset { .. }
                    | ToolCall::PrepareWorker { .. }
                    | ToolCall::McpList { .. }
                    | ToolCall::McpCall { .. }
                    | ToolCall::GitStatus { .. }
                    | ToolCall::GitDiff { .. }
                    | ToolCall::GitLog { .. }
                    | ToolCall::GitCommit { .. }
                    | ToolCall::GitBranch { .. }
                    | ToolCall::GitSwitch { .. }
                    | ToolCall::GitPush { .. }
                    | ToolCall::GhPrCreate { .. }
                    | ToolCall::GhPrList { .. }
                    | ToolCall::Glob { .. }
                    | ToolCall::Skill { .. } => !call.is_process() || leases.is_empty(),
                }
            })
        })?;
        Ok(if allowed {
            Ok(())
        } else {
            Err(super::super::ProposalRefusalCause::LeaseOverlap)
        })
    }

    pub(super) fn review_will_run(
        &self,
        runtime: &ToolRuntime,
        call: &ToolCall,
        review_allowed: bool,
        permission: Permission,
    ) -> Result<bool, EngineError> {
        Ok(self.ledger().command_timeout_limit(call)?.is_none()
            && self.plan_allows(call)?
            && runtime.allows(call)
            && permission
                .narrow_terminal(self.terminal_mode(), call)
                .approval
                == Some(ApprovalMode::Auto)
            && self.ledger().can_start_provider_call()
            && review_allowed
            && self.ledger().admits(Action::Review))
    }

    // The rule is saved only after the human verdict is committed; a failed save refuses dispatch.
    pub(super) fn save_remembered_rule(
        &self,
        runtime: &ToolRuntime,
        site: DispatchSite,
        verdict: Verdict,
        save: Option<RuleSave>,
    ) -> Result<Verdict, EngineError> {
        Ok(match (verdict, save) {
            (
                Verdict::Dispatch {
                    intent,
                    queue_pause,
                },
                Some(save),
            ) => {
                let saved = match save {
                    save @ (RuleSave::Prefix(_) | RuleSave::HostPrefix) => {
                        let expected = self
                            .controls
                            .host_settings
                            .lock()
                            .expect("host permissions lock")
                            .as_ref()
                            .filter(|settings| !settings.changed)
                            .map(|settings| settings.digest);
                        expected
                            .ok_or(ToolError::ChangedInput)
                            .and_then(|expected| {
                                let saved = match save {
                                    RuleSave::Prefix(prefix) => {
                                        runtime.record_typed_prefix_rule(prefix, expected)
                                    }
                                    RuleSave::HostPrefix => {
                                        runtime.record_host_prefix_rule(&intent, expected)
                                    }
                                    RuleSave::Session => {
                                        unreachable!("session save handled separately")
                                    }
                                };
                                saved.map(|digest| {
                                    let mut pinned = self
                                        .controls
                                        .host_settings
                                        .lock()
                                        .expect("host permissions lock");
                                    if let Some(settings) = pinned.as_mut()
                                        && !settings.changed
                                        && settings.digest == expected
                                    {
                                        settings.digest = Some(digest);
                                    }
                                })
                            })
                    }
                    RuleSave::Session => Ok(()),
                };
                if saved.is_ok() {
                    Verdict::Dispatch {
                        intent,
                        queue_pause,
                    }
                } else {
                    if let Some(pause) = queue_pause {
                        self.settle_active_pause(pause, None)?;
                    }
                    // The rule may already be installed, so the outcome is unknown and the Run stops.
                    let refusal = if matches!(saved, Err(crate::tools::ToolError::Uncertain)) {
                        refuse(
                            NotDispatched::SaveUnconfirmed,
                            Some(super::StopCause::Uncertain),
                        )
                    } else {
                        refuse(NotDispatched::UnsavedRule, None)
                    };
                    self.refused(site, intent, refusal)
                }
            }
            (verdict, _) => verdict,
        })
    }

    pub(super) fn terminal_mode(&self) -> Option<ApprovalMode> {
        self.controls.approvals.map(|approvals| approvals.mode())
    }

    pub(super) fn host_permission(
        &self,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        permission: Permission,
        hooks: &HookFold,
    ) -> Result<HostPermission, EngineError> {
        let (epoch, source, grants, primary) = self.ledger().view(|view| {
            let epoch = view
                .runs
                .last()
                .and_then(|run| run.config.as_ref())
                .and_then(|config| config.tool_policy.as_ref())
                .and_then(|policy| policy.host_epoch);
            (
                epoch,
                epoch.and_then(|epoch| view.host_approval(intent, epoch)),
                epoch.map_or(0, |epoch| view.host_grants(epoch).count()),
                view.runs
                    .last()
                    .and_then(|run| run.primary())
                    .is_some_and(|agent| agent.id == intent.agent_run_id),
            )
        })?;
        let rule = runtime
            .permissions()
            .and_then(|config| config.host_rule(intent))
            .filter(|_| primary)
            .cloned();
        let permission = permission.narrow_host_rule(rule.as_ref().map(|rule| rule.decision));
        let facts = HostReuseFacts {
            host: primary && intent.call.is_unguarded(),
            settings_admitted: runtime.host_settings_admitted,
            mode: runtime.permissions().map(|config| config.mode()),
            terminal_mode: self.terminal_mode(),
            session_scope: self
                .controls
                .approvals
                .is_some_and(|approvals| approvals.admits_session_scope()),
            permission,
            hook_restriction: hooks.restriction,
            state_target: intent
                .host_command
                .as_ref()
                .map(|pin| crate::tools::unguarded::state_target(&intent.call, pin)),
            epoch,
            source,
            grants,
        };
        Ok(HostPermission::decide(
            &facts,
            rule,
            intent,
            self.controls
                .approvals
                .is_some_and(|approvals| approvals.admits_prefix_rules()),
        ))
    }

    pub(super) fn permission(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        call: &ToolCall,
    ) -> Permission {
        // Engine-owned definition parsing and worker preparation are never model Tools, so rules and modes do not apply; each runs only in its fixed grant.
        if call.is_internal() {
            return Permission {
                decision: Decision::Allow,
                basis: PermissionSource::Mode,
                approval: None,
            };
        }
        let (mut decision, mut basis, mut approval) = runtime.permissions().map_or(
            (Decision::Allow, PermissionSource::Mode, None),
            |config| {
                if let ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::WorkerHandover { data, .. },
                } = call
                {
                    self.ledger()
                        .view(|view| {
                            view.runs
                                .last()
                                .and_then(|run| {
                                    run.workers
                                        .data
                                        .iter()
                                        .find(|sealed| &sealed.reference == data)
                                })
                                .and_then(|sealed| match &sealed.facts {
                                    crate::session::worker::WorkerDataFacts::Changes(paths) => {
                                        Some(config.evaluate_worker_paths(call, Some(paths)))
                                    }
                                    _ => None,
                                })
                        })
                        .ok()
                        .flatten()
                        .unwrap_or((Decision::Deny, PermissionSource::PinnedGrant, None))
                } else {
                    config.evaluate_skill_file(call, runtime.skill_workspace_path(call).as_deref())
                }
            },
        );
        let unguarded = call.is_unguarded();
        let researcher = matches!(grant, Grant::Researcher { .. });
        let git_write = call.is_git() && call.facts().mutates;
        if (runtime.permissions().is_some() || unguarded || git_write)
            && (!runtime.allows(call)
                || researcher
                    && !call.is_child_research()
                    && !matches!(grant, Grant::Researcher { child, .. } if child.kind.is_editor() && child.definition.as_ref().is_some_and(|definition| definition.allows_worker(call))))
        {
            decision = decision.narrow(Decision::Deny);
            basis = PermissionSource::PinnedGrant;
            approval = None;
        }
        if call.is_host()
            && decision != Decision::Deny
            && !(matches!(call, ToolCall::GhPrList { .. })
                && decision == Decision::Allow
                && basis == PermissionSource::TypedPrefixRule)
        {
            decision = Decision::Ask;
            approval = Some(ApprovalMode::Request);
        }
        if unguarded && decision != Decision::Deny {
            // Host approval never uses AI review or ordinary allow rules.
            decision = Decision::Ask;
            approval = Some(ApprovalMode::Request);
        }
        let permission = Permission {
            decision,
            basis,
            approval,
        };
        match grant {
            Grant::Researcher { child, .. } => {
                child.definition.as_ref().map_or(permission, |definition| {
                    permission.narrow_definition(definition, call)
                })
            }
            Grant::Primary { .. } => permission,
        }
    }
}

pub(super) fn typed_allow_takes_effect(
    mode: Option<crate::tools::permissions::PermissionMode>,
) -> bool {
    mode == Some(crate::tools::permissions::PermissionMode::Default)
}

pub(super) fn remember_candidate(
    grant: &Grant<'_>,
    mode: Option<crate::tools::permissions::PermissionMode>,
    permission: Permission,
    hooks: &HookFold,
    call: &ToolCall,
) -> Option<crate::tools::permissions::TypedGitPrefix> {
    (matches!(grant, Grant::Primary { .. })
        && (!call.is_host() || matches!(call, ToolCall::GhPrList { .. }))
        && typed_allow_takes_effect(mode)
        && permission.decision == Decision::Ask
        && permission.basis == PermissionSource::Mode
        && hooks.restriction.is_none())
    .then(|| crate::tools::permissions::TypedGitPrefix::of_call(call))
    .flatten()
}

pub(super) fn effective_permission_mode(
    config: Option<&crate::tools::permissions::PermissionConfig>,
    grant: &Grant<'_>,
) -> Option<crate::tools::permissions::PermissionMode> {
    config
        .map(crate::tools::permissions::PermissionConfig::mode)
        .or_else(|| match grant {
            Grant::Researcher { child, .. } => child
                .definition
                .as_ref()
                .and_then(|definition| definition.permission_mode),
            Grant::Primary { .. } => None,
        })
}

#[cfg(test)]
mod host_prefix_tests {
    use super::*;
    use crate::tools::permissions::PermissionMode as Mode;

    #[test]
    fn explicit_ask_in_dont_ask_blocks_prefix_and_session_reuse() {
        let config = crate::tools::permissions::PermissionConfig::parse(
            br#"{"permissions":{"defaultMode":"dontAsk","ask":["Bash"]}}"#,
        )
        .unwrap();
        let call = ToolCall::CommandUnguarded {
            program: "printf".into(),
            args: vec!["host value".into()],
            cwd: String::new(),
            justification: "Inspect a synthetic host value".into(),
            prefix_rule: None,
        };
        let (decision, basis, approval) = config.evaluate(&call);
        let facts = HostReuseFacts {
            host: true,
            settings_admitted: true,
            mode: Some(config.mode()),
            terminal_mode: Some(ApprovalMode::Request),
            session_scope: true,
            permission: Permission {
                decision,
                basis,
                approval,
            },
            hook_restriction: None,
            state_target: Some(false),
            epoch: Some(uuid::Uuid::now_v7()),
            source: Some([1; 32]),
            grants: 1,
        };
        assert_eq!(
            decide_host_prefix(&facts, Some(Decision::Allow), true, true),
            HostPrefixDecision::NoReuse
        );
        assert_eq!(decide_host_reuse(&facts), HostReuse::OnceOnly);
        assert_eq!(facts.permission.decision, Decision::Deny);
    }

    #[test]
    fn host_prefix_reuse_requires_every_admitted_fact() {
        let baseline = HostReuseFacts {
            host: true,
            settings_admitted: true,
            mode: Some(Mode::Default),
            terminal_mode: Some(ApprovalMode::Request),
            session_scope: true,
            permission: Permission {
                decision: Decision::Ask,
                basis: PermissionSource::Mode,
                approval: Some(ApprovalMode::Request),
            },
            hook_restriction: None,
            state_target: Some(false),
            epoch: Some(uuid::Uuid::now_v7()),
            source: None,
            grants: 0,
        };
        assert_eq!(
            decide_host_prefix(&baseline, Some(Decision::Allow), true, true),
            HostPrefixDecision::Reuse
        );
        for (facts, rule, eligible, attached) in [
            (
                HostReuseFacts {
                    host: false,
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (
                HostReuseFacts {
                    settings_admitted: false,
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (
                HostReuseFacts {
                    mode: Some(Mode::Auto),
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (
                HostReuseFacts {
                    mode: Some(Mode::AcceptEdits),
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (
                HostReuseFacts {
                    mode: Some(Mode::Plan),
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (
                HostReuseFacts {
                    terminal_mode: Some(ApprovalMode::Auto),
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (
                HostReuseFacts {
                    terminal_mode: Some(ApprovalMode::AutoEdits),
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (
                HostReuseFacts {
                    hook_restriction: Some(Decision::Ask),
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (
                HostReuseFacts {
                    state_target: Some(true),
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (
                HostReuseFacts {
                    permission: Permission {
                        basis: PermissionSource::Rule,
                        ..baseline.permission
                    },
                    ..baseline
                },
                Some(Decision::Allow),
                true,
                true,
            ),
            (baseline, Some(Decision::Deny), true, true),
            (baseline, Some(Decision::Ask), true, true),
            (baseline, Some(Decision::Allow), false, true),
            (baseline, Some(Decision::Allow), true, false),
        ] {
            assert_eq!(
                decide_host_prefix(&facts, rule, eligible, attached),
                HostPrefixDecision::NoReuse
            );
        }
    }
}
