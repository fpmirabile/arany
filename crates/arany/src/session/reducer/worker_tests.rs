use super::*;
use crate::session::worker::*;
use crate::session::{ChildGrant, CollaborationPolicy, GoalSuccessCriterion};
use crate::tools::{
    Attestation, CacheUpdate, EffectIntent, FileChange, FileVersion, GuardReceipt, ToolCall,
    ToolDisposition, ToolLimits, ToolObservation,
};

pub(crate) struct Fixture {
    pub session: SessionId,
    pub goal: GoalId,
    pub run: RunId,
    pub primary: AgentRunId,
    pub child: AgentRunId,
    pub events: Vec<Event>,
    pub changes: WorkerChangeset,
}
impl Fixture {
    fn push(&mut self, event: Event) -> u64 {
        self.events.push(event);
        self.events.len() as u64
    }
    fn envelope(&self, event: Event, sequence: u64) -> EventEnvelope {
        let (run_id, agent_run_id) = event.scope();
        EventEnvelope {
            session_id: self.session,
            run_id,
            agent_run_id,
            event,
            sequence,
            created_at_ms: 0,
        }
    }
    pub(crate) fn envelopes(&self) -> Vec<EventEnvelope> {
        self.events
            .iter()
            .enumerate()
            .map(|(i, e)| self.envelope(e.clone(), i as u64 + 1))
            .collect()
    }
    fn view(&self) -> SessionView {
        let mut view = None;
        for end in 1..=self.events.len() {
            match SessionView::replay_open(self.session, &self.envelopes()[..end]) {
                Ok(v) => view = v,
                Err(error) => panic!("event {end} {}: {error:?}", self.events[end - 1].kind()),
            }
        }
        view.unwrap()
    }
    fn intent(&self, actor: AgentRunId, call: ToolCall, target: EffectTarget) -> EffectIntent {
        EffectIntent {
            host: None,
            host_command: None,
            target,
            id: uuid::Uuid::now_v7(),
            run_id: self.run,
            agent_run_id: actor,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            use_count: 1,
        }
    }
    fn started(&mut self, intent: &EffectIntent) {
        let configured = self.events.iter().any(|event| matches!(event,
            Event::RunStarted { config, .. } if config.tool_policy.as_ref().is_some_and(|policy| policy.permissions_digest.is_some())))
            || intent.call.is_host() || intent.call.is_unguarded()
            || SessionView::replay_open(self.session, &self.envelopes()).ok().flatten()
                .is_some_and(|view| super::definition_mode(&view.runs[0], intent.agent_run_id).ok().flatten().is_some());
        self.push(if configured {
            Event::ToolProposed {
                run_id: self.run,
                agent_run_id: intent.agent_run_id,
                intent: intent.clone().into(),
                active_ceiling_ms: 0,
            }
        } else {
            Event::ToolStarted {
                run_id: self.run,
                agent_run_id: intent.agent_run_id,
                intent: intent.clone().into(),
                active_ceiling_ms: 0,
                verdict: None,
            }
        });
    }
    fn observed(
        &mut self,
        intent: &EffectIntent,
        disposition: ToolDisposition,
        attestation: Option<Attestation>,
        output: &str,
    ) -> u64 {
        let checkpoint = if crate::session::checkpoint::publication(intent)
            && (attestation.is_some() || disposition == ToolDisposition::Uncertain)
        {
            let view = self.view();
            let needed = crate::session::checkpoint::before_versions(&view.runs[0], &intent.call)
                .iter()
                .any(|(path, before)| {
                    before.as_ref().is_some_and(|digest| {
                        crate::session::checkpoint::capture_needed(&view, self.run, path, digest)
                    })
                });
            Some(
                if needed
                    && matches!(
                        disposition,
                        ToolDisposition::Succeeded | ToolDisposition::Uncertain
                    )
                {
                    crate::Checkpoint {
                        status: crate::CheckpointStatus::Captured,
                        reason: None,
                        pruned_before_run: None,
                    }
                } else {
                    crate::Checkpoint::not_needed()
                },
            )
        } else {
            None
        };
        self.push(Event::ToolFinished {
            checkpoint,
            run_id: self.run,
            agent_run_id: intent.agent_run_id,
            observation: ToolObservation {
                spill: None,
                intent: intent.clone(),
                disposition,
                attestation,
                output: output.into(),
                enforcement_cause: None,
                limit_cause: (disposition == crate::tools::ToolDisposition::Limit)
                    .then_some(crate::tools::LimitCause::Resource),
                approval_cause: None,
                cache_update: CacheUpdate::NotApplicable,
            },
        })
    }
    fn call(
        &mut self,
        actor: AgentRunId,
        phase: AgentPhase,
        disposition: ProviderCallDisposition,
        delegation: Option<Vec<ChildAssignment>>,
    ) {
        self.push(Event::GoalProviderCallReserved {
            goal_id: self.goal,
            run_id: self.run,
            agent_run_id: actor,
            active_ceiling_ms: 0,
        });
        self.push(Event::ProviderCallRecorded {
            run_id: self.run,
            agent_run_id: actor,
            record: ProviderCallRecord {
                rate_limits: None,
                delegation,
                phase,
                disposition,
                requested_tools: u8::from(disposition == ProviderCallDisposition::ToolRequested),
                response_id: None,
                input_tokens: matches!(
                    disposition,
                    ProviderCallDisposition::Finished
                        | ProviderCallDisposition::Delegated
                        | ProviderCallDisposition::ToolRequested
                        | ProviderCallDisposition::PlanRecorded
                )
                .then_some(0),
                output_tokens: matches!(
                    disposition,
                    ProviderCallDisposition::Finished
                        | ProviderCallDisposition::Delegated
                        | ProviderCallDisposition::ToolRequested
                        | ProviderCallDisposition::PlanRecorded
                )
                .then_some(0),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                wire_provenance: None,
                refusal: None,
                failure_reason: None,
            },
        });
    }
    fn mutation(&mut self) {
        self.call(
            self.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::ToolRequested,
            None,
        );
        let intent = self.intent(
            self.child,
            ToolCall::Write {
                path: self.changes.files[0].path.clone(),
                expected_digest: None,
                content: "new".into(),
            },
            EffectTarget::Workspace,
        );
        self.started(&intent);
        if self.events.iter().any(|e| matches!(e, Event::RunStarted { config, .. } if config.tool_policy.as_ref().unwrap().permissions_digest.is_some())) {
            allow_permission(self, &intent);
        }
        self.observed(&intent, ToolDisposition::Succeeded, Some(Attestation::for_site(&intent)),
            &serde_json::json!({"sha256":crate::validate::hex_digest(b"new"),"bytes":3,"executable":false,"before":null}).to_string());
    }
    fn finish_worker(&mut self) -> u64 {
        self.call(
            self.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::Finished,
            None,
        );
        self.push(Event::AgentFinished {
            run_id: self.run,
            agent_run_id: self.child,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
            worker: Some(WorkerOutcome {
                disposition: WorkerDisposition::Ready,
                cause: WorkerCause::Finished,
            }),
        })
    }
    fn release(&mut self, cause_sequence: u64) {
        self.push(Event::WorkerLeaseReleased {
            run_id: self.run,
            agent_run_id: self.primary,
            record: WorkerLeaseRelease {
                assignment_id: self.child,
                cause_sequence,
            },
        });
    }
    fn verify(&mut self) {
        self.call(
            self.primary,
            AgentPhase::RootSynthesis,
            ProviderCallDisposition::ToolRequested,
            None,
        );
        let intent = self.intent(
            self.primary,
            ToolCall::Read {
                path: "src/a".into(),
                offset: None,
                limit: Some(32),
            },
            EffectTarget::Workspace,
        );
        self.started(&intent);
        let seq = self.observed(
            &intent,
            ToolDisposition::Succeeded,
            Some(Attestation::for_site(&intent)),
            &serde_json::json!({"sha256":crate::validate::hex_digest(b"new")}).to_string(),
        );
        let criterion = GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/a".into(),
            content: "new".into(),
        };
        self.push(Event::WorkerVerificationRecorded {
            run_id: self.run,
            agent_run_id: self.primary,
            record: WorkerVerification {
                assignment_id: self.child,
                criterion_digest: crate::validate::digest(&serde_json::to_vec(&criterion).unwrap()),
                passed: true,
                evidence: vec![seq],
            },
        });
    }
    fn compact(&mut self) {
        self.push(Event::GoalCompactionCallReserved {
            goal_id: self.goal,
            run_id: self.run,
            active_ceiling_ms: 0,
        });
        let view = self.view();
        let run = &view.runs[0];
        let covered = run.tools.last().unwrap().finished_sequence.unwrap();
        let mut events = self.envelopes();
        events.pop();
        let source_digest = crate::session::prefix_digest(&events, covered).unwrap();
        self.push(Event::GoalContextCompacted {
            goal_id: self.goal,
            run_id: self.run,
            covered_tool_count: u32::try_from(run.tools.len()).unwrap(),
            record: crate::session::CompactionRecord {
                rate_limits: None,
                covered_run_id: self.run,
                covered_sequence: covered,
                source_digest,
                recent_files: Vec::new(),
                provider: "openai".into(),
                model: "gpt-5.4".into(),
                prompt_version: 1,
                schema_version: 1,
                compiler_version: crate::session::COMPACTION_COMPILER_VERSION,
                source_bytes: 1,
                source_tokens_estimate: 1,
                input_tokens: None,
                output_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                response_id: None,
                wire_provenance: None,
                chatgpt_provenance: None,
                output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
                status: crate::session::CompactionStatus::Failed {
                    reason: crate::session::CompactionFailure::ProviderUnavailable,
                },
            },
        });
        self.view();
    }
    fn close(&mut self) {
        self.call(
            self.primary,
            AgentPhase::RootSynthesis,
            ProviderCallDisposition::Finished,
            None,
        );
        self.push(Event::AgentFinished {
            run_id: self.run,
            agent_run_id: self.primary,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
            worker: None,
        });
        self.push(Event::MessageCommitted {
            run_id: self.run,
            text: "done".into(),
        });
        self.push(Event::RunFinished {
            run_id: self.run,
            disposition: RunDisposition::Finished,
            failure: None,
            active_ms: 0,
        });
    }
}
fn receipt(intent: &EffectIntent) -> GuardReceipt {
    GuardReceipt {
        contract_version: 1,
        intent_digest: intent.digest(),
        enforcement_digest: intent.enforcement_digest,
        limits: intent.limits.clone(),
    }
}

