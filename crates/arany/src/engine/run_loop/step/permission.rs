use super::hooks::HookFold;
use super::{Grant, RunLoop};
use crate::engine::EngineError;
use crate::tools::permissions::{Decision, PermissionSource};
use crate::tools::{ApprovalMode, EffectIntent, ToolCall, ToolRuntime};

pub(super) struct HostPermission {
    pub(super) permission: Permission,
    epoch: Option<uuid::Uuid>,
    source: Option<[u8; 32]>,
    pub(super) remember: bool,
}

impl HostPermission {
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

#[derive(Clone, Copy)]
pub(super) enum RuleSave {
    Prefix(crate::tools::permissions::TypedGitPrefix),
    Session,
}

#[derive(Clone, Copy)]
pub(super) struct Permission {
    pub(super) decision: Decision,
    pub(super) basis: PermissionSource,
    pub(super) approval: Option<ApprovalMode>,
}

impl Permission {
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
            && self.basis != PermissionSource::SessionApproval
            && let Some(mode) = mode.filter(|mode| {
                // A user-owned typed allow is the prompt-free path in Request and Auto-edits; Auto keeps its review.
                mode.asks(call)
                    && !(self.basis == PermissionSource::TypedPrefixRule
                        && self.decision == Decision::Allow
                        && *mode != ApprovalMode::Auto)
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
        let (epoch, source, grants) = self.ledger().view(|view| {
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
            )
        })?;
        let facts = HostReuseFacts {
            host: intent.call.is_unguarded(),
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
        let (permission, source, remember) = match decide_host_reuse(&facts) {
            HostReuse::OnceOnly => (permission, None, false),
            HostReuse::OfferSession => (permission, None, true),
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
        Ok(HostPermission {
            permission,
            epoch,
            source,
            remember,
        })
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
                    && !matches!(grant, Grant::Researcher { child, .. } if child.editor && child.definition.as_ref().is_some_and(|definition| definition.allows_worker(call))))
        {
            decision = decision.narrow(Decision::Deny);
            basis = PermissionSource::PinnedGrant;
            approval = None;
        }
        if call.is_host() && decision != Decision::Deny {
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
