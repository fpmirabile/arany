use super::ask::Resolution;
use super::hooks::HookFold;
use super::permission::Permission;
use super::{ActiveTime, Answer, HookDenial};
use crate::engine::ledger::ActiveDeadlineCause;
use crate::engine::run_loop::stop::{StepEnd, StopCause};
use crate::engine::run_loop::{ActivePause, RunLoop};
use crate::tools::permissions::{Decision, PermissionSource};
use crate::tools::types::DenialFeedback;
use crate::tools::{
    ApprovalMode, CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolObservation,
    UnguardedPrompt,
};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StepDispatch {
    ConcurrentRead,
    Serial,
}

impl StepDispatch {
    pub(super) fn decide(
        calls: usize,
        intent: &EffectIntent,
        reservation_overhead: Duration,
    ) -> Self {
        if calls > 1
            && intent.call.facts().site == crate::tools::catalog::Site::InProcess
            && !intent.call.facts().mutates
            && reservation_overhead.is_zero()
        {
            Self::ConcurrentRead
        } else {
            Self::Serial
        }
    }
}

#[must_use]
pub(super) enum Verdict {
    Dispatch {
        intent: EffectIntent,
        queue_pause: Option<ActivePause>,
    },
    Deny(Answer),
    Narrow(Answer, ToolCall),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DispatchSite {
    Primary,
    Researcher,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GuardCompletion {
    Observed,
    ActiveTime,
}

impl GuardCompletion {
    pub(super) fn decide(
        disposition: ToolDisposition,
        attested: bool,
        active_time_exhausted: bool,
    ) -> Self {
        if active_time_exhausted && (disposition != ToolDisposition::Limit || !attested) {
            Self::ActiveTime
        } else {
            Self::Observed
        }
    }
}

pub(super) fn completion_deadline(
    disposition: ToolDisposition,
    elapsed: bool,
    cause: Option<ActiveDeadlineCause>,
) -> StepEnd {
    if !elapsed || disposition != ToolDisposition::Cancelled {
        return StepEnd::Continue;
    }
    StepEnd::Stopped(match cause {
        Some(ActiveDeadlineCause::Goal) => StopCause::ActiveTime,
        Some(ActiveDeadlineCause::Delegation) => StopCause::DelegationDeadline,
        None => StopCause::ToolStopped,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UnguardedRefusal {
    SettingsChanged,
    UnavailableProgram,
    InterpreterNotAdmitted,
    PrivilegeTool,
    GuardAlias,
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub(super) enum NotDispatched {
    TimeoutLimit { requested: u32, maximum: u32 },
    HookDenied(HookDenial),
    DelegationDeadline,
    DenyRule,
    OutsideGrant,
    HostSourceUnavailable,
    ModeDenied,
    HumanOnlyUnavailable,
    UnguardedNotOffered(UnguardedRefusal),
    OutsidePlan,
    ActiveTime,
    Paused,
    Cancelled,
    TokenBudget,
    UnsavedRule,
    SaveUnconfirmed,
    UnguardedNeedsHuman,
    ApprovalRefused,
    ApprovalUnavailable,
}

impl RunLoop<'_> {
    pub(super) fn refused(
        &self,
        site: DispatchSite,
        intent: EffectIntent,
        refusal: Refusal<'_>,
    ) -> Verdict {
        let approval_stage = site == DispatchSite::Primary && refusal.reason.decided_at_approval();
        if approval_stage {
            self.clear_tool_queue();
        }
        // The primary re-reads active time after the ToolFinished it journals.
        let exhausted = match (refusal.reason.active_time_effect(), site) {
            (ActiveTimeEffect::Exhausted, DispatchSite::Primary | DispatchSite::Researcher) => {
                Some(true)
            }
            (ActiveTimeEffect::Recheck, DispatchSite::Primary) => Some(false),
            (ActiveTimeEffect::Recheck, DispatchSite::Researcher)
            | (ActiveTimeEffect::Untouched, DispatchSite::Primary | DispatchSite::Researcher) => {
                None
            }
        };
        let active_time = exhausted.map(|exhausted| ActiveTime {
            segment: None,
            exhausted,
        });
        let answer = Answer {
            checkpoint: None,
            observation: not_dispatched(intent, &refusal.reason),
            stop: refusal.stop,
            active_time,
        };
        match refusal.narrowed {
            Some(call) => Verdict::Narrow(answer, call.clone()),
            None => Verdict::Deny(answer),
        }
    }
}

#[derive(Clone, Copy)]
enum ActiveTimeEffect {
    Untouched,
    Recheck,
    Exhausted,
}

impl NotDispatched {
    fn active_time_effect(&self) -> ActiveTimeEffect {
        match self {
            Self::HookDenied(HookDenial::ActiveTime) | Self::ActiveTime => {
                ActiveTimeEffect::Exhausted
            }
            Self::UnguardedNotOffered(_)
            | Self::OutsidePlan
            | Self::Paused
            | Self::Cancelled
            | Self::TokenBudget
            | Self::UnsavedRule
            | Self::SaveUnconfirmed
            | Self::UnguardedNeedsHuman
            | Self::ApprovalUnavailable
            | Self::ApprovalRefused => ActiveTimeEffect::Recheck,
            Self::TimeoutLimit { .. }
            | Self::HookDenied(
                HookDenial::DelegationDeadline
                | HookDenial::Uncertain
                | HookDenial::Requested
                | HookDenial::Narrowed(_)
                | HookDenial::Denied(_),
            )
            | Self::DelegationDeadline
            | Self::DenyRule
            | Self::OutsideGrant
            | Self::HostSourceUnavailable
            | Self::ModeDenied
            | Self::HumanOnlyUnavailable => ActiveTimeEffect::Untouched,
        }
    }

    // The primary decides these after its gates, where it clears the queue hint and re-reads active time.
    fn decided_at_approval(&self) -> bool {
        match self {
            Self::UnguardedNotOffered(_)
            | Self::OutsidePlan
            | Self::ActiveTime
            | Self::Paused
            | Self::Cancelled
            | Self::TokenBudget
            | Self::UnsavedRule
            | Self::SaveUnconfirmed
            | Self::UnguardedNeedsHuman
            | Self::ApprovalUnavailable
            | Self::ApprovalRefused => true,
            Self::TimeoutLimit { .. }
            | Self::HookDenied(_)
            | Self::DelegationDeadline
            | Self::DenyRule
            | Self::OutsideGrant
            | Self::HostSourceUnavailable
            | Self::ModeDenied
            | Self::HumanOnlyUnavailable => false,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Refusal<'a> {
    pub(super) reason: NotDispatched,
    stop: Option<StopCause>,
    narrowed: Option<&'a ToolCall>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Dispatch {
    Direct,
    QueuePause,
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub(super) struct DispatchDecision<'a> {
    pub(super) outcome: Result<Dispatch, Refusal<'a>>,
    pub(super) decision: Decision,
    pub(super) source: PermissionSource,
}

pub(super) struct Unguarded {
    pub(super) gate: Result<UnguardedPrompt, UnguardedRefusal>,
}

// Read after the PreToolUse hooks, before the Ask.
pub(super) struct DispatchFacts<'a> {
    pub(super) site: DispatchSite,
    pub(super) permission: Permission,
    pub(super) hooks: &'a HookFold,
    pub(super) timeout_limit: Option<(u32, u32)>,
    pub(super) human_only_unavailable: bool,
    pub(super) host: bool,
    pub(super) typed_host_read: bool,
    pub(super) host_unpinned: bool,
    pub(super) unguarded: Option<Unguarded>,
    pub(super) goal_allows: bool,
    pub(super) runtime_allows: bool,
    pub(super) can_start_provider_call: bool,
    pub(super) guard_site: bool,
    pub(super) interactive: bool,
}

// Read after the Ask, or where it would have been.
pub(super) struct AfterAsk {
    pub(super) approval: Option<Resolution>,
    pub(super) interrupted: bool,
    pub(super) paused: bool,
    pub(super) can_start_provider_call: bool,
    pub(super) cancelled: bool,
    pub(super) deadline: Option<ActiveDeadlineCause>,
}

pub(super) fn refuse<'a>(reason: NotDispatched, stop: Option<StopCause>) -> Refusal<'a> {
    Refusal {
        reason,
        stop,
        narrowed: None,
    }
}

impl<'a> DispatchFacts<'a> {
    fn gate(&self) -> Result<(), Refusal<'a>> {
        let Permission {
            decision, basis, ..
        } = self.permission;
        if self
            .unguarded
            .as_ref()
            .is_some_and(|host| matches!(host.gate, Err(UnguardedRefusal::PrivilegeTool)))
        {
            return Err(refuse(
                NotDispatched::UnguardedNotOffered(UnguardedRefusal::PrivilegeTool),
                None,
            ));
        }
        if let Some((requested, maximum)) = self.timeout_limit {
            return Err(refuse(
                NotDispatched::TimeoutLimit { requested, maximum },
                None,
            ));
        }
        if decision == Decision::Deny || self.hooks.exhausted() {
            let (reason, stop) = if self.hooks.exhausted() || basis == PermissionSource::Hook {
                self.hooks.refusal(self.site)
            } else {
                let reason = match basis {
                    PermissionSource::Rule
                    | PermissionSource::TypedPrefixRule
                    | PermissionSource::HostPrefixRule => NotDispatched::DenyRule,
                    PermissionSource::PinnedGrant if self.host_unpinned => {
                        NotDispatched::HostSourceUnavailable
                    }
                    PermissionSource::PinnedGrant => NotDispatched::OutsideGrant,
                    PermissionSource::Mode
                    | PermissionSource::Human
                    | PermissionSource::AiReview
                    | PermissionSource::UnavailableApproval
                    | PermissionSource::SessionApproval
                    | PermissionSource::Hook => NotDispatched::ModeDenied,
                };
                (reason, None)
            };
            return Err(Refusal {
                reason,
                stop,
                narrowed: self.hooks.narrowed.as_ref(),
            });
        }
        if self.human_only_unavailable {
            return Err(refuse(NotDispatched::HumanOnlyUnavailable, None));
        }
        if let Some(unguarded) = &self.unguarded
            && let Err(refusal) = &unguarded.gate
        {
            return Err(refuse(NotDispatched::UnguardedNotOffered(*refusal), None));
        }
        if !self.goal_allows {
            return Err(refuse(NotDispatched::OutsidePlan, None));
        }
        Ok(())
    }

    pub(super) fn ask(&self) -> Option<ApprovalMode> {
        self.gate().ok()?;
        self.permission
            .approval
            .filter(|_| self.runtime_allows && self.can_start_provider_call)
    }

    pub(super) fn unguarded_prompt(&self) -> Option<UnguardedPrompt> {
        self.unguarded
            .as_ref()
            .and_then(|unguarded| unguarded.gate.as_ref().ok())
            .cloned()
    }

    fn unresolved_source(&self) -> PermissionSource {
        if self.permission.decision == Decision::Ask {
            PermissionSource::UnavailableApproval
        } else {
            self.permission.basis
        }
    }
}

pub(super) fn decide_dispatch<'a>(
    facts: &DispatchFacts<'a>,
    after: &AfterAsk,
) -> DispatchDecision<'a> {
    if let Err(refusal) = facts.gate() {
        return gate_refusal(facts, refusal);
    }

    let (approved, source, active_time_exhausted) = approval_gate(facts, after);
    let (approved, source) = host_gate(facts, after, approved, source);
    let outcome = dispatch_gate(facts, after, approved, source, active_time_exhausted);
    DispatchDecision {
        outcome,
        decision: facts.permission.decision,
        source,
    }
}

fn gate_refusal<'a>(facts: &DispatchFacts<'a>, refusal: Refusal<'a>) -> DispatchDecision<'a> {
    let (decision, source) = match refusal.reason {
        NotDispatched::HumanOnlyUnavailable => {
            (Decision::Ask, PermissionSource::UnavailableApproval)
        }
        NotDispatched::TimeoutLimit { .. }
        | NotDispatched::HookDenied(_)
        | NotDispatched::DelegationDeadline
        | NotDispatched::DenyRule
        | NotDispatched::OutsideGrant
        | NotDispatched::HostSourceUnavailable
        | NotDispatched::ModeDenied
        | NotDispatched::UnguardedNotOffered(_)
        | NotDispatched::OutsidePlan
        | NotDispatched::ActiveTime
        | NotDispatched::Paused
        | NotDispatched::Cancelled
        | NotDispatched::TokenBudget
        | NotDispatched::UnsavedRule
        | NotDispatched::SaveUnconfirmed
        | NotDispatched::UnguardedNeedsHuman
        | NotDispatched::ApprovalUnavailable
        | NotDispatched::ApprovalRefused => (facts.permission.decision, facts.unresolved_source()),
    };
    DispatchDecision {
        outcome: Err(refusal),
        decision,
        source,
    }
}

fn approval_gate(facts: &DispatchFacts<'_>, after: &AfterAsk) -> (bool, PermissionSource, bool) {
    let (approved, source, active_time_exhausted) = match (&after.approval, facts.site) {
        (Some(resolution), _) => (
            resolution.approved,
            resolution.source,
            resolution.active_time_exhausted,
        ),
        (None, DispatchSite::Primary) => (true, facts.unresolved_source(), false),
        (None, DispatchSite::Researcher) if facts.permission.approval.is_some() => {
            (false, PermissionSource::UnavailableApproval, false)
        }
        (None, DispatchSite::Researcher) => (true, facts.permission.basis, false),
    };
    let active_time_exhausted =
        active_time_exhausted || after.deadline == Some(ActiveDeadlineCause::Goal);
    if after.interrupted {
        (false, facts.unresolved_source(), active_time_exhausted)
    } else {
        (approved, source, active_time_exhausted)
    }
}

fn host_gate(
    facts: &DispatchFacts<'_>,
    after: &AfterAsk,
    approved: bool,
    source: PermissionSource,
) -> (bool, PermissionSource) {
    let typed_read_allow = facts.typed_host_read
        && facts.site == DispatchSite::Primary
        && facts.permission.decision == Decision::Allow
        && facts.permission.basis == PermissionSource::TypedPrefixRule
        && source == PermissionSource::TypedPrefixRule
        && after.approval.is_none();
    if facts.host
        && !typed_read_allow
        && (source != PermissionSource::Human || after.approval.is_none())
    {
        (false, PermissionSource::UnavailableApproval)
    } else {
        (approved, source)
    }
}

fn dispatch_gate<'a>(
    facts: &DispatchFacts<'a>,
    after: &AfterAsk,
    approved: bool,
    source: PermissionSource,
    active_time_exhausted: bool,
) -> Result<Dispatch, Refusal<'a>> {
    let refused = |reason| {
        let stop = match facts.site {
            DispatchSite::Primary if recoverable_primary_refusal(&reason) => None,
            DispatchSite::Researcher if recoverable_child_refusal(&reason) => None,
            DispatchSite::Primary | DispatchSite::Researcher => {
                Some(refusal_cause(after.paused, after.cancelled, after.deadline))
            }
        };
        Err(refuse(reason, stop))
    };
    match facts.site {
        _ if after.cancelled => refused(if after.paused {
            NotDispatched::Paused
        } else {
            NotDispatched::Cancelled
        }),
        _ if after.paused && after.approval.is_some() && !active_time_exhausted => {
            refused(NotDispatched::Paused)
        }
        DispatchSite::Researcher if active_time_exhausted => Err(refuse(
            NotDispatched::ActiveTime,
            Some(StopCause::ActiveTime),
        )),
        _ if after.deadline == Some(ActiveDeadlineCause::Delegation) => {
            refused(NotDispatched::DelegationDeadline)
        }
        DispatchSite::Primary if active_time_exhausted => Err(refuse(
            NotDispatched::ActiveTime,
            Some(StopCause::ActiveTime),
        )),
        _ if after.interrupted => refused(NotDispatched::ApprovalUnavailable),
        DispatchSite::Researcher if approved => Ok(Dispatch::Direct),
        DispatchSite::Researcher => refused(if source == PermissionSource::UnavailableApproval {
            NotDispatched::ApprovalUnavailable
        } else {
            NotDispatched::ApprovalRefused
        }),
        // The Guard path reserves its queue wait first, then reads the pause.
        DispatchSite::Primary if approved && after.can_start_provider_call && facts.guard_site => {
            Ok(Dispatch::QueuePause)
        }
        DispatchSite::Primary if after.paused => {
            Err(refuse(NotDispatched::Paused, Some(StopCause::Paused)))
        }
        DispatchSite::Primary if approved && !after.can_start_provider_call => {
            Err(refuse(NotDispatched::TokenBudget, None))
        }
        DispatchSite::Primary if approved => Ok(Dispatch::Direct),
        DispatchSite::Primary
            if !facts.interactive
                && facts
                    .unguarded
                    .as_ref()
                    .is_some_and(|unguarded| unguarded.gate.is_ok()) =>
        {
            refused(NotDispatched::UnguardedNeedsHuman)
        }
        DispatchSite::Primary => refused(if source == PermissionSource::UnavailableApproval {
            NotDispatched::ApprovalUnavailable
        } else {
            NotDispatched::ApprovalRefused
        }),
    }
}

fn recoverable_child_refusal(reason: &NotDispatched) -> bool {
    match reason {
        NotDispatched::HumanOnlyUnavailable
        | NotDispatched::ApprovalUnavailable
        | NotDispatched::ApprovalRefused => true,
        NotDispatched::TimeoutLimit { .. }
        | NotDispatched::HookDenied(_)
        | NotDispatched::DelegationDeadline
        | NotDispatched::DenyRule
        | NotDispatched::OutsideGrant
        | NotDispatched::HostSourceUnavailable
        | NotDispatched::ModeDenied
        | NotDispatched::UnguardedNotOffered(_)
        | NotDispatched::OutsidePlan
        | NotDispatched::ActiveTime
        | NotDispatched::Paused
        | NotDispatched::Cancelled
        | NotDispatched::TokenBudget
        | NotDispatched::UnsavedRule
        | NotDispatched::SaveUnconfirmed
        | NotDispatched::UnguardedNeedsHuman => false,
    }
}

pub(super) fn decide_queue_pause(reserved: bool, paused: bool) -> Result<(), Refusal<'static>> {
    if !reserved {
        Err(refuse(
            NotDispatched::ActiveTime,
            Some(StopCause::ActiveTime),
        ))
    } else if paused {
        Err(refuse(NotDispatched::Paused, Some(StopCause::Paused)))
    } else {
        Ok(())
    }
}

