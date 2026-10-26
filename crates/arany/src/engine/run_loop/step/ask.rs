use super::dispatch::UnguardedRefusal;
use super::permission::RuleSave;
use super::{Grant, ReviewProgress, RunLoop};
use crate::engine::ledger::{Action, Admission};
use crate::engine::{EngineError, RunCancellation};
use crate::tools::permissions::{PermissionSource, TypedGitPrefix};
use crate::tools::{
    ApprovalMode, ApprovalReply, ApprovalWithdrawal, EffectIntent, ToolApprovals, ToolCall,
    ToolRuntime, UnguardedPrompt,
};

pub(super) enum Resolver<'a> {
    AiReview { objective: &'a str },
    Human,
}

pub(super) enum AskSite {
    Primary(AskOptions),
    Delegated {
        active_deadline: Option<crate::engine::ledger::ActiveDeadline>,
        interrupted: Option<RunCancellation>,
    },
}

pub(super) struct AskOptions {
    pub(super) unguarded_prompt: Option<UnguardedPrompt>,
    pub(super) allow_auto_edits: bool,
    pub(super) remember_prefix: Option<TypedGitPrefix>,
    pub(super) interrupted: Option<RunCancellation>,
}

impl AskSite {
    fn interrupted(&self) -> Option<&RunCancellation> {
        match self {
            Self::Primary(options) => options.interrupted.as_ref(),
            Self::Delegated { interrupted, .. } => interrupted.as_ref(),
        }
    }
}

pub(super) async fn interrupted(cancellation: Option<&RunCancellation>) {
    match cancellation {
        Some(cancellation) => cancellation.clone().cancelled().await,
        None => std::future::pending().await,
    }
}

pub(super) struct Resolution {
    pub(super) approved: bool,
    pub(super) source: PermissionSource,
    pub(super) active_time_exhausted: bool,
    pub(super) save: Option<RuleSave>,
    pub(super) withdrawal: Option<ApprovalWithdrawal>,
}