fn allow_permission(f: &mut Fixture, intent: &EffectIntent) {
    use crate::tools::permissions::{
        Decision, PermissionMode, PermissionSource, PermissionVerdict,
    };

    f.push(Event::ToolStarted {
        intent: intent.clone().into(),
        active_ceiling_ms: 0,
        run_id: f.run,
        agent_run_id: intent.agent_run_id,
        verdict: Some(PermissionVerdict {
            host: None,
            intent_digest: intent.digest(),
            decision: Decision::Allow,
            mode: Some(PermissionMode::Default),
            basis: PermissionSource::Rule,
            source: PermissionSource::Rule,
            approved: true,
            hook_restriction: None,
            hooks: 0,
            narrowed: false,
        }),
    });
}

pub(crate) fn fixture() -> Fixture {
    fixture_with_grant(
        vec!["read".into(), "write".into()],
        vec![WorkerTool::Read, WorkerTool::Write],
        None,
    )
}
fn fixture_with_grant(
    definition_tools: Vec<String>,
    tools: Vec<WorkerTool>,
    permissions_digest: Option<[u8; 32]>,
) -> Fixture {
    let (session, goal, run, primary, child) = (
        SessionId::new(),
        GoalId::new(),
        RunId::new(),
        AgentRunId::new(),
        AgentRunId::new(),
    );
    let definition = crate::tools::definitions::AgentDefinition {
        version: 1,
        name: "editor".into(),
        description: "Edit".into(),
        tools: Some(definition_tools),
        disallowed_tools: Vec::new(),
        model: None,
        permission_mode: None,
        max_turns: 17,
        isolation: Some("worktree".into()),
        background: false,
        body: "Untrusted".into(),
    };
    let files = vec![crate::tools::DefinitionFilePin {
        private: false,
        filename: "editor.md".into(),
        digest: [3; 32],
    }];
    let grant = WorkerGrant {
        commands: Vec::new(),
        read_paths: vec!["src".into()],
        write_paths: vec!["src".into()],
        tools,
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        permission_mode: None,
    };
    let config = RunConfig {
        supervisor: Some(crate::supervisor::SupervisorPolicy::all_off()),
        worker_policy: Some(WorkerPolicy {
            parent_grant: grant.clone(),
            concurrency: 4,
            max_turns: 17,
            max_tools: 16,
            data_bytes: WORKER_DATA_BYTES as u32,
        }),
        agent_definitions: Some(crate::tools::DefinitionRegistryPin {
            digest: crate::validate::digest(&serde_json::to_vec(&files).unwrap()),
            files,
        }),
        roster: None,
        providers: None,
        provider: crate::ProviderRecord::parse("openai").expect("Provider record"),
        model: "gpt-5.4".into(),
        effort: None,
        custom_profile_provenance: None,
        saved_api_account_id: None,
        chatgpt_provenance: None,
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        context_window_tokens: None,
        policy: CollaborationPolicy::Team {
            max_active_children: 8,
        },
        output_token_cap: 4096,
        provider_concurrency: 4,
        workspace_device: 1,
        workspace_inode: 2,
        user_instruction_digest: None,
        instruction_sources: Vec::new(),
        launch_directory: String::new(),
        include_digests: Vec::new(),
        history_run_ids: Vec::new(),
        excluded_history_runs: 0,
        context_usage: None,
        compaction_event_sequence: None,
        compaction_content_digest: None,
        tool_policy: Some(Box::new(crate::tools::ToolPolicyReceipt {
            host_epoch: None,
            host_prefixes: Vec::new(),
            contract_version: 1,
            config_digest: [1; 32],
            permissions_digest,
            max_context_bytes: crate::tools::types::MAX_TOOL_CONTEXT_BYTES as u32,
            unguarded_rules_digest: None,
            unguarded: crate::tools::UnguardedGrant::HumanOnly,
            child_unguarded: crate::tools::UnguardedGrant::Denied,
        })),
    };
    let mut f = Fixture {
        session,
        goal,
        run,
        primary,
        child,
        events: Vec::new(),
        changes: WorkerChangeset {
            files: vec![FileChange {
                path: "src/a".into(),
                before: None,
                after: Some(FileVersion::from_bytes(b"new", false)),
            }],
        },
    };
    f.push(Event::SessionStarted {
        title: "Worker protocol".into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
    });
    f.push(Event::GoalStarted {
        include_paths: Some(Vec::new()),
        goal_id: goal,
        objective: "Edit".into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        budget: GoalBudget::default(),
        phase: GoalPhase::Implementation,
        autonomous: false,
        resource_settings: crate::tools::ToolResourceSettings::default(),
        continues: None,
    });
    f.push(Event::GoalRunLinked {
        goal_id: goal,
        run_id: run,
        phase: GoalPhase::Implementation,
    });
    f.push(Event::MessageAccepted {
        skill: None,
        run_id: run,
        text: "Edit".into(),
        images: Vec::new(),
    });
    f.push(Event::RunStarted {
        run_id: run,
        config,
        active_deadline_ms: None,
    });
    f.push(Event::AgentSpawned {
        run_id: run,
        agent_run_id: primary,
        role: AgentRole::Primary,
        ordinal: 0,
        objective: None,
        grant: None,
        definition: None,
        profile: None,
        assignment: None,
    });
    let parse = f.intent(
        primary,
        ToolCall::ParseAgentDefinition {
            index: 0,
            raw_digest: [3; 32],
        },
        EffectTarget::Workspace,
    );
    f.started(&parse);
    if permissions_digest.is_some() {
        allow_permission(&mut f, &parse);
    }
    f.observed(
        &parse,
        ToolDisposition::Succeeded,
        Some(Attestation::Guard(receipt(&parse))),
        &serde_json::json!({"Definition":{"definition":definition,"notes":[]}}).to_string(),
    );
    f.call(
        primary,
        AgentPhase::RootPlan,
        ProviderCallDisposition::PlanRecorded,
        None,
    );
    let contract = GoalPlanContract {
        mission: "Edit".into(),
        paths: vec!["src".into()],
        commands: Vec::new(),
        resource_class: crate::tools::ToolResourceClass::Default,
        command_timeout_ms: None,
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/a".into(),
            content: "new".into(),
        },
        budget: GoalBudget::default(),
    };
    f.push(Event::GoalPlanContractRecorded {
        goal_id: goal,
        run_id: run,
        contract: contract.clone(),
    });
    f.call(
        primary,
        AgentPhase::RootPlan,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let read = f.intent(
        primary,
        ToolCall::Read {
            path: "src/base".into(),
            offset: None,
            limit: Some(32),
        },
        EffectTarget::Workspace,
    );
    f.started(&read);
    if permissions_digest.is_some() {
        allow_permission(&mut f, &read);
    }
    f.observed(
        &read,
        ToolDisposition::Succeeded,
        Some(Attestation::for_site(&read)),
        "{}",
    );
    let request = EditorAssignment {
        commands: Vec::new(),
        definition: "editor".into(),
        definition_digest: crate::validate::digest(&serde_json::to_vec(&definition).unwrap()),
        mission: "Edit src".into(),
        direction: String::new(),
        lease: vec!["src/a".into()],
        criterion: contract.success_criterion,
        mode: WorkerMode::Foreground,
        max_turns: 17,
        wave: 1,
        resumes: None,
        supersedes: None,
    };
    f.call(
        primary,
        AgentPhase::RootPlan,
        ProviderCallDisposition::Delegated,
        Some(vec![ChildAssignment::Editor {
            assignment: Box::new(request.clone()),
        }]),
    );
    let mut grant = grant;
    grant.write_paths = request.lease.clone();
    let pins = WorkerAssignment {
        request,
        grant,
        policy_digest: [1; 32],
        permissions_digest,
    };
    f.push(Event::AgentSpawned {
        run_id: run,
        agent_run_id: child,
        role: AgentRole::Child,
        ordinal: 1,
        objective: Some(pins.request.mission.clone()),
        grant: Some(ChildGrant::EditOnly {
            unguarded: crate::tools::UnguardedGrant::Denied,
        }),
        definition: Some("editor".into()),
        profile: None,
        assignment: Some(pins),
    });
    f
}

