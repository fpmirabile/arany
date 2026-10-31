use super::*;
use crate::engine::goal::goal_context_metadata_reservation;

impl Engine {
    fn worker_policy(
        &self,
        runtime: Option<&crate::tools::ToolRuntime>,
        policy: CollaborationPolicy,
        concurrency: u8,
    ) -> Option<crate::session::worker::WorkerPolicy> {
        runtime
            .and_then(crate::tools::ToolRuntime::worker_policy)
            .map(|mut p| {
                p.concurrency = self
                    .config
                    .worker_concurrency
                    .unwrap_or(4)
                    .min(policy.max_children().max(1))
                    .min(concurrency);
                p
            })
    }

    pub(super) async fn prepare_turn(
        &self,
        submission: &AdmittedSubmission,
        request: &RunRequest,
        identity: &crate::provider::ProviderIdentity,
        admitted: &AdmittedTurn,
    ) -> Result<PreparedTurn, EngineError> {
        let (workspace_identity, history) = (admitted.workspace_identity, &admitted.history);
        let (goal, objective, budget) = (&admitted.goal, &admitted.objective, admitted.budget);
        if let Some(permissions) = submission.permissions() {
            permissions.verify(&request.workspace)?;
        }
        let mut tools = self.turn_tools(submission, request, admitted).await?;
        let tool_context = tools
            .as_ref()
            .map(crate::tools::ToolRuntime::context)
            .transpose()?;
        let (prompts, inputs) = self.turn_inputs(request, goal, tools.as_ref())?;
        if goal.entry == GoalEntry::New {
            goal_started_fits(goal, objective, workspace_identity)?;
        }
        let root_identity = inputs.root_identity()?;
        if root_identity != workspace_identity {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        if let Some(view) = &history {
            ensure_session_workspace(view, root_identity)?;
        }
        let context_started = SystemTime::now();
        let (context, context_window_tokens) =
            compile_turn_context(submission, request, admitted, &inputs, tools.as_ref())?;
        if let Some(runtime) = &mut tools {
            runtime.set_context_limit(context.tool_context_budget_bytes)?;
        }
        if !context.images.is_empty()
            && submission.providers.agent_chains().iter().any(|chain| {
                chain
                    .iter()
                    .any(|provider| provider.identity().custom_provenance().is_some())
            })
        {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let context_ended = SystemTime::now();
        let provider_concurrency = if budget.max_tokens.is_some() {
            1
        } else {
            submission.providers.agent_concurrency()
        };
        let config = self.run_config(
            submission,
            request,
            identity,
            admitted.session_id,
            ConfigSources {
                tools: tools.as_ref(),
                inputs: &inputs,
                prompts: &prompts,
                context: &context,
                context_window_tokens,
                provider_concurrency,
                root_identity,
            },
        )?;
        Ok(PreparedTurn {
            tools,
            tool_context,
            inputs,
            images: context.images,
            history: context.history,
            summary: context.summary,
            prompts,
            config,
            context_window_tokens,
            context_started,
            context_ended,
            provider_concurrency,
        })
    }

    async fn turn_tools(
        &self,
        submission: &AdmittedSubmission,
        request: &RunRequest,
        admitted: &AdmittedTurn,
    ) -> Result<Option<crate::tools::ToolRuntime>, EngineError> {
        let (workspace_identity, goal) = (admitted.workspace_identity, &admitted.goal);
        let mut tools = submission
            .tools
            .as_ref()
            .map(|tools| {
                let permissions = if goal.entry == GoalEntry::Continue {
                    let snapshot = crate::tools::load_permissions(&self.state)?;
                    if self.host_settings_match(snapshot.as_ref().map(|snapshot| snapshot.digest)) {
                        snapshot
                    } else {
                        tools.permissions.clone()
                    }
                } else {
                    tools.permissions.clone()
                };
                crate::tools::ToolRuntime::admit(
                    tools.config.clone(),
                    permissions,
                    tools.unguarded_rules.clone(),
                    &request.workspace,
                    &self.state,
                    tools.guard_executable.clone(),
                )
            })
            .transpose()?;
        if let Some(runtime) = &mut tools {
            runtime.host_settings_admitted =
                self.host_settings_match(runtime.receipt().permissions_digest);
            // A configured Run also honors the hooks answer saved with the folder's trust entry.
            let accepted = match (&submission.grant, submission.permissions()) {
                (_, Some(permissions)) => permissions.accepted_git_hooks(),
                (ToolGrant::Configured, None) => {
                    crate::tools::WorkspacePermissions::saved_git_hooks(&request.workspace)
                }
                _ => None,
            };
            runtime
                .admit_git_hooks(accepted, submission.approvals.is_some())
                .await;
            runtime.repository = match (&submission.grant, submission.permissions()) {
                (_, Some(permissions)) => permissions.repository().map(str::to_owned),
                (ToolGrant::Configured, None) => {
                    crate::tools::WorkspacePermissions::saved_repository(&request.workspace)
                }
                _ => None,
            };
        }
        if let Some(runtime) = &tools
            && runtime.workspace_identity() != workspace_identity
        {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        if let (Some(permissions), Some(runtime)) = (submission.permissions(), &tools)
            && !permissions.matches_identity(runtime.workspace_identity())
        {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        if request.skill.as_ref().is_some_and(|name| {
            !crate::validate::valid_name(name)
                || tools
                    .as_ref()
                    .is_none_or(|runtime| !runtime.allows_skill(name))
        }) {
            return Err(EngineError::InvalidRequest);
        }
        Ok(tools)
    }

    fn turn_inputs(
        &self,
        request: &RunRequest,
        goal: &GoalAdmission,
        tools: Option<&crate::tools::ToolRuntime>,
    ) -> Result<(crate::provider::EffectivePrompts, WorkspaceInputs), EngineError> {
        let include_paths = goal
            .include_paths
            .as_ref()
            .ok_or(EngineError::GoalIncludesUnavailable)?
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let prompts = crate::provider::EffectivePrompts::load(&self.state)?;
        let inputs = match tools {
            Some(runtime) => WorkspaceInputs::load_pinned(
                &request.workspace,
                &self.state,
                request.launch_directory.as_deref(),
                &include_paths,
                Some(runtime.workspace_identity()),
            )?,
            None => WorkspaceInputs::load(
                &request.workspace,
                &self.state,
                request.launch_directory.as_deref(),
                &include_paths,
            )?,
        };
        Ok((prompts, inputs))
    }

    fn run_config(
        &self,
        submission: &AdmittedSubmission,
        request: &RunRequest,
        identity: &crate::provider::ProviderIdentity,
        session_id: SessionId,
        sources: ConfigSources<'_>,
    ) -> Result<RunConfig, EngineError> {
        let ConfigSources {
            tools,
            inputs,
            prompts,
            context,
            context_window_tokens,
            provider_concurrency,
            root_identity,
        } = sources;
        let routes = submission.providers.identities();
        let inherited = routes
            .values()
            .iter()
            .all(|chain| chain.len() == 1 && chain[0] == *identity);
        let config = RunConfig {
            supervisor: supervisor_policy(session_id, tools.is_some()),
            worker_policy: self.worker_policy(tools, request.policy, provider_concurrency),
            agent_definitions: tools.and_then(|tools| tools.definitions.pin()),
            roster: submission
                .providers
                .roster
                .as_ref()
                .map(|roster| Box::new(roster.snapshot.clone())),
            providers: (!inherited).then_some(routes),
            provider: crate::provider::ProviderRecord::parse(&identity.profile)
                .ok_or(EngineError::InvalidRequest)?,
            model: identity.model.clone(),
            effort: identity.effort,
            custom_profile_provenance: identity.custom_provenance().cloned(),
            saved_api_account_id: identity.saved_api_account_id(),
            chatgpt_provenance: identity.chatgpt_provenance().cloned(),
            output_token_bound: identity.limits.output_token_bound,
            context_window_tokens: submission.providers.context_window_tokens(),
            effective_context_window_tokens: context_window_tokens,
            policy: request.policy,
            output_token_cap: identity.limits.max_output_tokens,
            provider_concurrency,
            workspace_device: root_identity.device,
            workspace_inode: root_identity.inode,
            user_instruction_digest: inputs
                .user_instructions
                .as_ref()
                .map(|snapshot| snapshot.digest),
            instruction_sources: instruction_sources(inputs),
            prompts: prompts.pins(),
            launch_directory: inputs.launch_directory.clone(),
            include_sources: include_sources(inputs),
            history_run_ids: context.history_run_ids.clone(),
            excluded_history_runs: context.excluded_history_runs,
            context_usage: Some(context.context_usage.clone()),
            compaction_event_sequence: context.compaction_event_sequence,
            compaction_content_digest: context.compaction_content_digest,
            tool_policy: tools.map(|tools| {
                let mut receipt = tools.receipt();
                receipt.host_epoch = Some(self.host_epoch(session_id));
                Box::new(receipt)
            }),
        };
        config.validate().map_err(|_| EngineError::InvalidRequest)?;
        if !crate::session::run_started_fits(&config) {
            return Err(EngineError::InputTooLarge);
        }
        Ok(config)
    }
}

fn goal_started_fits(
    goal: &GoalAdmission,
    objective: &str,
    workspace_identity: WorkspaceIdentity,
) -> Result<(), EngineError> {
    let event = goal.started_event(objective, workspace_identity);
    if event
        .payload()
        .map_err(|_| EngineError::InvalidRequest)?
        .len()
        > crate::store::MAX_GOAL_STARTED_BYTES
    {
        return Err(EngineError::GoalInputTooLarge);
    }
    Ok(())
}

fn instruction_sources(inputs: &WorkspaceInputs) -> Vec<InstructionSource> {
    inputs
        .instructions
        .iter()
        .map(|snapshot| InstructionSource {
            directory: snapshot.directory().to_owned(),
            file: snapshot.instruction_file(),
            digest: snapshot.digest,
        })
        .collect()
}

fn include_sources(inputs: &WorkspaceInputs) -> Vec<crate::session::IncludeSource> {
    inputs
        .includes
        .iter()
        .filter(|snapshot| !matches!(snapshot.fit, crate::session::IncludeFit::Reference { .. }))
        .map(|snapshot| crate::session::IncludeSource {
            path: snapshot.path.clone(),
            digest: snapshot.digest,
        })
        .collect()
}

struct ConfigSources<'a> {
    tools: Option<&'a crate::tools::ToolRuntime>,
    inputs: &'a WorkspaceInputs,
    prompts: &'a crate::provider::EffectivePrompts,
    context: &'a CompiledContext,
    context_window_tokens: Option<u32>,
    provider_concurrency: u8,
    root_identity: WorkspaceIdentity,
}

fn compile_turn_context(
    submission: &AdmittedSubmission,
    request: &RunRequest,
    admitted: &AdmittedTurn,
    inputs: &WorkspaceInputs,
    tools: Option<&crate::tools::ToolRuntime>,
) -> Result<(CompiledContext, Option<u32>), EngineError> {
    let (goal, history) = (&admitted.goal, &admitted.history);
    let feedback_reserved = usize::from(submission.steering.is_some()) * MAX_STEERING_CONTEXT_BYTES
        + tools
            .as_ref()
            .map_or(0, |tools| tools.stop_feedback_reserve());
    let goal_metadata_reserved = goal_context_metadata_reservation(
        goal.plan_contract
            .as_ref()
            .map_or(&goal.budget, |plan| &plan.budget),
        goal.plan_contract.as_ref(),
    )?
    .checked_add(feedback_reserved)
    .ok_or(EngineError::ContextTooLarge)?;
    let model_context_window_tokens = submission.providers.context_window_tokens();
    let context_window_tokens = request
        .goal
        .context_window
        .resolve(model_context_window_tokens);
    let context = CompiledContext::compile(
        history.as_ref(),
        &admitted.objective,
        &request.images,
        inputs,
        request.policy,
        tools.map_or(0, |_| {
            crate::tools::types::MAX_TOOL_CATALOG_BYTES
                + crate::tools::ToolRuntime::continuation_reserve(
                    submission
                        .providers
                        .primary
                        .iter()
                        .map(|provider| {
                            crate::provider::max_turn_bytes(
                                provider.identity().limits.max_output_tokens,
                            )
                        })
                        .max()
                        .unwrap_or(0),
                    feedback_reserved,
                )
        }),
        goal_metadata_reserved,
        context_window_tokens,
    )
    .map_err(|_| EngineError::ContextTooLarge)?;
    Ok((context, context_window_tokens))
}

pub(super) fn root_request(
    scope: CallScope,
    policy: crate::session::CollaborationPolicy,
    identity: &crate::provider::ProviderIdentity,
    goal: &GoalAdmission,
    input: RootInput<'_>,
) -> ProviderRequest {
    let CallScope {
        session_id,
        run_id,
        agent_run_id,
    } = scope;
    let tools = input.tool_context;
    let role = if tools.is_some() {
        PromptRole::Primary
    } else {
        PromptRole::ReadOnly
    };
    ProviderRequest {
        session_id,
        run_id,
        agent_run_id,
        phase: AgentPhase::RootPlan,
        collaboration: policy,
        model: identity.model.clone(),
        max_output_tokens: identity.limits.max_output_tokens,
        input: RunInput {
            context_window_tokens: input.context_window_tokens,
            assignment_profiles: input.assignment_profiles,
            images: input.images,
            goal_budget: (goal.autonomous || goal.plan_contract.is_some()).then_some(input.budget),
            plan_offered: policy.offers_plan(goal.autonomous),
            pinned_budget: input.budget,
            goal_plan_contract: goal.plan_contract.clone(),
            workspace_guidance: input
                .inputs
                .user_instructions
                .take()
                .into_iter()
                .chain(std::mem::take(&mut input.inputs.instructions))
                .map(|snapshot| WorkspaceGuidance {
                    path: snapshot.path,
                    text: snapshot.content,
                })
                .collect(),
            includes: std::mem::take(&mut input.inputs.includes)
                .into_iter()
                .map(|snapshot| snapshot.model_include())
                .collect(),
            history: input.history,
            context_summary: input.summary,
            tools,
            prompts: input.prompts,
            ..RunInput::new(role, input.objective)
        },
    }
}

fn supervisor_policy(
    session_id: SessionId,
    tools_enabled: bool,
) -> Option<crate::supervisor::SupervisorPolicy> {
    tools_enabled.then(|| crate::fault::supervisor_policy(session_id))
}
