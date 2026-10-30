use super::*;
use crate::session::{BudgetRefusal, Event, GoalId, GoalReport, GoalStatus, SessionId};

pub(super) struct GoalClose {
    pub(super) status: GoalStatus,
    pub(super) active_ms: u64,
    pub(super) report: GoalReport,
    pub(super) exhausted: bool,
    pub(super) budget_refusal: Option<BudgetRefusal>,
}

impl Engine {
    pub(super) async fn finish_goal(
        &self,
        session_id: SessionId,
        goal_id: GoalId,
        close: GoalClose,
    ) -> Result<(), EngineError> {
        let GoalClose {
            status,
            active_ms,
            report,
            exhausted,
            budget_refusal,
        } = close;
        if exhausted && budget_refusal.is_none() {
            return Err(EngineError::InvalidHistory);
        }
        self.append_goal_lifecycle(
            session_id,
            None,
            Event::GoalFinished {
                goal_id,
                status,
                active_ms,
                report,
                exhausted,
                budget_refusal,
            },
        )
        .await
        .map_err(|error| match (error, budget_refusal) {
            (EngineError::GoalAdmissionUncertain, _) => EngineError::GoalAdmissionUncertain,
            (source, Some(refusal)) => EngineError::GoalBudgetUncommitted {
                refusal: Box::new(refusal),
                source: Box::new(source),
            },
            (source, None) => source,
        })?;
        Ok(())
    }
}

pub(super) fn goal_context_metadata_reservation(
    budget: &GoalBudget,
    plan_contract: Option<&GoalPlanContract>,
) -> Result<usize, EngineError> {
    let facts = crate::provider::GoalContextFacts::maximum();
    let serialized = serde_json::to_vec(&(budget, plan_contract, facts))
        .map_err(|_| EngineError::ContextTooLarge)?
        .len();
    serialized
        .checked_add(if plan_contract.is_none() {
            crate::session::MAX_GOAL_PLAN_CONTRACT_BYTES
        } else {
            0
        })
        .and_then(|bytes| bytes.checked_add(64 + crate::provider::MAX_HARNESS_FEEDBACK_BYTES))
        .ok_or(EngineError::ContextTooLarge)
}

