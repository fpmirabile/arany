mod admit;
mod prepare;

use super::*;
use admit::validate_turn_providers;
use prepare::root_request;

impl Engine {
    /// Admits one submission and returns its handle. It is refused with `Busy` while another
    /// operation holds this Engine, when a Provider chain is invalid, and when its Tool grant
    /// cannot be loaded. The per-role Provider chains are this submission's: the next one may bring
    /// others.
    pub fn submit(
        &self,
        providers: crate::provider::AdmittedProviders,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<RunHandle<'_>, EngineError> {
        let slot = self.claim()?;
        admit_providers(&providers)?;
        let guard = || {
            self.config
                .guard_executable
                .clone()
                .ok_or(EngineError::GuardUnavailable)
        };
        if request.goal.id.is_some() {
            self.clear_host_epoch();
        }
        let tools = match &controls.tools {
            ToolGrant::None => None,
            ToolGrant::Configured => {
                let guard = guard()?;
                let snapshot = crate::tools::load_permissions(&self.state)?;
                let unguarded_rules =
                    crate::tools::ToolRuntime::load_exact_command_rules(&self.state)?;
                Some(ToolAdmission {
                    config: crate::tools::config::Config::load(&self.state)?,
                    permissions: snapshot,
                    unguarded_rules,
                    guard_executable: guard,
                })
            }
            ToolGrant::Trusted(permissions) => {
                let guard = guard()?;
                let snapshot = crate::tools::load_permissions(&self.state)?;
                let unguarded_rules =
                    crate::tools::ToolRuntime::load_exact_command_rules(&self.state)?;
                Some(ToolAdmission {
                    config: permissions.config(&request.workspace)?,
                    permissions: snapshot,
                    unguarded_rules,
                    guard_executable: guard,
                })
            }
        };
        let RunControls {
            tools: grant,
            approvals,
            steering,
            cancellation,
            hints,
        } = controls;
        let submission = AdmittedSubmission {
            providers,
            grant,
            tools,
            approvals: approvals.clone(),
            steering: steering.clone(),
        };
        let events = self.journal.subscribe();
        let run_cancellation = cancellation.clone();
        let result = Box::pin(async move {
            let _slot = slot;
            let result = self
                .run_inner(&submission, request, run_cancellation, hints)
                .await;
            if let Err(error) = &result {
                crate::diagnostics::engine_failure(error);
            }
            result
        });
        Ok(RunHandle {
            cancel: cancellation,
            steer: steering,
            approvals,
            events,
            result,
        })
    }

    async fn run_inner(
        &self,
        submission: &AdmittedSubmission,
        request: RunRequest,
        cancellation: RunCancellation,
        hints: Option<RunHints>,
    ) -> Result<RunResult, EngineError> {
        let base = continuation_request(&request, None, GoalRequest::default());
        let mut leases = SubmissionLeases::default();
        let entry = if request.goal.id.is_some() {
            GoalEntry::Resume
        } else {
            GoalEntry::New
        };
        // Boxed: keeps the large Run future off the caller stack in debug builds.
        let mut turn = Box::pin(self.run_turn(
            submission,
            request,
            cancellation.clone(),
            hints.clone(),
            entry,
            &mut leases,
        ))
        .await?;
        let mut continuation_error = None;
        while turn.continues() && !cancellation.is_cancelled() {
            crate::fault::autonomous_continuation(turn.outcome.session_id).await;
            let next = continuation_request(
                &base,
                Some(turn.outcome.session_id),
                GoalRequest::resume(turn.outcome.goal_id, false),
            );
            // Settlement left the Goal open under the same admits_next_run check, so this cannot be refused for budget.
            turn = match Box::pin(self.run_turn(
                submission,
                next,
                cancellation.clone(),
                hints.clone(),
                GoalEntry::Continue,
                &mut leases,
            ))
            .await
            {
                Ok(next) if next.outcome.run.status.is_outcome() => next,
                // The earlier Finished Run keeps the answer; the projection below names the unsettled Run's stop.
                Ok(_) => break,
                // Cancellation before MessageAccepted admits no Run; the committed Finished Run keeps its outcome.
                Err(EngineError::CancelledBeforeStart) => break,
                // A refusal before MessageAccepted keeps the committed Finished Run with the failure;
                // after acceptance the error propagates, as for the first Run.
                Err(error) => {
                    if self.run_stop(&turn.outcome).await?.0.id != turn.outcome.run.id {
                        return Err(error);
                    }
                    crate::diagnostics::engine_failure(&error);
                    continuation_error = Some(error);
                    break;
                }
            };
        }
        let (latest, stopped) = self.run_stop(&turn.outcome).await?;
        let mut run = turn.outcome;
        // An unsettled Run is returned as history reopens it.
        if latest.id == run.run.id {
            run.run = latest;
        }
        Ok(RunResult {
            run,
            stopped,
            continuation_error,
        })
    }

