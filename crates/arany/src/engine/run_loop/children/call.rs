use super::*;

impl RunLoop<'_> {
    pub(super) fn hook_facts(&self) -> Result<HookFacts, EngineError> {
        Ok(HookFacts {
            uncertain: self.uncertain_hook()?,
            stop_requested: self.hook_requested_stop()?,
            settled: self.ledger().view(|view| {
                view.runs
                    .last()
                    .is_some_and(crate::session::RunView::hooks_settled)
            })?,
        })
    }

    pub(super) fn child_facts(
        &self,
        child: &ChildWork,
        deadline: Instant,
    ) -> Result<ChildFacts, EngineError> {
        Ok(ChildFacts {
            has_tools: child.tools.is_some(),
            refusal_limit_reached: self.ledger().view(|view| {
                view.runs.last().is_some_and(|run| {
                    run.agents.iter().any(|agent| {
                        agent.id == child.agent_run_id && agent.refusal_limit_reached()
                    })
                })
            })?,
            turns_left: child.turns_remaining > 0,
            tool_calls_left: child.tool_calls_remaining > 0,
            before_deadline: Instant::now() < deadline,
            cancelled: self.controls.cancellation.is_cancelled(),
            paused: self.pause_requested(),
            tokens_open: self.ledger().can_start_provider_call(),
            calls_left: self.ledger().remaining_calls() != Some(0),
            hook: self.hook_facts()?,
            actor_hook_stop: self.actor_hook_requested_stop(child.agent_run_id)?,
        })
    }

    pub(super) async fn child_steering_note(
        &self,
        child: &mut ChildWork,
        request: &mut ProviderRequest,
    ) -> Result<(), EngineError> {
        if let Some(steering) = &self.controls.steering {
            let target = crate::engine::ChildSteeringTarget {
                session_id: self.session_id,
                run_id: self.run_id,
                agent_run_id: child.agent_run_id,
            };
            if let Some(text) = steering.take_child_note(target) {
                self.append(Event::AgentSteeringNoteRecorded {
                    goal_id: self.goal()?,
                    run_id: self.run_id,
                    agent_run_id: child.agent_run_id,
                    text: text.clone(),
                })
                .await?;
                super::super::place_steering_note(&mut request.input, text);
                child.steering_notes = request.input.steering_notes.clone();
                child.tools = request.input.tools.clone();
                child.stop_hook_feedback = request.input.stop_hook_feedback.clone();
            }
        }
        Ok(())
    }

    pub(super) fn child_request(
        &self,
        child: &mut ChildWork,
        root_request: &ProviderRequest,
    ) -> Result<ProviderRequest, EngineError> {
        let mut context: Vec<String> = child
            .definition
            .iter()
            .map(|definition| format!("Agent definition:\n{}", definition.body))
            .collect();
        if child.kind.is_editor() {
            let assignment = self.proposed_editor(child.agent_run_id)?;
            context.push(format!(
                "Worker assignment: {}",
                serde_json::to_string(&assignment).map_err(|_| EngineError::InvalidRequest)?,
            ));
            if let Some(s) = &child.source_context {
                context.push(format!(
                    "Historical source (bounded; no effects or authority resume): {}",
                    serde_json::json!({"objective":s.objective,"summary":s.summary,"result":s.result})
                ));
            }
        }
        let context = (!context.is_empty()).then(|| context.join("\n\n"));
        child.turns_remaining = child.turns_remaining.saturating_sub(1);
        if child.turns_remaining == 0 {
            child.tool_calls_remaining = 0;
        }
        if let Some(tools) = child.tools.as_mut() {
            let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
            let room = runtime
                .result_capacity(
                    tools.rendered_len()
                        + root_request
                            .input
                            .workspace_guidance
                            .iter()
                            .map(|guidance| {
                                crate::provider::instructions_text(&guidance.path, &guidance.text)
                                    .len()
                            })
                            .sum::<usize>()
                        + context.as_ref().map_or(0, String::len)
                        + CHILD_INTENT_RESERVE_BYTES
                        + if self.controls.steering.is_some() {
                            crate::session::MAX_STEERING_CONTEXT_BYTES
                        } else {
                            0
                        },
                    crate::tools::ToolRuntime::step_context_reserve(
                        false,
                        Self::maximum_feedback_reserve(runtime),
                    ),
                )
                .is_some_and(|capacity| capacity >= MIN_CHILD_RESULT_BYTES);
            if !room {
                child.tool_calls_remaining = 0;
            }
            // Keep the request head frozen; later calls carry their remaining budget in the newest step.
            match tools.steps.last_mut() {
                Some(step) => step.calls_remaining = Some(child.tool_calls_remaining),
                None => {
                    if child.kind.is_editor() {
                        let mut catalog: serde_json::Value = serde_json::from_str(tools.catalog())
                            .map_err(|_| EngineError::InvalidRequest)?;
                        catalog["tool_calls_remaining"] =
                            serde_json::json!(child.tool_calls_remaining);
                        tools.set_catalog(&catalog);
                    } else {
                        let refreshed = runtime.research_context(child.tool_calls_remaining)?;
                        tools.set_catalog(
                            &serde_json::from_str(refreshed.catalog())
                                .map_err(|_| EngineError::InvalidRequest)?,
                        );
                    }
                    // Only the first call rebuilds the catalog; later calls keep this restriction.
                    if !child.kind.is_editor()
                        && let Some(definition) = &child.definition
                    {
                        restrict_catalog(tools, definition)?;
                    }
                }
            }
        }
        let researcher = child.tools.is_some();
        let root = &root_request.input;
        let request = ProviderRequest {
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id: child.agent_run_id,
            phase: AgentPhase::ChildWork,
            collaboration: CollaborationPolicy::Single,
            model: String::new(),
            max_output_tokens: if child.kind.is_editor() {
                MAX_OUTPUT_TOKEN_CAP
            } else {
                DEFAULT_OUTPUT_TOKEN_CAP
            },
            input: RunInput {
                images: root
                    .images
                    .iter()
                    .filter(|value| value.origin == crate::provider::ImageOrigin::Objective)
                    .cloned()
                    .collect(),
                goal_budget: if self.ledger().autonomous()
                    || self.ledger().plan_contract()?.is_some()
                {
                    Some(self.ledger().effective_budget()?)
                } else {
                    None
                },
                goal_plan_contract: if researcher {
                    self.ledger().plan_contract()?
                } else {
                    None
                },
                steering_notes: child.steering_notes.clone(),
                stop_hook_feedback: child.stop_hook_feedback.clone(),
                workspace_guidance: root.workspace_guidance.clone(),
                assignment_context: context,
                includes: root.includes.clone(),
                tools: child.tools.clone(),
                prompts: root.prompts.clone(),
                ..RunInput::new(
                    if child.kind.is_editor() {
                        PromptRole::Editor
                    } else if researcher {
                        PromptRole::Researcher
                    } else {
                        PromptRole::ReadOnly
                    },
                    child.objective.clone(),
                )
            },
        };
        Ok(request)
    }

    pub(super) async fn admit_worker_call(
        &self,
        queue: &mut ChildQueue,
    ) -> Result<bool, EngineError> {
        match self.admit_worker_action().await {
            Ok(()) => Ok(true),
            Err(
                error @ EngineError::Store(
                    crate::store::StoreError::ReplayLimit | crate::store::StoreError::StorageFull,
                ),
            ) => {
                self.record_capacity_stop().await?;
                queue.stop.get_or_insert(StopCause::Capacity);
                queue.capacity_error = Some(error);
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn valid_child_tools(
        &self,
        child: &ChildWork,
        requests: &[ToolRequest],
        contract: Option<&crate::session::GoalPlanContract>,
        settings: crate::tools::ToolResourceSettings,
    ) -> bool {
        !requests.is_empty()
            && requests.len() <= usize::from(crate::session::MAX_STEP_TOOL_CALLS)
            && (!child.kind.is_editor() || requests.len() == 1)
            && !requests.iter().enumerate().any(|(index, request)| {
                requests[..index].iter().any(|prior| prior.id == request.id)
            })
            && requests.len() <= child.tool_calls_remaining
            && (requests.len() == 1
                || requests.iter().all(|request| {
                    contract.is_none_or(|contract| contract.allows_call(&request.call))
                }))
            && child.tools.is_some()
            && child.turns_remaining > 0
            && requests.iter().all(|request| {
                request.call.valid()
                    && crate::session::requested_command_resources(
                        &request.call,
                        contract,
                        settings,
                    )
                    .is_ok()
                    && self.model_may_propose(&request.call)
                    && (if child.kind.is_editor() {
                        self.proposed_editor(child.agent_run_id)
                            .and_then(|a| self.editor_grant(&a))
                            .is_ok_and(|g| g.allows(&request.call))
                            && self
                                .ledger()
                                .view(|view| {
                                    crate::session::digest_known(
                                        view,
                                        self.run_id,
                                        child.agent_run_id,
                                        &request.call,
                                    )
                                })
                                .is_ok_and(|known| known)
                    } else {
                        request.call.is_child_research()
                            && (!request.call.is_web()
                                || self.controls.tools.is_some_and(|runtime| {
                                    runtime.allows_research_web(&request.call)
                                }))
                            && !self
                                .controls
                                .tools
                                .is_some_and(|runtime| runtime.is_remote_mcp(&request.call))
                            && child
                                .definition
                                .as_ref()
                                .is_none_or(|definition| definition.allows(&request.call))
                    })
            })
    }
}