fn recoverable_primary_refusal(reason: &NotDispatched) -> bool {
    match reason {
        NotDispatched::ApprovalRefused
        | NotDispatched::ApprovalUnavailable
        | NotDispatched::UnguardedNeedsHuman => true,
        NotDispatched::TimeoutLimit { .. }
        | NotDispatched::HookDenied(_)
        | NotDispatched::DelegationDeadline
        | NotDispatched::DenyRule
        | NotDispatched::OutsideGrant
        | NotDispatched::HostSourceUnavailable
        | NotDispatched::ModeDenied
        | NotDispatched::HumanOnlyUnavailable
        | NotDispatched::UnguardedNotOffered(_)
        | NotDispatched::OutsidePlan
        | NotDispatched::ActiveTime
        | NotDispatched::Paused
        | NotDispatched::Cancelled
        | NotDispatched::TokenBudget
        | NotDispatched::UnsavedRule
        | NotDispatched::SaveUnconfirmed => false,
    }
}

fn refusal_cause(
    paused: bool,
    cancelled: bool,
    deadline: Option<ActiveDeadlineCause>,
) -> StopCause {
    if paused {
        StopCause::Paused
    } else if cancelled {
        StopCause::Cancelled
    } else {
        match deadline {
            Some(ActiveDeadlineCause::Goal) => StopCause::ActiveTime,
            Some(ActiveDeadlineCause::Delegation) => StopCause::DelegationDeadline,
            None => StopCause::ToolStopped,
        }
    }
}

