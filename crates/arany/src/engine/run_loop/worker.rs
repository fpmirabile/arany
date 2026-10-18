use super::children::ChildWork;
use super::step::{BoxedStep, Grant};
use super::{Action, RunLoop};
use crate::engine::EngineError;
use crate::provider::{ChildAssignment, ProviderRequest};
use crate::session::worker::*;
use crate::session::{AgentDisposition, AgentRole, AgentRunId, ChildGrant, Event, GoalPhase};
use crate::session::{evidence_admitted, verification_passed};
use crate::tools::{
    Attestation, CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolObservation, ToolRuntime,
};
use base64::Engine as _;
use std::time::Duration;

pub(super) struct CompletedWorker {
    stopped: bool,
    disposition: WorkerDisposition,
    data: Option<WorkerDataRef>,
    bytes: Option<Vec<u8>>,
}

impl RunLoop<'_> {
    pub(super) fn valid_editor_delegation(
        &self,
        children: &[ChildAssignment],
        agent: Option<&str>,
    ) -> bool {
        let Some(runtime) = self.controls.tools else {
            return false;
        };
        let Some(policy) = runtime.worker_policy() else {
            return false;
        };
        let Ok(Some(plan)) = self.ledger().plan_contract() else {
            return false;
        };
        let Ok((attempts, wave)) = self.ledger().view(|v| {
            let r = v.runs.last().expect("active Run");
            let proposals: Vec<_> = r
                .primary()
                .into_iter()
                .flat_map(|a| &a.provider_calls)
                .filter_map(|c| c.delegation.as_ref())
                .collect();
            (
                proposals.iter().map(|p| p.len()).sum::<usize>(),
                proposals.len() + 1,
            )
        }) else {
            return false;
        };
        self.controls.goal_phase == GoalPhase::Implementation
            && crate::provider::valid_assignments(children, agent)
            && agent.is_none_or(|name| self.definitions.get().is_some_and(|r| r.contains_key(name)))
            && attempts + children.len() <= usize::from(self.controls.policy.max_children())
            && self.wave_settled().unwrap_or(false)
            && children.iter().enumerate().all(|(index, child)| {
                let Some(a) = &child.editor else {
                    return self.valid_research_assignment(child, agent);
                };
                let Some(definition) = self.definitions.get().and_then(|d| d.get(&a.definition))
                else {
                    return false;
                };
                usize::from(a.wave) == wave
                    && a.resumes.is_none()
                    && a.supersedes.is_none_or(|id| {
                        self.ledger()
                            .view(|v| {
                                v.runs.last().is_some_and(|r| {
                                    r.agents.iter().any(|prior| {
                                        prior.id == id
                                            && prior.status != crate::session::AgentStatus::Active
                                            && prior
                                                .assignment
                                                .as_ref()
                                                .is_some_and(|w| w.released.is_some())
                                    })
                                })
                            })
                            .unwrap_or(false)
                    })
                    && definition.editor()
                    && (!definition.background || a.mode == WorkerMode::Background)
                    && a.definition_digest
                        == crate::tools::types::digest(
                            &serde_json::to_vec(definition).expect("normalized definition"),
                        )
                    && a.max_turns <= definition.max_turns
                    && a.max_turns <= policy.max_turns
                    && a.lease.iter().all(|p| {
                        within(p, &plan.paths) && within(p, &policy.parent_grant.write_paths)
                    })
                    && !children[..index]
                        .iter()
                        .filter_map(|c| c.editor.as_ref())
                        .any(|b| {
                            a.lease
                                .iter()
                                .any(|p| b.lease.iter().any(|q| overlaps(p, q)))
                        })
                    && match &a.criterion {
                        crate::session::GoalSuccessCriterion::ToolCallSucceeded { call } => {
                            !matches!(call.as_ref(), ToolCall::Mkdir { .. })
                                && plan.allows_call(call)
                                && runtime.allows(call)
                        }
                        crate::session::GoalSuccessCriterion::WorkspaceFileContent {
                            path, ..
                        }
                        | crate::session::GoalSuccessCriterion::WorkspaceFileDigest {
                            path, ..
                        } => within(path, &plan.paths),
                    }
                    && self.editor_grant(a).is_ok_and(|g| g.valid())
            })
    }

    pub(super) fn wave_settled(&self) -> Result<bool, EngineError> {
        self.ledger()
            .view(|v| v.runs.last().is_some_and(crate::session::wave_settled))
    }

    pub(super) fn coordination_ready(&self) -> Result<bool, EngineError> {
        self.ledger().view(|v| {
            v.runs.last().is_some_and(|r| {
                crate::session::coordination_ready(r)
                    && r.agents.iter().any(|a| {
                        a.status == crate::session::AgentStatus::Active && a.assignment.is_some()
                    })
            })
        })
    }

    pub(super) fn worker_stopped(&self) -> Result<bool, EngineError> {
        self.ledger()
            .view(|v| v.runs.last().is_some_and(|r| r.workers.stop.is_some()))
    }

    pub(super) fn editor_grant(&self, a: &EditorAssignment) -> Result<WorkerGrant, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let mut grant = runtime
            .worker_policy()
            .ok_or(EngineError::InvalidRequest)?
            .parent_grant;
        let def = self
            .definitions
            .get()
            .and_then(|d| d.get(&a.definition))
            .ok_or(EngineError::InvalidRequest)?;
        grant.read_paths = a.lease.clone();
        grant.write_paths = a.lease.clone();
        grant.tools.retain(|tool| {
            !matches!(tool, WorkerTool::Skill | WorkerTool::McpList)
                && def.allows_worker_tool(*tool)
        });
        grant.skills.clear();
        grant.mcp_servers.clear();
        grant.permission_mode = def.permission_mode;
        Ok(grant)
    }

    pub(super) fn proposed_editor(&self, id: AgentRunId) -> Result<EditorAssignment, EngineError> {
        self.ledger()
            .view(|v| {
                v.runs
                    .last()?
                    .agents
                    .iter()
                    .find(|a| a.id == id)?
                    .assignment
                    .as_ref()
                    .map(|w| w.pins.request.clone())
            })?
            .ok_or(EngineError::InvalidHistory)
    }

    pub(super) fn worker_allowance(&self, id: AgentRunId) -> Result<u32, EngineError> {
        self.worker_data_bytes
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?
            .get(&id)
            .copied()
            .ok_or(EngineError::CoordinatorFailed)
    }

    fn sequence(&self) -> Result<u64, EngineError> {
        self.ledger().view(|v| v.last_sequence)
    }

    pub(super) fn worker_target(&self, id: AgentRunId) -> Result<EffectTarget, EngineError> {
        self.worker
            .try_lock()
            .map_err(|_| EngineError::CoordinatorFailed)?
            .get(&id)
            .map(crate::tools::worker::View::target)
            .ok_or(EngineError::CoordinatorFailed)
    }

    pub(super) fn worker_call_chunks(
        &self,
        id: AgentRunId,
        call: &ToolCall,
    ) -> Result<usize, EngineError> {
        Ok(match call {
            ToolCall::PrepareWorker { preparation } => usize::from(preparation.max_chunks),
            ToolCall::Write { .. } | ToolCall::Edit { .. } | ToolCall::Mkdir { .. } => {
                (self.worker_allowance(id)? as usize).div_ceil(WORKER_CHUNK_BYTES)
            }
            _ => 0,
        })
    }

    pub(super) async fn admit_worker_action(&self, chunks: usize) -> Result<(), EngineError> {
        let action = Action::Worker { chunks };
        self.journal
            .admit(
                self.session_id,
                action.max_events_for_admitted(&self.routing.providers),
                action.tool_observations(),
            )
            .await
            .map_err(|error| match error {
                crate::store::StoreError::StorageFull | crate::store::StoreError::ReplayLimit => {
                    EngineError::Tool(crate::tools::ToolError::Limit)
                }
                error => error.into(),
            })?;
        Ok(())
    }

    async fn seal_worker_data(
        &self,
        intent: &EffectIntent,
        assignment_id: AgentRunId,
        purpose: WorkerDataPurpose,
        bytes: &[u8],
    ) -> Result<WorkerDataRef, EngineError> {
        let first_sequence = self
            .sequence()?
            .checked_add(1)
            .ok_or(EngineError::InvalidHistory)?;
        let chunks = u16::try_from(bytes.len().div_ceil(WORKER_CHUNK_BYTES))
            .map_err(|_| EngineError::InvalidHistory)?;
        for (ordinal, chunk) in bytes.chunks(WORKER_CHUNK_BYTES).enumerate() {
            self.append(Event::WorkerDataChunk {
                run_id: self.run_id,
                agent_run_id: intent.agent_run_id,
                record: WorkerDataChunk {
                    assignment_id,
                    purpose,
                    tool_id: intent.id,
                    ordinal: u16::try_from(ordinal).map_err(|_| EngineError::InvalidHistory)?,
                    bytes: u32::try_from(chunk.len()).map_err(|_| EngineError::InvalidHistory)?,
                    sha256: crate::tools::types::digest(chunk),
                    content: base64::engine::general_purpose::STANDARD.encode(chunk),
                },
            })
            .await?;
        }
        let data = WorkerDataRef {
            assignment_id,
            purpose,
            first_sequence,
            seal_sequence: self
                .sequence()?
                .checked_add(1)
                .ok_or(EngineError::InvalidHistory)?,
            chunks,
            bytes: u32::try_from(bytes.len()).map_err(|_| EngineError::InvalidHistory)?,
            sha256: crate::tools::types::digest(bytes),
        };
        self.append(Event::WorkerDataSealed {
            run_id: self.run_id,
            agent_run_id: intent.agent_run_id,
            record: WorkerDataSealed {
                tool_id: intent.id,
                data: data.clone(),
            },
        })
        .await?;
        Ok(data)
    }

    pub(super) async fn capture_worker_base(
        &self,
        runtime: &ToolRuntime,
        intent: EffectIntent,
    ) -> Result<ToolObservation, EngineError> {
        let ToolCall::PrepareWorker { preparation } = &intent.call else {
            return Err(EngineError::InvalidRequest);
        };
        let a = self
            .worker_requests
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?
            .get(&preparation.assignment_id)
            .cloned()
            .ok_or(EngineError::CoordinatorFailed)?;
        let capture_runtime = runtime.clone();
        let run_id = self.run_id;
        let id = preparation.assignment_id;
        let grant = self.editor_grant(&a)?;
        let data_bytes = self.worker_allowance(id)?;
        let view = tokio::task::spawn_blocking(move || {
            crate::tools::worker::View::capture(&capture_runtime, run_id, id, grant, data_bytes)
        })
        .await
        .map_err(|_| EngineError::CoordinatorFailed)?;
        let retained_limit = self
            .ledger()
            .view(|v| {
                v.runs
                    .last()
                    .and_then(|r| r.config.as_ref())
                    .and_then(|c| c.worker_policy.as_ref())
                    .map(WorkerPolicy::retained_bytes)
            })?
            .ok_or(EngineError::InvalidHistory)?;
        let view = match view.and_then(|mut view| {
            view.restrict_retained(retained_limit)?;
            Ok(view)
        }) {
            Ok(view) => view,
            Err(error) => {
                return Ok(ToolObservation {
                    intent,
                    disposition: ToolDisposition::Failed,
                    output: error.to_string(),
                    attestation: None,
                    cache_update: CacheUpdate::NotApplicable,
                    enforcement_cause: None,
                });
            }
        };
        let bytes = match view.manifest() {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(ToolObservation {
                    intent,
                    disposition: ToolDisposition::Failed,
                    output: error.to_string(),
                    attestation: None,
                    cache_update: CacheUpdate::NotApplicable,
                    enforcement_cause: None,
                });
            }
        };
        let data = self
            .seal_worker_data(
                &intent,
                preparation.assignment_id,
                WorkerDataPurpose::Base,
                &bytes,
            )
            .await?;
        let observation = ToolObservation {
            intent: intent.clone(),
            disposition: ToolDisposition::Succeeded,
            output: "Worker base captured; Workspace unchanged".into(),
            attestation: Some(Attestation::WorkerData {
                guard: None,
                intent_digest: intent.digest(),
                data,
            }),
            cache_update: CacheUpdate::NotApplicable,
            enforcement_cause: None,
        };
        self.worker.lock().await.insert(id, view);
        Ok(observation)
    }

    pub(super) async fn dispatch_worker(
        &self,
        runtime: &ToolRuntime,
        intent: EffectIntent,
    ) -> Result<
        (
            ToolObservation,
            Option<(crate::tools::worker::Candidate, WorkerDataRef)>,
        ),
        EngineError,
    > {
        let mut slot = self.worker.lock().await;
        let view = slot
            .get_mut(&intent.agent_run_id)
            .ok_or(EngineError::CoordinatorFailed)?;
        view.note_intent(&intent);
        if matches!(
            intent.call,
            ToolCall::Read { .. } | ToolCall::List { .. } | ToolCall::Search { .. }
        ) {
            return Ok((view.read(intent), None));
        }
        let runtime = runtime.with_queue_accounting();
        let result = view
            .edit(&runtime, intent.clone(), self.controls.cancellation.clone())
            .await;
        if self.ledger().in_flight_children() == 0 {
            self.ledger()
                .exclude_idle_approval(runtime.queued_duration()?)?;
        }
        let candidate = match result {
            Ok(candidate) => candidate,
            Err(observation) => return Ok((*observation, None)),
        };
        let purpose = if intent.call.is_internal() {
            WorkerDataPurpose::Handover
        } else {
            WorkerDataPurpose::Mutation
        };
        let data = if purpose == WorkerDataPurpose::Handover {
            view.reusable_data(&candidate.data)
        } else {
            None
        };
        drop(slot);
        let data = match data {
            Some(data) => data,
            None => {
                self.seal_worker_data(&intent, intent.agent_run_id, purpose, &candidate.data)
                    .await?
            }
        };
        let observation = candidate.observation(&data)?;
        Ok((observation, Some((candidate, data))))
    }

    pub(super) fn begin_worker<'a>(
        &'a self,
        root: &'a mut ProviderRequest,
        assignment: Box<EditorAssignment>,
    ) -> BoxedStep<'a, Option<ChildWork>> {
        Box::pin(self.begin_worker_inner(root, assignment))
    }

    async fn begin_worker_inner(
        &self,
        root: &mut ProviderRequest,
        assignment: Box<EditorAssignment>,
    ) -> Result<Option<ChildWork>, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        if !self.ledger().admits(Action::Delegation {
            children: 1,
            research: true,
        }) {
            self.record_capacity_stop().await?;
            return Err(EngineError::GoalBudgetExhausted);
        }
        let mut allowance = WORKER_DATA_BYTES;
        loop {
            let action = Action::Worker {
                chunks: allowance.div_ceil(WORKER_CHUNK_BYTES),
            };
            match self
                .journal
                .admit(
                    self.session_id,
                    action.max_events_for_admitted(&self.routing.providers),
                    2,
                )
                .await
            {
                Ok(()) => break,
                Err(
                    crate::store::StoreError::StorageFull | crate::store::StoreError::ReplayLimit,
                ) if allowance > WORKER_CHUNK_BYTES => allowance /= 2,
                Err(
                    crate::store::StoreError::StorageFull | crate::store::StoreError::ReplayLimit,
                ) => {
                    return Err(EngineError::GoalBudgetExhausted);
                }
                Err(error) => return Err(error.into()),
            }
        }
        let allowance = u32::try_from(allowance).map_err(|_| EngineError::CoordinatorFailed)?;
        let id = AgentRunId::new();
        self.worker_data_bytes
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?
            .insert(id, allowance);
        self.worker_requests
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?
            .insert(id, (*assignment).clone());
        let observation = match self.prepare_worker_base(root, id, allowance).await {
            Ok(observation) => observation,
            Err(EngineError::GoalBudgetExhausted) => {
                return Err(EngineError::GoalBudgetExhausted);
            }
            Err(error) => return Err(error),
        };
        let Some(observation) = observation else {
            return Ok(None);
        };
        let Some(Attestation::WorkerData { data: base, .. }) = observation.attestation else {
            return Ok(None);
        };
        if self.observe_worker_stop().await? {
            self.worker
                .lock()
                .await
                .remove(&id)
                .ok_or(EngineError::CoordinatorFailed)?
                .dispose()?;
            return Err(EngineError::GoalBudgetExhausted);
        }
        let grant = self.editor_grant(&assignment)?;
        self.append(Event::AgentSpawned {
            run_id: self.run_id,
            agent_run_id: id,
            role: AgentRole::Child,
            ordinal: self
                .ledger()
                .view(|v| v.runs.last().expect("Run").agents.len() as u8)?,
            objective: Some(assignment.mission.clone()),
            definition: Some(assignment.definition.clone()),
            profile: None,
            grant: Some(ChildGrant::EditOnly),
            assignment: Some(WorkerAssignment {
                request: *assignment.clone(),
                grant: grant.clone(),
                policy_digest: observation.intent.policy_digest,
                permissions_digest: runtime.receipt().permissions_digest,
                base,
                data_bytes: allowance,
            }),
        })
        .await?;
        let remaining = self.ledger().remaining_calls().map_or(16, |n| {
            usize::try_from(n.saturating_sub(3) / 2)
                .unwrap_or(16)
                .min(16)
        });
        let child = ChildWork {
            agent_run_id: id,
            objective: assignment.mission.clone(),
            definition: self
                .definitions
                .get()
                .and_then(|d| d.get(&assignment.definition))
                .cloned(),
            editor: true,
            turns_remaining: assignment.max_turns,
            stop_hook_feedback: Vec::new(),
            tools: Some(runtime.editor_context(&grant, remaining)?),
            tool_calls_remaining: remaining,
        };
        Ok(Some(child))
    }

    async fn prepare_worker_base(
        &self,
        root: &mut ProviderRequest,
        id: AgentRunId,
        allowance: u32,
    ) -> Result<Option<crate::tools::ToolObservation>, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let preparation = ToolCall::PrepareWorker {
            preparation: WorkerPreparation {
                assignment_id: id,
                purpose: WorkerDataPurpose::Base,
                max_chunks: u16::try_from(
                    WORKER_MANIFEST_BYTES
                        .min(allowance as usize)
                        .div_ceil(WORKER_CHUNK_BYTES),
                )
                .map_err(|_| EngineError::CoordinatorFailed)?,
                max_bytes: u32::try_from(WORKER_MANIFEST_BYTES)
                    .map_err(|_| EngineError::CoordinatorFailed)?
                    .min(allowance),
            },
        };
        let intent = self
            .primary_intent(runtime, preparation.clone())?
            .ok_or(EngineError::GoalBudgetExhausted)?;
        let observation = self
            .run_intent(
                &Grant::Primary {
                    request: root,
                    review_allowed: false,
                },
                runtime,
                intent,
                &preparation,
                false,
            )
            .await?;
        Ok(observation.map(|(observation, _)| observation))
    }

    pub(super) fn complete_worker<'a>(
        &'a self,
        child: &'a mut ChildWork,
        finish: Option<crate::provider::Finish>,
        deadline: tokio::time::Instant,
        cancelled: bool,
    ) -> BoxedStep<'a, CompletedWorker> {
        Box::pin(self.complete_worker_inner(child, finish, deadline, cancelled))
    }

    async fn complete_worker_inner(
        &self,
        child: &mut ChildWork,
        finish: Option<crate::provider::Finish>,
        deadline: tokio::time::Instant,
        cancelled: bool,
    ) -> Result<CompletedWorker, EngineError> {
        let id = child.agent_run_id;
        let mut stopped = self.observe_worker_stop().await?;
        let data = if finish.is_some() && !stopped {
            let call = ToolCall::PrepareWorker {
                preparation: WorkerPreparation {
                    assignment_id: id,
                    purpose: WorkerDataPurpose::Handover,
                    max_chunks: (self.worker_allowance(id)? as usize).div_ceil(WORKER_CHUNK_BYTES)
                        as u16,
                    max_bytes: self.worker_allowance(id)?,
                },
            };
            let captured = self.capture_handover(child, call, deadline).await;
            match captured {
                Ok(data) => data,
                Err(
                    EngineError::Tool(crate::tools::ToolError::Limit)
                    | EngineError::GoalBudgetExhausted,
                ) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        stopped |= self.observe_worker_stop().await?;
        let latest = self.ledger().view(|v| {
            v.runs
                .last()
                .and_then(|r| r.agents.iter().find(|a| a.id == id))
                .and_then(|a| a.tools.last())
                .and_then(|t| t.observation.clone())
        })?;
        let private_uncertain = latest
            .as_ref()
            .is_some_and(|o| o.disposition == ToolDisposition::Uncertain);
        let bytes = if data.is_some() {
            Some(
                self.worker
                    .lock()
                    .await
                    .get(&id)
                    .ok_or(EngineError::CoordinatorFailed)?
                    .handover()?,
            )
        } else {
            None
        };
        if let Some(observation) = latest {
            self.worker
                .lock()
                .await
                .get_mut(&id)
                .ok_or(EngineError::CoordinatorFailed)?
                .note_intent(&observation.intent);
        }
        let cleanup = self
            .worker
            .lock()
            .await
            .remove(&id)
            .ok_or(EngineError::CoordinatorFailed)?
            .dispose()?;
        let no_change = bytes.as_ref().is_some_and(|b| matches!(decode_data(b, WorkerDataPurpose::Handover), Ok(WorkerDataFacts::Changes(files)) if files.is_empty()));
        let success = data.is_some();
        let (disposition, cause) = if private_uncertain {
            (
                WorkerDisposition::PrivateUncertain,
                WorkerCause::PrivateUncertain,
            )
        } else if success {
            (
                if no_change {
                    WorkerDisposition::NoChange
                } else {
                    WorkerDisposition::Ready
                },
                WorkerCause::Finished,
            )
        } else if cancelled || self.controls.cancellation.is_cancelled() || self.pause_requested() {
            (WorkerDisposition::Cancelled, WorkerCause::Cancelled)
        } else {
            (
                WorkerDisposition::Failed,
                if stopped {
                    WorkerCause::Budget
                } else {
                    WorkerCause::ToolFailed
                },
            )
        };
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id: id,
            disposition: if success {
                AgentDisposition::Finished
            } else if disposition == WorkerDisposition::Cancelled {
                AgentDisposition::Cancelled
            } else {
                AgentDisposition::Failed
            },
            summary: if success {
                finish.as_ref().map(|f| f.summary.clone())
            } else {
                None
            },
            result: if success {
                finish.as_ref().map(|f| f.result.clone())
            } else {
                None
            },
            worker: Some(WorkerOutcome {
                disposition,
                cause,
                data: data.clone(),
                cleanup: Some(cleanup),
            }),
        })
        .await?;
        Ok(CompletedWorker {
            stopped,
            disposition,
            data,
            bytes,
        })
    }

    pub(super) async fn settle_worker_handover(
        &self,
        root: &mut ProviderRequest,
        child: &ChildWork,
        completed: CompletedWorker,
    ) -> Result<(), EngineError> {
        let CompletedWorker {
            stopped,
            disposition,
            data,
            bytes,
        } = completed;
        let id = child.agent_run_id;
        let no_change = disposition == WorkerDisposition::NoChange;
        let private_uncertain = disposition == WorkerDisposition::PrivateUncertain;
        let success = data.is_some();
        let finished = self.sequence()?;
        if stopped || !success || no_change {
            let reason = if stopped {
                WorkerSettlementReason::Stopped
            } else if no_change {
                WorkerSettlementReason::NoChange
            } else if private_uncertain {
                WorkerSettlementReason::PrivateUncertain
            } else if disposition == WorkerDisposition::Cancelled {
                WorkerSettlementReason::Cancelled
            } else {
                WorkerSettlementReason::Failed
            };
            let cause_sequence = if stopped {
                self.stop_sequence()?
            } else {
                finished
            };
            self.discard_worker(id, None, reason, cause_sequence)
                .await?;
        } else {
            let data = data.ok_or(EngineError::CoordinatorFailed)?;
            self.handover
                .lock()
                .map_err(|_| EngineError::CoordinatorFailed)?
                .insert(id, bytes.ok_or(EngineError::CoordinatorFailed)?.into());
            self.append(Event::WorkerHandoverRecorded {
                run_id: self.run_id,
                agent_run_id: id,
                record: WorkerHandover {
                    assignment_id: id,
                    base_digest: cleanup_base(self, id)?,
                    data: data.clone(),
                },
            })
            .await?;
            let sequence = self.sequence()?;
            if !Box::pin(self.integrate_worker(root, id, sequence, data)).await? {
                self.handover
                    .lock()
                    .map_err(|_| EngineError::CoordinatorFailed)?
                    .remove(&id);
                return Ok(());
            }
        }
        self.handover
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?
            .remove(&id);
        Ok(())
    }

    async fn capture_handover(
        &self,
        child: &mut ChildWork,
        call: ToolCall,
        deadline: tokio::time::Instant,
    ) -> Result<Option<WorkerDataRef>, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let Some(mut intent) =
            self.researcher_intent(runtime, child.agent_run_id, call.clone(), deadline)
        else {
            return Ok(None);
        };
        let available = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .saturating_sub(runtime.reservation_overhead(&call) + Duration::from_secs(15));
        intent.limits.runtime_ms = intent
            .limits
            .runtime_ms
            .min(u32::try_from(available.as_millis() / 4).unwrap_or(u32::MAX));
        if intent.limits.runtime_ms < 1_000 {
            return Ok(None);
        }
        intent.expires_at_ms =
            crate::tools::now_ms().saturating_add(u64::from(intent.limits.runtime_ms));
        intent.target = self.worker_target(child.agent_run_id)?;
        intent.limits.cache_requested_bytes = 0;
        intent.limits.cache_bytes = 0;
        intent.limits.cache_candidate_bytes = 0;
        let observation = self
            .run_intent(
                &Grant::Researcher { child, deadline },
                runtime,
                intent,
                &call,
                false,
            )
            .await?;
        Ok(observation.and_then(|(o, _)| match o.attestation {
            Some(Attestation::WorkerData { data, .. }) => Some(data),
            _ => None,
        }))
    }

    async fn integrate_worker(
        &self,
        root: &mut ProviderRequest,
        id: AgentRunId,
        handover_sequence: u64,
        data: WorkerDataRef,
    ) -> Result<bool, EngineError> {
        if self.observe_worker_stop().await? {
            self.discard_worker(
                id,
                Some(handover_sequence),
                WorkerSettlementReason::Stopped,
                self.stop_sequence()?,
            )
            .await?;
            return Ok(false);
        }
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let call = ToolCall::ApplyChangeset {
            source: ChangesetSource::WorkerHandover {
                assignment_id: id,
                handover_sequence,
                data,
            },
        };
        let Some(intent) = self.primary_intent(runtime, call.clone())? else {
            self.record_capacity_stop().await?;
            self.discard_worker(
                id,
                Some(handover_sequence),
                WorkerSettlementReason::Stopped,
                self.stop_sequence()?,
            )
            .await?;
            return Ok(false);
        };
        let tool_id = intent.id;
        let result = self
            .run_intent(
                &Grant::Primary {
                    request: root,
                    review_allowed: true,
                },
                runtime,
                intent,
                &call,
                false,
            )
            .await;
        match result {
            Ok(_) | Err(EngineError::GoalBudgetExhausted) => {}
            Err(EngineError::Tool(crate::tools::ToolError::Limit)) => {
                self.record_capacity_stop().await?
            }
            Err(error) => return Err(error),
        }
        let tool = self
            .ledger()
            .view(|v| v.runs.last().and_then(|r| r.tools.last()).cloned())?
            .ok_or(EngineError::InvalidHistory)?;
        if tool.intent.id != tool_id {
            self.record_capacity_stop().await?;
            self.discard_worker(
                id,
                Some(handover_sequence),
                WorkerSettlementReason::Stopped,
                self.stop_sequence()?,
            )
            .await?;
            return Ok(false);
        }
        self.observe_worker_stop().await?;
        let observation = tool.observation.ok_or(EngineError::InvalidHistory)?;
        let outcome = match observation.disposition {
            ToolDisposition::Succeeded => WorkerIntegrationOutcome::Integrated,
            ToolDisposition::Conflict => WorkerIntegrationOutcome::Conflict,
            ToolDisposition::Denied => WorkerIntegrationOutcome::Denied,
            ToolDisposition::Failed | ToolDisposition::Limit | ToolDisposition::Cancelled
                if observation.attestation.is_none() =>
            {
                WorkerIntegrationOutcome::Denied
            }
            ToolDisposition::Uncertain => WorkerIntegrationOutcome::Uncertain,
            _ => return Err(crate::tools::ToolError::Uncertain.into()),
        };
        self.append(Event::WorkerIntegrationRecorded {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            record: WorkerIntegration {
                assignment_id: id,
                handover_sequence,
                tool_id: tool.intent.id,
                finished_sequence: tool.finished_sequence.ok_or(EngineError::InvalidHistory)?,
                outcome,
            },
        })
        .await?;
        let cause = self.sequence()?;
        match outcome {
            WorkerIntegrationOutcome::Integrated => {
                self.release_worker(
                    id,
                    Some(handover_sequence),
                    WorkerSettlementReason::Integrated,
                    cause,
                )
                .await?
            }
            WorkerIntegrationOutcome::Uncertain => {
                return Err(crate::tools::ToolError::Uncertain.into());
            }
            WorkerIntegrationOutcome::Conflict | WorkerIntegrationOutcome::Denied => {
                self.discard_worker(
                    id,
                    Some(handover_sequence),
                    if outcome == WorkerIntegrationOutcome::Conflict {
                        WorkerSettlementReason::Conflict
                    } else {
                        WorkerSettlementReason::Denied
                    },
                    cause,
                )
                .await?;
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn discard_worker(
        &self,
        id: AgentRunId,
        handover: Option<u64>,
        reason: WorkerSettlementReason,
        cause: u64,
    ) -> Result<(), EngineError> {
        self.append(Event::WorkerHandoverDiscarded {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            record: WorkerSettlement {
                assignment_id: id,
                handover_sequence: handover,
                reason,
                cause_sequence: cause,
            },
        })
        .await?;
        self.release_worker(id, handover, reason, self.sequence()?)
            .await
    }
    async fn release_worker(
        &self,
        id: AgentRunId,
        handover: Option<u64>,
        reason: WorkerSettlementReason,
        cause: u64,
    ) -> Result<(), EngineError> {
        self.append(Event::WorkerLeaseReleased {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            record: WorkerSettlement {
                assignment_id: id,
                handover_sequence: handover,
                reason,
                cause_sequence: cause,
            },
        })
        .await
    }
    async fn record_capacity_stop(&self) -> Result<(), EngineError> {
        if self
            .ledger()
            .view(|v| v.runs.last().is_some_and(|r| r.workers.stop.is_some()))?
        {
            return Ok(());
        }
        self.append(Event::WorkerStopObserved {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            record: WorkerStopRecord {
                reason: WorkerStopReason::Budget,
            },
        })
        .await
    }

    fn stop_sequence(&self) -> Result<u64, EngineError> {
        self.ledger()
            .view(|v| v.runs.last().and_then(|r| r.workers.stop).map(|(s, _)| s))?
            .ok_or(EngineError::InvalidHistory)
    }
    pub(super) async fn observe_worker_stop(&self) -> Result<bool, EngineError> {
        if self
            .ledger()
            .view(|v| v.runs.last().is_some_and(|r| r.workers.stop.is_some()))?
        {
            return Ok(true);
        }
        let reason = if self.controls.cancellation.is_cancelled() {
            Some(WorkerStopReason::Cancelled)
        } else if self.pause_requested() {
            Some(WorkerStopReason::Paused)
        } else if !self.ledger().can_start_provider_call()
            || self.ledger().remaining_calls() == Some(0)
            || self.ledger().spent_active_time_exhausted()
        {
            Some(WorkerStopReason::Budget)
        } else {
            None
        };
        if let Some(reason) = reason {
            // The stop fact is one Event; a refused admission must not relabel a cancel, pause or budget stop.
            let _ = self.admit_worker_action(0).await;
            self.append(Event::WorkerStopObserved {
                run_id: self.run_id,
                agent_run_id: self.primary_id,
                record: WorkerStopRecord { reason },
            })
            .await?;
            if reason == WorkerStopReason::Paused {
                self.pause_at_boundary();
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) async fn verify_workers(&self) -> Result<(), EngineError> {
        let records = self.ledger().view(|view| {
            let Some(run) = view.runs.last() else {
                return Vec::new();
            };
            let Some(plan) = self
                .goal_id
                .and_then(|id| view.goal(id))
                .and_then(|g| g.plan_contract.as_ref())
            else {
                return Vec::new();
            };
            run.agents
                .iter()
                .filter_map(|a| {
                    let w = a.assignment.as_ref()?;
                    let released = w.released?;
                    if w.verification.as_ref().is_some_and(|(_, v)| v.passed)
                        || !w.outcome.as_ref().is_some_and(|(_, o)| {
                            matches!(
                                o.disposition,
                                WorkerDisposition::Ready | WorkerDisposition::NoChange
                            )
                        })
                        || w.integration
                            .as_ref()
                            .is_some_and(|(_, i)| i.outcome != WorkerIntegrationOutcome::Integrated)
                    {
                        return None;
                    }
                    let mut contract = plan.clone();
                    contract.success_criterion = w.pins.request.criterion.clone();
                    let mut evidence: Vec<u64> = run
                        .tools
                        .iter()
                        .filter_map(|t| {
                            let seq = t.finished_sequence.filter(|seq| *seq > released)?;
                            t.observation
                                .as_ref()
                                .is_some_and(|o| evidence_admitted(&contract, o))
                                .then_some(seq)
                        })
                        .collect();
                    evidence.drain(..evidence.len().saturating_sub(8));
                    if evidence.is_empty() {
                        return None;
                    }
                    let passed = verification_passed(&contract, run, released, &evidence).ok()?;
                    Some(WorkerVerification {
                        assignment_id: a.id,
                        criterion_digest: crate::tools::types::digest(
                            &serde_json::to_vec(&contract.success_criterion).expect("criterion"),
                        ),
                        passed,
                        evidence,
                    })
                })
                .collect::<Vec<_>>()
        })?;
        if !records.is_empty() && self.admit_worker_action(0).await.is_err() {
            return self.record_capacity_stop().await;
        }
        for record in records {
            self.append(Event::WorkerVerificationRecorded {
                run_id: self.run_id,
                agent_run_id: self.primary_id,
                record,
            })
            .await?;
        }
        Ok(())
    }
}

fn cleanup_base(loop_: &RunLoop<'_>, id: AgentRunId) -> Result<[u8; 32], EngineError> {
    loop_
        .ledger()
        .view(|v| {
            v.runs
                .last()
                .and_then(|r| r.agents.iter().find(|a| a.id == id))
                .and_then(|a| a.assignment.as_ref())
                .map(|w| w.pins.base.sha256)
        })?
        .ok_or(EngineError::InvalidHistory)
}