pub(super) fn resolve_goal_admission(
    request: &RunRequest,
    history: Option<&crate::session::SessionView>,
    entry: GoalEntry,
) -> Result<GoalAdmission, EngineError> {
    if let Some(id) = request.goal.id {
        let goal = history
            .and_then(|view| view.goal(id))
            .ok_or(EngineError::GoalUnavailable)?;
        // Only the most recent unfinished Goal is resumable; older paused Goals stay history.
        if goal.status == GoalStatus::Interrupted
            && history.and_then(crate::session::SessionView::interrupted_goal_id) != Some(id)
        {
            return Err(EngineError::GoalUnavailable);
        }
        if !request.objective.trim().is_empty()
            && !goal.run_ids.iter().any(|run_id| {
                history.is_some_and(|view| view.runs.iter().any(|run| run.id == *run_id))
            })
            && goal.objective != request.objective
        {
            return Err(EngineError::GoalUnavailable);
        }
        if goal.status == GoalStatus::Interrupted
            && history.is_none_or(|view| !goal.can_resume(view))
            || goal.active_run.is_some()
        {
            return Err(EngineError::GoalUnavailable);
        }
        let entry = match goal.status {
            GoalStatus::Running => GoalEntry::Continue,
            GoalStatus::Interrupted if entry == GoalEntry::Continue => GoalEntry::Continue,
            GoalStatus::Interrupted => GoalEntry::Resume,
            GoalStatus::Completed | GoalStatus::Failed => {
                return Err(EngineError::GoalUnavailable);
            }
        };
        let view = history.ok_or(EngineError::GoalUnavailable)?;
        Ok(GoalAdmission {
            id,
            objective: goal.objective.clone(),
            include_paths: goal.include_paths.clone(),
            phase: goal.phase,
            budget: goal.budget,
            autonomous: goal.autonomous,
            resource_settings: goal.resource_settings,
            plan_contract: goal.plan_contract.clone(),
            entry,
            usage: goal.resume_usage(view),
            continues: goal.continues,
        })
    } else if let Some(source_id) = request.goal.continues {
        let source = history
            .and_then(crate::session::SessionView::continuable_goal)
            .filter(|source| source.id == source_id && request.objective.trim().is_empty())
            .ok_or(EngineError::GoalNotContinuable)?;
        let paths = source
            .include_paths
            .as_ref()
            .ok_or(EngineError::GoalIncludesUnavailable)?;
        if !request.include_paths.is_empty()
            && request.include_paths != paths.iter().map(PathBuf::from).collect::<Vec<_>>()
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(GoalAdmission {
            id: GoalId::new(),
            objective: source.objective.clone(),
            include_paths: Some(paths.clone()),
            phase: source.phase,
            budget: request.goal.budget.ok_or(EngineError::InvalidRequest)?,
            autonomous: source.autonomous,
            resource_settings: resources::admit(request.goal.resource_settings),
            plan_contract: None,
            entry: GoalEntry::New,
            usage: GoalUsage {
                runs: 0,
                provider_calls: 0,
                active_ms: 0,
                known_tokens: 0,
                token_usage_unknown: false,
            },
            continues: Some(source_id),
        })
    } else {
        // A paused Goal does not block a new one; the Session lock serializes live Runs.
        Ok(GoalAdmission {
            id: GoalId::new(),
            objective: request.objective.clone(),
            include_paths: Some(
                request
                    .include_paths
                    .iter()
                    .map(|path| {
                        path.to_str()
                            .map(str::to_owned)
                            .ok_or(EngineError::InvalidWorkspacePath)
                    })
                    .collect::<Result<_, _>>()?,
            ),
            phase: request.goal.phase.ok_or(EngineError::InvalidRequest)?,
            budget: request.goal.budget.ok_or(EngineError::InvalidRequest)?,
            autonomous: request.goal.autonomous,
            resource_settings: resources::admit(request.goal.resource_settings),
            plan_contract: None,
            entry: GoalEntry::New,
            usage: GoalUsage {
                runs: 0,
                provider_calls: 0,
                active_ms: 0,
                known_tokens: 0,
                token_usage_unknown: false,
            },
            continues: None,
        })
    }
}

impl Engine {
    pub(super) fn acquire_goal_workspace(
        &self,
        leases: &WorkspaceLeases,
        goal_id: GoalId,
        identity: WorkspaceIdentity,
    ) -> Result<Option<WorkspaceRunLock>, EngineError> {
        if let Some(held) = leases.get(&identity) {
            return if held.goal_id == goal_id {
                Ok(None)
            } else {
                Err(EngineError::WorkspaceBusy)
            };
        }
        let lock = self.state.lock_workspace(identity).map_err(|error| {
            if matches!(error, StoreError::WorkspaceBusy) {
                EngineError::WorkspaceBusy
            } else {
                EngineError::Store(error)
            }
        })?;
        Ok(Some(lock))
    }

    pub(super) fn retain_goal_workspace(
        leases: &mut WorkspaceLeases,
        goal_id: GoalId,
        identity: WorkspaceIdentity,
        lock: Option<WorkspaceRunLock>,
    ) {
        if let Some(lock) = lock {
            leases.insert(
                identity,
                GoalWorkspaceLock {
                    goal_id,
                    _lock: lock,
                },
            );
        }
    }

    fn release_goal_workspace(
        leases: &mut WorkspaceLeases,
        goal_id: GoalId,
        identity: WorkspaceIdentity,
    ) {
        if leases
            .get(&identity)
            .is_some_and(|held| held.goal_id == goal_id)
        {
            leases.remove(&identity);
        }
    }

