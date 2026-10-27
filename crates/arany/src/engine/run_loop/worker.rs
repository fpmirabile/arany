use super::children::{ChildKind, ChildWork};
use super::step::BoxedStep;
use super::stop::StopCause;
use super::{Action, RunLoop};
use crate::engine::EngineError;
use crate::provider::{ChildAssignment, ProviderRequest};
use crate::session::worker::*;
use crate::session::{AgentDisposition, AgentRole, AgentRunId, ChildGrant, Event, GoalPhase};
use crate::session::{evidence_admitted, verification_passed};
use crate::tools::{ToolDisposition, ToolRuntime};

#[must_use]
pub(super) enum WorkerAdmission {
    Admitted(Box<ChildWork>),
    Exhausted,
}
#[must_use]
pub(super) enum WorkerCompletion {
    Settled,
    Uncertain,
}

pub(super) struct WorkerEnd {
    pub(super) answer: Option<crate::provider::Answer>,
    pub(super) torn_down: bool,
    pub(super) failure: WorkerCause,
    pub(super) step_stop: Option<StopCause>,
}
impl WorkerEnd {
    pub(super) fn stopped(stop: super::children::WaveStop) -> Self {
        use super::children::WaveStop;
        Self {
            answer: None,
            torn_down: !matches!(
                stop,
                WaveStop::Boundary(StopCause::Budget | StopCause::Capacity | StopCause::ActiveTime)
            ),
            failure: WorkerCause::ToolFailed,
            step_stop: None,
        }
    }
}
struct WorkerSpawn<'a> {
    id: AgentRunId,
    runtime: &'a ToolRuntime,
    assignment: &'a EditorAssignment,
}
struct EditorFacts {
    answered: bool,
    unknown: bool,
    changed: bool,
    stopped: bool,
    cancelled: bool,
    actor_hook_stop: bool,
    step_stop: Option<StopCause>,
}
#[must_use]
#[derive(Debug, Eq, PartialEq)]
enum EditorTerminal {
    Unknown,
    Known {
        agent: AgentDisposition,
        outcome: WorkerOutcome,
    },
}
fn editor_terminal(failure: WorkerCause, facts: &EditorFacts) -> EditorTerminal {
    if facts.unknown {
        return EditorTerminal::Unknown;
    }
    let cause = if facts.actor_hook_stop {
        WorkerCause::StoppedByHook
    } else if facts.cancelled {
        WorkerCause::Cancelled
    } else if facts.stopped {
        WorkerCause::Budget
    } else if facts.answered {
        WorkerCause::Finished
    } else {
        facts.step_stop.map_or(failure, step_cause)
    };
    let (agent, disposition) = match cause {
        WorkerCause::Finished => (
            AgentDisposition::Finished,
            if facts.changed {
                WorkerDisposition::Ready
            } else {
                WorkerDisposition::NoChange
            },
        ),
        WorkerCause::StoppedByHook => (AgentDisposition::StoppedByHook, WorkerDisposition::Failed),
        WorkerCause::Cancelled => (AgentDisposition::Cancelled, WorkerDisposition::Cancelled),
        WorkerCause::InvalidOutcome
        | WorkerCause::ToolFailed
        | WorkerCause::TurnLimit
        | WorkerCause::Budget
        | WorkerCause::SnapshotDrift
        | WorkerCause::CriterionUnverified
        | WorkerCause::PrivateUncertain
        | WorkerCause::Lost => (AgentDisposition::Failed, WorkerDisposition::Failed),
    };
    EditorTerminal::Known {
        agent,
        outcome: WorkerOutcome { disposition, cause },
    }
}

fn step_cause(stop: StopCause) -> WorkerCause {
    match stop {
        StopCause::Cancelled | StopCause::Paused | StopCause::HookStop => WorkerCause::Cancelled,
        StopCause::ActiveTime | StopCause::Budget => WorkerCause::Budget,
        StopCause::InvalidCommandResult => WorkerCause::InvalidOutcome,
        // Context overflow and a delegation-deadline Limit keep `tool_failed` (execution spec).
        StopCause::Uncertain
        | StopCause::Capacity
        | StopCause::ContextFull
        | StopCause::DelegationDeadline
        | StopCause::Refused(_)
        | StopCause::ToolRefused(_)
        | StopCause::ToolStopped => WorkerCause::ToolFailed,
    }
}