    // Projected from the reopened history, so a live result and its replayed turn agree.
    async fn run_stop(
        &self,
        outcome: &RunOutcome,
    ) -> Result<(RunView, Option<crate::session::RunStop>), EngineError> {
        let view = self
            .journal
            .store()
            .load_view(outcome.session_id)
            .await?
            .ok_or(EngineError::InvalidHistory)?;
        let goal = view
            .goal(outcome.goal_id)
            .ok_or(EngineError::InvalidHistory)?;
        let latest = view
            .runs
            .iter()
            .rev()
            .find(|run| goal.run_ids.contains(&run.id))
            .ok_or(EngineError::InvalidHistory)?;
        Ok((latest.clone(), view.run_stop(latest)))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered Run lifecycle together"
    )]
    async fn run_turn(
        &self,
        submission: &AdmittedSubmission,
        request: RunRequest,
        cancellation: RunCancellation,
        hints: Option<RunHints>,
        entry: GoalEntry,
        leases: &mut SubmissionLeases,
    ) -> Result<RunTurn, EngineError> {
        submission.cancel_if_paused(&cancellation);
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }

        let identity = checked_identity(submission.providers.primary[0].as_ref())?;
        validate_turn_providers(submission, &request)?;
        let admitted = self
            .admit_turn(
                submission,
                &request,
                identity,
                entry,
                hints.as_ref().is_some_and(RunHints::previews_enabled),
                leases,
            )
            .await?;
        let prepared = self
            .prepare_turn(submission, &request, identity, &admitted)
            .await?;
        let previous_run_failure = admitted
            .history
            .as_ref()
            .and_then(|view| view.previous_run_failure());
        let AdmittedTurn {
            session_id,
            workspace_identity,
            _lineage_locks,
            history: _,
            goal,
            objective,
            budget,
            provisional_workspace_lock,
        } = admitted;
        let PreparedTurn {
            mut tools,
            tool_context,
            mut inputs,
            images,
            history,
            summary,
            prompts,
            config,
            context_window_tokens,
            context_started,
            context_ended,
            provider_concurrency,
        } = prepared;
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }

        if request.session_id.is_none() {
            self.start_session(
                &mut leases.session.as_mut().expect("admitted Session lease").1,
                session_id,
                request.title,
                workspace_identity,
            )
            .await?;
        }
        let run_id = RunId::new();
        if let Some(runtime) = &mut tools {
            let events = self.journal.store().load_session(session_id).await?;
            runtime.admit_spills(&self.state, run_id, &events);
        }
        let agent_run_id = AgentRunId::new();
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        let trace =
            self.config
                .telemetry
                .trace_run(session_id, goal.id, (context_started, context_ended));
        // Reduces the open state that appends validate; the closed view would reject this Run's links.
        let ledger = self.open_ledger(session_id, goal.id).await?;
        let publisher = hints.map(|hints| HintPublisher::new(hints, run_id));
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        // Subscribers follow the turn's Events and drain them when it ends; they never hold it back.
        let trace_tap = trace.as_ref().map(|_| self.journal.subscribe());
        let (end, _) = tokio::sync::watch::channel(());
        let ended = |mut ended: tokio::sync::watch::Receiver<()>| async move {
            let _ = ended.changed().await;
        };
        let trace = trace_follower(trace, trace_tap, ended(end.subscribe()), &ledger, goal.id);
        let started = StartedRun {
            session_id,
            run_id,
            goal: &goal,
            finish_after_run: request.goal.finish_after_run,
            workspace_identity,
        };
        let segment = end_turn(
            async {
                Self::retain_goal_workspace(
                    &mut leases.workspaces,
                    goal.id,
                    workspace_identity,
                    provisional_workspace_lock,
                );
                self.begin_goal(&started, &ledger, &objective, &cancellation)
                    .await?;
                self.open_run(
                    &ledger,
                    session_id,
                    RunOpening {
                        skill: request.skill,
                        run_id,
                        agent_run_id,
                        objective: objective.clone(),
                        images: request.images,
                        config,
                    },
                )
                .await?;
                let mut provider_request = root_request(
                    CallScope {
                        session_id,
                        run_id,
                        agent_run_id,
                    },
                    request.policy,
                    identity,
                    &goal,
                    RootInput {
                        objective,
                        budget,
                        assignment_profiles: submission
                            .providers
                            .roster
                            .as_ref()
                            .map_or_else(Vec::new, |roster| roster.snapshot.child_profiles.clone()),
                        context_window_tokens,
                        tool_context,
                        inputs: &mut inputs,
                        images,
                        history,
                        summary,
                        prompts,
                    },
                );
                provider_request.input.previous_run_failure = previous_run_failure;
                if let Some(steering) = &submission.steering {
                    steering.begin_run(session_id, run_id);
                }
                self.drive_turn(
                    submission,
                    goal.id,
                    LoopControls {
                        host_settings: &self.host_permissions,
                        policy: request.policy,
                        concurrency: provider_concurrency,
                        goal_phase: goal.phase,
                        cancellation,
                        tools: tools.as_ref(),
                        approvals: submission.approvals.as_ref(),
                        steering: submission.steering.clone(),
                    },
                    provider_request,
                    &ledger,
                    publisher.as_ref(),
                )
                .await?;
                self.settle_turn(started, &ledger, &mut leases.workspaces)
                    .await
            },
            end,
        );
        tokio::join!(segment, trace).0
    }

    async fn drive_turn(
        &self,
        submission: &AdmittedSubmission,
        goal_id: GoalId,
        controls: LoopControls<'_>,
        request: ProviderRequest,
        ledger: &GoalLedger,
        publisher: Option<&HintPublisher>,
    ) -> Result<(), EngineError> {
        // Boxed: the Run loop's future is large and would overflow a small caller stack.
        Box::pin(
            RunLoop::new(
                &self.journal,
                submission.providers.clone(),
                RunIdentity {
                    session_id: request.session_id,
                    run_id: request.run_id,
                    goal_id: Some(goal_id),
                    primary_id: request.agent_run_id,
                },
                controls,
                ledger,
                publisher,
            )?
            .execute(request),
        )
        .await
    }

    async fn open_run(
        &self,
        ledger: &GoalLedger,
        session_id: SessionId,
        opening: RunOpening,
    ) -> Result<(), EngineError> {
        let RunOpening {
            run_id,
            agent_run_id,
            objective,
            images,
            config,
            skill,
        } = opening;
        let accepted = Event::MessageAccepted {
            skill,
            run_id,
            text: objective,
            images,
        };
        ledger.append(&self.journal, session_id, accepted).await?;
        let profile = config
            .roster
            .as_ref()
            .map(|roster| roster.providers.primary.clone());
        let started = Event::RunStarted {
            run_id,
            config,
            active_deadline_ms: ledger.run_active_deadline()?,
        };
        ledger.append(&self.journal, session_id, started).await?;
        let spawned = Event::AgentSpawned {
            profile,
            assignment: None,
            definition: None,
            run_id,
            agent_run_id,
            role: AgentRole::Primary,
            ordinal: 0,
            objective: None,
            grant: None,
        };
        ledger.append(&self.journal, session_id, spawned).await?;
        Ok(())
    }

    async fn open_ledger(
        &self,
        session_id: SessionId,
        goal_id: GoalId,
    ) -> Result<GoalLedger, EngineError> {
        let view = self
            .journal
            .store()
            .load_open_view(session_id)
            .await?
            .ok_or(EngineError::InvalidHistory)?;
        Ok(GoalLedger::new(view, Some(goal_id)))
    }

    async fn start_session(
        &self,
        session_lock: &mut SessionRunLock,
        session_id: SessionId,
        title: Option<String>,
        workspace_identity: WorkspaceIdentity,
    ) -> Result<(), EngineError> {
        self.journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: title.unwrap_or_else(|| "New Session".into()),
                    workspace_identity,
                },
            )
            .await?;
        session_lock.keep();
        Ok(())
    }
}