pub(crate) fn host_protocol_fixture() -> Fixture {
    use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
    let mut f = fixture();
    let first = f
        .events
        .iter()
        .position(|e| matches!(e, Event::ToolStarted { intent, .. } if matches!(intent.call, ToolCall::Read { .. })))
        .unwrap();
    f.events.truncate(first);
    for event in &mut f.events {
        if let Event::GoalPlanContractRecorded { contract, .. } = event {
            contract.paths = vec![".".into()];
        }
    }
    let mut intent = f.intent(
        f.primary,
        ToolCall::GitPush {
            remote: "https://github.com/o/r.git".into(),
            branch: "main".into(),
        },
        EffectTarget::Workspace,
    );
    intent.host = Some(crate::HostIntent {
        source_oid: "1".repeat(40),
        refspec: format!("{}:refs/heads/main", "1".repeat(40)),
    });
    intent.limits.network = crate::NetworkGrant::Host;
    f.started(&intent);
    f.push(Event::ToolStarted {
        intent: intent.clone().into(),
        active_ceiling_ms: 0,
        run_id: f.run,
        agent_run_id: f.primary,
        verdict: Some(PermissionVerdict {
            host: None,
            intent_digest: intent.digest(),
            decision: Decision::Ask,
            mode: None,
            basis: PermissionSource::Mode,
            source: PermissionSource::Human,
            approved: true,
            hook_restriction: None,
            hooks: 0,
            narrowed: false,
        }),
    });
    f
}

