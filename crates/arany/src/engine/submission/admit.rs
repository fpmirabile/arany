use super::*;
use crate::engine::goal::resolve_goal_admission;

impl Engine {
    pub(super) async fn admit_turn(
        &self,
        submission: &AdmittedSubmission,
        request: &RunRequest,
        identity: &crate::provider::ProviderIdentity,
        entry: GoalEntry,
        previews: bool,
        leases: &mut SubmissionLeases,
    ) -> Result<AdmittedTurn, EngineError> {
        self.validate_request(request)?;
        if identity.custom_provenance().is_some() && !request.images.is_empty() {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let workspace_identity = crate::tools::workspace_identity(&request.workspace)?;
        let session_id = request.session_id.unwrap_or_default();
        let session_lock = self.session_lease(leases, session_id)?;
        let history = self.load_history(request, session_id).await?;
        if history.is_some() {
            session_lock.keep();
        }
        let _lineage_locks = match &history {
            Some(view) => self.lock_lineage(view).await?,
            None => Vec::new(),
        };
        if let Some(view) = &history {
            if view.provider_setup_blocks_continuation() {
                return Err(EngineError::ProviderSetupBlocked);
            }
            ensure_session_workspace(view, workspace_identity)?;
        }
        if entry == GoalEntry::Resume
            && let Some(view) = &history
            && let Some(goal) = request.goal.id
        {
            report_stale_git_locks(view, goal, submission.tools.is_some(), &request.workspace)
                .await?;
        }
        let goal = resolve_goal_admission(request, history.as_ref(), entry)?;
        if entry == GoalEntry::Resume {
            ensure_resumed_roster(history.as_ref(), goal.id, submission)?;
        }
        if request.goal.id.is_some() && goal.include_paths.is_none() {
            return Err(EngineError::GoalIncludesUnavailable);
        }
        let objective = turn_objective(request, &goal);
        let budget = goal.budget.effective(goal.plan_contract.as_ref());
        if !budget.admits_next_run(goal.usage) {
            if goal.entry == GoalEntry::Resume {
                let view = history.as_ref().ok_or(EngineError::InvalidHistory)?;
                self.close_exhausted_goal(session_id, view, &goal).await?;
            }
            return Err(EngineError::GoalBudgetExhausted);
        }
        let event_slots = run_start_slots(submission, request, previews);
        self.journal
            .admit_as(session_id, event_slots, 0, AdmissionKind::RunStart)
            .await?;
        let provisional_workspace_lock = if goal.phase == crate::session::GoalPhase::Implementation
            && grant_writes(submission)
        {
            self.acquire_goal_workspace(&leases.workspaces, goal.id, workspace_identity)?
        } else {
            None
        };
        Ok(AdmittedTurn {
            session_id,
            workspace_identity,
            _lineage_locks,
            history,
            goal,
            objective,
            budget,
            provisional_workspace_lock,
        })
    }

    fn session_lease<'a>(
        &self,
        leases: &'a mut SubmissionLeases,
        session_id: SessionId,
    ) -> Result<&'a mut SessionRunLock, EngineError> {
        if leases.session.is_none() {
            leases.session = Some((session_id, self.lock_session(session_id)?));
        }
        let (locked_session, session_lock) =
            leases.session.as_mut().expect("admitted Session lease");
        if *locked_session != session_id {
            return Err(EngineError::InvalidRequest);
        }
        Ok(session_lock)
    }

    async fn load_history(
        &self,
        request: &RunRequest,
        session_id: SessionId,
    ) -> Result<Option<crate::session::SessionView>, EngineError> {
        Ok(if request.session_id.is_some() {
            Some(
                self.journal
                    .store()
                    .load_view(session_id)
                    .await?
                    .ok_or(EngineError::MissingSession)?,
            )
        } else {
            None
        })
    }

    fn validate_request(&self, request: &RunRequest) -> Result<(), EngineError> {
        if (request.objective.trim().is_empty()
            && request.images.is_empty()
            && request.goal.id.is_none()
            && request.goal.continues.is_none())
            || request.objective.len() > MAX_USER_MESSAGE_BYTES
            || !crate::provider::valid_image_collection(&request.images)
            || (request.session_id.is_some() && request.title.is_some())
            || request
                .title
                .as_ref()
                .is_some_and(|title| !crate::session::valid_session_title(title))
            || request.policy.validate().is_err()
            || !request.goal.resource_settings.valid()
            || !request.goal.context_window.valid()
            || match (&request.goal.id, &request.goal.budget) {
                (Some(_), Some(_)) | (None, None) => true,
                (Some(_), None) => false,
                (None, Some(budget)) => budget.validate().is_err(),
            }
            || (request.goal.id.is_some() && request.session_id.is_none())
            || (request.goal.autonomous && request.goal.id.is_some())
            || (request.goal.continues.is_some()
                && (request.goal.id.is_some()
                    || request.session_id.is_none()
                    || !request.images.is_empty()))
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }
}