    async fn append_goal_lifecycle(
        &self,
        session_id: SessionId,
        ledger: Option<&GoalLedger>,
        event: Event,
    ) -> Result<u64, EngineError> {
        let appended = match ledger {
            Some(ledger) => {
                ledger
                    .append(&self.journal, session_id, event.clone())
                    .await
            }
            None => self
                .journal
                .append(session_id, event.clone())
                .await
                .map(|committed| committed.sequence)
                .map_err(EngineError::from),
        };
        match appended {
            Ok(sequence) => Ok(sequence),
            // The Session lock is held, so a commit that lost its acknowledgment is the last Event.
            Err(error) => match self.journal.store().load_session(session_id).await {
                Ok(events)
                    if events
                        .last()
                        .is_some_and(|envelope| envelope.event == event) =>
                {
                    Err(EngineError::GoalAdmissionUncertain)
                }
                Ok(_) => Err(error),
                Err(_) => Err(EngineError::GoalAdmissionUncertain),
            },
        }
    }

    pub(super) async fn close_exhausted_goal(
        &self,
        session_id: SessionId,
        view: &crate::session::SessionView,
        goal: &GoalAdmission,
    ) -> Result<(), EngineError> {
        let goal_view = view.goal(goal.id).ok_or(EngineError::InvalidHistory)?;
        let run = goal_view
            .run_ids
            .iter()
            .rev()
            .find_map(|run_id| view.runs.iter().find(|run| run.id == *run_id))
            .ok_or(EngineError::InvalidHistory)?;
        let mut goal_view = goal_view.clone();
        goal_view.uncertain_active_ms = goal_view
            .uncertain_active_ms
            .saturating_add(goal.usage.active_ms.saturating_sub(goal_view.active_ms));
        goal_view.active_ms = goal.usage.active_ms;
        goal_view.token_usage_unknown = goal.usage.token_usage_unknown;
        goal_view
            .run_ids
            .retain(|id| view.runs.iter().any(|run| run.id == *id));
        let mut refusal = goal_view
            .effective_budget()
            .refusal(goal.usage, crate::session::BudgetAction::Resume)
            .ok_or(EngineError::InvalidHistory)?;
        goal_view.qualify_charge(&mut refusal);
        goal_view.budget_refusal = Some(refusal);
        self.finish_goal(
            session_id,
            goal.id,
            GoalClose {
                status: GoalStatus::Failed,
                active_ms: goal.usage.active_ms,
                report: goal_report(view, run, &goal_view),
                exhausted: true,
                budget_refusal: Some(refusal),
            },
        )
        .await
    }

    pub(super) async fn begin_goal(
        &self,
        started: &StartedRun<'_>,
        ledger: &GoalLedger,
        objective: &str,
        cancellation: &RunCancellation,
    ) -> Result<(), EngineError> {
        let StartedRun {
            session_id,
            run_id,
            goal,
            workspace_identity,
            ..
        } = *started;
        match goal.entry {
            GoalEntry::New => {
                self.append_goal_lifecycle(
                    session_id,
                    Some(ledger),
                    goal.started_event(objective, workspace_identity),
                )
                .await?;
            }
            GoalEntry::Resume => {
                self.append_goal_lifecycle(
                    session_id,
                    Some(ledger),
                    Event::GoalResumed { goal_id: goal.id },
                )
                .await?;
            }
            GoalEntry::Continue => {}
        }
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        // A link is an admission acknowledgment: a lost acknowledgment may still have committed it.
        self.append_goal_lifecycle(
            session_id,
            Some(ledger),
            Event::GoalRunLinked {
                goal_id: goal.id,
                run_id,
                phase: goal.phase,
            },
        )
        .await?;
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        Ok(())
    }

