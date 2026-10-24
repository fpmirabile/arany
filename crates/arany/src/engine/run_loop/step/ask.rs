use super::permission::RuleSave;
use super::{Grant, RunLoop};
use crate::engine::EngineError;
use crate::engine::ledger::{Action, Admission};
use crate::tools::permissions::{PermissionSource, TypedGitPrefix};
use crate::tools::{ApprovalMode, ApprovalWithdrawal, EffectIntent, UnguardedPrompt};

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
}