impl RunLoop<'_> {
    pub(super) async fn ask(
        &self,
        grant: &Grant<'_>,
        intent: &EffectIntent,
        mode: ApprovalMode,
        options: AskOptions,
    ) -> Result<Admission<Resolution>, EngineError> {
        match grant {
            Grant::Primary {
                request,
                review_allowed,
            } => {
                let resolvers;
                let chain = if mode == ApprovalMode::Auto
                    && *review_allowed
                    && self.ledger().admits(Action::Review)
                {
                    resolvers = [
                        Resolver::AiReview {
                            objective: &request.input.objective,
                        },
                        Resolver::Human,
                    ];
                    &resolvers[..]
                } else {
                    &[Resolver::Human]
                };
                self.resolve_ask(intent, chain, AskSite::Primary(options))
                    .await
            }
            Grant::Researcher {
                active_deadline, ..
            } => {
                let objective =
                    if mode == ApprovalMode::Auto && self.ledger().admits(Action::Review) {
                        Some(
                            self.ledger()
                                .view(|view| {
                                    self.goal_id
                                        .and_then(|id| view.goal(id))
                                        .map(|goal| goal.objective.clone())
                                })?
                                .ok_or(EngineError::InvalidHistory)?,
                        )
                    } else {
                        None
                    };
                let resolvers;
                let chain = if let Some(objective) = &objective {
                    resolvers = [Resolver::AiReview { objective }, Resolver::Human];
                    &resolvers[..]
                } else {
                    &[Resolver::Human]
                };
                self.resolve_ask(
                    intent,
                    chain,
                    AskSite::Delegated {
                        active_deadline: active_deadline.clone(),
                        interrupted: options.interrupted,
                    },
                )
                .await
            }
        }
    }

    async fn resolve_ask(
        &self,
        intent: &EffectIntent,
        chain: &[Resolver<'_>],
        site: AskSite,
    ) -> Result<Admission<Resolution>, EngineError> {
        self.propose_tool(intent).await?;
        let initial_mode = self.terminal_mode();
        let mut resolution = Resolution {
            approved: false,
            source: PermissionSource::UnavailableApproval,
            active_time_exhausted: false,
            save: None,
            withdrawal: None,
        };
        for resolver in chain {
            if site
                .interrupted()
                .is_some_and(RunCancellation::is_cancelled)
            {
                break;
            }
            match resolver {
                Resolver::AiReview { objective } => {
                    match self
                        .resolve_ai_review(objective, intent, &site, initial_mode, &mut resolution)
                        .await?
                    {
                        ReviewProgress::Continue => {}
                        ReviewProgress::Finish => break,
                        ReviewProgress::Exhausted => return Ok(Admission::Exhausted),
                    }
                }
                Resolver::Human => match &site {
                    AskSite::Delegated {
                        active_deadline,
                        interrupted,
                    } => {
                        self.resolve_delegated_human(
                            intent,
                            active_deadline.as_ref(),
                            &mut resolution,
                            interrupted.as_ref(),
                        )
                        .await?
                    }
                    AskSite::Primary(options) => {
                        self.resolve_primary_human(intent, options, &mut resolution)
                            .await?
                    }
                },
            }
            if resolution.approved {
                break;
            }
        }
        Ok(Admission::Reserved(resolution))
    }

    async fn resolve_ai_review(
        &self,
        objective: &str,
        intent: &EffectIntent,
        site: &AskSite,
        initial_mode: Option<ApprovalMode>,
        resolution: &mut Resolution,
    ) -> Result<ReviewProgress, EngineError> {
        let reviewed = self
            .resolve_review(objective, intent, site, initial_mode)
            .await?;
        resolution.approved = reviewed.approved;
        resolution.source = reviewed.source;
        resolution.active_time_exhausted = reviewed.active_time_exhausted;
        Ok(reviewed.progress)
    }

    async fn resolve_delegated_human(
        &self,
        intent: &EffectIntent,
        active_deadline: Option<&crate::engine::ledger::ActiveDeadline>,
        resolution: &mut Resolution,
        interrupted: Option<&RunCancellation>,
    ) -> Result<(), EngineError> {
        let Some(approvals) = self.controls.approvals else {
            return Ok(());
        };
        let (reply, withdrawal) = self
            .delegated_human_reply(intent, active_deadline, approvals, interrupted)
            .await?;
        resolution.withdrawal = withdrawal;
        resolution.approved = matches!(
            reply,
            ApprovalReply::AllowOnce
                | ApprovalReply::AllowForSession
                | ApprovalReply::AllowPrefixInFolder
        ) && !self.pause_requested()
            && !self.controls.cancellation.is_cancelled();
        resolution.source = if reply == ApprovalReply::Unavailable {
            PermissionSource::UnavailableApproval
        } else {
            PermissionSource::Human
        };
        resolution.active_time_exhausted = active_deadline.is_some_and(|deadline| {
            deadline.reached() && deadline.cause == crate::engine::ledger::ActiveDeadlineCause::Goal
        });
        Ok(())
    }

    pub(super) async fn approval_paused(&self) {
        match self.controls.steering.as_ref() {
            Some(steering) => steering.paused().await,
            None => std::future::pending().await,
        }
    }

    pub(super) async fn human_reply(
        &self,
        approvals: &ToolApprovals,
        intent: &EffectIntent,
        options: &AskOptions,
    ) -> (ApprovalReply, Option<ApprovalWithdrawal>) {
        let prompt = options.unguarded_prompt.clone();
        let mut cancellation = self.controls.cancellation.clone();
        let (reply, withdrawal) = if prompt.is_none() && options.interrupted.is_some() {
            self.interruptible_local_reply(approvals, intent, options)
                .await
        } else {
            let reply = tokio::select! {
                biased;
                () = cancellation.cancelled() => ApprovalReply::Unavailable,
                () = self.approval_paused() => ApprovalReply::Unavailable,
                reply = async {
                    match prompt.clone() {
                        Some(prompt) => approvals.ask_unguarded(intent, prompt).await,
                        None => approvals.ask_local(intent, options.allow_auto_edits, options.remember_prefix).await,
                    }
                } => reply,
            };
            (reply, None)
        };
        let remember = prompt.as_ref().is_some_and(|prompt| prompt.remember);
        let reply = match reply {
            ApprovalReply::AllowForSession if !remember => ApprovalReply::Unavailable,
            ApprovalReply::AllowPrefixInFolder
                if options.remember_prefix.is_none()
                    && !prompt.as_ref().is_some_and(|prompt| prompt.prefix) =>
            {
                if intent.call.is_unguarded() || intent.call.is_host() {
                    ApprovalReply::Unavailable
                } else {
                    ApprovalReply::AllowOnce
                }
            }
            reply @ (ApprovalReply::Deny
            | ApprovalReply::Unavailable
            | ApprovalReply::AllowOnce
            | ApprovalReply::AllowForSession
            | ApprovalReply::AllowPrefixInFolder) => reply,
        };
        (reply, withdrawal)
    }

    async fn interruptible_local_reply(
        &self,
        approvals: &ToolApprovals,
        intent: &EffectIntent,
        options: &AskOptions,
    ) -> (ApprovalReply, Option<ApprovalWithdrawal>) {
        let mut cancellation = self.controls.cancellation.clone();
        let mut pending = tokio::select! {
            biased;
            () = interrupted(options.interrupted.as_ref()) => return (ApprovalReply::Unavailable, None),
            () = cancellation.cancelled() => return (ApprovalReply::Unavailable, None),
            () = self.approval_paused() => return (ApprovalReply::Unavailable, None),
            pending = approvals.begin_local(intent, options.allow_auto_edits, options.remember_prefix) => pending,
        };
        let reply = tokio::select! {
            biased;
            () = interrupted(options.interrupted.as_ref()) => None,
            () = cancellation.cancelled() => None,
            () = self.approval_paused() => None,
            reply = pending.reply() => Some(reply),
        };
        match reply {
            Some(crate::tools::ApprovalWaitReply::Replied(reply)) => (reply, None),
            Some(crate::tools::ApprovalWaitReply::Closed) => (ApprovalReply::Unavailable, None),
            None => (ApprovalReply::Unavailable, pending.reject_late_replies()),
        }
    }
}