async fn end_turn(
    turn: impl Future<Output = Result<RunTurn, EngineError>>,
    end: tokio::sync::watch::Sender<()>,
) -> Result<RunTurn, EngineError> {
    let turn = turn.await;
    drop(end);
    turn
}

async fn trace_follower(
    trace: Option<crate::telemetry::RunTrace>,
    tap: Option<Subscription>,
    ended: impl Future<Output = ()>,
    ledger: &GoalLedger,
    goal_id: GoalId,
) {
    if let (Some(trace), Some(tap)) = (trace, tap) {
        let goal = || {
            ledger
                .view(|view| view.goal(goal_id).cloned())
                .ok()
                .flatten()
        };
        trace.follow(tap, ended, goal).await;
    }
}

// An autonomous continuation repeats the first request's Workspace inputs and context window.
fn continuation_request(
    base: &RunRequest,
    session_id: Option<SessionId>,
    goal: GoalRequest,
) -> RunRequest {
    RunRequest {
        skill: None,
        session_id,
        title: None,
        objective: String::new(),
        images: Vec::new(),
        workspace: base.workspace.clone(),
        launch_directory: base.launch_directory.clone(),
        include_paths: base.include_paths.clone(),
        policy: base.policy,
        goal: GoalRequest {
            context_window: base.goal.context_window,
            ..goal
        },
    }
}