// Locks an interrupted Git write may have left are reported, with their paths, before admission
// refuses to resume past its unsettled effect; nothing is moved or retried.
async fn report_stale_git_locks(
    view: &crate::SessionView,
    goal: crate::session::GoalId,
    tools: bool,
    workspace: &std::path::Path,
) -> Result<(), EngineError> {
    let Some(goal) = view.goal(goal) else {
        return Ok(());
    };
    let unsettled = view
        .runs
        .iter()
        .filter(|r| goal.run_ids.contains(&r.id))
        .flat_map(|r| &r.tools)
        .any(|t| t.intent.call.is_git() && t.intent.call.mutates() && !t.settled_without_stop());
    if !unsettled {
        return Ok(());
    }
    if !tools {
        return Err(EngineError::GoalUnavailable);
    }
    let workspace = workspace.to_owned();
    let locks = tokio::task::spawn_blocking(move || crate::tools::stale_locks(&workspace))
        .await
        .map_err(|_| crate::tools::ToolError::Operation)?;
    if locks.is_empty() {
        return Ok(());
    }
    Err(crate::tools::ToolError::StaleGitLocks(locks.join(", ")).into())
}

pub(super) fn validate_turn_providers(
    submission: &AdmittedSubmission,
    request: &RunRequest,
) -> Result<(), EngineError> {
    let providers = &submission.providers;
    if !request.images.is_empty()
        && providers.agent_chains().iter().any(|chain| {
            chain
                .iter()
                .any(|candidate| candidate.identity().custom_provenance().is_some())
        })
    {
        return Err(EngineError::ImagesNotAdmitted);
    }
    if submission.tools.is_some()
        && providers.agent_chains().iter().any(|chain| {
            chain
                .iter()
                .any(|candidate| !candidate.identity().shares_account(chain[0].identity()))
        })
    {
        return Err(EngineError::IncompatibleProviderChain);
    }
    Ok(())
}

// A resumed Goal keeps the roster selection its last Run was pinned to.
fn ensure_resumed_roster(
    history: Option<&crate::session::SessionView>,
    goal_id: GoalId,
    submission: &AdmittedSubmission,
) -> Result<(), EngineError> {
    let previous = history
        .and_then(|view| {
            let stored = view.goal(goal_id)?;
            view.runs
                .iter()
                .rev()
                .filter(|run| stored.run_ids.contains(&run.id))
                .find_map(|run| run.config.as_ref())
        })
        .and_then(|config| config.roster.as_ref());
    let selected = submission
        .providers
        .roster
        .as_ref()
        .map(|roster| &roster.snapshot);
    if previous.is_some_and(|roster| {
        Some(roster.selection_digest) != selected.map(|selected| selected.selection_digest)
    }) {
        return Err(EngineError::RosterSelectionMismatch);
    }
    Ok(())
}

// Run start reserves the GoalStarted objective envelope, the MessageAccepted objective and image envelopes, the Goal lifecycle and the first action; each later action admits its own.
fn run_start_slots(submission: &AdmittedSubmission, request: &RunRequest, previews: bool) -> usize {
    let first_action = Action::ProviderCall {
        previews,
        steering: submission.steering.is_some(),
    };
    RUN_START_EVENTS
        + first_action.max_events_for_admitted(&submission.providers)
        + if submission.tools.is_some() {
            first_action.verdict_slots()
        } else {
            0
        }
        + usize::from(request.session_id.is_none())
}

// A resumed or continued Goal without a new objective keeps its own.
fn turn_objective(request: &RunRequest, goal: &GoalAdmission) -> String {
    if request.objective.trim().is_empty()
        && (request.goal.id.is_some() || request.goal.continues.is_some())
    {
        goal.objective.clone()
    } else {
        request.objective.clone()
    }
}

fn grant_writes(submission: &AdmittedSubmission) -> bool {
    match &submission.grant {
        ToolGrant::None => false,
        ToolGrant::Configured => submission
            .tools
            .as_ref()
            .is_some_and(|tools| tools.config.write),
        ToolGrant::Trusted(_) => true,
    }
}

#[cfg(test)]
mod stale_git_lock_scope_tests {
    use super::*;

    #[tokio::test]
    async fn only_the_resumed_goals_unsettled_git_writes_are_considered() {
        let fixture = crate::session::worker_protocol_fixture();
        let mut view = crate::SessionView::replay(fixture.session, &fixture.envelopes())
            .unwrap()
            .unwrap();
        let goal = view.goals[0].clone();
        let mut write = view
            .runs
            .iter()
            .flat_map(|run| &run.tools)
            .next()
            .expect("a fixture Tool")
            .clone();
        write.intent.call = crate::tools::ToolCall::GitSwitch {
            branch: "topic".into(),
        };
        write.observation = None;
        let mut other = view.runs[0].clone();
        other.id = crate::session::RunId::new();
        other.tools = vec![write.clone()];
        view.runs.push(other);
        let workspace = tempfile::tempdir().unwrap();
        // Without Tools an in-scope unsettled write refuses; another Goal's write must not.
        assert!(
            report_stale_git_locks(&view, goal.id, false, workspace.path())
                .await
                .is_ok()
        );
        let own = view
            .runs
            .iter()
            .position(|run| goal.run_ids.contains(&run.id))
            .unwrap();
        view.runs[own].tools.push(write);
        assert!(matches!(
            report_stale_git_locks(&view, goal.id, false, workspace.path()).await,
            Err(EngineError::GoalUnavailable)
        ));
    }
}