    pub(super) async fn settle_turn(
        &self,
        started: StartedRun<'_>,
        ledger: &GoalLedger,
        leases: &mut WorkspaceLeases,
    ) -> Result<RunTurn, EngineError> {
        let StartedRun {
            session_id,
            run_id,
            goal,
            finish_after_run,
            workspace_identity,
        } = started;
        let budget_stopped = ledger.budget_stopped();
        let captured_refusal = ledger.budget_refusal()?;
        let (run, terminal) = ledger.view(|view| -> Result<_, EngineError> {
            let run = view
                .runs
                .last()
                .filter(|run| run.id == run_id)
                .ok_or(EngineError::InvalidHistory)?
                .clone();
            let goal_view = view.goal(goal.id).ok_or(EngineError::InvalidHistory)?;
            // Only Run failure or a satisfied/exhausted Goal settles it; a cancelled Run leaves its Goal open to resume.
            let terminal = match run.status {
                crate::session::RunStatus::Finished if goal.autonomous => {
                    if goal_view.plan_contract.is_none() || goal_view.success_criterion_satisfied {
                        Some((GoalStatus::Completed, false))
                    } else if !goal_view.admits_next_run() {
                        // Decide the exhaustion before this segment closes, so the next admission cannot find it open.
                        Some((GoalStatus::Failed, true))
                    } else {
                        None
                    }
                }
                crate::session::RunStatus::Finished => {
                    finish_after_run.then_some((GoalStatus::Completed, false))
                }
                crate::session::RunStatus::Failed => Some((
                    GoalStatus::Failed,
                    budget_stopped && goal_view.budget_spent(),
                )),
                _ => None,
            }
            .map(|(status, exhausted)| {
                let mut goal_view = goal_view.clone();
                let mut refusal = if exhausted {
                    captured_refusal.or_else(|| {
                        goal_view.effective_budget().refusal(
                            goal_view.resume_usage(view),
                            crate::session::BudgetAction::Run,
                        )
                    })
                } else {
                    None
                };
                if let Some(refusal) = &mut refusal {
                    goal_view.qualify_charge(refusal);
                }
                goal_view.budget_refusal = refusal;
                GoalClose {
                    status,
                    active_ms: goal_view.active_ms,
                    report: goal_report(view, &run, &goal_view),
                    exhausted,
                    budget_refusal: goal_view.budget_refusal,
                }
            });
            Ok((run, terminal))
        })??;
        let mut goal_status = GoalStatus::Running;
        if let Some(close) = terminal {
            let status = close.status;
            self.finish_goal(session_id, goal.id, close).await?;
            Self::release_goal_workspace(leases, goal.id, workspace_identity);
            goal_status = status;
        }
        // Only an unsettled primary leaves its Run open here; the Goal stays open as after a crash.
        if !run.status.is_outcome() && run.status != crate::session::RunStatus::Active {
            return Err(EngineError::InvalidHistory);
        }
        Ok(RunTurn {
            outcome: RunOutcome {
                session_id,
                goal_id: goal.id,
                run,
            },
            autonomous: goal.autonomous,
            goal_status,
        })
    }
}

#[cfg(test)]
mod goal_workspace_lease_tests {
    use super::*;
    use crate::provider::{ProviderError, ProviderResponse};
    use std::time::Duration;

    struct NeverProvider;

    #[async_trait::async_trait]
    impl Provider for NeverProvider {
        fn identity(&self) -> &crate::provider::ProviderIdentity {
            static IDENTITY: std::sync::LazyLock<crate::provider::ProviderIdentity> =
                std::sync::LazyLock::new(|| {
                    crate::provider::ProviderIdentity::new(
                        "openai",
                        "lease-test",
                        crate::provider::Provenance::native(),
                    )
                });
            &IDENTITY
        }

        async fn invoke(&self, _: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            panic!("a dropped Goal admission must not call the Provider")
        }
    }