pub(crate) fn session_host_protocol_fixture() -> Fixture {
    use crate::tools::permissions::{Decision, HostApproval, PermissionSource, PermissionVerdict};
    let mut f = host_protocol_fixture();
    f.events.truncate(f.events.len() - 2);
    let epoch = uuid::Uuid::now_v7();
    for event in &mut f.events {
        match event {
            Event::RunStarted { config, .. } => {
                config.tool_policy.as_mut().unwrap().host_epoch = Some(epoch);
            }
            Event::GoalPlanContractRecorded { contract, .. } => {
                contract.commands = vec!["sh".into()];
            }
            _ => {}
        }
    }
    let mut intent = f.intent(
        f.primary,
        ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: String::new(),
            justification: "Run the exact host command".into(),
            prefix_rule: None,
        },
        EffectTarget::Workspace,
    );
    intent.host_command = Some(crate::tools::types::test_host_pin());
    let first = intent.digest();
    for source in [PermissionSource::Human, PermissionSource::SessionApproval] {
        let human = source == PermissionSource::Human;
        f.started(&intent);
        f.push(Event::ToolStarted {
            intent: intent.clone().into(),
            active_ceiling_ms: 0,
            run_id: f.run,
            agent_run_id: f.primary,
            verdict: Some(PermissionVerdict {
                host: Some(HostApproval {
                    rule: None,
                    epoch,
                    session: human,
                    source_intent: (!human).then_some(first),
                }),
                intent_digest: intent.digest(),
                decision: if human {
                    Decision::Ask
                } else {
                    Decision::Allow
                },
                mode: None,
                basis: if human {
                    PermissionSource::Mode
                } else {
                    source
                },
                source,
                approved: true,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            }),
        });
        if human {
            f.observed(
                &intent,
                ToolDisposition::Succeeded,
                Some(Attestation::for_site(&intent)),
                r#"{"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":"","stderr":"","changeset":{"files":[]},"stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0}}"#,
            );
            f.call(
                f.primary,
                AgentPhase::RootPlan,
                ProviderCallDisposition::ToolRequested,
                None,
            );
            intent.id = uuid::Uuid::now_v7();
            intent.limits.runtime_ms /= 2;
        }
    }
    f.view();
    f
}