pub(super) async fn unguarded_gate(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
) -> Result<UnguardedPrompt, UnguardedRefusal> {
    let ToolCall::CommandUnguarded { args, .. } = &intent.call else {
        return Err(UnguardedRefusal::UnavailableProgram);
    };
    if crate::tools::unguarded::prohibited_intent(intent) {
        return Err(UnguardedRefusal::PrivilegeTool);
    }
    if !runtime.host_settings_admitted {
        return Err(UnguardedRefusal::SettingsChanged);
    }
    let pin = intent
        .host_command
        .as_ref()
        .ok_or(UnguardedRefusal::UnavailableProgram)?;
    if !runtime.host_settings_current(pin) {
        return Err(UnguardedRefusal::SettingsChanged);
    }
    if !runtime.host_interpreter_admitted(intent).await {
        return Err(UnguardedRefusal::InterpreterNotAdmitted);
    }
    if crate::tools::types::uses_guard_alias(args) {
        return Err(UnguardedRefusal::GuardAlias);
    }
    Ok(UnguardedPrompt {
        executable: pin.executable.clone(),
        attribution: None,
        remember: true,
        prefix: false,
        sensitive: crate::tools::unguarded::sensitive(&intent.call, pin),
        inactive_rules: runtime.has_inactive_host_rules(),
    })
}