// One arm per refusal keeps the observation mapping exhaustive.
pub(super) fn not_dispatched(intent: EffectIntent, reason: &NotDispatched) -> ToolObservation {
    // Every other Limit here, including a hook's delegation deadline, stays a Resource limit.
    let timed = match reason {
        NotDispatched::TimeoutLimit { .. } => Some(crate::tools::LimitCause::RequestedTime),
        NotDispatched::DelegationDeadline => Some(crate::tools::LimitCause::Timeout),
        NotDispatched::HookDenied(_)
        | NotDispatched::DenyRule
        | NotDispatched::OutsideGrant
        | NotDispatched::HostSourceUnavailable
        | NotDispatched::ModeDenied
        | NotDispatched::HumanOnlyUnavailable
        | NotDispatched::UnguardedNotOffered(_)
        | NotDispatched::OutsidePlan
        | NotDispatched::ActiveTime
        | NotDispatched::Paused
        | NotDispatched::Cancelled
        | NotDispatched::TokenBudget
        | NotDispatched::UnsavedRule
        | NotDispatched::SaveUnconfirmed
        | NotDispatched::UnguardedNeedsHuman
        | NotDispatched::ApprovalUnavailable
        | NotDispatched::ApprovalRefused => None,
    };
    let (disposition, output) = match reason {
        NotDispatched::TimeoutLimit { requested, maximum } => (
            ToolDisposition::Limit,
            crate::tools::types::command_timeout_limit_message(*requested, *maximum),
        ),
        NotDispatched::ActiveTime => (ToolDisposition::Limit, ACTIVE_TIME_EXHAUSTED.into()),
        NotDispatched::DelegationDeadline => (ToolDisposition::Limit, DELEGATION_DEADLINE.into()),
        NotDispatched::TokenBudget => (
            ToolDisposition::Limit,
            "Goal token budget exhausted; no operation was dispatched".into(),
        ),
        NotDispatched::HookDenied(denial) => hook_denial(denial),
        NotDispatched::SaveUnconfirmed => (
            ToolDisposition::Uncertain,
            "rule save not confirmed; no operation was dispatched".into(),
        ),
        NotDispatched::DenyRule => (
            ToolDisposition::Denied,
            DenialFeedback::RuleDenied.output().into(),
        ),
        NotDispatched::OutsideGrant => (
            ToolDisposition::Denied,
            "Tool operation is outside the pinned grant".into(),
        ),
        NotDispatched::HostSourceUnavailable => (
            ToolDisposition::Denied,
            "Host source data is unavailable; no operation was dispatched".into(),
        ),
        NotDispatched::ModeDenied => (
            ToolDisposition::Denied,
            DenialFeedback::ModeDenied.output().into(),
        ),
        NotDispatched::HumanOnlyUnavailable => (
            ToolDisposition::Denied,
            DenialFeedback::ChannelUnavailable.output().into(),
        ),
        NotDispatched::UnguardedNotOffered(UnguardedRefusal::SettingsChanged) => (
            ToolDisposition::Denied,
            "Host command refused: permission settings changed outside admission; use /permissions to re-admit them. Nothing ran".into(),
        ),
        NotDispatched::UnguardedNotOffered(UnguardedRefusal::UnavailableProgram) => (
            ToolDisposition::Denied,
            "Host program could not be pinned through an absolute PATH outside the Workspace; nothing ran".into(),
        ),
        NotDispatched::UnguardedNotOffered(UnguardedRefusal::InterpreterNotAdmitted) => (
            ToolDisposition::Denied,
            "Host interpreter requires a matching tools.json command with interpreter: true, executable path and digest; nothing ran".into(),
        ),
        NotDispatched::UnguardedNotOffered(UnguardedRefusal::GuardAlias) => (
            ToolDisposition::Denied,
            "Guard path aliases such as /workspace do not exist on the host; an unguarded re-run is not offered; no operation was dispatched".into(),
        ),
        NotDispatched::UnguardedNotOffered(UnguardedRefusal::PrivilegeTool) => (
            ToolDisposition::Denied, crate::tools::unguarded::manual_handoff(&intent.call),
        ),
        NotDispatched::OutsidePlan => (
            ToolDisposition::Denied,
            "The effect is outside the active Goal plan; no operation was dispatched".into(),
        ),
        NotDispatched::Cancelled => (
            ToolDisposition::Denied,
            DenialFeedback::Cancelled.output().into(),
        ),
        NotDispatched::Paused => (
            ToolDisposition::Denied,
            DenialFeedback::Paused.output().into(),
        ),
        NotDispatched::UnsavedRule => (
            ToolDisposition::Denied,
            "The always-allow rule could not be saved; no operation was dispatched".into(),
        ),
        NotDispatched::UnguardedNeedsHuman => (
            ToolDisposition::Denied,
            DenialFeedback::ChannelUnavailable.output().into(),
        ),
        NotDispatched::ApprovalUnavailable => (
            ToolDisposition::Denied,
            DenialFeedback::ChannelUnavailable.output().into(),
        ),
        NotDispatched::ApprovalRefused => (
            ToolDisposition::Denied,
            DenialFeedback::UserDeclined.output().into(),
        ),
    };
    let mut observation = undispatched(intent, disposition, output);
    observation.approval_cause = match reason {
        NotDispatched::Cancelled => Some(crate::tools::ApprovalCause::Cancelled),
        NotDispatched::Paused => Some(crate::tools::ApprovalCause::Paused),
        NotDispatched::ApprovalUnavailable
        | NotDispatched::HumanOnlyUnavailable
        | NotDispatched::UnguardedNeedsHuman => {
            Some(crate::tools::ApprovalCause::ChannelUnavailable)
        }
        NotDispatched::ApprovalRefused => Some(crate::tools::ApprovalCause::Declined),
        NotDispatched::HostSourceUnavailable
        | NotDispatched::TimeoutLimit { .. }
        | NotDispatched::HookDenied(_)
        | NotDispatched::DelegationDeadline
        | NotDispatched::DenyRule
        | NotDispatched::OutsideGrant
        | NotDispatched::ModeDenied
        | NotDispatched::UnguardedNotOffered(_)
        | NotDispatched::OutsidePlan
        | NotDispatched::ActiveTime
        | NotDispatched::TokenBudget
        | NotDispatched::UnsavedRule
        | NotDispatched::SaveUnconfirmed => None,
    };
    if timed.is_some() {
        observation.limit_cause = timed;
    }
    observation
}

