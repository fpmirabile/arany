use super::*;

impl RunLoop<'_> {
    pub(super) async fn spawn_researchers(
        &self,
        queue: &mut ChildQueue,
        children: &mut Vec<ChildWork>,
        root_request: &mut ProviderRequest,
        segment: &mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> Result<RunProgress<ResearchSpawn>, EngineError> {
        while !queue.unspawned.is_empty() && !self.controls.cancellation.is_cancelled() {
            let child = match self
                .spawn_next_child(queue, root_request, segment, deadline)
                .await
            {
                Ok(RunProgress::Ready(Some(child))) => child,
                Ok(RunProgress::Ready(None)) => break,
                Ok(RunProgress::Stopped(cause)) => {
                    return Ok(queue.abort_with(RunProgress::Stopped(cause)).await);
                }
                Err(error) => return Err(queue.abort_with(error).await),
            };
            queue.pending.push_back(children.len());
            children.push(child);
        }
        if !self.controls.cancellation.is_cancelled() {
            return Ok(RunProgress::Ready(ResearchSpawn::Ready));
        }
        if let Some(segment) = segment.take() {
            self.settle_active_segment(segment)?;
        }
        for child in children.iter() {
            self.finish_child(child.agent_run_id, AgentDisposition::Cancelled, None, false)
                .await?;
        }
        Ok(RunProgress::Ready(ResearchSpawn::Cancelled))
    }

    pub(super) async fn spawn_children(
        &self,
        count: usize,
        workers: bool,
    ) -> Result<Option<Spawned>, EngineError> {
        let research = self.controls.tools.is_some();
        let action = Action::Delegation {
            children: count,
            research,
        };
        if !self.ledger().admits(action) {
            return Ok(None);
        }
        let capacity = if workers { Action::Worker } else { action };
        self.journal
            .admit(
                self.session_id,
                capacity.max_events_for_admitted(&self.routing.providers)
                    + self.ledger().verdicts(capacity),
                capacity.tool_observations(),
            )
            .await?;
        let parallel = self.ledger().provider_concurrency(
            usize::from(self.controls.concurrency)
                .min(usize::from(self.controls.policy.max_children()))
                .min(self.ledger().view(|v| {
                    v.runs
                        .last()
                        .and_then(|r| r.config.as_ref())
                        .and_then(|c| c.worker_policy.as_ref())
                        .map_or(8, |p| usize::from(p.concurrency))
                })?),
        );
        let bound = delegation_active_bound(
            research,
            self.routing
                .providers
                .child_chains()
                .into_iter()
                .flatten()
                .map(|p| p.identity().limits.deadline)
                .max()
                .expect("child chain"),
            count.div_ceil(parallel.max(1)),
        );
        let Admission::Reserved(segment) = self.reserve_active_segment(
            bound,
            crate::session::BudgetAction::Delegation,
            self.primary_id,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(Spawned {
            children: Vec::with_capacity(count),
            parallel,
            active_segment: Some(segment),
            bound,
        }))
    }

    pub(super) async fn spawn_next_child(
        &self,
        queue: &mut ChildQueue,
        root: &mut ProviderRequest,
        segments: &mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> Result<RunProgress<Option<ChildWork>>, EngineError> {
        let Some(assignment) = queue.unspawned.pop_front() else {
            return Ok(RunProgress::Ready(None));
        };
        if let Some(editor) = assignment.editor {
            if let Some(s) = segments.take() {
                self.settle_active_segment(s)?;
            }
            let _active_work = self.ledger().active_work();
            let mut child = match self.begin_worker(root, editor).await? {
                WorkerAdmission::Admitted(child) => *child,
                WorkerAdmission::Exhausted => {
                    return Ok(RunProgress::Stopped(RunRefusal::Budget));
                }
            };
            child.tool_calls_remaining = child.tool_calls_remaining.min(queue.tool_allowance);
            child.active_deadline = queue.active_deadline.clone();
            *segments = match self
                .reserve_active_segment(
                    deadline.saturating_duration_since(Instant::now()),
                    crate::session::BudgetAction::Delegation,
                    self.primary_id,
                )?
                .map(Some)
            {
                Admission::Reserved(segment) => segment,
                Admission::Exhausted => {
                    queue.stop.get_or_insert(StopCause::ActiveTime);
                    None
                }
            };
            return Ok(RunProgress::Ready(Some(child)));
        }
        self.spawn_researcher(queue, assignment)
            .await
            .map(|child| RunProgress::Ready(Some(child)))
    }

    async fn spawn_researcher(
        &self,
        queue: &ChildQueue,
        assignment: ChildAssignment,
    ) -> Result<ChildWork, EngineError> {
        let definition = queue
            .definition
            .as_ref()
            .and_then(|n| self.definitions.get()?.get(n))
            .cloned();
        let id = AgentRunId::new();
        let research = self.controls.tools.is_some();
        self.append(Event::AgentSpawned {
            run_id: self.run_id,
            agent_run_id: id,
            role: AgentRole::Child,
            ordinal: self
                .ledger()
                .view(|v| v.runs.last().expect("Run").agents.len())?
                .try_into()
                .map_err(|_| EngineError::CoordinatorFailed)?,
            objective: Some(assignment.objective.clone()),
            definition: queue.definition.clone(),
            profile: assignment.profile.or_else(|| self.default_child_profile()),
            grant: Some(if research {
                ChildGrant::ReadOnly {
                    unguarded: crate::tools::UnguardedGrant::Denied,
                }
            } else {
                ChildGrant::NoTools
            }),
            assignment: None,
        })
        .await?;
        Ok(ChildWork {
            agent_run_id: id,
            objective: assignment.objective,
            turns_remaining: definition.as_ref().map_or(MAX_CHILD_TURNS, |d| d.max_turns),
            definition,
            kind: ChildKind::Researcher,
            source_context: None,
            steering_notes: Vec::new(),
            stop_hook_feedback: Vec::new(),
            tools: self
                .controls
                .tools
                .map(|r| r.research_context(queue.tool_allowance))
                .transpose()?,
            tool_calls_remaining: queue.tool_allowance,
            active_deadline: queue.active_deadline.clone(),
        })
    }

    pub(super) fn spawn_child_attempt(
        &self,
        queue: &mut ChildQueue,
        index: usize,
        request: ProviderRequest,
        provider: std::sync::Arc<dyn crate::provider::Provider>,
        deadline: Instant,
    ) -> Result<(), EngineError> {
        let preview = (self
            .routing
            .chain(crate::provider::ProviderRole::Child, request.agent_run_id)?
            .len()
            > 1)
        .then(|| crate::provider::ResponsePreview::new().0);
        self.spawn_attempt(
            queue,
            ActiveChild {
                limits: crate::provider::RateLimitSlot::default(),
                index: Some(index),
                request,
            },
            provider,
            preview,
            deadline,
        );
        Ok(())
    }

    pub(super) fn spawn_attempt(
        &self,
        queue: &mut ChildQueue,
        active: ActiveChild,
        provider: std::sync::Arc<dyn crate::provider::Provider>,
        preview: Option<crate::provider::ResponsePreview>,
        deadline: Instant,
    ) {
        let cancellation = self.controls.cancellation.clone();
        let request = active.request.clone();
        let limits = active.limits.clone();
        let provider_activity = self.ledger().provider_activity();
        let handle = queue.calls.spawn(async move {
            let _provider_work = provider_activity.start();
            crate::engine::routing::invoke_attempt(
                provider.as_ref(),
                crate::engine::routing::AttemptInput {
                    request,
                    deadline,
                    cancellation,
                    preview,
                    limits,
                },
                crate::fault::LatencyReceipt::default(),
            )
            .await
        });
        queue.active.insert(handle.id(), active);
    }
}