impl RunLoop<'_> {
    pub(super) fn editor_delegation(
        &self,
        children: &[ChildAssignment],
        agent: Option<&str>,
    ) -> Result<Result<(), crate::session::ProposalRefusalCause>, EngineError> {
        use crate::session::ProposalRefusalCause::*;
        if !crate::provider::valid_assignments(children, agent) {
            return Ok(Err(DelegateInvalid));
        }
        let Some(runtime) = self.controls.tools else {
            return Ok(Err(EditorGrant));
        };
        let valid = self.ledger().view(|view| {
            let run = view.runs.last().expect("active Run");
            for child in children {
                if let Some(a) = &child.editor {
                    crate::session::validate_editor_request(view, run, a, None)?;
                }
            }
            Ok(())
        })?;
        if valid.is_err() {
            return Ok(valid);
        }
        let (attempts, wave) = self.ledger().view(|v| {
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
        })?;
        let valid = self.controls.goal_phase == GoalPhase::Implementation
            && agent
                .is_none_or(|name| self.definitions.get().is_some_and(|r| r.contains_key(name)))
            && attempts + children.len() <= usize::from(self.controls.policy.max_children())
            && self.wave_settled()?;
        if !valid {
            return Ok(Err(DelegateInvalid));
        }
        for (index, child) in children.iter().enumerate() {
            let valid = match &child.editor {
                Some(a) => self.valid_live_editor(a, &children[..index], wave, runtime)?,
                None => self.valid_research_assignment(child, agent),
            };
            if !valid {
                return Ok(Err(DelegateInvalid));
            }
        }
        Ok(Ok(()))
    }

    fn valid_live_editor(
        &self,
        a: &EditorAssignment,
        previous: &[ChildAssignment],
        wave: usize,
        runtime: &ToolRuntime,
    ) -> Result<bool, EngineError> {
        if usize::from(a.wave) != wave {
            return Ok(false);
        }
        if let Some(id) = a.supersedes
            && !self.ledger().view(|v| {
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
            })?
        {
            return Ok(false);
        }
        Ok(!previous.iter().filter_map(|c| c.editor.as_ref()).any(|b| {
            a.lease
                .iter()
                .any(|p| b.lease.iter().any(|q| overlaps(p, q)))
        }) && match &a.criterion {
            crate::session::GoalSuccessCriterion::ToolCallSucceeded { call } => {
                runtime.allows(call)
            }
            crate::session::GoalSuccessCriterion::WorkspaceFileContent { .. }
            | crate::session::GoalSuccessCriterion::WorkspaceFileDigest { .. } => true,
        } && self.editor_catalog_fits(runtime, a)?
            && self.editor_grant(a)?.valid())
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
        let parent = runtime
            .worker_policy()
            .ok_or(EngineError::InvalidRequest)?
            .parent_grant;
        let def = self
            .definitions
            .get()
            .and_then(|d| d.get(&a.definition))
            .ok_or(EngineError::InvalidRequest)?;
        let plan = self
            .ledger()
            .plan_contract()?
            .ok_or(EngineError::InvalidRequest)?;
        let mut grant =
            parent.narrowed_for_editor(a, &plan.commands, |tool| def.allows_worker_tool(tool));
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

    pub(super) async fn admit_worker_action(&self) -> Result<(), EngineError> {
        let action = Action::Worker;
        self.journal
            .admit(
                self.session_id,
                action.max_events_for_admitted(&self.routing.providers)
                    + self.ledger().verdicts(action),
                action.tool_observations(),
            )
            .await?;
        Ok(())
    }
    pub(super) fn begin_worker<'a>(
        &'a self,
        root: &'a mut ProviderRequest,
        assignment: Box<EditorAssignment>,
    ) -> BoxedStep<'a, WorkerAdmission> {
        Box::pin(self.begin_worker_inner(root, assignment))
    }

    // A catalog over its bound refuses before a delegation or lease is committed.
    fn editor_catalog_fits(
        &self,
        runtime: &ToolRuntime,
        assignment: &EditorAssignment,
    ) -> Result<bool, EngineError> {
        Ok(runtime
            .editor_context(&self.editor_grant(assignment)?, 16)
            .is_ok())
    }

    async fn begin_worker_inner(
        &self,
        _root: &mut ProviderRequest,
        assignment: Box<EditorAssignment>,
    ) -> Result<WorkerAdmission, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        self.ledger()
            .view(|v| {
                crate::session::validate_editor_request(
                    v,
                    v.runs.last().expect("active Run"),
                    &assignment,
                    None,
                )
            })?
            .map_err(|_| EngineError::InvalidHistory)?;
        if !self.ledger().admits(Action::Delegation {
            children: 1,
            research: true,
        }) || self.observe_worker_stop().await?
        {
            return Ok(WorkerAdmission::Exhausted);
        }
        self.admit_worker_action().await?;
        let spawn = WorkerSpawn {
            id: AgentRunId::new(),
            runtime,
            assignment: &assignment,
        };
        let grant = self.editor_grant(&assignment)?;
        self.spawn_worker(&spawn, &grant).await?;
        Ok(WorkerAdmission::Admitted(Box::new(
            self.worker_child(&spawn, &grant)?,
        )))
    }
    async fn spawn_worker(
        &self,
        spawn: &WorkerSpawn<'_>,
        grant: &WorkerGrant,
    ) -> Result<(), EngineError> {
        let WorkerSpawn {
            id,
            assignment,
            runtime,
        } = *spawn;
        self.append(Event::AgentSpawned {
            run_id: self.run_id,
            agent_run_id: id,
            role: AgentRole::Child,
            ordinal: self
                .ledger()
                .view(|v| v.runs.last().expect("Run").agents.len())?
                .try_into()
                .map_err(|_| EngineError::CoordinatorFailed)?,
            objective: Some(assignment.mission.clone()),
            definition: Some(assignment.definition.clone()),
            profile: None,
            grant: Some(ChildGrant::EditOnly {
                unguarded: crate::tools::UnguardedGrant::Denied,
            }),
            assignment: Some(WorkerAssignment {
                request: assignment.clone(),
                grant: grant.clone(),
                policy_digest: runtime.receipt().config_digest,
                permissions_digest: runtime.receipt().permissions_digest,
            }),
        })
        .await
    }
    fn worker_child(
        &self,
        spawn: &WorkerSpawn<'_>,
        grant: &WorkerGrant,
    ) -> Result<ChildWork, EngineError> {
        let WorkerSpawn {
            id,
            assignment,
            runtime,
            ..
        } = *spawn;
        let remaining = self.ledger().remaining_calls().map_or(16, |n| {
            usize::try_from(n.saturating_sub(3) / 2)
                .unwrap_or(16)
                .min(16)
        });
        let source_context = if let Some(source) = assignment.resumes {
            Some(
                self.ledger()
                    .view(|v| {
                        let (run, agent) = v.worker_resume_source(self.goal_id?, source)?;
                        let mut report = agent.result.clone().unwrap_or_default();
                        report.truncate(crate::tools::types::truncate_utf8(&report, 2048).len());
                        let history = run
                            .worker_history()
                            .into_iter()
                            .find(|w| w.assignment_id == source);
                        let disposition = history.as_ref().map_or_else(
                            || format!("Child {}: {}", agent.id, agent.status),
                            |w| w.description(),
                        );
                        let changes = history
                            .map(|worker| worker.changed_paths)
                            .unwrap_or_default();
                        Some(crate::provider::ChildResult {
                            objective: agent.objective.clone().unwrap_or_default(),
                            summary: agent.summary.clone().unwrap_or_else(|| disposition.clone()),
                            result: format!("{disposition}\n{changes}\n{report}"),
                        })
                    })?
                    .ok_or(EngineError::InvalidHistory)?,
            )
        } else {
            None
        };
        Ok(ChildWork {
            agent_run_id: id,
            objective: assignment.mission.clone(),
            definition: self
                .definitions
                .get()
                .and_then(|d| d.get(&assignment.definition))
                .cloned(),
            kind: ChildKind::Editor,
            source_context,
            turns_remaining: assignment.max_turns,
            steering_notes: Vec::new(),
            stop_hook_feedback: Vec::new(),
            tools: Some(runtime.editor_context(grant, remaining)?),
            tool_calls_remaining: remaining,
            active_deadline: None,
        })
    }

    pub(super) fn complete_worker<'a>(
        &'a self,
        child: &'a mut ChildWork,
        end: WorkerEnd,
    ) -> BoxedStep<'a, WorkerCompletion> {
        Box::pin(self.complete_worker_inner(child, end))
    }
    pub(super) fn criterion_unmet(&self, id: AgentRunId) -> Result<bool, EngineError> {
        self.ledger()
            .view(|v| {
                crate::session::criterion_unmet_in_view(v.runs.last().expect("active Run"), id)
            })?
            .map_err(|_| EngineError::InvalidHistory)
    }

    async fn complete_worker_inner(
        &self,
        child: &mut ChildWork,
        end: WorkerEnd,
    ) -> Result<WorkerCompletion, EngineError> {
        let id = child.agent_run_id;
        let stopped = self.observe_worker_stop().await?;
        let (unknown, changed) = self.ledger().view(|v| {
            let a = v
                .runs
                .last()
                .expect("Run")
                .agents
                .iter()
                .find(|a| a.id == id)
                .expect("editor");
            (
                a.tools
                    .iter()
                    .any(crate::session::ToolView::outcome_unknown),
                a.tools.iter().any(|t| {
                    t.intent.call.mutates()
                        && t.observation.as_ref().is_some_and(|o| {
                            o.disposition == ToolDisposition::Succeeded && o.attested_for(&t.intent)
                        })
                }),
            )
        })?;
        let terminal = editor_terminal(
            end.failure,
            &EditorFacts {
                answered: end.answer.is_some(),
                unknown,
                changed,
                stopped,
                cancelled: end.torn_down
                    || self.controls.cancellation.is_cancelled()
                    || self.pause_requested()
                    || self.hook_requested_stop()?,
                actor_hook_stop: self.actor_hook_requested_stop(id)?,
                step_stop: end.step_stop,
            },
        );
        let EditorTerminal::Known {
            agent: disposition,
            outcome,
        } = terminal
        else {
            return Ok(WorkerCompletion::Uncertain);
        };
        let success = disposition == AgentDisposition::Finished;
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id: id,
            disposition,
            summary: end
                .answer
                .as_ref()
                .filter(|_| success)
                .map(|a| crate::session::answer_summary(&a.text)),
            result: end.answer.filter(|_| success).map(|a| a.text),
            worker: Some(outcome),
        })
        .await?;
        let cause_sequence = self.ledger().view(|v| v.last_sequence)?;
        self.append(Event::WorkerLeaseReleased {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            record: WorkerLeaseRelease {
                assignment_id: id,
                cause_sequence,
            },
        })
        .await?;
        Ok(WorkerCompletion::Settled)
    }
    pub(super) async fn record_capacity_stop(&self) -> Result<(), EngineError> {
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

    pub(super) async fn observe_worker_stop(&self) -> Result<bool, EngineError> {
        if self
            .ledger()
            .view(|v| v.runs.last().is_some_and(|r| r.workers.stop.is_some()))?
        {
            return Ok(true);
        }
        if self.ledger().view(|view| {
            view.runs
                .last()
                .and_then(|run| run.primary())
                .is_some_and(|actor| actor.tool_refusal.is_some())
        })? {
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
            let _ = self.admit_worker_action().await;
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
                    evidence.drain(..evidence.len().saturating_sub(MAX_WORKER_EVIDENCE));
                    if evidence.is_empty() {
                        return None;
                    }
                    let passed = verification_passed(&contract, run, released, &evidence).ok()?;
                    Some(WorkerVerification {
                        assignment_id: a.id,
                        criterion_digest: crate::validate::digest(
                            &serde_json::to_vec(&contract.success_criterion).expect("criterion"),
                        ),
                        passed,
                        evidence,
                    })
                })
                .collect::<Vec<_>>()
        })?;
        if !records.is_empty() && self.admit_worker_action().await.is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wave_stop_worker_ends_preserve_budget_and_cancellation_causes() {
        use super::super::children::WaveStop;
        for (cause, stopped, expected_agent, expected_cause) in [
            (
                StopCause::Budget,
                true,
                AgentDisposition::Failed,
                WorkerCause::Budget,
            ),
            (
                StopCause::ActiveTime,
                true,
                AgentDisposition::Failed,
                WorkerCause::Budget,
            ),
            (
                StopCause::Capacity,
                true,
                AgentDisposition::Failed,
                WorkerCause::Budget,
            ),
            (
                StopCause::DelegationDeadline,
                false,
                AgentDisposition::Cancelled,
                WorkerCause::Cancelled,
            ),
            (
                StopCause::Paused,
                false,
                AgentDisposition::Cancelled,
                WorkerCause::Cancelled,
            ),
            (
                StopCause::Cancelled,
                false,
                AgentDisposition::Cancelled,
                WorkerCause::Cancelled,
            ),
        ] {
            let end = WorkerEnd::stopped(WaveStop::Boundary(cause));
            let terminal = editor_terminal(
                end.failure,
                &EditorFacts {
                    answered: false,
                    unknown: false,
                    changed: true,
                    stopped,
                    cancelled: end.torn_down,
                    actor_hook_stop: false,
                    step_stop: end.step_stop,
                },
            );
            let EditorTerminal::Known { agent, outcome } = terminal else {
                panic!("known wave stop: {cause:?}");
            };
            assert_eq!(agent, expected_agent, "{cause:?}");
            assert_eq!(outcome.cause, expected_cause, "{cause:?}");
            assert!(outcome.valid());
        }
    }

    #[test]
    fn terminal_facts_preserve_uncertainty_hook_cancel_budget_and_success_precedence() {
        for (unknown, hook, cancelled, stopped, answered, expected) in [
            (true, true, true, true, true, None),
            (
                false,
                true,
                true,
                true,
                true,
                Some((AgentDisposition::StoppedByHook, WorkerCause::StoppedByHook)),
            ),
            (
                false,
                false,
                true,
                true,
                true,
                Some((AgentDisposition::Cancelled, WorkerCause::Cancelled)),
            ),
            (
                false,
                false,
                false,
                true,
                true,
                Some((AgentDisposition::Failed, WorkerCause::Budget)),
            ),
            (
                false,
                false,
                false,
                false,
                true,
                Some((AgentDisposition::Finished, WorkerCause::Finished)),
            ),
            (
                false,
                false,
                false,
                false,
                false,
                Some((AgentDisposition::Failed, WorkerCause::ToolFailed)),
            ),
        ] {
            let terminal = editor_terminal(
                WorkerCause::ToolFailed,
                &EditorFacts {
                    answered,
                    unknown,
                    changed: true,
                    stopped,
                    cancelled,
                    actor_hook_stop: hook,
                    step_stop: None,
                },
            );
            match (terminal, expected) {
                (EditorTerminal::Unknown, None) => (),
                (
                    EditorTerminal::Known { agent, outcome },
                    Some((expected_agent, expected_cause)),
                ) => {
                    assert_eq!(agent, expected_agent);
                    assert_eq!(outcome.cause, expected_cause);
                    assert!(outcome.valid());
                }
                pair => panic!("unexpected terminal decision: {pair:?}"),
            }
        }
    }
}