const DELEGATION_DEADLINE: &str = "Delegation deadline reached; no operation was dispatched";
const ACTIVE_TIME_EXHAUSTED: &str =
    "Goal active-time budget exhausted; no operation was dispatched";

fn hook_denial(denial: &HookDenial) -> (ToolDisposition, String) {
    let output = match denial {
        HookDenial::ActiveTime => return (ToolDisposition::Limit, ACTIVE_TIME_EXHAUSTED.into()),
        HookDenial::DelegationDeadline => {
            return (ToolDisposition::Limit, DELEGATION_DEADLINE.into());
        }
        HookDenial::Uncertain => "Hook outcome uncertain; no operation was dispatched".into(),
        HookDenial::Requested => "Hook stopped this Run; no operation was dispatched".into(),
        HookDenial::Narrowed(name) => format!("Hook narrowed this Tool to: {name}"),
        HookDenial::Denied(_) => DenialFeedback::HookDenied.output().to_owned(),
    };
    (ToolDisposition::Denied, output)
}

pub(super) fn undispatched(
    intent: EffectIntent,
    disposition: ToolDisposition,
    output: impl Into<String>,
) -> ToolObservation {
    ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: (disposition == ToolDisposition::Limit)
            .then_some(crate::tools::LimitCause::Resource),
        intent,
        disposition,
        output: output.into(),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: CacheUpdate::NotApplicable,
    }
}