#[test]
fn host_effect_tracking_and_rewind_keep_approved_unknown_effects() {
    for outcome in [
        None,
        Some(ToolDisposition::Succeeded),
        Some(ToolDisposition::Uncertain),
        Some(ToolDisposition::Denied),
    ] {
        let mut f = host_protocol_fixture();
        let intent: EffectIntent = f
            .events
            .iter()
            .rev()
            .find_map(|event| match event {
                Event::ToolStarted { intent, .. } if intent.call.is_host() => {
                    Some(intent.clone().into())
                }
                _ => None,
            })
            .expect("folded host start");
        if let Some(disposition) = outcome {
            let receipt =
                (disposition != ToolDisposition::Denied).then(|| Attestation::for_site(&intent));
            f.observed(&intent, disposition, receipt, "host: synthetic outcome");
        }
        let view = f.view();
        let expected = outcome != Some(ToolDisposition::Denied);
        assert_eq!(
            view.runs[0]
                .tools
                .iter()
                .find(|t| t.intent.call.is_host())
                .unwrap()
                .ran_host(),
            expected
        );
        assert_eq!(view.runs[0].host_tools(), usize::from(expected));
        let plan = crate::session::rewind::plan(&view, &view.rewinds.checkpoints[0]).unwrap();
        assert_eq!(
            plan.non_rewindable
                .iter()
                .any(|(_, effect)| *effect == crate::session::rewind::NonRewindable::Host),
            expected
        );
    }
}