    fn submit(engine: &Engine, request: RunRequest) -> RunFuture<'_> {
        engine
            .submit(
                crate::provider::AdmittedProviders::single(Arc::new(NeverProvider)),
                request,
                RunControls::default(),
            )
            .expect("idle Engine")
            .result
    }

    #[tokio::test]
    async fn dropped_start_acknowledgment_releases_the_lease_once_the_goal_is_paused() {
        let temp = tempfile::tempdir().expect("temporary root");
        let state_path = temp.path().join("state-a");
        let second_state_path = temp.path().join("state-b");
        let account_root = temp.path().join("account");
        let state = StateRoot::admit(&state_path).expect("private State root");
        let second_state = StateRoot::admit(&second_state_path).expect("second private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let workspace_identity = WorkspaceIdentity {
            device: 7,
            inode: 11,
        };
        engine
            .journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Goal lease acknowledgment".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");

        let workspace_lock = engine
            .state
            .lock_workspace_in(&account_root, workspace_identity)
            .expect("provisional Workspace lock");
        let (acknowledgment_entered, release_acknowledgment) =
            crate::fault::FaultPlan::gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalStarted { .. } | Event::GoalResumed { .. })
            });
        let mut leases = WorkspaceLeases::new();
        Engine::retain_goal_workspace(
            &mut leases,
            goal_id,
            workspace_identity,
            Some(workspace_lock),
        );
        let mut admission = Box::pin(engine.append_goal_lifecycle(
            session_id,
            None,
            Event::GoalStarted {
                include_paths: Some(Vec::new()),
                goal_id,
                objective: "Keep the Workspace lease through Store acknowledgment".into(),
                workspace_identity,
                budget: GoalBudget::default(),
                phase: GoalPhase::Implementation,
                autonomous: false,
                resource_settings: crate::tools::ToolResourceSettings::default(),
                continues: None,
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut admission => panic!("append returned before its controlled acknowledgment: {result:?}"),
                result = acknowledgment_entered => result.expect("committed append reached acknowledgment gate"),
            }
        })
        .await
        .expect("Goal append acknowledgment gate");
        drop(admission);
        release_acknowledgment
            .send(())
            .expect("release Store acknowledgment");

        assert!(matches!(
            second_state.lock_workspace_in(&account_root, workspace_identity),
            Err(StoreError::WorkspaceBusy)
        ));
        // The call ends with its dropped future; the committed Goal is paused and no longer holds the writer lock.
        drop(leases);
        let view = engine
            .journal
            .store()
            .load_view(session_id)
            .await
            .expect("canonical Session view")
            .expect("Session exists");
        assert_eq!(view.interrupted_goal_id(), Some(goal_id));
        let writer = second_state
            .lock_workspace_in(&account_root, workspace_identity)
            .expect("paused Goal releases the Workspace writer lock");
        drop(writer);
        let events = engine
            .journal
            .store()
            .load_session(session_id)
            .await
            .expect("canonical Events");
        assert_eq!(events.len(), 2, "no Run or effect was admitted");
        engine.close().await.expect("close Engine");
    }

    #[tokio::test]
    async fn lost_run_link_acknowledgment_is_admission_uncertain() {
        let temp = tempfile::tempdir().expect("temporary root");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&state_path).expect("private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let session_id = SessionId::new();
        let workspace_identity = crate::tools::workspace_identity(&workspace).expect("identity");
        engine
            .journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Lost link acknowledgment".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");
        let (link_entered, release_link) =
            crate::fault::FaultPlan::gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalRunLinked { .. })
            });
        let request = RunRequest {
            skill: None,
            session_id: Some(session_id),
            title: None,
            objective: "Link the Run before its acknowledgment is lost".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            launch_directory: None,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: GoalRequest::default(),
        };
        let mut run = submit(&engine, request);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut run => panic!("Run returned before its link was held: {}", result.is_ok()),
                result = link_entered => result.expect("link reached the acknowledgment gate"),
            }
        })
        .await
        .expect("link acknowledgment gate");
        drop(release_link);
        match run.await {
            Err(EngineError::GoalAdmissionUncertain) => {}
            Err(error) => panic!("a possibly committed link must be uncertain, not {error}"),
            Ok(_) => panic!("Run completed without an acknowledged link"),
        }
        engine.close().await.expect("close Engine");
    }

    const RESUMABLE_OBJECTIVE: &str = "Resume the paused Goal after a failed append";

    async fn paused_goal_session(
        engine: &Engine,
        workspace: &std::path::Path,
    ) -> (SessionId, GoalId) {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let workspace_identity = crate::tools::workspace_identity(workspace).expect("identity");
        engine
            .journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Resumable Goal".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");
        engine
            .journal
            .append(
                session_id,
                Event::GoalStarted {
                    include_paths: Some(Vec::new()),
                    goal_id,
                    objective: RESUMABLE_OBJECTIVE.into(),
                    workspace_identity,
                    budget: GoalBudget::default(),
                    phase: GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: crate::tools::ToolResourceSettings {
                        build_jobs: Some(2),
                        build_cpu_cores: Some(8),
                        ..Default::default()
                    },
                    continues: None,
                },
            )
            .await
            .expect("Goal start");
        (session_id, goal_id)
    }

    fn resume_request(
        session_id: SessionId,
        goal_id: GoalId,
        workspace: &std::path::Path,
    ) -> RunRequest {
        RunRequest {
            skill: None,
            session_id: Some(session_id),
            title: None,
            objective: RESUMABLE_OBJECTIVE.into(),
            images: Vec::new(),
            workspace: workspace.to_path_buf(),
            launch_directory: None,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: GoalRequest {
                id: Some(goal_id),
                phase: None,
                budget: None,
                resource_settings: crate::tools::ToolResourceSettings::default(),
                context_window: crate::session::ContextWindow::default(),
                finish_after_run: true,
                autonomous: false,
                continues: None,
            },
        }
    }

    #[tokio::test]
    async fn goal_resume_failure_before_commit_is_store_error() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let (session_id, goal_id) = paused_goal_session(&engine, &workspace).await;
        crate::fault::FaultPlan::fail_goal_append(session_id, |event| {
            matches!(event, Event::GoalResumed { .. })
        });
        match submit(&engine, resume_request(session_id, goal_id, &workspace)).await {
            Err(EngineError::Store(_)) => {}
            Err(error) => panic!("an uncommitted resume must report its Store error, not {error}"),
            Ok(_) => panic!("Run resumed without a committed GoalResumed"),
        }
        let events = engine
            .journal
            .store()
            .load_session(session_id)
            .await
            .expect("canonical Events");
        assert_eq!(events.len(), 2, "GoalResumed did not commit");
        let history = crate::SessionView::replay(session_id, &events)
            .unwrap()
            .unwrap();
        let mut request = resume_request(session_id, goal_id, &workspace);
        request.goal.resource_settings.build_jobs = Some(u32::MAX);
        request.goal.resource_settings.build_cpu_cores = Some(u32::MAX);
        let resumed = resolve_goal_admission(&request, Some(&history), GoalEntry::Resume).unwrap();
        assert_eq!(resumed.resource_settings.build_jobs, Some(2));
        assert_eq!(resumed.resource_settings.build_cpu_cores, Some(8));
        engine.close().await.expect("close Engine");
    }

    #[tokio::test]
    async fn lost_goal_resume_acknowledgment_is_admission_uncertain() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let (session_id, goal_id) = paused_goal_session(&engine, &workspace).await;
        let (resume_entered, release_resume) =
            crate::fault::FaultPlan::gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalResumed { .. })
            });
        let mut run = submit(&engine, resume_request(session_id, goal_id, &workspace));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut run => panic!("Run returned before its resume was held: {}", result.is_ok()),
                result = resume_entered => result.expect("resume reached the acknowledgment gate"),
            }
        })
        .await
        .expect("resume acknowledgment gate");
        drop(release_resume);
        match run.await {
            Err(EngineError::GoalAdmissionUncertain) => {}
            Err(error) => panic!("a possibly committed resume must be uncertain, not {error}"),
            Ok(_) => panic!("Run completed without an acknowledged resume"),
        }
        engine.close().await.expect("close Engine");
    }

    #[tokio::test]
    async fn goal_run_link_failure_before_commit_is_store_error() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let (session_id, goal_id) = paused_goal_session(&engine, &workspace).await;
        crate::fault::FaultPlan::fail_goal_append(session_id, |event| {
            matches!(event, Event::GoalRunLinked { .. })
        });
        match submit(&engine, resume_request(session_id, goal_id, &workspace)).await {
            Err(EngineError::Store(_)) => {}
            Err(error) => panic!("an uncommitted link must report its Store error, not {error}"),
            Ok(_) => panic!("Run completed without a committed link"),
        }
        engine.close().await.expect("close Engine");
    }
}