#[cfg(test)]
mod tests {
    use super::super::hooks::HookStop;
    use super::*;

    #[test]
    fn deadline_completion_only_converts_timeout_cancelled_outcomes() {
        for (disposition, cancelled) in [
            (ToolDisposition::Succeeded, false),
            (ToolDisposition::Denied, false),
            (ToolDisposition::Failed, false),
            (ToolDisposition::Conflict, false),
            (ToolDisposition::Limit, false),
            (ToolDisposition::Cancelled, true),
            (ToolDisposition::Uncertain, false),
        ] {
            for elapsed in [false, true] {
                for (cause, stop) in [
                    (None, StopCause::ToolStopped),
                    (Some(ActiveDeadlineCause::Goal), StopCause::ActiveTime),
                    (
                        Some(ActiveDeadlineCause::Delegation),
                        StopCause::DelegationDeadline,
                    ),
                ] {
                    let expected = if elapsed && cancelled {
                        StepEnd::Stopped(stop)
                    } else {
                        StepEnd::Continue
                    };
                    assert_eq!(
                        completion_deadline(disposition, elapsed, cause),
                        expected,
                        "{disposition:?}, elapsed={elapsed}, cause={cause:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn step_dispatch_uses_fetched_overhead_and_keeps_effects_serial() {
        use StepDispatch::{ConcurrentRead, Serial};
        let mut search = read_intent();
        search.call = ToolCall::Search {
            path: "file".into(),
            query: "fact".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 1,
        };
        let mut skill = read_intent();
        skill.call = ToolCall::Skill {
            name: "review".into(),
            resource: None,
        };
        let mut mutation = read_intent();
        mutation.call = ToolCall::Mkdir {
            path: "directory".into(),
        };
        let mut guarded = read_intent();
        guarded.call = ToolCall::Command {
            program: "check".into(),
            args: Vec::new(),
            cwd: String::new(),
            resource_class: None,
            timeout_ms: None,
        };
        for (calls, intent, overhead, expected) in [
            (1, read_intent(), Duration::ZERO, Serial),
            (2, read_intent(), Duration::ZERO, ConcurrentRead),
            (4, read_intent(), Duration::ZERO, ConcurrentRead),
            (2, read_intent(), Duration::from_secs(1), Serial),
            (2, search.clone(), Duration::ZERO, ConcurrentRead),
            (2, search, Duration::from_secs(1), Serial),
            (2, skill, Duration::ZERO, ConcurrentRead),
            (2, mutation, Duration::ZERO, Serial),
            (2, guarded, Duration::ZERO, Serial),
        ] {
            assert_eq!(StepDispatch::decide(calls, &intent, overhead), expected);
        }
    }

    #[test]
    fn exhausted_active_time_replaces_unstarted_limits_but_preserves_attested_limits() {
        for (disposition, attested, exhausted, expected) in [
            (
                ToolDisposition::Limit,
                false,
                true,
                GuardCompletion::ActiveTime,
            ),
            (
                ToolDisposition::Limit,
                true,
                true,
                GuardCompletion::Observed,
            ),
            (
                ToolDisposition::Limit,
                false,
                false,
                GuardCompletion::Observed,
            ),
            (
                ToolDisposition::Failed,
                false,
                true,
                GuardCompletion::ActiveTime,
            ),
            (
                ToolDisposition::Failed,
                false,
                false,
                GuardCompletion::Observed,
            ),
        ] {
            assert_eq!(
                GuardCompletion::decide(disposition, attested, exhausted),
                expected
            );
        }
    }

    #[test]
    fn refusals_keep_their_own_cause() {
        for (paused, cancelled, deadline, expected) in [
            (
                true,
                true,
                Some(ActiveDeadlineCause::Goal),
                StopCause::Paused,
            ),
            (
                false,
                true,
                Some(ActiveDeadlineCause::Goal),
                StopCause::Cancelled,
            ),
            (
                false,
                false,
                Some(ActiveDeadlineCause::Goal),
                StopCause::ActiveTime,
            ),
            (
                false,
                false,
                Some(ActiveDeadlineCause::Delegation),
                StopCause::DelegationDeadline,
            ),
            (false, false, None, StopCause::ToolStopped),
        ] {
            assert_eq!(refusal_cause(paused, cancelled, deadline), expected);
        }
    }

    fn allow(basis: PermissionSource) -> Permission {
        Permission {
            decision: Decision::Allow,
            basis,
            approval: None,
        }
    }

    fn asks(mode: ApprovalMode) -> Permission {
        Permission {
            decision: Decision::Ask,
            basis: PermissionSource::Rule,
            approval: Some(mode),
        }
    }

    fn deny(basis: PermissionSource) -> Permission {
        Permission {
            decision: Decision::Deny,
            basis,
            approval: None,
        }
    }

    fn hook(stop: HookStop, narrowed: Option<ToolCall>) -> HookFold {
        HookFold {
            restriction: Some(Decision::Deny),
            count: 1,
            narrowed,
            reason: Some("no".into()),
            stop,
        }
    }

    fn resolution(approved: bool, source: PermissionSource) -> Resolution {
        Resolution {
            approved,
            source,
            active_time_exhausted: false,
            save: None,
            withdrawal: None,
        }
    }

    fn prompt() -> Result<UnguardedPrompt, UnguardedRefusal> {
        Ok(UnguardedPrompt {
            sensitive: false,
            inactive_rules: false,
            executable: "/bin/make".into(),
            attribution: None,
            remember: true,
            prefix: false,
        })
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Fact {
        Host,
        TypedHostRead,
        HostUnpinned,
        Timeout,
        HumanOnly,
        NotOffered,
        PrivilegeTool,
        SettingsChanged,
        UnavailableProgram,
        InterpreterNotAdmitted,
        Offered,
        OutsidePlan,
        Blocked,
        TokensSpent,
        TokensSpentAfterAsk,
        InProcess,
        Batch,
        Paused,
        Cancelled,
        Interrupted,
        Deadline,
        GoalDeadline,
    }

    fn decide<'a>(
        site: DispatchSite,
        permission: Permission,
        hooks: &'a HookFold,
        facts: &[Fact],
        approval: Option<Resolution>,
    ) -> (Option<ApprovalMode>, DispatchDecision<'a>) {
        let has = |fact| facts.contains(&fact);
        let gate = if has(Fact::PrivilegeTool) {
            Some(Err(UnguardedRefusal::PrivilegeTool))
        } else if has(Fact::SettingsChanged) {
            Some(Err(UnguardedRefusal::SettingsChanged))
        } else if has(Fact::UnavailableProgram) {
            Some(Err(UnguardedRefusal::UnavailableProgram))
        } else if has(Fact::InterpreterNotAdmitted) {
            Some(Err(UnguardedRefusal::InterpreterNotAdmitted))
        } else if has(Fact::NotOffered) {
            Some(Err(UnguardedRefusal::GuardAlias))
        } else if has(Fact::Offered) {
            Some(prompt())
        } else {
            None
        };
        let facts = DispatchFacts {
            site,
            permission,
            hooks,
            timeout_limit: has(Fact::Timeout).then_some((9_000, 5_000)),
            host: has(Fact::Host) || has(Fact::HostUnpinned),
            typed_host_read: has(Fact::TypedHostRead),
            host_unpinned: has(Fact::HostUnpinned),
            human_only_unavailable: has(Fact::HumanOnly),
            unguarded: gate.map(|gate| Unguarded { gate }),
            goal_allows: !has(Fact::OutsidePlan),
            runtime_allows: !has(Fact::Blocked),
            can_start_provider_call: !has(Fact::TokensSpent),
            guard_site: !has(Fact::InProcess),
            interactive: !has(Fact::Batch),
        };
        let after = AfterAsk {
            approval,
            interrupted: has(Fact::Interrupted),
            paused: has(Fact::Paused),
            can_start_provider_call: !has(Fact::TokensSpent) && !has(Fact::TokensSpentAfterAsk),
            cancelled: has(Fact::Cancelled),
            deadline: if has(Fact::GoalDeadline) {
                Some(ActiveDeadlineCause::Goal)
            } else if has(Fact::Deadline) {
                Some(ActiveDeadlineCause::Delegation)
            } else {
                None
            },
        };
        (facts.ask(), decide_dispatch(&facts, &after))
    }

    #[test]
    fn host_dispatch_requires_an_explicit_human_resolution() {
        let permission = Permission {
            decision: Decision::Ask,
            basis: PermissionSource::Mode,
            approval: Some(ApprovalMode::Request),
        };
        let hooks = HookFold::none();
        for source in [
            None,
            Some(PermissionSource::Mode),
            Some(PermissionSource::Rule),
            Some(PermissionSource::AiReview),
            Some(PermissionSource::Human),
        ] {
            let resolution = source.map(|source| Resolution {
                approved: true,
                source,
                active_time_exhausted: false,
                save: None,
                withdrawal: None,
            });
            let (_, decision) = decide(
                DispatchSite::Primary,
                permission,
                &hooks,
                &[Fact::Host, Fact::InProcess],
                resolution,
            );
            assert_eq!(
                decision.outcome.is_ok(),
                source == Some(PermissionSource::Human)
            );
        }
    }

    #[test]
    fn primary_typed_host_read_rule_authorizes_dispatch_without_human_channel() {
        let permission = allow(PermissionSource::TypedPrefixRule);
        let hooks = HookFold::none();
        let (_, allowed) = decide(
            DispatchSite::Primary,
            permission,
            &hooks,
            &[Fact::Host, Fact::TypedHostRead, Fact::InProcess],
            None,
        );
        assert!(allowed.outcome.is_ok());
        assert_eq!(allowed.source, PermissionSource::TypedPrefixRule);

        let (_, untyped) = decide(
            DispatchSite::Primary,
            permission,
            &hooks,
            &[Fact::Host, Fact::InProcess],
            None,
        );
        assert!(untyped.outcome.is_err());

        let (_, child) = decide(
            DispatchSite::Researcher,
            permission,
            &hooks,
            &[Fact::Host, Fact::TypedHostRead, Fact::InProcess],
            None,
        );
        assert!(child.outcome.is_err());
    }

    #[test]
    fn host_dispatch_preserves_explicit_human_denial() {
        let permission = Permission {
            decision: Decision::Ask,
            basis: PermissionSource::Mode,
            approval: Some(ApprovalMode::Request),
        };
        let hooks = HookFold::none();
        let (_, decision) = decide(
            DispatchSite::Primary,
            permission,
            &hooks,
            &[Fact::Host, Fact::InProcess],
            Some(Resolution {
                approved: false,
                source: PermissionSource::Human,
                active_time_exhausted: false,
                save: None,
                withdrawal: None,
            }),
        );
        assert!(decision.outcome.is_err());
        assert_eq!(decision.source, PermissionSource::Human);
    }

    #[test]
    fn dispatch_decisions_cover_every_reason_verdict_and_site() {
        use ApprovalMode::{Auto, Request};
        use DispatchSite::{Primary, Researcher};
        use Fact::*;
        use NotDispatched as N;
        use PermissionSource as S;
        let call = || ToolCall::Read {
            path: "file".into(),
            offset: None,
            limit: Some(1),
        };
        let none = HookFold::none();
        let denied = hook(HookStop::None, None);
        let narrowing = hook(HookStop::None, Some(call()));
        let stopped = hook(HookStop::Requested, None);
        let uncertain = hook(HookStop::Uncertain, None);
        let exhausted = hook(HookStop::ActiveTime, None);
        let no = |reason, stop| Err(refuse(reason, stop));
        let narrow = |reason| {
            Err(Refusal {
                reason,
                stop: None,
                narrowed: narrowing.narrowed.as_ref(),
            })
        };
        let ok = |approved, source| Some(resolution(approved, source));
        let exhausted_ask = Some(Resolution {
            active_time_exhausted: true,
            ..resolution(false, S::Human)
        });
        let timeout = || N::TimeoutLimit {
            requested: 9_000,
            maximum: 5_000,
        };
        let (allow_, ask_, deny_) = (Decision::Allow, Decision::Ask, Decision::Deny);
        #[rustfmt::skip]
        let rows = [
            (decide(Primary, allow(S::Rule), &none, &[InProcess, Interrupted], None), None, no(N::ApprovalUnavailable, None), allow_, S::Rule),
            (decide(Researcher, allow(S::Rule), &none, &[InProcess, Interrupted], None), None, no(N::ApprovalUnavailable, None), allow_, S::Rule),
            (decide(Primary, allow(S::Rule), &none, &[InProcess, Interrupted, GoalDeadline], None), None, no(N::ActiveTime, Some(StopCause::ActiveTime)), allow_, S::Rule),
            (decide(Primary, allow(S::Rule), &none, &[InProcess, Interrupted, Cancelled], None), None, no(N::Cancelled, Some(StopCause::Cancelled)), allow_, S::Rule),
            // Gate refusals, decided before any Ask.
            (decide(Primary, asks(Request), &none, &[PrivilegeTool, Timeout], None), None, no(N::UnguardedNotOffered(UnguardedRefusal::PrivilegeTool), None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[SettingsChanged], None), None, no(N::UnguardedNotOffered(UnguardedRefusal::SettingsChanged), None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[UnavailableProgram], None), None, no(N::UnguardedNotOffered(UnguardedRefusal::UnavailableProgram), None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[InterpreterNotAdmitted], None), None, no(N::UnguardedNotOffered(UnguardedRefusal::InterpreterNotAdmitted), None), ask_, S::UnavailableApproval),
            (decide(Primary, allow(S::SessionApproval), &none, &[Offered, InProcess], None), None, Ok(Dispatch::Direct), allow_, S::SessionApproval),
            (decide(Primary, allow(S::HostPrefixRule), &none, &[Offered, InProcess], None), None, Ok(Dispatch::Direct), allow_, S::HostPrefixRule),
            (decide(Primary, allow(S::Rule), &none, &[Timeout], None), None, no(timeout(), None), allow_, S::Rule),
            (decide(Researcher, asks(Request), &none, &[Timeout], None), None, no(timeout(), None), ask_, S::UnavailableApproval),
            (decide(Primary, deny(S::Rule), &none, &[], None), None, no(N::DenyRule, None), deny_, S::Rule),
            (decide(Primary, deny(S::HostPrefixRule), &none, &[], None), None, no(N::DenyRule, None), deny_, S::HostPrefixRule),
            (decide(Researcher, deny(S::PinnedGrant), &none, &[], None), None, no(N::OutsideGrant, None), deny_, S::PinnedGrant),
            (decide(Primary, deny(S::PinnedGrant), &none, &[HostUnpinned], None), None, no(N::HostSourceUnavailable, None), deny_, S::PinnedGrant),
            (decide(Primary, deny(S::Hook), &denied, &[HostUnpinned], None), None, no(N::HookDenied(HookDenial::Denied("no".into())), None), deny_, S::Hook),
            (decide(Primary, deny(S::PinnedGrant), &exhausted, &[HostUnpinned], None), None, no(N::HookDenied(HookDenial::ActiveTime), None), deny_, S::PinnedGrant),
            (decide(Primary, deny(S::Mode), &none, &[], None), None, no(N::ModeDenied, None), deny_, S::Mode),
            (decide(Primary, deny(S::Hook), &denied, &[], None), None, no(N::HookDenied(HookDenial::Denied("no".into())), None), deny_, S::Hook),
            (decide(Primary, deny(S::Hook), &narrowing, &[], None), None, narrow(N::HookDenied(HookDenial::Narrowed("Read".into()))), deny_, S::Hook),
            (decide(Primary, deny(S::Rule), &narrowing, &[], None), None, narrow(N::DenyRule), deny_, S::Rule),
            (decide(Primary, deny(S::Hook), &stopped, &[], None), None, no(N::HookDenied(HookDenial::Requested), Some(StopCause::HookStop)), deny_, S::Hook),
            (decide(Researcher, deny(S::Hook), &uncertain, &[], None), None, no(N::HookDenied(HookDenial::Uncertain), Some(StopCause::Uncertain)), deny_, S::Hook),
            (decide(Primary, asks(Request), &exhausted, &[], None), None, no(N::HookDenied(HookDenial::ActiveTime), None), ask_, S::UnavailableApproval),
            (decide(Researcher, allow(S::Rule), &exhausted, &[], None), None, no(N::HookDenied(HookDenial::DelegationDeadline), Some(StopCause::DelegationDeadline)), allow_, S::Rule),
            (decide(Primary, asks(Request), &none, &[HumanOnly], None), None, no(N::HumanOnlyUnavailable, None), ask_, S::UnavailableApproval),
            (decide(Researcher, allow(S::Rule), &none, &[HumanOnly], None), None, no(N::HumanOnlyUnavailable, None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[NotOffered], None), None, no(N::UnguardedNotOffered(UnguardedRefusal::GuardAlias), None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Auto), &none, &[OutsidePlan], None), None, no(N::OutsidePlan, None), ask_, S::UnavailableApproval),
            (decide(Researcher, allow(S::Mode), &none, &[OutsidePlan], None), None, no(N::OutsidePlan, None), allow_, S::Mode),
            // The primary after its Ask, or where it would have been.
            (decide(Primary, asks(Request), &none, &[InProcess, Interrupted], None), Some(Request), no(N::ApprovalUnavailable, None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[InProcess, Interrupted], ok(true, S::Human)), Some(Request), no(N::ApprovalUnavailable, None), ask_, S::UnavailableApproval),
            (decide(Researcher, asks(Request), &none, &[InProcess, Interrupted], None), Some(Request), no(N::ApprovalUnavailable, None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[Offered], ok(true, S::Human)), Some(Request), Ok(Dispatch::QueuePause), ask_, S::Human),
            (decide(Primary, allow(S::Rule), &none, &[Paused], None), None, Ok(Dispatch::QueuePause), allow_, S::Rule),
            (decide(Primary, allow(S::Rule), &none, &[InProcess], None), None, Ok(Dispatch::Direct), allow_, S::Rule),
            (decide(Primary, asks(Auto), &none, &[Blocked], None), None, Ok(Dispatch::QueuePause), ask_, S::UnavailableApproval),
            (decide(Primary, allow(S::Rule), &none, &[InProcess, Paused], None), None, no(N::Paused, Some(StopCause::Paused)), allow_, S::Rule),
            (decide(Primary, asks(Request), &none, &[TokensSpent, Paused], None), None, no(N::Paused, Some(StopCause::Paused)), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[TokensSpent], None), None, no(N::TokenBudget, None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Auto), &none, &[TokensSpentAfterAsk], ok(true, S::AiReview)), Some(Auto), no(N::TokenBudget, None), ask_, S::AiReview),
            (decide(Primary, asks(Request), &none, &[Paused], exhausted_ask), Some(Request), no(N::ActiveTime, Some(StopCause::ActiveTime)), ask_, S::Human),
            (decide(Primary, asks(Request), &none, &[Offered, Batch], ok(false, S::UnavailableApproval)), Some(Request), no(N::UnguardedNeedsHuman, None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[Offered, Cancelled], ok(false, S::Human)), Some(Request), no(N::Cancelled, Some(StopCause::Cancelled)), ask_, S::Human),
            (decide(Primary, asks(Request), &none, &[], ok(false, S::Human)), Some(Request), no(N::ApprovalRefused, None), ask_, S::Human),
            (decide(Primary, asks(Auto), &none, &[Batch], ok(false, S::UnavailableApproval)), Some(Auto), no(N::ApprovalUnavailable, None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[Cancelled], ok(true, S::Human)), Some(Request), no(N::Cancelled, Some(StopCause::Cancelled)), ask_, S::Human),
            // A researcher after its Ask, or where it would have been.
            (decide(Researcher, allow(S::Rule), &none, &[Paused], None), None, Ok(Dispatch::Direct), allow_, S::Rule),
            (decide(Researcher, asks(Auto), &none, &[], ok(true, S::AiReview)), Some(Auto), Ok(Dispatch::Direct), ask_, S::AiReview),
            (decide(Researcher, asks(Request), &none, &[], ok(false, S::Human)), Some(Request), no(N::ApprovalRefused, None), ask_, S::Human),
            (decide(Researcher, asks(Request), &none, &[Blocked], None), None, no(N::ApprovalUnavailable, None), ask_, S::UnavailableApproval),
            (decide(Researcher, asks(Request), &none, &[Blocked, Deadline], None), None, no(N::DelegationDeadline, Some(StopCause::DelegationDeadline)), ask_, S::UnavailableApproval),
            (decide(Researcher, asks(Request), &none, &[Blocked, GoalDeadline], None), None, no(N::ActiveTime, Some(StopCause::ActiveTime)), ask_, S::UnavailableApproval),
            (decide(Researcher, asks(Request), &none, &[GoalDeadline], ok(true, S::Human)), Some(Request), no(N::ActiveTime, Some(StopCause::ActiveTime)), ask_, S::Human),
            (decide(Researcher, asks(Request), &none, &[GoalDeadline, Cancelled], ok(true, S::Human)), Some(Request), no(N::Cancelled, Some(StopCause::Cancelled)), ask_, S::Human),
            (decide(Researcher, asks(Request), &none, &[TokensSpent, Cancelled], None), None, no(N::Cancelled, Some(StopCause::Cancelled)), ask_, S::UnavailableApproval),
            (decide(Researcher, asks(Request), &none, &[Paused, Cancelled], ok(false, S::Human)), Some(Request), no(N::Paused, Some(StopCause::Paused)), ask_, S::Human),
        ];
        for (index, ((asked, decided), asks, outcome, decision, source)) in
            rows.into_iter().enumerate()
        {
            assert_eq!(asked, asks, "row {index}");
            assert_eq!(
                decided,
                DispatchDecision {
                    outcome,
                    decision,
                    source,
                },
                "row {index}"
            );
        }
        for (reserved, paused, reason, stop) in [
            (true, true, N::Paused, StopCause::Paused),
            (false, true, N::ActiveTime, StopCause::ActiveTime),
            (false, false, N::ActiveTime, StopCause::ActiveTime),
        ] {
            assert_eq!(
                decide_queue_pause(reserved, paused),
                Err(refuse(reason, Some(stop)))
            );
        }
        assert_eq!(decide_queue_pause(true, false), Ok(()));
    }

    fn read_intent() -> EffectIntent {
        EffectIntent {
            host: None,
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::session::RunId::new(),
            agent_run_id: crate::session::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Read {
                path: "file".into(),
                offset: None,
                limit: Some(1),
            },
            limits: crate::tools::ToolLimits::default(),
            use_count: 1,
        }
    }

    #[test]
    fn hook_denial_feedback_omits_untrusted_reason() {
        let canary = "untrusted hook reason";
        let observation = not_dispatched(
            read_intent(),
            &NotDispatched::HookDenied(HookDenial::Denied(canary.into())),
        );
        assert_eq!(
            observation.output,
            "hook_denied: no operation was dispatched"
        );
        assert!(observation.valid());
        assert!(!observation.output.contains(canary));
    }

    #[test]
    fn approval_denial_history_preserves_dispatch_output_without_redaction() {
        let mut historical_timeout = undispatched(
            read_intent(),
            ToolDisposition::Denied,
            DenialFeedback::ChildTimeout.output(),
        );
        historical_timeout.approval_cause = Some(crate::tools::ApprovalCause::ChildTimeout);
        for (observation, expected) in [
            (
                not_dispatched(read_intent(), &NotDispatched::Cancelled),
                "Run cancelled before this operation; no operation was dispatched",
            ),
            (
                not_dispatched(read_intent(), &NotDispatched::Paused),
                "Goal paused before this operation; no operation was dispatched",
            ),
            (
                historical_timeout,
                "Child approval wait timed out; no operation was dispatched",
            ),
            (
                not_dispatched(read_intent(), &NotDispatched::ApprovalRefused),
                "user_declined: no operation was dispatched",
            ),
            (
                not_dispatched(read_intent(), &NotDispatched::ApprovalUnavailable),
                "channel_unavailable: no operation was dispatched",
            ),
        ] {
            assert_eq!(observation.output, expected);
            let tool = crate::session::ToolView {
                checkpoint: None,
                intent: observation.intent.clone(),
                observation: Some(observation),
                review_call_index: None,
                active_time_reserved: false,
                narrowing_source: None,
                permission_verdict: None,
                started_sequence: Some(0),
                hooks: Vec::new(),
                hook_sequence: None,
                finished_sequence: None,
            };
            assert_eq!(tool.model_denial_output(), Some(expected));
            assert!(!tool.denial_needs_redaction());
        }
    }
}