pub(crate) fn completed_fixture() -> Fixture {
    compacted_fixture(false)
}
pub(crate) fn compacted_fixture(compacted: bool) -> Fixture {
    let mut f = fixture();
    f.mutation();
    let cause = f.finish_worker();
    f.release(cause);
    f.verify();
    if compacted {
        f.compact();
    }
    f.close();
    f
}
#[test]
fn forged_invalid_editor_delegations_reject_before_child_work() {
    use crate::test_support::worker_teams::{INVALID_EDITOR_CASES, invalidate_editor};
    for (case, cause) in INVALID_EDITOR_CASES {
        let mut f = fixture();
        let delegation = f
            .events
            .iter()
            .position(|e| {
                matches!(e,
            Event::ProviderCallRecorded { record, .. } if record.delegation.is_some())
            })
            .unwrap();
        let mut event = f.events[delegation].clone();
        if let Event::ProviderCallRecorded { record, .. } = &mut event
            && let ChildAssignment::Editor { assignment } =
                &mut record.delegation.as_mut().unwrap()[0]
        {
            invalidate_editor(assignment, case);
        }
        if *case == "invalid_turns" {
            for event in &mut f.events {
                if let Event::RunStarted { config, .. } = event {
                    config.worker_policy.as_mut().unwrap().max_turns = 1;
                }
            }
        }
        f.events.truncate(delegation);
        let view = f.view();
        let Event::ProviderCallRecorded { record, .. } = &event else {
            unreachable!()
        };
        let ChildAssignment::Editor { assignment } = &record.delegation.as_ref().unwrap()[0] else {
            unreachable!()
        };
        assert_eq!(
            super::worker::validate_request(&view, &view.runs[0], assignment, None),
            Err(*cause),
            "{case}"
        );
        refused(&f, event, case);
    }
}

