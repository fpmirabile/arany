use super::dispatch::UnguardedRefusal;
use super::permission::RuleSave;
use super::{Grant, RunLoop};
use crate::engine::EngineError;
use crate::engine::ledger::{Action, Admission};
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
    },
}

pub(super) struct AskOptions {
    pub(super) unguarded_prompt: Option<UnguardedPrompt>,
    pub(super) allow_auto_edits: bool,
    pub(super) remember_prefix: Option<TypedGitPrefix>,
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
                    },
                )
                .await
            }
        }
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
        prompt: Option<UnguardedPrompt>,
        allow_auto_edits: bool,
        remember_prefix: Option<TypedGitPrefix>,
    ) -> ApprovalReply {
        let mut cancellation = self.controls.cancellation.clone();
        let reply = tokio::select! {
            biased;
            () = cancellation.cancelled() => ApprovalReply::Unavailable,
            () = self.approval_paused() => ApprovalReply::Unavailable,
            reply = async {
                match prompt.clone() {
                    Some(prompt) => approvals.ask_unguarded(intent, prompt).await,
                    None => approvals.ask_local(intent, allow_auto_edits, remember_prefix).await,
                }
            } => reply,
        };
        let remember = prompt.as_ref().is_some_and(|prompt| prompt.remember);
        match reply {
            ApprovalReply::AllowForSession if !remember => ApprovalReply::Unavailable,
            ApprovalReply::AllowPrefixInFolder
                if remember_prefix.is_none()
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