#[test]
fn worker_protocol_child_permission_mode_never_removes_parent_approval() {
    use crate::tools::permissions::{
        Decision, PermissionMode, PermissionSource, PermissionVerdict,
    };
    let mut f = fixture();
    if let Event::RunStarted { config, .. } = f
        .events
        .iter_mut()
        .find(|e| matches!(e, Event::RunStarted { .. }))
        .unwrap()
    {
        config
            .worker_policy
            .as_mut()
            .unwrap()
            .parent_grant
            .permission_mode = Some(PermissionMode::Default);
    }
    if let Event::AgentSpawned {
        profile: None,
        assignment: Some(p),
        ..
    } = f.events.last_mut().unwrap()
    {
        p.grant.permission_mode = Some(PermissionMode::AcceptEdits);
    }
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        f.child,
        ToolCall::Write {
            path: "src/a".into(),
            expected_digest: None,
            content: "new".into(),
        },
        EffectTarget::Workspace,
    );
    f.started(&intent);
    f.push(Event::ToolStarted {
        intent: intent.clone().into(),
        active_ceiling_ms: 0,
        run_id: f.run,
        agent_run_id: f.child,
        verdict: Some(PermissionVerdict {
            host: None,
            intent_digest: intent.digest(),
            decision: Decision::Allow,
            mode: Some(PermissionMode::AcceptEdits),
            basis: PermissionSource::Mode,
            source: PermissionSource::Mode,
            approved: true,
            hook_restriction: None,
            hooks: 0,
            narrowed: false,
        }),
    });
    let invalid = f.events.pop().unwrap();
    refused(&f, invalid, "child acceptEdits cannot erase parent Ask");
}

#[test]
fn worker_protocol_researcher_only_run_rejects_a_call_after_an_invalid_synthesis() {
    let mut f = fixture();
    let delegated = f
        .events
        .iter()
        .position(|e| matches!(e, Event::ProviderCallRecorded { record, .. } if record.delegation.is_some()))
        .unwrap();
    f.events.truncate(delegated + 1);
    let Event::ProviderCallRecorded { record, .. } = &mut f.events[delegated] else {
        unreachable!()
    };
    record.delegation = Some(vec![ChildAssignment::Researcher {
        objective: "Research".into(),
        definition: None,
        profile: None,
    }]);
    let researcher = AgentRunId::new();
    f.push(Event::AgentSpawned {
        run_id: f.run,
        agent_run_id: researcher,
        ordinal: 1,
        role: AgentRole::Child,
        objective: Some("Research".into()),
        grant: Some(ChildGrant::ReadOnly {
            unguarded: crate::tools::UnguardedGrant::Denied,
        }),
        definition: None,
        profile: None,
        assignment: None,
    });
    f.call(
        researcher,
        AgentPhase::ChildWork,
        ProviderCallDisposition::Finished,
        None,
    );
    f.push(Event::AgentFinished {
        run_id: f.run,
        agent_run_id: researcher,
        disposition: AgentDisposition::Finished,
        summary: Some("done".into()),
        result: Some("done".into()),
        worker: None,
    });
    f.call(
        f.primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::InvalidResponse,
        None,
    );
    f.view();
    let mut again = Fixture {
        events: f.events.clone(),
        ..f
    };
    again.call(
        again.primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::Finished,
        None,
    );
    let recorded = again.events.pop().unwrap();
    refused(&again, recorded, "synthesis after an invalid synthesis");
}

fn refused(f: &Fixture, event: Event, name: &str) {
    let mut view = f.view();
    let envelope = f.envelope(event.clone(), f.events.len() as u64 + 1);
    assert!(view.apply_committed(&envelope).is_err(), "decoded {name}");
    if let Ok(payload) = event.payload() {
        let decoded = Event::decode(
            event.kind(),
            event.version(),
            &payload,
            envelope.run_id,
            envelope.agent_run_id,
        )
        .unwrap();
        assert!(
            f.view()
                .apply_committed(&EventEnvelope {
                    event: decoded,
                    ..envelope
                })
                .is_err(),
            "serialized {name}"
        );
    }
}

#[path = "worker/workspace_tests.rs"]
mod workspace_tests;
