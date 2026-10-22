use super::*;
use crate::session::worker::*;
use crate::session::{ChildGrant, CollaborationPolicy, GoalSuccessCriterion};
use crate::tools::{
    Attestation, CacheUpdate, EffectIntent, FileChange, FileVersion, GuardReceipt, ToolCall,
    ToolDisposition, ToolLimits, ToolObservation,
};
use base64::Engine as _;

pub(crate) struct Fixture {
    pub session: SessionId,
    pub goal: GoalId,
    pub run: RunId,
    pub primary: AgentRunId,
    pub child: AgentRunId,
    pub events: Vec<Event>,
    pub base: WorkerDataRef,
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
            expires_at_ms: 1,
            use_count: 1,
        }
    }
    fn started(&mut self, intent: &EffectIntent) {
        self.push(Event::ToolStarted {
            run_id: self.run,
            agent_run_id: intent.agent_run_id,
            intent: intent.clone(),
            active_ceiling_ms: 0,
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
                intent: intent.clone(),
                disposition,
                attestation,
                output: output.into(),
                enforcement_cause: None,
                limit_cause: (disposition == crate::tools::ToolDisposition::Limit)
                    .then_some(crate::tools::LimitCause::Resource),
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
    fn data(
        &mut self,
        intent: &EffectIntent,
        purpose: WorkerDataPurpose,
        bytes: &[u8],
    ) -> WorkerDataRef {
        let first_sequence = self.events.len() as u64 + 1;
        for (i, bytes) in bytes.chunks(WORKER_CHUNK_BYTES).enumerate() {
            self.push(Event::WorkerDataChunk {
                run_id: self.run,
                agent_run_id: intent.agent_run_id,
                record: WorkerDataChunk {
                    assignment_id: self.child,
                    purpose,
                    tool_id: intent.id,
                    ordinal: u16::try_from(i).unwrap(),
                    bytes: u32::try_from(bytes.len()).unwrap(),
                    sha256: crate::tools::types::digest(bytes),
                    content: base64::engine::general_purpose::STANDARD.encode(bytes),
                },
            });
        }
        let reference = WorkerDataRef {
            assignment_id: self.child,
            purpose,
            first_sequence,
            seal_sequence: self.events.len() as u64 + 1,
            chunks: u16::try_from(bytes.len().div_ceil(WORKER_CHUNK_BYTES)).unwrap(),
            bytes: u32::try_from(bytes.len()).unwrap(),
            sha256: crate::tools::types::digest(bytes),
        };
        self.push(Event::WorkerDataSealed {
            run_id: self.run,
            agent_run_id: intent.agent_run_id,
            record: WorkerDataSealed {
                tool_id: intent.id,
                data: reference.clone(),
            },
        });
        reference
    }
    fn target(&self, revision: u32) -> EffectTarget {
        EffectTarget::WorkerView {
            cache_generation: self.events.iter().find_map(|e| match e {
                Event::AgentSpawned {
                    assignment: Some(p),
                    ..
                } => p.cache_generation,
                _ => None,
            }),
            assignment_id: self.child,
            base_digest: self.base.sha256,
            revision,
        }
    }
    fn mutation(&mut self) {
        self.call(
            self.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::ToolRequested,
            None,
        );
        let revision = self.view().runs[0]
            .agents
            .iter()
            .find(|a| a.id == self.child)
            .unwrap()
            .assignment
            .as_ref()
            .unwrap()
            .revision;
        let intent = self.intent(
            self.child,
            ToolCall::Write {
                path: self.changes.files[0].path.clone(),
                expected_digest: None,
                content: "new".into(),
            },
            self.target(revision),
        );
        self.started(&intent);
        let data = self.data(
            &intent,
            WorkerDataPurpose::Mutation,
            &serde_json::to_vec(&self.changes).unwrap(),
        );
        self.observed(
            &intent,
            ToolDisposition::Succeeded,
            Some(Attestation::WorkerMutation {
                receipt: receipt(&intent),
                mutation: Box::new(WorkerMutation {
                    previous_revision: revision,
                    next_revision: revision + 1,
                    data,
                }),
            }),
            "{}",
        );
    }
    fn finish_worker(&mut self) -> WorkerDataRef {
        self.call(
            self.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::Finished,
            None,
        );
        let revision = self.view().runs[0]
            .agents
            .iter()
            .find(|a| a.id == self.child)
            .unwrap()
            .assignment
            .as_ref()
            .unwrap()
            .revision;
        let intent = self.intent(
            self.child,
            ToolCall::PrepareWorker {
                preparation: WorkerPreparation {
                    assignment_id: self.child,
                    purpose: WorkerDataPurpose::Handover,
                    max_bytes: 4096,
                    max_chunks: 1,
                },
            },
            self.target(revision),
        );
        self.started(&intent);
        let data = self.data(
            &intent,
            WorkerDataPurpose::Handover,
            &serde_json::to_vec(&self.changes).unwrap(),
        );
        self.observed(
            &intent,
            ToolDisposition::Succeeded,
            Some(Attestation::WorkerData {
                guard: (intent.call.site() == crate::tools::catalog::Site::Guard)
                    .then(|| Box::new(receipt(&intent))),
                intent_digest: intent.digest(),
                data: data.clone(),
            }),
            "{}",
        );
        self.push(Event::AgentFinished {
            run_id: self.run,
            agent_run_id: self.child,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
            worker: Some(WorkerOutcome {
                cleanup: None,
                disposition: if self.changes.files.is_empty() {
                    WorkerDisposition::NoChange
                } else {
                    WorkerDisposition::Ready
                },
                cause: WorkerCause::Finished,
                data: Some(data.clone()),
            }),
        });
        data
    }
    fn handover(&mut self, data: WorkerDataRef) -> u64 {
        self.push(Event::WorkerHandoverRecorded {
            run_id: self.run,
            agent_run_id: self.child,
            record: WorkerHandover {
                criterion_evidence: private_criterion_evidence(&self.view().runs[0], self.child)
                    .unwrap(),
                assignment_id: self.child,
                base_digest: self.base.sha256,
                data,
            },
        })
    }
    fn integrate(
        &mut self,
        handover_sequence: u64,
        data: WorkerDataRef,
        disposition: ToolDisposition,
    ) -> u64 {
        let intent = self.intent(
            self.primary,
            ToolCall::ApplyChangeset {
                source: ChangesetSource::WorkerHandover {
                    assignment_id: self.child,
                    handover_sequence,
                    data,
                },
            },
            EffectTarget::Workspace,
        );
        self.started(&intent);
        let attestation = (disposition == ToolDisposition::Succeeded)
            .then(|| Attestation::Guard(receipt(&intent)));
        let finished_sequence = self.observed(&intent, disposition, attestation, "{}");
        let outcome = match disposition {
            ToolDisposition::Succeeded => WorkerIntegrationOutcome::Integrated,
            ToolDisposition::Conflict => WorkerIntegrationOutcome::Conflict,
            ToolDisposition::Denied
            | ToolDisposition::Failed
            | ToolDisposition::Limit
            | ToolDisposition::Cancelled => WorkerIntegrationOutcome::Denied,
            ToolDisposition::Uncertain => WorkerIntegrationOutcome::Uncertain,
        };
        self.push(Event::WorkerIntegrationRecorded {
            run_id: self.run,
            agent_run_id: self.primary,
            record: WorkerIntegration {
                assignment_id: self.child,
                handover_sequence,
                tool_id: intent.id,
                finished_sequence,
                outcome,
            },
        })
    }
    fn release(&mut self, handover_sequence: Option<u64>, cause_sequence: u64) {
        self.push(Event::WorkerLeaseReleased {
            run_id: self.run,
            agent_run_id: self.primary,
            record: WorkerSettlement {
                assignment_id: self.child,
                handover_sequence,
                cause_sequence,
                reason: WorkerSettlementReason::Integrated,
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
            &serde_json::json!({"sha256":crate::tools::types::hex_digest(b"new")}).to_string(),
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
                criterion_digest: crate::tools::types::digest(
                    &serde_json::to_vec(&criterion).unwrap(),
                ),
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
                provider: "openai".into(),
                model: "gpt-5.4".into(),
                prompt_version: 1,
                schema_version: 1,
                compiler_version: 3,
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

pub(crate) fn fixture() -> Fixture {
    fixture_with_manifest(
        WorkerManifest {
            entries: vec![WorkerManifestEntry {
                path: "src".into(),
                version: WorkerBaseVersion::Directory,
            }],
        },
        vec!["read".into(), "write".into()],
        vec![WorkerTool::Read, WorkerTool::Write],
    )
}

fn fixture_with_manifest(
    manifest: WorkerManifest,
    definition_tools: Vec<String>,
    tools: Vec<WorkerTool>,
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
            digest: crate::tools::types::digest(&serde_json::to_vec(&files).unwrap()),
            files,
        }),
        roster: None,
        providers: None,
        provider: "openai".into(),
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
            contract_version: 1,
            config_digest: [1; 32],
            permissions_digest: None,
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
        base: WorkerDataRef {
            assignment_id: child,
            purpose: WorkerDataPurpose::Base,
            first_sequence: 1,
            seal_sequence: 2,
            chunks: 1,
            bytes: 1,
            sha256: [1; 32],
        },
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
    f.observed(
        &read,
        ToolDisposition::Succeeded,
        Some(Attestation::for_site(&read)),
        "{}",
    );
    let request = EditorAssignment {
        commands: Vec::new(),
        definition: "editor".into(),
        definition_digest: crate::tools::types::digest(&serde_json::to_vec(&definition).unwrap()),
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
    let prep = f.intent(
        primary,
        ToolCall::PrepareWorker {
            preparation: WorkerPreparation {
                assignment_id: child,
                purpose: WorkerDataPurpose::Base,
                max_bytes: 4096,
                max_chunks: 1,
            },
        },
        EffectTarget::Workspace,
    );
    f.started(&prep);
    f.base = f.data(
        &prep,
        WorkerDataPurpose::Base,
        &serde_json::to_vec(&manifest).unwrap(),
    );
    f.observed(
        &prep,
        ToolDisposition::Succeeded,
        Some(Attestation::WorkerData {
            guard: (prep.call.site() == crate::tools::catalog::Site::Guard)
                .then(|| Box::new(receipt(&prep))),
            intent_digest: prep.digest(),
            data: f.base.clone(),
        }),
        "{}",
    );
    let mut grant = grant;
    grant.write_paths = request.lease.clone();
    let pins = WorkerAssignment {
        cache_generation: None,
        request,
        grant,
        policy_digest: [1; 32],
        permissions_digest: None,
        base: f.base.clone(),
        data_bytes: WORKER_DATA_BYTES as u32,
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

#[test]
fn worker_protocol_legal_lifecycle_and_every_cold_prefix() {
    let mut f = fixture();
    f.mutation();
    let observation = f
        .events
        .iter()
        .rev()
        .find_map(|e| {
            if let Event::ToolFinished { observation, .. } = e {
                Some(observation)
            } else {
                None
            }
        })
        .unwrap();
    let projection = crate::tools::types::model_observation(observation);
    assert_eq!(projection["workspace_effect"], "none");
    assert_eq!(projection["private_effect"], "applied");
    let data = f.finish_worker();
    let h = f.handover(data.clone());
    let i = f.integrate(h, data, ToolDisposition::Succeeded);
    f.release(Some(h), i);
    f.verify();
    f.close();
    let view = f.view();
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    for end in 1..=f.events.len() {
        let events = f.envelopes();
        let cold = SessionView::replay(f.session, &events[..end])
            .unwrap()
            .unwrap();
        let decoded: Vec<_> = events[..end]
            .iter()
            .map(|e| EventEnvelope {
                event: Event::decode(
                    e.event.kind(),
                    e.event.version(),
                    &e.event.payload().unwrap(),
                    e.run_id,
                    e.agent_run_id,
                )
                .unwrap(),
                ..e.clone()
            })
            .collect();
        assert_eq!(
            SessionView::replay(f.session, &decoded).unwrap().unwrap(),
            cold,
            "prefix {end}"
        );
        if end < f.events.len()
            && let Some(a) = cold
                .runs
                .last()
                .and_then(|r| r.agents.iter().find(|a| a.id == f.child))
        {
            assert!(a.assignment.as_ref().unwrap().interrupted);
            let run = cold.runs.last().unwrap();
            let workspace_unknown = run.tools.iter().any(|t| {
                !t.settled_without_stop()
                    && !matches!(t.intent.call, ToolCall::PrepareWorker { .. })
            });
            assert_eq!(
                cold.goal(f.goal).unwrap().can_resume(&cold),
                !workspace_unknown,
                "resume prefix {end}"
            );
            let history = run.worker_history();
            let worker = history.iter().find(|w| w.assignment_id == f.child).unwrap();
            let published = run.tools.iter().any(|t| {
                matches!(t.intent.call, ToolCall::ApplyChangeset { .. })
                    && t.observation
                        .as_ref()
                        .is_some_and(|o| o.disposition == ToolDisposition::Succeeded)
            });
            if published {
                assert_eq!(
                    worker.publication,
                    WorkerPublication::Confirmed,
                    "publication prefix {end}"
                );
            }
            if a.tools.iter().any(|t| t.observation.is_none()) {
                assert!(
                    worker.private_outcome_unknown,
                    "unknown private prefix {end}"
                );
            }
            assert_eq!(
                crate::provider::history_tool_block(run),
                crate::provider::history_tool_block(
                    &SessionView::replay(f.session, &decoded)
                        .unwrap()
                        .unwrap()
                        .runs[0]
                )
            );
        }
    }
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

#[test]
fn worker_protocol_refuses_widened_stale_and_unsettled_transitions() {
    let mut f = fixture();
    let spawn = f.events.pop().unwrap();
    for case in [
        "parent read",
        "parent write",
        "policy",
        "permission pin",
        "definition",
        "base",
        "turns",
        "lease alias",
        "unicode",
        "nested lease",
        "resume",
    ] {
        let mut event = spawn.clone();
        let Event::AgentSpawned {
            profile: None,
            assignment: Some(p),
            ..
        } = &mut event
        else {
            unreachable!()
        };
        match case {
            "parent read" => p.grant.read_paths = vec!["outside".into()],
            "parent write" => p.grant.write_paths = vec!["outside".into()],
            "policy" => p.policy_digest = [4; 32],
            "permission pin" => p.permissions_digest = Some([4; 32]),
            "definition" => p.request.definition_digest = [4; 32],
            "base" => p.base.sha256 = [4; 32],
            "turns" => p.request.max_turns = 18,
            "lease alias" => p.request.lease = vec!["src/A".into(), "src/a".into()],
            "unicode" => p.request.lease = vec!["src/é".into()],
            "nested lease" => p.request.lease = vec!["src".into(), "src/a".into()],
            "resume" => p.request.resumes = Some(AgentRunId::new()),
            _ => unreachable!(),
        }
        refused(&f, event, case);
    }
    let mut widened = spawn.clone();
    if let Event::AgentSpawned { grant, .. } = &mut widened {
        *grant = Some(ChildGrant::EditOnly {
            unguarded: crate::tools::UnguardedGrant::HumanOnly,
        });
    }
    refused(&f, widened, "child unguarded grant");
    f.push(spawn);
    refused(
        &f,
        Event::WorkerLeaseReleased {
            run_id: f.run,
            agent_run_id: f.primary,
            record: WorkerSettlement {
                assignment_id: f.child,
                handover_sequence: None,
                cause_sequence: f.events.len() as u64,
                reason: WorkerSettlementReason::NoChange,
            },
        },
        "release active lease",
    );
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    for case in [
        "workspace",
        "actor",
        "base",
        "revision",
        "out of lease",
        "command",
        "unguarded",
        "git",
        "mcp",
    ] {
        let call = match case {
            "out of lease" => ToolCall::Write {
                path: "outside".into(),
                expected_digest: None,
                content: "new".into(),
            },
            "command" => ToolCall::Command {
                program: "tool".into(),
                args: Vec::new(),
                cwd: ".".into(),
            },
            "unguarded" => ToolCall::CommandUnguarded {
                program: "tool".into(),
                args: Vec::new(),
                cwd: ".".into(),
                justification: "Run".into(),
            },
            "git" => ToolCall::GitStatus { path: ".".into() },
            "mcp" => ToolCall::McpCall {
                server: "server".into(),
                tool: "tool".into(),
                schema_digest: "0".repeat(64),
                arguments: "{}".into(),
            },
            _ => ToolCall::Write {
                path: "src/a".into(),
                expected_digest: None,
                content: "new".into(),
            },
        };
        let mut target = f.target(0);
        match case {
            "workspace" => target = EffectTarget::Workspace,
            "actor" => {
                if let EffectTarget::WorkerView { assignment_id, .. } = &mut target {
                    *assignment_id = AgentRunId::new();
                }
            }
            "base" => {
                if let EffectTarget::WorkerView { base_digest, .. } = &mut target {
                    *base_digest = [8; 32];
                }
            }
            "revision" => {
                if let EffectTarget::WorkerView { revision, .. } = &mut target {
                    *revision = 1;
                }
            }
            _ => (),
        }
        refused(
            &f,
            Event::ToolStarted {
                run_id: f.run,
                agent_run_id: f.child,
                intent: f.intent(f.child, call, target),
                active_ceiling_ms: 0,
            },
            case,
        );
    }
}

#[test]
fn worker_protocol_receipts_handover_order_and_verification_are_causal() {
    let mut f = fixture();
    f.mutation();
    let observed = f.events.pop().unwrap();
    for case in [
        "missing receipt",
        "workspace receipt",
        "wrong revision",
        "wrong data",
        "wrong actor",
    ] {
        let mut e = observed.clone();
        let Event::ToolFinished { observation: o, .. } = &mut e else {
            unreachable!()
        };
        match case {
            "missing receipt" => o.attestation = None,
            "workspace receipt" => o.attestation = Some(Attestation::Guard(receipt(&o.intent))),
            "wrong revision" => {
                if let Some(Attestation::WorkerMutation { mutation, .. }) = &mut o.attestation {
                    mutation.next_revision = 2;
                }
            }
            "wrong data" => {
                if let Some(Attestation::WorkerMutation { mutation, .. }) = &mut o.attestation {
                    mutation.data.sha256 = [5; 32];
                }
            }
            "wrong actor" => o.intent.agent_run_id = f.primary,
            _ => unreachable!(),
        }
        refused(&f, e, case);
    }
    f.push(observed);
    let data = f.finish_worker();
    let handover = Event::WorkerHandoverRecorded {
        run_id: f.run,
        agent_run_id: f.child,
        record: WorkerHandover {
            criterion_evidence: private_criterion_evidence(&f.view().runs[0], f.child).unwrap(),
            assignment_id: f.child,
            base_digest: f.base.sha256,
            data: data.clone(),
        },
    };
    let finished = f.events.pop().unwrap();
    refused(&f, handover.clone(), "handover before Answer");
    f.push(finished);
    let h = f.push(handover.clone());
    refused(&f, handover, "duplicate handover");
    let i = f.integrate(h, data, ToolDisposition::Succeeded);
    let integration = f.events.last().unwrap().clone();
    refused(&f, integration, "duplicate integration");
    f.release(Some(h), i);
    refused(&f, f.events.last().unwrap().clone(), "duplicate release");
    f.verify();
    let verification = f.events.pop().unwrap();
    for case in [
        "integration evidence",
        "private evidence",
        "wrong criterion",
        "false pass",
        "duplicate evidence",
    ] {
        let mut e = verification.clone();
        let Event::WorkerVerificationRecorded { record: v, .. } = &mut e else {
            unreachable!()
        };
        match case {
            "integration evidence" => v.evidence = vec![i-1],
            "private evidence" => v.evidence = vec![f.events.iter().position(|e| matches!(e, Event::ToolFinished { observation, .. } if matches!(observation.attestation, Some(Attestation::WorkerMutation { .. })))).unwrap() as u64 + 1],
            "wrong criterion" => v.criterion_digest = [5;32],
            "false pass" => v.passed = false,
            "duplicate evidence" => v.evidence.push(v.evidence[0]),
            _ => unreachable!(),
        }
        refused(&f, e, case);
    }
    f.push(verification);
    f.close();
    f.view();
    refused(
        &f,
        Event::WorkerHandoverRecorded {
            run_id: f.run,
            agent_run_id: f.child,
            record: WorkerHandover {
                criterion_evidence: private_criterion_evidence(&f.view().runs[0], f.child).unwrap(),
                assignment_id: f.child,
                base_digest: f.base.sha256,
                data: match &f.events[h as usize - 1] {
                    Event::WorkerHandoverRecorded { record, .. } => record.data.clone(),
                    _ => unreachable!(),
                },
            },
        },
        "late closed Run",
    );
}

#[test]
fn worker_protocol_stop_and_workspace_uncertainty_block_dispatch() {
    for reason in [
        WorkerStopReason::Cancelled,
        WorkerStopReason::Paused,
        WorkerStopReason::Budget,
    ] {
        let mut f = fixture();
        f.mutation();
        let data = f.finish_worker();
        f.push(Event::WorkerStopObserved {
            run_id: f.run,
            agent_run_id: f.primary,
            record: WorkerStopRecord { reason },
        });
        refused(
            &f,
            Event::WorkerHandoverRecorded {
                run_id: f.run,
                agent_run_id: f.child,
                record: WorkerHandover {
                    criterion_evidence: private_criterion_evidence(&f.view().runs[0], f.child)
                        .unwrap(),
                    assignment_id: f.child,
                    base_digest: f.base.sha256,
                    data,
                },
            },
            "late stopped handover",
        );
        refused(
            &f,
            Event::GoalProviderCallReserved {
                goal_id: f.goal,
                run_id: f.run,
                agent_run_id: f.primary,
                active_ceiling_ms: 0,
            },
            "stopped call",
        );
        refused(&f, f.events.last().unwrap().clone(), "duplicate stop");
    }
    let mut f = fixture();
    f.mutation();
    let data = f.finish_worker();
    let h = f.handover(data.clone());
    f.integrate(h, data, ToolDisposition::Uncertain);
    f.view();
    refused(
        &f,
        Event::GoalProviderCallReserved {
            goal_id: f.goal,
            run_id: f.run,
            agent_run_id: f.primary,
            active_ceiling_ms: 0,
        },
        "Workspace uncertainty continuation",
    );
}

#[test]
fn worker_protocol_chunk_seal_and_transfer_bounds_fail_closed() {
    let mut f = fixture();
    f.mutation();
    let seal_index = f
        .events
        .iter()
        .rposition(|e| matches!(e, Event::WorkerDataSealed { .. }))
        .unwrap();
    let chunk_index = seal_index - 1;
    for case in [
        "ordinal",
        "hash",
        "length",
        "actor",
        "tool",
        "purpose",
        "trailing",
        "duplicate",
        "seal digest",
        "seal count",
        "seal range",
    ] {
        let index = if case.starts_with("seal") {
            seal_index
        } else {
            chunk_index
        };
        let mut e = f.events[index].clone();
        match &mut e {
            Event::WorkerDataChunk {
                agent_run_id,
                record,
                ..
            } => match case {
                "ordinal" => record.ordinal = 1,
                "hash" => record.sha256 = [6; 32],
                "length" => record.bytes += 1,
                "actor" => *agent_run_id = f.primary,
                "tool" => record.tool_id = uuid::Uuid::now_v7(),
                "purpose" => record.purpose = WorkerDataPurpose::Base,
                "trailing" => record.content.push('='),
                "duplicate" => (),
                _ => unreachable!(),
            },
            Event::WorkerDataSealed { record, .. } => match case {
                "seal digest" => record.data.sha256 = [7; 32],
                "seal count" => record.data.chunks += 1,
                "seal range" => record.data.first_sequence += 1,
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
        let mut prefix = fixture_from(&f);
        prefix
            .events
            .truncate(index + usize::from(case == "duplicate"));
        refused(&prefix, e, case);
    }
    f.events.truncate(chunk_index + 1);
    let cold = SessionView::replay(f.session, &f.envelopes())
        .unwrap()
        .unwrap();
    assert_eq!(cold.runs[0].agents[1].status, AgentStatus::Interrupted);
}
fn fixture_from(f: &Fixture) -> Fixture {
    Fixture {
        session: f.session,
        goal: f.goal,
        run: f.run,
        primary: f.primary,
        child: f.child,
        events: f.events.clone(),
        base: f.base.clone(),
        changes: f.changes.clone(),
    }
}

#[test]
fn worker_protocol_researcher_spawn_must_match_its_proposed_profile() {
    for (proposed, spawned) in [(Some("spare"), None), (None, Some("spare"))] {
        let mut f = fixture();
        let Event::ProviderCallRecorded { record, .. } = f.events.iter_mut().rev().find(|e| matches!(e, Event::ProviderCallRecorded { record, .. } if record.delegation.is_some())).unwrap() else { unreachable!() };
        record
            .delegation
            .as_mut()
            .unwrap()
            .push(ChildAssignment::Researcher {
                objective: "Research".into(),
                definition: None,
                profile: proposed.map(str::to_owned),
            });
        refused(
            &f,
            Event::AgentSpawned {
                run_id: f.run,
                agent_run_id: AgentRunId::new(),
                ordinal: 2,
                role: AgentRole::Child,
                objective: Some("Research".into()),
                grant: Some(ChildGrant::ReadOnly {
                    unguarded: crate::tools::UnguardedGrant::Denied,
                }),
                definition: None,
                profile: spawned.map(str::to_owned),
                assignment: None,
            },
            "profile differs from the proposal",
        );
    }
}

#[test]
fn worker_protocol_private_loss_is_local_and_c1_failure_remains_fatal() {
    for research_fails in [false, true] {
        let mut f = fixture();
        let researcher = AgentRunId::new();
        let Event::ProviderCallRecorded { record, .. } = f.events.iter_mut().rev().find(|e| matches!(e, Event::ProviderCallRecorded { record, .. } if record.delegation.is_some())).unwrap() else { unreachable!() };
        record
            .delegation
            .as_mut()
            .unwrap()
            .push(ChildAssignment::Researcher {
                objective: "Research".into(),
                definition: None,
                profile: None,
            });
        f.push(Event::AgentSpawned {
            run_id: f.run,
            agent_run_id: researcher,
            ordinal: 2,
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
            f.target(0),
        );
        f.started(&intent);
        f.observed(
            &intent,
            ToolDisposition::Uncertain,
            None,
            "unknown private edit",
        );
        let outcome = WorkerOutcome {
            disposition: WorkerDisposition::PrivateUncertain,
            cause: WorkerCause::PrivateUncertain,
            data: None,
            cleanup: Some(WorkerCleanupReceipt {
                assignment_id: f.child,
                base_digest: f.base.sha256,
                revision: 0,
                enforcement_digest: intent.enforcement_digest,
                last_intent_digest: Some(intent.digest()),
            }),
        };
        let finish = Event::AgentFinished {
            run_id: f.run,
            agent_run_id: f.child,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
            worker: Some(outcome),
        };
        let mut missing = finish.clone();
        if let Event::AgentFinished {
            worker: Some(o), ..
        } = &mut missing
        {
            o.cleanup = None;
        }
        refused(&f, missing, "private loss without positive cleanup");
        let seq = f.push(finish);
        let settlement = WorkerSettlement {
            assignment_id: f.child,
            handover_sequence: None,
            cause_sequence: seq,
            reason: WorkerSettlementReason::PrivateUncertain,
        };
        let discarded = f.push(Event::WorkerHandoverDiscarded {
            run_id: f.run,
            agent_run_id: f.primary,
            record: settlement.clone(),
        });
        f.push(Event::WorkerLeaseReleased {
            run_id: f.run,
            agent_run_id: f.primary,
            record: WorkerSettlement {
                cause_sequence: discarded,
                ..settlement
            },
        });
        f.call(
            researcher,
            AgentPhase::ChildWork,
            if research_fails {
                ProviderCallDisposition::Unavailable
            } else {
                ProviderCallDisposition::Finished
            },
            None,
        );
        f.push(Event::AgentFinished {
            run_id: f.run,
            agent_run_id: researcher,
            disposition: if research_fails {
                AgentDisposition::Failed
            } else {
                AgentDisposition::Finished
            },
            summary: (!research_fails).then(|| "done".into()),
            result: (!research_fails).then(|| "done".into()),
            worker: None,
        });
        f.view();
        if research_fails {
            f.push(Event::GoalProviderCallReserved {
                goal_id: f.goal,
                run_id: f.run,
                agent_run_id: f.primary,
                active_ceiling_ms: 0,
            });
            refused(
                &f,
                Event::ProviderCallRecorded {
                    run_id: f.run,
                    agent_run_id: f.primary,
                    record: ProviderCallRecord {
                        rate_limits: None,
                        delegation: None,
                        phase: AgentPhase::RootSynthesis,
                        disposition: ProviderCallDisposition::Finished,
                        requested_tools: 0,
                        response_id: None,
                        input_tokens: Some(0),
                        output_tokens: Some(0),
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                        refusal: None,
                        failure_reason: None,
                        wire_provenance: None,
                    },
                },
                "C1 failure forbids successful synthesis",
            );
        } else {
            f.close();
            assert_eq!(f.view().runs[0].status, RunStatus::Finished);
        }
    }
}

#[test]
fn worker_protocol_requires_current_versions_nulls_and_closed_records() {
    let mut f = fixture();
    f.mutation();
    let data = f.finish_worker();
    let h = f.handover(data.clone());
    let i = f.integrate(h, data, ToolDisposition::Succeeded);
    f.release(Some(h), i);
    f.verify();
    f.close();
    f.push(Event::WorkerStopObserved {
        run_id: f.run,
        agent_run_id: f.primary,
        record: WorkerStopRecord {
            reason: WorkerStopReason::Paused,
        },
    });
    f.push(Event::WorkerHandoverDiscarded {
        run_id: f.run,
        agent_run_id: f.primary,
        record: WorkerSettlement {
            assignment_id: f.child,
            handover_sequence: None,
            cause_sequence: 1,
            reason: WorkerSettlementReason::Stopped,
        },
    });
    let expected = [
        "RunStarted",
        "AgentSpawned",
        "ProviderCallRecorded",
        "AgentFinished",
        "ToolStarted",
        "ToolFinished",
        "WorkerStopObserved",
        "WorkerHandoverDiscarded",
        "WorkerDataChunk",
        "WorkerDataSealed",
        "WorkerHandoverRecorded",
        "WorkerIntegrationRecorded",
        "WorkerLeaseReleased",
        "WorkerVerificationRecorded",
    ];
    for kind in expected {
        let e = f
            .envelopes()
            .into_iter()
            .find(|e| e.event.kind() == kind)
            .unwrap();
        let version = e.event.version();
        let payload = e.event.payload().unwrap();
        for v in [0, version - 1, version + 1] {
            assert!(matches!(
                Event::decode(kind, v, &payload, e.run_id, e.agent_run_id),
                Err(ReplayError::UnknownEvent)
            ));
        }
        let mut extra: serde_json::Value = serde_json::from_str(&payload).unwrap();
        extra["extra"] = serde_json::json!(true);
        assert!(
            Event::decode(kind, version, &extra.to_string(), e.run_id, e.agent_run_id).is_err(),
            "unknown {kind}"
        );
        let key = serde_json::from_str::<serde_json::Value>(&payload)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let duplicate = format!(
            "{{{}:{},{}",
            serde_json::to_string(&key).unwrap(),
            value[&key],
            &payload[1..]
        );
        assert!(
            Event::decode(kind, version, &duplicate, e.run_id, e.agent_run_id).is_err(),
            "duplicate {kind}"
        );
    }
    for (kind, key) in [
        ("AgentSpawned", "assignment"),
        ("AgentFinished", "worker"),
        ("ProviderCallRecorded", "delegation"),
    ] {
        let e = f
            .envelopes()
            .into_iter()
            .find(|e| e.event.kind() == kind)
            .unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&e.event.payload().unwrap()).unwrap();
        value.as_object_mut().unwrap().remove(key);
        assert!(
            Event::decode(
                kind,
                e.event.version(),
                &value.to_string(),
                e.run_id,
                e.agent_run_id
            )
            .is_err(),
            "missing nullable {key}"
        );
    }
}

#[test]
fn worker_protocol_bulk_file_content_and_serialized_bounds() {
    assert_eq!((WORKER_CHUNKS, WORKER_MANIFEST_CHUNKS), (1093, 69));
    let f = fixture();
    let bytes = vec![255; WORKER_CHUNK_BYTES];
    let chunk = Event::WorkerDataChunk {
        run_id: f.run,
        agent_run_id: f.child,
        record: WorkerDataChunk {
            assignment_id: f.child,
            purpose: WorkerDataPurpose::Mutation,
            tool_id: uuid::Uuid::now_v7(),
            ordinal: 0,
            bytes: WORKER_CHUNK_BYTES as u32,
            sha256: crate::tools::types::digest(&bytes),
            content: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    };
    assert!(chunk.payload().unwrap().len() <= crate::store::MAX_EVENT_BYTES);
    let files: Vec<_> = (0..WORKER_FILES)
        .map(|n| FileChange {
            path: format!("src/{n:04}"),
            before: None,
            after: Some(FileVersion::from_bytes(b"", false)),
        })
        .collect();
    let mut changes = WorkerChangeset { files };
    assert!(
        decode_data(
            &serde_json::to_vec(&changes).unwrap(),
            WorkerDataPurpose::Handover
        )
        .is_ok()
    );
    changes.files.push(FileChange {
        path: "src/z".into(),
        before: None,
        after: Some(FileVersion::from_bytes(b"", false)),
    });
    assert!(
        decode_data(
            &serde_json::to_vec(&changes).unwrap(),
            WorkerDataPurpose::Handover
        )
        .is_err()
    );
    let before = FileVersion::from_bytes(&vec![b'a'; 1024 * 1024], false);
    let after = FileVersion::from_bytes(&vec![b'b'; 1024 * 1024], false);
    let files = (0..16)
        .map(|n| FileChange {
            path: format!("src/{n:04}"),
            before: Some(before.clone()),
            after: Some(after.clone()),
        })
        .collect();
    let mut changes = WorkerChangeset { files };
    let exact = serde_json::to_vec(&changes).unwrap();
    assert!(exact.len() <= WORKER_DATA_BYTES);
    assert!(decode_data(&exact, WorkerDataPurpose::Handover).is_ok());
    drop(exact);
    changes.files.push(FileChange {
        path: "src/z".into(),
        before: None,
        after: Some(FileVersion::from_bytes(b"x", false)),
    });
    assert!(
        decode_data(
            &serde_json::to_vec(&changes).unwrap(),
            WorkerDataPurpose::Handover
        )
        .is_err()
    );
    drop(changes);
    let overflow = WorkerChangeset {
        files: vec![FileChange {
            path: "src/a".into(),
            before: None,
            after: Some(FileVersion::from_bytes(&vec![0; 1024 * 1024 + 1], false)),
        }],
    };
    assert!(
        decode_data(
            &serde_json::to_vec(&overflow).unwrap(),
            WorkerDataPurpose::Handover
        )
        .is_err()
    );
    for raw in [
        b"{\"files\":[],\"files\":[]}".as_slice(),
        b"{\"files\":[]} ".as_slice(),
        b"{\"files\":[],\"extra\":0}".as_slice(),
    ] {
        assert!(decode_data(raw, WorkerDataPurpose::Handover).is_err());
    }
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
    f.push(Event::ToolPermissionDecided {
        run_id: f.run,
        agent_run_id: f.primary,
        verdict: PermissionVerdict {
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
        },
    });
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
        let intent = match &f.events[f.events.len() - 2] {
            Event::ToolStarted { intent, .. } => intent.clone(),
            _ => unreachable!(),
        };
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

// With `compacted`, a Goal compaction covers every Tool of the Run before it closes.
pub(crate) fn compacted_fixture(compacted: bool) -> Fixture {
    let mut f = fixture();
    f.mutation();
    let data = f.finish_worker();
    let h = f.handover(data.clone());
    let i = f.integrate(h, data, ToolDisposition::Succeeded);
    f.release(Some(h), i);
    f.verify();
    if compacted {
        f.compact();
    }
    f.close();
    f
}

#[test]
fn worker_protocol_nonoverlap_and_completion_order() {
    let mut f = fixture();
    let first = f.child;
    let Event::AgentSpawned {
        profile: None,
        assignment: Some(mut pins),
        ..
    } = f.events.last().unwrap().clone()
    else {
        unreachable!()
    };
    pins.request.lease = vec!["src/b".into()];
    pins.request.mission = "Edit b".into();
    pins.grant.write_paths = pins.request.lease.clone();
    pins.request.criterion = GoalSuccessCriterion::WorkspaceFileContent {
        path: "src/b".into(),
        content: "new".into(),
    };
    let delegation = f
        .events
        .iter()
        .rposition(
            |e| matches!(e,Event::ProviderCallRecorded {record,..} if record.delegation.is_some()),
        )
        .unwrap();
    let Event::ProviderCallRecorded { record, .. } = &mut f.events[delegation] else {
        unreachable!()
    };
    record
        .delegation
        .as_mut()
        .unwrap()
        .push(ChildAssignment::Editor {
            assignment: Box::new(pins.request.clone()),
        });
    for alias in ["src/a", "src/A", "src/a/child"] {
        let mut prefix = fixture_from(&f);
        prefix.events.truncate(delegation);
        let mut invalid = f.events[delegation].clone();
        if let Event::ProviderCallRecorded { record, .. } = &mut invalid
            && let ChildAssignment::Editor { assignment } =
                &mut record.delegation.as_mut().unwrap()[1]
        {
            assignment.lease = vec![alias.into()];
        }
        refused(&prefix, invalid, "overlapping proposal");
    }
    pins.request.mode = WorkerMode::Background;
    for event in &mut f.events {
        match event {
            Event::ProviderCallRecorded { record, .. } => {
                for proposal in record.delegation.iter_mut().flatten() {
                    if let ChildAssignment::Editor { assignment } = proposal {
                        assignment.mode = WorkerMode::Background;
                    }
                }
            }
            Event::AgentSpawned {
                assignment: Some(pins),
                ..
            } => pins.request.mode = WorkerMode::Background,
            _ => {}
        }
    }
    f.call(
        f.primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let first_base = f.base.clone();
    let first_changes = f.changes.clone();
    f.child = AgentRunId::new();
    let second = f.child;
    let prep = f.intent(
        f.primary,
        ToolCall::PrepareWorker {
            preparation: WorkerPreparation {
                assignment_id: second,
                purpose: WorkerDataPurpose::Base,
                max_bytes: 4096,
                max_chunks: 1,
            },
        },
        EffectTarget::Workspace,
    );
    f.started(&prep);
    f.base = f.data(
        &prep,
        WorkerDataPurpose::Base,
        &serde_json::to_vec(&WorkerManifest {
            entries: vec![WorkerManifestEntry {
                path: "src".into(),
                version: WorkerBaseVersion::Directory,
            }],
        })
        .unwrap(),
    );
    f.observed(
        &prep,
        ToolDisposition::Succeeded,
        Some(Attestation::WorkerData {
            guard: (prep.call.site() == crate::tools::catalog::Site::Guard)
                .then(|| Box::new(receipt(&prep))),
            intent_digest: prep.digest(),
            data: f.base.clone(),
        }),
        "{}",
    );
    pins.base = f.base.clone();
    f.push(Event::AgentSpawned {
        run_id: f.run,
        agent_run_id: second,
        role: AgentRole::Child,
        ordinal: 2,
        objective: Some(pins.request.mission.clone()),
        grant: Some(ChildGrant::EditOnly {
            unguarded: crate::tools::UnguardedGrant::Denied,
        }),
        definition: Some("editor".into()),
        profile: None,
        assignment: Some(pins),
    });
    f.changes.files[0].path = "src/b".into();
    f.mutation();
    let second_data = f.finish_worker();
    let second_h = f.handover(second_data.clone());
    let second_base = f.base.clone();
    f.child = first;
    f.base = first_base;
    f.changes = first_changes;
    f.mutation();
    let first_data = f.finish_worker();
    let first_h = f.handover(first_data.clone());
    let out_of_order = f.intent(
        f.primary,
        ToolCall::ApplyChangeset {
            source: ChangesetSource::WorkerHandover {
                assignment_id: first,
                handover_sequence: first_h,
                data: first_data.clone(),
            },
        },
        EffectTarget::Workspace,
    );
    refused(
        &f,
        Event::ToolStarted {
            run_id: f.run,
            agent_run_id: f.primary,
            intent: out_of_order,
            active_ceiling_ms: 0,
        },
        "completion-order queue",
    );
    let first_base = f.base.clone();
    f.child = second;
    f.base = second_base;
    let integrated = f.integrate(second_h, second_data, ToolDisposition::Succeeded);
    f.release(Some(second_h), integrated);
    f.child = first;
    f.base = first_base;
    let integrated = f.integrate(first_h, first_data, ToolDisposition::Succeeded);
    f.release(Some(first_h), integrated);
    f.view();
}

#[test]
fn worker_protocol_escaped_assignment_bounds() {
    let mut f = fixture();
    let mut spawn = f.events.pop().unwrap();
    if let Event::AgentSpawned {
        profile: None,
        assignment: Some(p),
        objective,
        ..
    } = &mut spawn
    {
        p.request.mission = "\0".repeat(2048);
        *objective = Some(p.request.mission.clone());
        p.request.direction = "\0".repeat(2048);
        p.request.lease = (0..64)
            .map(|i| format!("{i:02}{}", "a".repeat(510)))
            .collect();
        p.grant.write_paths = p.request.lease.clone();
        p.request.criterion = GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/a".into(),
            content: "\0".repeat(1024),
        };
    }
    assert!(spawn.payload().is_err(), "escaped metadata exceeds 64 KiB");
}

#[test]
fn worker_protocol_no_change_conflict_and_denial_settle_without_publication() {
    let mut f = fixture_with_manifest(
        WorkerManifest {
            entries: vec![WorkerManifestEntry {
                path: "src/a".into(),
                version: WorkerBaseVersion::File {
                    sha256: crate::tools::types::hex_digest(b"new"),
                    bytes: 3,
                    executable: false,
                },
            }],
        },
        vec!["read".into(), "write".into()],
        vec![WorkerTool::Read, WorkerTool::Write],
    );
    f.changes.files.clear();
    f.finish_worker();
    let finished = f.events.len() as u64;
    let record = WorkerSettlement {
        assignment_id: f.child,
        handover_sequence: None,
        reason: WorkerSettlementReason::NoChange,
        cause_sequence: finished,
    };
    let discarded = f.push(Event::WorkerHandoverDiscarded {
        run_id: f.run,
        agent_run_id: f.primary,
        record: record.clone(),
    });
    f.push(Event::WorkerLeaseReleased {
        run_id: f.run,
        agent_run_id: f.primary,
        record: WorkerSettlement {
            cause_sequence: discarded,
            ..record
        },
    });
    f.view();
    f.verify();
    f.close();
    f.view();
    for (disposition, reason) in [
        (ToolDisposition::Conflict, WorkerSettlementReason::Conflict),
        (ToolDisposition::Denied, WorkerSettlementReason::Denied),
        (ToolDisposition::Failed, WorkerSettlementReason::Denied),
        (ToolDisposition::Limit, WorkerSettlementReason::Denied),
        (ToolDisposition::Cancelled, WorkerSettlementReason::Denied),
    ] {
        let mut f = fixture();
        f.mutation();
        let data = f.finish_worker();
        let h = f.handover(data.clone());
        let integration = f.integrate(h, data, disposition);
        let record = WorkerSettlement {
            assignment_id: f.child,
            handover_sequence: Some(h),
            reason,
            cause_sequence: integration,
        };
        let discarded = f.push(Event::WorkerHandoverDiscarded {
            run_id: f.run,
            agent_run_id: f.primary,
            record: record.clone(),
        });
        f.push(Event::WorkerLeaseReleased {
            run_id: f.run,
            agent_run_id: f.primary,
            record: WorkerSettlement {
                cause_sequence: discarded,
                ..record
            },
        });
        let view = f.view();
        assert!(
            view.runs[0].agents[1]
                .assignment
                .as_ref()
                .unwrap()
                .released
                .is_some()
        );
        assert!(!view.goals[0].success_criterion_satisfied);
    }
}

#[test]
fn worker_protocol_reuses_an_identical_seal_for_handover() {
    let mut f = fixture();
    f.mutation();
    let data = f
        .events
        .iter()
        .rev()
        .find_map(|e| {
            if let Event::WorkerDataSealed { record, .. } = e {
                Some(record.data.clone())
            } else {
                None
            }
        })
        .unwrap();
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::Finished,
        None,
    );
    let prep = f.intent(
        f.child,
        ToolCall::PrepareWorker {
            preparation: WorkerPreparation {
                assignment_id: f.child,
                purpose: WorkerDataPurpose::Handover,
                max_bytes: 4096,
                max_chunks: 1,
            },
        },
        f.target(1),
    );
    f.started(&prep);
    f.observed(
        &prep,
        ToolDisposition::Succeeded,
        Some(Attestation::WorkerData {
            guard: Some(Box::new(receipt(&prep))),
            intent_digest: prep.digest(),
            data: data.clone(),
        }),
        "{}",
    );
    f.push(Event::AgentFinished {
        run_id: f.run,
        agent_run_id: f.child,
        disposition: AgentDisposition::Finished,
        summary: Some("done".into()),
        result: Some("done".into()),
        worker: Some(WorkerOutcome {
            disposition: WorkerDisposition::Ready,
            cause: WorkerCause::Finished,
            data: Some(data.clone()),
            cleanup: None,
        }),
    });
    let h = f.handover(data.clone());
    let i = f.integrate(h, data, ToolDisposition::Succeeded);
    f.release(Some(h), i);
    f.verify();
    f.close();
    f.view();
}

#[test]
fn worker_protocol_verification_cannot_skip_a_later_workspace_mutation() {
    let mut f = completed_fixture();
    let index = f
        .events
        .iter()
        .position(|e| matches!(e, Event::WorkerVerificationRecorded { .. }))
        .unwrap();
    let verification = f.events[index].clone();
    f.events.truncate(index);
    f.call(
        f.primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        f.primary,
        ToolCall::Write {
            path: "src/a".into(),
            expected_digest: Some(crate::tools::types::hex_digest(b"new")),
            content: "other".into(),
        },
        EffectTarget::Workspace,
    );
    f.started(&intent);
    f.observed(
        &intent,
        ToolDisposition::Succeeded,
        Some(Attestation::for_site(&intent)),
        &serde_json::json!({"sha256":crate::tools::types::hex_digest(b"other"), "bytes":5, "executable":false, "before":{"sha256":crate::tools::types::hex_digest(b"new"),"bytes":3,"executable":false}}).to_string(),
    );
    refused(&f, verification, "stale positive evidence after a mutation");
}

#[test]
fn worker_protocol_sequence_ranges_allow_other_committed_facts() {
    let f = completed_fixture();
    let mut events = f.envelopes();
    for e in &mut events {
        e.sequence *= 2;
        match &mut e.event {
            Event::AgentSpawned {
                profile: None,
                assignment: Some(p),
                ..
            } => {
                p.base.first_sequence *= 2;
                p.base.seal_sequence *= 2;
            }
            Event::ToolFinished { observation, .. } => {
                if let Some(Attestation::WorkerData { data, .. }) = &mut observation.attestation {
                    data.first_sequence *= 2;
                    data.seal_sequence *= 2;
                }
                if let Some(Attestation::WorkerMutation { mutation, .. }) =
                    &mut observation.attestation
                {
                    mutation.data.first_sequence *= 2;
                    mutation.data.seal_sequence *= 2;
                }
            }
            Event::WorkerDataSealed { record, .. } => {
                record.data.first_sequence *= 2;
                record.data.seal_sequence *= 2;
            }
            Event::AgentFinished {
                worker: Some(o), ..
            } => {
                if let Some(data) = &mut o.data {
                    data.first_sequence *= 2;
                    data.seal_sequence *= 2;
                }
            }
            Event::WorkerHandoverRecorded { record, .. } => {
                record.criterion_evidence.iter_mut().for_each(|s| *s *= 2);
                record.data.first_sequence *= 2;
                record.data.seal_sequence *= 2;
            }
            Event::WorkerIntegrationRecorded { record, .. } => {
                record.handover_sequence *= 2;
                record.finished_sequence *= 2;
            }
            Event::WorkerLeaseReleased { record, .. } => {
                record.handover_sequence = record.handover_sequence.map(|s| s * 2);
                record.cause_sequence *= 2;
            }
            Event::WorkerVerificationRecorded { record, .. } => {
                record.evidence.iter_mut().for_each(|s| *s *= 2)
            }
            Event::ToolStarted { intent, .. } => {
                if let ToolCall::ApplyChangeset {
                    source:
                        ChangesetSource::WorkerHandover {
                            handover_sequence,
                            data,
                            ..
                        },
                } = &mut intent.call
                {
                    *handover_sequence *= 2;
                    data.first_sequence *= 2;
                    data.seal_sequence *= 2;
                }
            }
            _ => (),
        }
        if let Event::ToolFinished { observation, .. } = &mut e.event
            && let ToolCall::ApplyChangeset {
                source:
                    ChangesetSource::WorkerHandover {
                        handover_sequence,
                        data,
                        ..
                    },
            } = &mut observation.intent.call
        {
            *handover_sequence *= 2;
            data.first_sequence *= 2;
            data.seal_sequence *= 2;
            observation.attestation = Some(Attestation::for_site(&observation.intent));
        }
    }
    // The global Store sequence can have gaps belonging to another Session.
    assert_eq!(
        SessionView::replay(f.session, &events)
            .unwrap()
            .unwrap()
            .runs[0]
            .status,
        RunStatus::Finished
    );
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
        f.target(0),
    );
    f.started(&intent);
    f.push(Event::ToolPermissionDecided {
        run_id: f.run,
        agent_run_id: f.child,
        verdict: PermissionVerdict {
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
        },
    });
    let data = f.data(
        &intent,
        WorkerDataPurpose::Mutation,
        &serde_json::to_vec(&f.changes).unwrap(),
    );
    refused(
        &f,
        Event::ToolFinished {
            checkpoint: None,
            run_id: f.run,
            agent_run_id: f.child,
            observation: ToolObservation {
                intent: intent.clone(),
                disposition: ToolDisposition::Succeeded,
                enforcement_cause: None,
                limit_cause: None,
                output: "{}".into(),
                attestation: Some(Attestation::WorkerMutation {
                    receipt: receipt(&intent),
                    mutation: Box::new(WorkerMutation {
                        previous_revision: 0,
                        next_revision: 1,
                        data,
                    }),
                }),
                cache_update: CacheUpdate::NotApplicable,
            },
        },
        "child acceptEdits cannot erase parent Ask",
    );
}

#[test]
fn worker_protocol_publication_cannot_be_reclassified_as_discarded() {
    for disposition in [ToolDisposition::Succeeded, ToolDisposition::Uncertain] {
        let mut f = fixture();
        f.mutation();
        let data = f.finish_worker();
        let h = f.handover(data.clone());
        f.integrate(h, data, disposition);
        let stopped = f.push(Event::WorkerStopObserved {
            run_id: f.run,
            agent_run_id: f.primary,
            record: WorkerStopRecord {
                reason: WorkerStopReason::Cancelled,
            },
        });
        refused(
            &f,
            Event::WorkerHandoverDiscarded {
                run_id: f.run,
                agent_run_id: f.primary,
                record: WorkerSettlement {
                    assignment_id: f.child,
                    handover_sequence: Some(h),
                    reason: WorkerSettlementReason::Stopped,
                    cause_sequence: stopped,
                },
            },
            "publication receipt or uncertainty survives stop",
        );
    }
}

#[test]
fn worker_protocol_in_flight_integration_blocks_discard_and_release() {
    for observed in [false, true] {
        let mut f = fixture();
        f.mutation();
        let data = f.finish_worker();
        let h = f.handover(data.clone());
        let intent = f.intent(
            f.primary,
            ToolCall::ApplyChangeset {
                source: ChangesetSource::WorkerHandover {
                    assignment_id: f.child,
                    handover_sequence: h,
                    data,
                },
            },
            EffectTarget::Workspace,
        );
        f.started(&intent);
        if observed {
            let attestation = Some(Attestation::Guard(receipt(&intent)));
            f.observed(&intent, ToolDisposition::Succeeded, attestation, "{}");
        }
        let stopped = f.push(Event::WorkerStopObserved {
            run_id: f.run,
            agent_run_id: f.primary,
            record: WorkerStopRecord {
                reason: WorkerStopReason::Cancelled,
            },
        });
        let settlement = WorkerSettlement {
            assignment_id: f.child,
            handover_sequence: Some(h),
            reason: WorkerSettlementReason::Stopped,
            cause_sequence: stopped,
        };
        refused(
            &f,
            Event::WorkerHandoverDiscarded {
                run_id: f.run,
                agent_run_id: f.primary,
                record: settlement.clone(),
            },
            "discard while the integration is in flight",
        );
        refused(
            &f,
            Event::WorkerLeaseReleased {
                run_id: f.run,
                agent_run_id: f.primary,
                record: settlement,
            },
            "release while the integration is in flight",
        );
    }
}

#[test]
fn worker_protocol_refused_base_capture_spends_its_slot_and_other_workers_proceed() {
    let mut f = fixture();
    let first = f.child;
    let cut = f
        .events
        .iter()
        .position(|e| {
            matches!(e, Event::ToolStarted { intent, .. }
                if matches!(intent.call, ToolCall::PrepareWorker { .. }))
        })
        .unwrap();
    f.events.truncate(cut);
    let mut second = None;
    for event in &mut f.events {
        if let Event::ProviderCallRecorded { record, .. } = event
            && let Some(plan) = record.delegation.as_mut()
        {
            let ChildAssignment::Editor { assignment } = plan[0].clone() else {
                unreachable!()
            };
            let mut other = assignment;
            other.lease = vec!["src/b".into()];
            other.criterion = GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/b".into(),
                content: "new".into(),
            };
            other.mission = "Edit src b".into();
            plan.push(ChildAssignment::Editor {
                assignment: other.clone(),
            });
            second = Some(*other);
        }
    }
    let second = second.unwrap();
    let refused = f.intent(
        f.primary,
        ToolCall::PrepareWorker {
            preparation: WorkerPreparation {
                assignment_id: first,
                purpose: WorkerDataPurpose::Base,
                max_bytes: 4096,
                max_chunks: 1,
            },
        },
        EffectTarget::Workspace,
    );
    f.started(&refused);
    f.observed(&refused, ToolDisposition::Denied, None, "refused");
    f.child = AgentRunId::new();
    let prep = f.intent(
        f.primary,
        ToolCall::PrepareWorker {
            preparation: WorkerPreparation {
                assignment_id: f.child,
                purpose: WorkerDataPurpose::Base,
                max_bytes: 4096,
                max_chunks: 1,
            },
        },
        EffectTarget::Workspace,
    );
    f.started(&prep);
    f.base = f.data(
        &prep,
        WorkerDataPurpose::Base,
        &serde_json::to_vec(&WorkerManifest {
            entries: vec![WorkerManifestEntry {
                path: "src".into(),
                version: WorkerBaseVersion::Directory,
            }],
        })
        .unwrap(),
    );
    f.observed(
        &prep,
        ToolDisposition::Succeeded,
        Some(Attestation::WorkerData {
            guard: None,
            intent_digest: prep.digest(),
            data: f.base.clone(),
        }),
        "{}",
    );
    let grant = WorkerGrant {
        commands: Vec::new(),
        read_paths: vec!["src".into()],
        write_paths: second.lease.clone(),
        tools: vec![WorkerTool::Read, WorkerTool::Write],
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        permission_mode: None,
    };
    f.push(Event::AgentSpawned {
        run_id: f.run,
        agent_run_id: f.child,
        role: AgentRole::Child,
        ordinal: 1,
        objective: Some(second.mission.clone()),
        grant: Some(ChildGrant::EditOnly {
            unguarded: crate::tools::UnguardedGrant::Denied,
        }),
        definition: Some("editor".into()),
        profile: None,
        assignment: Some(WorkerAssignment {
            cache_generation: None,
            request: second,
            grant,
            policy_digest: [1; 32],
            permissions_digest: None,
            base: f.base.clone(),
            data_bytes: WORKER_DATA_BYTES as u32,
        }),
    });
    let view = f.view();
    assert_eq!(view.runs[0].agents.len(), 2);
    for event in &mut f.events {
        if let Event::ToolFinished { observation, .. } = event
            && observation.intent.id == refused.id
        {
            observation.disposition = ToolDisposition::Failed;
        }
    }
    assert_eq!(f.view().runs[0].agents.len(), 2);
}

#[test]
fn worker_protocol_private_reads_require_the_worker_read_attestation() {
    let mut f = fixture();
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        f.child,
        ToolCall::Read {
            path: "src/base".into(),
            offset: None,
            limit: Some(32),
        },
        f.target(0),
    );
    f.started(&intent);
    let finished = |attestation| Event::ToolFinished {
        checkpoint: None,
        run_id: f.run,
        agent_run_id: f.child,
        observation: ToolObservation {
            intent: intent.clone(),
            disposition: ToolDisposition::Succeeded,
            attestation: Some(attestation),
            output: "{}".into(),
            enforcement_cause: None,
            limit_cause: None,
            cache_update: CacheUpdate::NotApplicable,
        },
    };
    for wrong in [
        Attestation::WorkerRead {
            intent_digest: [9; 32],
            guard: None,
        },
        Attestation::for_site(&intent),
        Attestation::WorkerRead {
            intent_digest: intent.digest(),
            guard: Some(Box::new(receipt(&intent))),
        },
    ] {
        refused(&f, finished(wrong), "private read attestation");
    }
    f.events.push(finished(Attestation::WorkerRead {
        intent_digest: intent.digest(),
        guard: None,
    }));
    f.view();
}

#[test]
fn worker_protocol_background_workers_let_the_primary_continue_before_release() {
    for background in [false, true] {
        let mut f = fixture();
        if background {
            for event in &mut f.events {
                match event {
                    Event::ProviderCallRecorded { record, .. } => {
                        for plan in record.delegation.iter_mut().flatten() {
                            if let ChildAssignment::Editor { assignment } = plan {
                                assignment.mode = WorkerMode::Background;
                            }
                        }
                    }
                    Event::AgentSpawned {
                        profile: None,
                        assignment: Some(pins),
                        ..
                    } => pins.request.mode = WorkerMode::Background,
                    _ => {}
                }
            }
        }
        f.push(Event::GoalProviderCallReserved {
            goal_id: f.goal,
            run_id: f.run,
            agent_run_id: f.primary,
            active_ceiling_ms: 0,
        });
        let recorded = Event::ProviderCallRecorded {
            run_id: f.run,
            agent_run_id: f.primary,
            record: ProviderCallRecord {
                rate_limits: None,
                delegation: None,
                phase: AgentPhase::RootSynthesis,
                disposition: ProviderCallDisposition::ToolRequested,
                requested_tools: 1,
                response_id: None,
                input_tokens: Some(0),
                output_tokens: Some(0),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                wire_provenance: None,
                refusal: None,
                failure_reason: None,
            },
        };
        if background {
            f.push(recorded);
            f.view();
        } else {
            refused(&f, recorded, "foreground worker still active");
        }
    }
}

#[test]
fn worker_protocol_lineage_accepts_only_released_children_of_the_goal() {
    for reference in ["released", "unknown"] {
        let mut f = fixture();
        let first = f.child;
        f.mutation();
        let data = f.finish_worker();
        let h = f.handover(data.clone());
        let integrated = f.integrate(h, data, ToolDisposition::Succeeded);
        f.release(Some(h), integrated);
        f.verify();
        let Some(Event::ProviderCallRecorded { record, .. }) = f
            .events
            .iter()
            .find(|e| matches!(e, Event::ProviderCallRecorded { record, .. } if record.delegation.is_some()))
        else {
            unreachable!()
        };
        let ChildAssignment::Editor { assignment } = record.delegation.as_ref().unwrap()[0].clone()
        else {
            unreachable!()
        };
        let mut request = *assignment;
        request.lease = vec!["src/c".into()];
        request.criterion = GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/c".into(),
            content: "new".into(),
        };
        request.mission = "Edit src c".into();
        request.wave = 2;
        request.resumes = Some(if reference == "released" {
            first
        } else {
            AgentRunId::new()
        });
        f.push(Event::GoalProviderCallReserved {
            goal_id: f.goal,
            run_id: f.run,
            agent_run_id: f.primary,
            active_ceiling_ms: 0,
        });
        let recorded = Event::ProviderCallRecorded {
            run_id: f.run,
            agent_run_id: f.primary,
            record: ProviderCallRecord {
                rate_limits: None,
                delegation: Some(vec![ChildAssignment::Editor {
                    assignment: Box::new(request.clone()),
                }]),
                phase: AgentPhase::RootSynthesis,
                disposition: ProviderCallDisposition::Delegated,
                requested_tools: 0,
                response_id: None,
                input_tokens: Some(0),
                output_tokens: Some(0),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                wire_provenance: None,
                refusal: None,
                failure_reason: None,
            },
        };
        if reference == "unknown" {
            refused(&f, recorded, "resumes an unknown child");
            continue;
        }
        f.push(recorded);
        f.child = AgentRunId::new();
        let prep = f.intent(
            f.primary,
            ToolCall::PrepareWorker {
                preparation: WorkerPreparation {
                    assignment_id: f.child,
                    purpose: WorkerDataPurpose::Base,
                    max_bytes: 4096,
                    max_chunks: 1,
                },
            },
            EffectTarget::Workspace,
        );
        f.started(&prep);
        f.base = f.data(
            &prep,
            WorkerDataPurpose::Base,
            &serde_json::to_vec(&WorkerManifest {
                entries: vec![WorkerManifestEntry {
                    path: "src".into(),
                    version: WorkerBaseVersion::Directory,
                }],
            })
            .unwrap(),
        );
        f.observed(
            &prep,
            ToolDisposition::Succeeded,
            Some(Attestation::WorkerData {
                guard: None,
                intent_digest: prep.digest(),
                data: f.base.clone(),
            }),
            "{}",
        );
        let grant = WorkerGrant {
            commands: Vec::new(),
            read_paths: vec!["src".into()],
            write_paths: request.lease.clone(),
            tools: vec![WorkerTool::Read, WorkerTool::Write],
            skills: Vec::new(),
            mcp_servers: Vec::new(),
            permission_mode: None,
        };
        f.push(Event::AgentSpawned {
            run_id: f.run,
            agent_run_id: f.child,
            role: AgentRole::Child,
            ordinal: 2,
            objective: Some(request.mission.clone()),
            grant: Some(ChildGrant::EditOnly {
                unguarded: crate::tools::UnguardedGrant::Denied,
            }),
            definition: Some("editor".into()),
            profile: None,
            assignment: Some(WorkerAssignment {
                cache_generation: None,
                request,
                grant,
                policy_digest: [1; 32],
                permissions_digest: None,
                base: f.base.clone(),
                data_bytes: WORKER_DATA_BYTES as u32,
            }),
        });
        assert_eq!(f.view().runs[0].agents.len(), 3);
    }
}

#[test]
fn worker_protocol_stop_after_publication_still_records_and_releases_as_integrated() {
    let mut f = fixture();
    f.mutation();
    let data = f.finish_worker();
    let h = f.handover(data.clone());
    let intent = f.intent(
        f.primary,
        ToolCall::ApplyChangeset {
            source: ChangesetSource::WorkerHandover {
                assignment_id: f.child,
                handover_sequence: h,
                data,
            },
        },
        EffectTarget::Workspace,
    );
    f.started(&intent);
    let attestation = Some(Attestation::Guard(receipt(&intent)));
    let finished = f.observed(&intent, ToolDisposition::Succeeded, attestation, "{}");
    f.push(Event::WorkerStopObserved {
        run_id: f.run,
        agent_run_id: f.primary,
        record: WorkerStopRecord {
            reason: WorkerStopReason::Cancelled,
        },
    });
    let recorded = f.push(Event::WorkerIntegrationRecorded {
        run_id: f.run,
        agent_run_id: f.primary,
        record: WorkerIntegration {
            assignment_id: f.child,
            handover_sequence: h,
            tool_id: intent.id,
            finished_sequence: finished,
            outcome: WorkerIntegrationOutcome::Integrated,
        },
    });
    f.release(Some(h), recorded);
    f.view();
}

#[test]
fn worker_protocol_command_criterion_requires_both_command_and_its_publication() {
    let mut f = command_fixture();
    let call = ToolCall::Command {
        program: "check".into(),
        args: Vec::new(),
        cwd: ".".into(),
    };
    let criterion = GoalSuccessCriterion::ToolCallSucceeded {
        call: Box::new(call.clone()),
    };
    for event in &mut f.events {
        match event {
            Event::GoalPlanContractRecorded { contract, .. } => {
                contract.commands = vec!["check".into()]
            }
            Event::ProviderCallRecorded { record, .. } => {
                if let Some(assignments) = &mut record.delegation
                    && let ChildAssignment::Editor { assignment } = &mut assignments[0]
                {
                    assignment.criterion = criterion.clone();
                }
            }
            Event::AgentSpawned {
                profile: None,
                assignment: Some(p),
                ..
            } => p.request.criterion = criterion.clone(),
            _ => (),
        }
    }
    f.mutation();
    f.command(PrivateCapture::None {});
    let data = f.finish_worker();
    let h = f.handover(data.clone());
    let integration = f.integrate(h, data, ToolDisposition::Succeeded);
    f.release(Some(h), integration);
    f.call(
        f.primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let command = f.intent(f.primary, call, EffectTarget::Workspace);
    f.started(&command);
    let changeset = crate::tools::CommandChangeset {
        files: vec![FileChange {
            path: "src/artifact".into(),
            before: None,
            after: Some(FileVersion::from_bytes(b"ok", false)),
        }],
    };
    let command_seq = f.observed(
        &command,
        ToolDisposition::Succeeded,
        Some(Attestation::for_site(&command)),
        &serde_json::to_string(&crate::tools::types::CommandResult {
            selection: Some(Default::default()),
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            stdout_total: Default::default(),
            stderr_total: Default::default(),
            changeset: changeset.clone(),
        })
        .unwrap(),
    );
    let apply = f.intent(
        f.primary,
        ToolCall::ApplyChangeset {
            source: ChangesetSource::CommandReceipt {
                source_intent_digest: command.digest(),
                changeset,
            },
        },
        EffectTarget::Workspace,
    );
    f.started(&apply);
    let apply_seq = f.observed(
        &apply,
        ToolDisposition::Succeeded,
        Some(Attestation::for_site(&apply)),
        "{}",
    );
    let verification = WorkerVerification {
        assignment_id: f.child,
        criterion_digest: crate::tools::types::digest(&serde_json::to_vec(&criterion).unwrap()),
        passed: true,
        evidence: vec![command_seq, apply_seq],
    };
    refused(
        &f,
        Event::WorkerVerificationRecorded {
            run_id: f.run,
            agent_run_id: f.primary,
            record: WorkerVerification {
                evidence: vec![apply_seq],
                ..verification.clone()
            },
        },
        "integration alone cannot prove command success",
    );
    f.push(Event::WorkerVerificationRecorded {
        run_id: f.run,
        agent_run_id: f.primary,
        record: verification,
    });
    f.close();
    f.view();
}

#[test]
fn worker_protocol_known_private_failure_stops_only_its_actor() {
    let mut f = fixture();
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
        f.target(0),
    );
    f.started(&intent);
    f.observed(
        &intent,
        ToolDisposition::Conflict,
        None,
        "private snapshot conflict",
    );
    refused(
        &f,
        Event::GoalProviderCallReserved {
            goal_id: f.goal,
            run_id: f.run,
            agent_run_id: f.child,
            active_ceiling_ms: 0,
        },
        "failed worker continuation",
    );
    let finished = f.push(Event::AgentFinished {
        run_id: f.run,
        agent_run_id: f.child,
        disposition: AgentDisposition::Failed,
        summary: None,
        result: None,
        worker: Some(WorkerOutcome {
            disposition: WorkerDisposition::Failed,
            cause: WorkerCause::ToolFailed,
            data: None,
            cleanup: Some(WorkerCleanupReceipt {
                assignment_id: f.child,
                base_digest: f.base.sha256,
                revision: 0,
                enforcement_digest: intent.enforcement_digest,
                last_intent_digest: Some(intent.digest()),
            }),
        }),
    });
    let record = WorkerSettlement {
        assignment_id: f.child,
        handover_sequence: None,
        reason: WorkerSettlementReason::Failed,
        cause_sequence: finished,
    };
    let discarded = f.push(Event::WorkerHandoverDiscarded {
        run_id: f.run,
        agent_run_id: f.primary,
        record: record.clone(),
    });
    f.push(Event::WorkerLeaseReleased {
        run_id: f.run,
        agent_run_id: f.primary,
        record: WorkerSettlement {
            cause_sequence: discarded,
            ..record
        },
    });
    f.close();
    f.view();
}

#[test]
fn worker_protocol_pairs_each_engine_cause_with_its_dispositions() {
    use AgentDisposition as A;
    use WorkerCause as C;
    use WorkerDisposition as W;
    let mut f = fixture();
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
        f.target(0),
    );
    f.started(&intent);
    f.observed(
        &intent,
        ToolDisposition::Conflict,
        None,
        "private snapshot conflict",
    );
    let finished = |agent, disposition, cause| Event::AgentFinished {
        run_id: f.run,
        agent_run_id: f.child,
        disposition: agent,
        summary: None,
        result: None,
        worker: Some(WorkerOutcome {
            disposition,
            cause,
            data: None,
            cleanup: Some(WorkerCleanupReceipt {
                assignment_id: f.child,
                base_digest: f.base.sha256,
                revision: 0,
                enforcement_digest: intent.enforcement_digest,
                last_intent_digest: Some(intent.digest()),
            }),
        }),
    };
    for (agent, disposition, cause) in [
        (A::Failed, W::Failed, C::ToolFailed),
        (A::Failed, W::Failed, C::InvalidOutcome),
        (A::Failed, W::Failed, C::TurnLimit),
        (A::Failed, W::Failed, C::Budget),
        (A::Failed, W::Failed, C::CriterionUnverified),
        (A::Cancelled, W::Cancelled, C::Cancelled),
    ] {
        let mut view = f.view();
        let envelope = f.envelope(
            finished(agent, disposition, cause),
            f.events.len() as u64 + 1,
        );
        assert!(
            view.apply_committed(&envelope).is_ok(),
            "{agent:?} {disposition:?} {cause:?}"
        );
    }
    for (agent, disposition, cause) in [
        (A::Failed, W::Failed, C::Cancelled),
        (A::Failed, W::Cancelled, C::Cancelled),
        (A::Cancelled, W::Failed, C::ToolFailed),
        (A::Cancelled, W::Cancelled, C::ToolFailed),
        (A::Failed, W::Failed, C::StoppedByHook),
        (A::StoppedByHook, W::Failed, C::InvalidOutcome),
        (A::StoppedByHook, W::Failed, C::StoppedByHook),
    ] {
        refused(
            &f,
            finished(agent, disposition, cause),
            &format!("{agent:?} {disposition:?} {cause:?}"),
        );
    }
}

#[test]
fn worker_protocol_stop_blocks_an_otherwise_admitted_provider_fallback() {
    let mut f = fixture();
    let Event::RunStarted { config, .. } = f
        .events
        .iter_mut()
        .find(|e| matches!(e, Event::RunStarted { .. }))
        .unwrap()
    else {
        unreachable!()
    };
    let initial = config.identity().unwrap();
    let mut next = initial.clone();
    next.model = "alternate-model".into();
    config.providers = Some(crate::provider::Providers::all(vec![
        initial.clone(),
        next.clone(),
    ]));
    f.push(Event::GoalProviderCallReserved {
        goal_id: f.goal,
        run_id: f.run,
        agent_run_id: f.child,
        active_ceiling_ms: 0,
    });
    let fallback = Event::ProviderFallbackUsed {
        run_id: f.run,
        agent_run_id: f.child,
        fallback: crate::provider::ProviderFallback {
            rate_limits: None,
            role: crate::provider::ProviderRole::Child,
            call_index: 0,
            failure: ProviderCallDisposition::Unavailable,
            from: initial,
            to: next,
        },
    };
    let mut control = fixture_from(&f);
    control.push(fallback.clone());
    control.view();
    f.push(Event::WorkerStopObserved {
        run_id: f.run,
        agent_run_id: f.primary,
        record: WorkerStopRecord {
            reason: WorkerStopReason::Paused,
        },
    });
    refused(&f, fallback, "stopped fallback dispatch");
}

#[test]
fn worker_protocol_absent_subtree_accepts_private_mkdir_then_file_creation() {
    let mut f = fixture_with_manifest(
        WorkerManifest {
            entries: vec![WorkerManifestEntry {
                path: "src/a".into(),
                version: WorkerBaseVersion::Absent,
            }],
        },
        vec!["mkdir".into(), "read".into(), "write".into()],
        vec![WorkerTool::Read, WorkerTool::Write, WorkerTool::Mkdir],
    );
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let mkdir = f.intent(
        f.child,
        ToolCall::Mkdir {
            path: "src/a".into(),
        },
        f.target(0),
    );
    f.started(&mkdir);
    let data = f.data(
        &mkdir,
        WorkerDataPurpose::Mutation,
        &serde_json::to_vec(&WorkerChangeset { files: Vec::new() }).unwrap(),
    );
    f.observed(
        &mkdir,
        ToolDisposition::Succeeded,
        Some(Attestation::WorkerMutation {
            receipt: receipt(&mkdir),
            mutation: Box::new(WorkerMutation {
                previous_revision: 0,
                next_revision: 1,
                data,
            }),
        }),
        "{}",
    );
    f.changes.files[0].path = "src/a/file".into();
    for e in &mut f.events {
        let request = match e {
            Event::AgentSpawned {
                assignment: Some(p),
                ..
            } => Some(&mut p.request),
            Event::ProviderCallRecorded { record, .. } => {
                record.delegation.as_mut().and_then(|d| match &mut d[0] {
                    ChildAssignment::Editor { assignment } => Some(assignment.as_mut()),
                    _ => None,
                })
            }
            _ => None,
        };
        if let Some(r) = request {
            r.criterion = GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/a/file".into(),
                content: "new".into(),
            };
        }
    }
    f.mutation();
    f.finish_worker();
    let view = f.view();
    let worker = view.runs[0]
        .agents
        .iter()
        .find(|a| a.id == f.child)
        .unwrap()
        .assignment
        .as_ref()
        .unwrap();
    assert_eq!(worker.revision, 2);
    assert_eq!(worker.overlay.len(), 1);
    assert_eq!(worker.overlay[0].before, WorkerBaseVersion::Absent);
}

#[test]
fn worker_protocol_unverified_synthesis_is_recorded_but_cannot_finish() {
    let mut f = fixture();
    f.mutation();
    let data = f.finish_worker();
    let h = f.handover(data.clone());
    let i = f.integrate(h, data, ToolDisposition::Succeeded);
    f.release(Some(h), i);
    f.call(
        f.primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::Finished,
        None,
    );
    let events = f.envelopes();
    let decoded: Vec<_> = events
        .iter()
        .map(|e| EventEnvelope {
            event: Event::decode(
                e.event.kind(),
                e.event.version(),
                &e.event.payload().unwrap(),
                e.run_id,
                e.agent_run_id,
            )
            .unwrap(),
            ..e.clone()
        })
        .collect();
    assert!(SessionView::replay(f.session, &decoded).unwrap().is_some());
    refused(
        &f,
        Event::AgentFinished {
            run_id: f.run,
            agent_run_id: f.primary,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
            worker: None,
        },
        "finish without fresh verification",
    );
}

#[test]
fn worker_hook_replay_binds_private_tools_stop_and_handover_to_the_actor() {
    use crate::tools::permissions::{
        Decision, HookEvent, HookInvocation, HookOutcome, HookResult, PermissionMode,
        PermissionSource, PermissionVerdict, stop_subject,
    };
    for site in [
        "private",
        "stop",
        "stopped",
        "handover_pre",
        "handover_post",
    ] {
        let mut f = fixture();
        f.mutation();
        let event = if matches!(site, "stop" | "stopped") {
            HookEvent::SubagentStop
        } else if site == "handover_post" {
            HookEvent::PostToolUse
        } else {
            HookEvent::PreToolUse
        };
        let actor = if site.starts_with("handover") {
            f.primary
        } else {
            f.child
        };
        if matches!(site, "stop" | "stopped") {
            f.call(
                f.child,
                AgentPhase::ChildWork,
                ProviderCallDisposition::Finished,
                None,
            );
        } else if site.starts_with("handover") {
            let data = f.finish_worker();
            let sequence = f.handover(data.clone());
            f.integrate(sequence, data, ToolDisposition::Succeeded);
            f.events.pop();
            if site == "handover_pre" {
                f.events.pop();
            }
        } else {
            let start = f
                .events
                .iter()
                .rposition(|event| matches!(event, Event::ToolStarted { .. }))
                .unwrap();
            f.events.truncate(start + 1);
        }
        let mut view = f.view();
        view.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap()
            .permissions_digest = Some([9; 32]);
        let tool = if actor == f.primary {
            view.runs[0].tools.last().unwrap()
        } else {
            view.runs[0].agents[1].tools.last().unwrap()
        };
        let pending = tool.intent.clone();
        let subject = if matches!(site, "stop" | "stopped") {
            stop_subject(actor, view.runs[0].agents[1].provider_calls.len() - 1)
        } else {
            pending.digest()
        };
        let intent = f.intent(
            actor,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 2".into()],
                cwd: String::new(),
            },
            EffectTarget::Workspace,
        );
        let started = Event::HookStarted {
            run_id: f.run,
            agent_run_id: actor,
            hook: HookInvocation {
                event,
                group: 0,
                handler: 0,
                subject_digest: subject,
                input_digest: [8; 32],
                intent: intent.clone(),
                active_ceiling_ms: (matches!(site, "stop" | "stopped")).then_some(1),
            },
        };
        let append = |view: &mut SessionView, event: Event| {
            let sequence = view.last_sequence + 1;
            view.apply_committed(&f.envelope(event, sequence))
        };
        let mut foreign = started.clone();
        if let Event::HookStarted { agent_run_id, .. } = &mut foreign {
            *agent_run_id = if actor == f.primary {
                f.child
            } else {
                f.primary
            };
        }
        assert!(append(&mut view.clone(), foreign).is_err(), "{site}");
        append(&mut view, started).unwrap();
        assert!(!view.runs[0].hooks_settled(), "{site}");
        let result = if site == "stopped" {
            HookResult::Stopped {
                reason: Some("halt".into()),
            }
        } else if site == "stop" {
            HookResult::Blocked {
                reason: Some("blocked".into()),
            }
        } else if site == "handover_post" {
            HookResult::Feedback {
                lines: vec!["blocked".into()],
            }
        } else {
            HookResult::Restricted {
                decision: Decision::Deny,
                reason: Some("blocked".into()),
            }
        };
        let observation = ToolObservation {
            attestation: Some(Attestation::Guard(receipt(&intent))),
            intent,
            disposition: if site == "stopped" {
                ToolDisposition::Succeeded
            } else {
                ToolDisposition::Failed
            },
            output: if site == "stopped" {
                serde_json::json!({"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":r#"{"continue":false,"stopReason":"halt"}"#,"stderr":"","stdout_total":{"bytes":(r#"{"continue":false,"stopReason":"halt"}"#).len(),"lines":(r#"{"continue":false,"stopReason":"halt"}"#).lines().count()},"stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}}).to_string()
            } else {
                serde_json::json!({"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":2,"stdout":"","stderr":"blocked","stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":7,"lines":1},"changeset":{"files":[]}}).to_string()
            },
            cache_update: CacheUpdate::NotApplicable,
            enforcement_cause: None,
            limit_cause: None,
        };
        append(
            &mut view,
            Event::HookFinished {
                run_id: f.run,
                agent_run_id: actor,
                outcome: HookOutcome {
                    observation,
                    result,
                    boundary_active_ms: None,
                },
            },
        )
        .unwrap();
        assert!(view.runs[0].hooks_settled(), "{site}");
        if event == HookEvent::PreToolUse {
            let verdict = |narrowed| Event::ToolPermissionDecided {
                run_id: f.run,
                agent_run_id: actor,
                verdict: PermissionVerdict {
                    host: None,
                    intent_digest: pending.digest(),
                    mode: Some(PermissionMode::Default),
                    decision: Decision::Deny,
                    basis: PermissionSource::Hook,
                    source: PermissionSource::Hook,
                    approved: false,
                    hook_restriction: Some(Decision::Deny),
                    hooks: 1,
                    narrowed,
                },
            };
            if actor != f.primary {
                assert!(append(&mut view.clone(), verdict(true)).is_err(), "{site}");
            }
            append(&mut view, verdict(false)).unwrap();
            append(
                &mut view,
                Event::ToolFinished {
                    checkpoint: None,
                    run_id: f.run,
                    agent_run_id: actor,
                    observation: ToolObservation {
                        intent: pending,
                        disposition: ToolDisposition::Denied,
                        output: "Hook denied this Tool: blocked".into(),
                        attestation: None,
                        cache_update: CacheUpdate::NotApplicable,
                        enforcement_cause: None,
                        limit_cause: None,
                    },
                },
            )
            .unwrap();
        } else if site == "stopped" {
            assert!(view.runs[0].actor_stopped_by_hook(actor));
            let finished = |disposition, cause| Event::AgentFinished {
                run_id: f.run,
                agent_run_id: actor,
                disposition,
                summary: None,
                result: None,
                worker: Some(WorkerOutcome {
                    disposition: WorkerDisposition::Failed,
                    cause,
                    data: None,
                    cleanup: Some(WorkerCleanupReceipt {
                        assignment_id: actor,
                        base_digest: f.base.sha256,
                        revision: 1,
                        enforcement_digest: pending.enforcement_digest,
                        last_intent_digest: Some(pending.digest()),
                    }),
                }),
            };
            for (disposition, cause) in [
                (AgentDisposition::Failed, WorkerCause::StoppedByHook),
                (AgentDisposition::StoppedByHook, WorkerCause::ToolFailed),
            ] {
                assert!(append(&mut view.clone(), finished(disposition, cause)).is_err());
            }
            append(
                &mut view,
                Event::AgentFinished {
                    run_id: f.run,
                    agent_run_id: actor,
                    disposition: AgentDisposition::StoppedByHook,
                    summary: None,
                    result: None,
                    worker: Some(WorkerOutcome {
                        disposition: WorkerDisposition::Failed,
                        cause: WorkerCause::StoppedByHook,
                        data: None,
                        cleanup: Some(WorkerCleanupReceipt {
                            assignment_id: actor,
                            base_digest: f.base.sha256,
                            revision: 1,
                            enforcement_digest: pending.enforcement_digest,
                            last_intent_digest: Some(pending.digest()),
                        }),
                    }),
                },
            )
            .unwrap();
        } else if site == "stop" {
            assert!(view.runs[0].stop_blocked(actor));
            let unstopped = Event::AgentFinished {
                run_id: f.run,
                agent_run_id: actor,
                disposition: AgentDisposition::StoppedByHook,
                summary: None,
                result: None,
                worker: Some(WorkerOutcome {
                    disposition: WorkerDisposition::Failed,
                    cause: WorkerCause::StoppedByHook,
                    data: None,
                    cleanup: Some(WorkerCleanupReceipt {
                        assignment_id: actor,
                        base_digest: f.base.sha256,
                        revision: 1,
                        enforcement_digest: pending.enforcement_digest,
                        last_intent_digest: Some(pending.digest()),
                    }),
                }),
            };
            assert!(append(&mut view.clone(), unstopped).is_err());
        }
    }
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

fn supervised_handover() -> (Fixture, u64, WorkerDataRef, Event) {
    let mut f = fixture();
    for event in &mut f.events {
        match event {
            Event::RunStarted { config, .. } => {
                config.supervisor = Some(crate::supervisor::SupervisorPolicy::default())
            }
            Event::ProviderCallRecorded { record, .. } => {
                if let Some(assignments) = &mut record.delegation
                    && let ChildAssignment::Editor { assignment } = &mut assignments[0]
                {
                    assignment.lease = vec!["src".into()];
                }
            }
            Event::AgentSpawned {
                assignment: Some(pins),
                ..
            } => {
                pins.request.lease = vec!["src".into()];
                pins.grant.write_paths = vec!["src".into()];
            }
            _ => (),
        }
    }
    let files: Vec<_> = (0..16)
        .map(|i| FileChange {
            path: if i == 0 {
                "src/a".into()
            } else {
                format!("src/b{i:02}")
            },
            before: None,
            after: Some(FileVersion::from_bytes(b"new", false)),
        })
        .collect();
    for file in &files {
        f.changes = WorkerChangeset {
            files: vec![file.clone()],
        };
        f.mutation();
    }
    f.changes = WorkerChangeset { files };
    let data = f.finish_worker();
    let subject = f.handover(data.clone());
    let verdict = f.view().runs[0]
        .supervisor
        .pending_event()
        .expect("sixteen-file handover notice");
    assert!(
        matches!(&verdict,Event::SupervisorVerdictRecorded {verdict,..} if verdict.site==crate::supervisor::Site::Handover && verdict.subject_sequence==subject)
    );
    (f, subject, data, verdict)
}

#[test]
fn supervisor_verdicts_are_part_of_the_canonical_journal_prefix() {
    let (mut f, _, _, verdict) = supervised_handover();
    let without = f.envelopes();
    let omitted = crate::session::prefix_digest(&without, without.len() as u64).unwrap();
    f.push(verdict);
    let envelopes = f.envelopes();
    let canonical = crate::session::prefix_digest(&envelopes, envelopes.len() as u64).unwrap();
    assert_ne!(canonical, omitted);
    let view = SessionView::replay_open(f.session, &envelopes)
        .unwrap()
        .unwrap();
    assert_eq!(view.rewinds.prefix.digest(), canonical);
    assert_ne!(view.rewinds.prefix.digest(), omitted);
}

#[test]
fn supervisor_handover_verdict_preserves_the_original_integration_subject() {
    let (mut f, subject, data, verdict) = supervised_handover();
    f.push(verdict);
    let wrong = f.intent(
        f.primary,
        ToolCall::ApplyChangeset {
            source: ChangesetSource::WorkerHandover {
                assignment_id: f.child,
                handover_sequence: subject + 1,
                data: data.clone(),
            },
        },
        EffectTarget::Workspace,
    );
    refused(
        &f,
        Event::ToolStarted {
            run_id: f.run,
            agent_run_id: f.primary,
            intent: wrong,
            active_ceiling_ms: 0,
        },
        "verdict is not a handover",
    );
    let integration = f.integrate(subject, data, ToolDisposition::Succeeded);
    f.release(Some(subject), integration);
    f.verify();
    f.close();
    assert_eq!(f.view().runs[0].status, RunStatus::Finished);
}

#[test]
fn worker_protocol_subagent_stop_uses_the_editor_actor_and_preserves_unknown_hooks() {
    use crate::tools::permissions::{
        HookEvent, HookInvocation, HookOutcome, HookResult, stop_subject,
    };
    for result in ["passed", "blocked", "stopped", "stopped_unknown", "unknown"] {
        let mut f = fixture();
        f.call(
            f.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::Finished,
            None,
        );
        let mut view = f.view();
        view.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap()
            .permissions_digest = Some([3; 32]);
        let intent = f.intent(
            f.child,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 0".into()],
                cwd: String::new(),
            },
            EffectTarget::Workspace,
        );
        let call_index = view.runs[0].agents[1].provider_calls.len() - 1;
        let started = Event::HookStarted {
            run_id: f.run,
            agent_run_id: f.child,
            hook: HookInvocation {
                event: HookEvent::SubagentStop,
                group: 0,
                handler: 0,
                subject_digest: stop_subject(f.child, call_index),
                intent: intent.clone(),
                input_digest: [9; 32],
                active_ceiling_ms: Some(1),
            },
        };
        let sequence = view.last_sequence + 1;
        view.apply_committed(&f.envelope(started, sequence))
            .unwrap();
        let (disposition, stdout, stderr, exit, hook_result) = match result {
            "passed" => (ToolDisposition::Succeeded, "", "", 0, HookResult::Passed),
            "blocked" => (
                ToolDisposition::Failed,
                "",
                "verify",
                2,
                HookResult::Blocked {
                    reason: Some("verify".into()),
                },
            ),
            "stopped" | "stopped_unknown" => (
                ToolDisposition::Succeeded,
                r#"{"continue":false,"stopReason":"halt"}"#,
                "",
                0,
                HookResult::Stopped {
                    reason: Some("halt".into()),
                },
            ),
            _ => (
                ToolDisposition::Uncertain,
                "",
                "",
                0,
                HookResult::Ignored {
                    why: crate::tools::permissions::HookIgnoreReason::NotCompleted,
                },
            ),
        };
        let observation = ToolObservation {
            intent: intent.clone(), disposition, output: serde_json::json!({"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":exit,"stdout":stdout,"stderr":stderr,"stdout_total":{"bytes":(stdout).len(),"lines":(stdout).lines().count()},"stderr_total":{"bytes":(stderr).len(),"lines":(stderr).lines().count()},"changeset":{"files":[]}}).to_string(),
            attestation: (result != "unknown").then(|| Attestation::Guard(receipt(&intent))), enforcement_cause: None,
            limit_cause: (disposition == ToolDisposition::Limit).then_some(crate::LimitCause::Resource),
            cache_update: if result == "stopped_unknown" { CacheUpdate::Uncertain } else { CacheUpdate::NotApplicable },
        };
        let finished = Event::HookFinished {
            run_id: f.run,
            agent_run_id: f.child,
            outcome: HookOutcome {
                observation,
                result: hook_result,
                boundary_active_ms: None,
            },
        };
        let sequence = view.last_sequence + 1;
        view.apply_committed(&f.envelope(finished, sequence))
            .unwrap();
        let unknown = matches!(result, "unknown" | "stopped_unknown");
        assert_eq!(view.runs[0].hooks_settled(), !unknown);
        assert_eq!(view.runs[0].stop_blocked(f.child), result == "blocked");
        assert_eq!(
            view.runs[0].actor_stopped_by_hook(f.child),
            matches!(result, "stopped" | "stopped_unknown")
        );
        if result == "stopped_unknown" {
            // An editor whose own stop is journaled while hooks are unsettled cannot end stopped_by_hook.
            let finished = |disposition, cause| Event::AgentFinished {
                run_id: f.run,
                agent_run_id: f.child,
                disposition,
                summary: None,
                result: None,
                worker: Some(WorkerOutcome {
                    disposition: WorkerDisposition::Failed,
                    cause,
                    data: None,
                    cleanup: Some(WorkerCleanupReceipt {
                        assignment_id: f.child,
                        base_digest: f.base.sha256,
                        revision: 0,
                        enforcement_digest: [7; 32],
                        last_intent_digest: None,
                    }),
                }),
            };
            let sequence = view.last_sequence + 1;
            for (disposition, cause) in [
                (AgentDisposition::StoppedByHook, WorkerCause::StoppedByHook),
                (AgentDisposition::Failed, WorkerCause::StoppedByHook),
            ] {
                assert!(
                    view.clone()
                        .apply_committed(&f.envelope(finished(disposition, cause), sequence))
                        .is_err(),
                    "{disposition:?} {cause:?}"
                );
            }
            view.apply_committed(&f.envelope(
                finished(AgentDisposition::Failed, WorkerCause::ToolFailed),
                sequence,
            ))
            .unwrap();
        }
    }
}

fn command_fixture() -> Fixture {
    let mut f = fixture_with_manifest(
        WorkerManifest {
            entries: vec![WorkerManifestEntry {
                path: "src".into(),
                version: WorkerBaseVersion::Directory,
            }],
        },
        vec!["command".into(), "read".into(), "write".into()],
        vec![WorkerTool::Read, WorkerTool::Write, WorkerTool::Command],
    );
    for event in &mut f.events {
        match event {
            Event::RunStarted { config, .. } => {
                config.worker_policy.as_mut().unwrap().parent_grant.commands = vec!["check".into()]
            }
            Event::GoalPlanContractRecorded { contract, .. } => {
                contract.commands = vec!["check".into()]
            }
            Event::ProviderCallRecorded { record, .. } => {
                if let Some(assignments) = &mut record.delegation
                    && let ChildAssignment::Editor { assignment } = &mut assignments[0]
                {
                    assignment.commands = vec!["check".into()];
                }
            }
            Event::AgentSpawned {
                assignment: Some(p),
                ..
            } => {
                p.request.commands = vec!["check".into()];
                p.grant.commands = vec!["check".into()];
                p.cache_generation = Some([9; 32]);
            }
            _ => (),
        }
    }
    f
}

impl Fixture {
    fn command(&mut self, capture: PrivateCapture) -> EffectIntent {
        self.call(
            self.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::ToolRequested,
            None,
        );
        let revision = self.view().runs[0]
            .agents
            .iter()
            .find(|a| a.id == self.child)
            .unwrap()
            .assignment
            .as_ref()
            .unwrap()
            .revision;
        let intent = self.intent(
            self.child,
            ToolCall::Command {
                program: "check".into(),
                args: Vec::new(),
                cwd: ".".into(),
            },
            self.target(revision),
        );
        self.started(&intent);
        self.push(Event::ToolFinished {
            checkpoint: None,
            run_id: self.run,
            agent_run_id: self.child,
            observation: ToolObservation {
                intent: intent.clone(),
                disposition: ToolDisposition::Succeeded,
                enforcement_cause: None,
                limit_cause: None,
                attestation: Some(Attestation::WorkerCommand {
                    receipt: receipt(&intent),
                }),
                cache_update: CacheUpdate::Discarded,
                output: serde_json::to_string(&WorkerCommandResult {
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                    stdout_total: Default::default(),
                    stderr_total: Default::default(),
                    private_capture: capture,
                })
                .unwrap(),
            },
        });
        intent
    }
    fn command_capture(&mut self) -> EffectIntent {
        let bytes = serde_json::to_vec(&self.changes).unwrap();
        let first_sequence = self.events.len() as u64 + 5;
        let data = WorkerDataRef {
            assignment_id: self.child,
            purpose: WorkerDataPurpose::Mutation,
            first_sequence,
            seal_sequence: first_sequence + 1,
            chunks: 1,
            bytes: bytes.len() as u32,
            sha256: crate::tools::types::digest(&bytes),
        };
        let intent = self.command(PrivateCapture::Sealed { data: data.clone() });
        assert_eq!(
            self.data(&intent, WorkerDataPurpose::Mutation, &bytes),
            data
        );
        intent
    }
}

pub(crate) fn completed_command_fixture() -> Fixture {
    let mut f = command_fixture();
    f.command_capture();
    let data = f.finish_worker();
    let h = f.handover(data.clone());
    let i = f.integrate(h, data, ToolDisposition::Succeeded);
    f.release(Some(h), i);
    f.verify();
    f.close();
    f
}

#[test]
fn worker_protocol_commands_decode_and_cold_replay_without_execution() {
    let f = completed_command_fixture();
    let decoded: Vec<_> = f
        .envelopes()
        .into_iter()
        .map(|e| EventEnvelope {
            event: Event::decode(
                e.event.kind(),
                e.event.version(),
                &e.event.payload().unwrap(),
                e.run_id,
                e.agent_run_id,
            )
            .unwrap(),
            ..e
        })
        .collect();
    for end in 1..=decoded.len() {
        assert_eq!(
            SessionView::replay(f.session, &decoded[..end]).unwrap(),
            SessionView::replay(f.session, &f.envelopes()[..end]).unwrap(),
            "decoded cold prefix {end}"
        );
    }
    let view = f.view();
    assert_eq!(
        view.runs[0].agents[1].assignment.as_ref().unwrap().revision,
        1
    );
}

#[test]
fn worker_protocol_command_authority_revision_generation_and_capture_order() {
    for case in [
        "parent",
        "plan",
        "assignment",
        "grant",
        "definition",
        "unknown",
        "revision",
        "generation",
        "published",
        "workspace receipt",
        "before observation",
        "missing seal",
        "wrong seal",
        "allowance",
        "out of lease",
    ] {
        let mut f = command_fixture();
        if ["parent", "plan", "assignment", "grant", "definition"].contains(&case) {
            let mut definition_digest = None;
            for e in &mut f.events {
                match (case, e) {
                    ("parent", Event::RunStarted { config, .. }) => config
                        .worker_policy
                        .as_mut()
                        .unwrap()
                        .parent_grant
                        .commands
                        .clear(),
                    ("plan", Event::GoalPlanContractRecorded { contract, .. }) => {
                        contract.commands.clear()
                    }
                    ("assignment", Event::ProviderCallRecorded { record, .. }) => {
                        if let Some(assignments) = &mut record.delegation
                            && let ChildAssignment::Editor { assignment } = &mut assignments[0]
                        {
                            assignment.commands.push("extra".into());
                        }
                    }
                    (
                        "grant",
                        Event::AgentSpawned {
                            assignment: Some(p),
                            ..
                        },
                    ) => p.grant.commands.push("extra".into()),
                    ("definition", Event::ToolFinished { observation, .. })
                        if matches!(
                            observation.intent.call,
                            ToolCall::ParseAgentDefinition { .. }
                        ) =>
                    {
                        let mut value: serde_json::Value =
                            serde_json::from_str(&observation.output).unwrap();
                        value["Definition"]["definition"]["disallowed_tools"] =
                            serde_json::json!(["command"]);
                        let definition: crate::tools::definitions::AgentDefinition =
                            serde_json::from_value(value["Definition"]["definition"].clone())
                                .unwrap();
                        definition_digest = Some(crate::tools::types::digest(
                            &serde_json::to_vec(&definition).unwrap(),
                        ));
                        observation.output = value.to_string();
                    }
                    _ => (),
                }
            }
            if let Some(digest) = definition_digest {
                for e in &mut f.events {
                    match e {
                        Event::ProviderCallRecorded { record, .. } => {
                            if let Some(assignments) = &mut record.delegation
                                && let ChildAssignment::Editor { assignment } = &mut assignments[0]
                            {
                                assignment.definition_digest = digest;
                            }
                        }
                        Event::AgentSpawned {
                            assignment: Some(p),
                            ..
                        } => p.request.definition_digest = digest,
                        _ => (),
                    }
                }
            }
            assert!(
                SessionView::replay(f.session, &f.envelopes()).is_err(),
                "{case}"
            );
            continue;
        }
        f.command_capture();
        let start = f.events.iter().position(|e| matches!(e, Event::ToolStarted { intent, .. } if matches!(intent.call, ToolCall::Command { .. }))).unwrap();
        let observed = start + 1;
        match case {
            "unknown" | "revision" | "generation" => {
                let mutate = |intent: &mut crate::tools::EffectIntent| match case {
                    "unknown" => {
                        let ToolCall::Command { program, .. } = &mut intent.call else {
                            unreachable!()
                        };
                        *program = "extra".into();
                    }
                    _ => {
                        let EffectTarget::WorkerView {
                            revision,
                            cache_generation,
                            ..
                        } = &mut intent.target
                        else {
                            unreachable!()
                        };
                        if case == "revision" {
                            *revision = 1;
                        } else {
                            *cache_generation = None;
                        }
                    }
                };
                let Event::ToolStarted { intent, .. } = &mut f.events[start] else {
                    unreachable!()
                };
                mutate(intent);
                let Event::ToolFinished { observation, .. } = &mut f.events[observed] else {
                    unreachable!()
                };
                mutate(&mut observation.intent);
                observation.attestation = Some(Attestation::WorkerCommand {
                    receipt: receipt(&observation.intent),
                });
            }
            "published" | "workspace receipt" => {
                let Event::ToolFinished { observation, .. } = &mut f.events[observed] else {
                    unreachable!()
                };
                if case == "published" {
                    observation.cache_update = CacheUpdate::Published;
                } else {
                    observation.attestation =
                        Some(Attestation::Guard(receipt(&observation.intent)));
                }
            }
            "before observation" => f.events.swap(observed, observed + 1),
            "missing seal" => {
                f.events.truncate(observed + 1);
                f.call(
                    f.child,
                    AgentPhase::ChildWork,
                    ProviderCallDisposition::Finished,
                    None,
                );
            }
            "wrong seal" => {
                let Event::WorkerDataSealed { record, .. } = f.events.last_mut().unwrap() else {
                    unreachable!()
                };
                record.data.sha256 = [5; 32];
            }
            "allowance" => {
                for e in &mut f.events {
                    if let Event::AgentSpawned {
                        assignment: Some(p),
                        ..
                    } = e
                    {
                        p.data_bytes = p.base.bytes;
                    }
                }
            }
            "out of lease" => {
                f = command_fixture();
                f.changes.files[0].path = "src/outside".into();
                f.command_capture();
            }
            _ => unreachable!(),
        }
        assert!(
            SessionView::replay(f.session, &f.envelopes()).is_err(),
            "{case}"
        );
    }
}

#[test]
fn worker_protocol_command_criterion_requires_private_evidence() {
    let mut f = command_fixture();
    set_command_criterion(&mut f);
    f.mutation();
    f.finish_worker();
    assert!(SessionView::replay(f.session, &f.envelopes()).is_err());
}

#[test]
fn worker_protocol_denied_command_without_a_required_verdict_cannot_continue() {
    let mut f = command_fixture();
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        f.child,
        ToolCall::Command {
            program: "check".into(),
            args: Vec::new(),
            cwd: ".".into(),
        },
        f.target(0),
    );
    f.started(&intent);
    f.observed(&intent, ToolDisposition::Denied, None, "refused");
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::Finished,
        None,
    );
    assert!(SessionView::replay(f.session, &f.envelopes()).is_err());
}

fn set_command_criterion(f: &mut Fixture) {
    let criterion = GoalSuccessCriterion::ToolCallSucceeded {
        call: Box::new(ToolCall::Command {
            program: "check".into(),
            args: Vec::new(),
            cwd: ".".into(),
        }),
    };
    for e in &mut f.events {
        match e {
            Event::ProviderCallRecorded { record, .. } => {
                if let Some(d) = &mut record.delegation
                    && let ChildAssignment::Editor { assignment } = &mut d[0]
                {
                    assignment.criterion = criterion.clone();
                }
            }
            Event::AgentSpawned {
                assignment: Some(p),
                ..
            } => p.request.criterion = criterion.clone(),
            _ => (),
        }
    }
}

#[test]
fn worker_protocol_call_criterion_must_be_runnable_inside_the_lease() {
    for (call, ok) in [
        (
            ToolCall::Command {
                program: "check".into(),
                args: Vec::new(),
                cwd: ".".into(),
            },
            true,
        ),
        (
            ToolCall::Command {
                program: "check".into(),
                args: Vec::new(),
                cwd: "elsewhere".into(),
            },
            false,
        ),
        (
            ToolCall::Mkdir {
                path: "src/a/d".into(),
            },
            false,
        ),
        (
            ToolCall::Write {
                path: "src/b".into(),
                expected_digest: None,
                content: "x".into(),
            },
            false,
        ),
    ] {
        let mut f = command_fixture();
        let criterion = GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(call),
        };
        check_criterion_admission(&mut f, criterion, ok);
    }
    for (path, ok) in [("src/a", true), ("src/b", false)] {
        let mut f = command_fixture();
        let criterion = GoalSuccessCriterion::WorkspaceFileContent {
            path: path.into(),
            content: "x".into(),
        };
        check_criterion_admission(&mut f, criterion, ok);
    }
}

fn check_criterion_admission(f: &mut Fixture, criterion: GoalSuccessCriterion, ok: bool) {
    {
        for e in &mut f.events {
            match e {
                Event::ProviderCallRecorded { record, .. } => {
                    if let Some(d) = &mut record.delegation
                        && let ChildAssignment::Editor { assignment } = &mut d[0]
                    {
                        assignment.criterion = criterion.clone();
                    }
                }
                Event::AgentSpawned {
                    assignment: Some(p),
                    ..
                } => p.request.criterion = criterion.clone(),
                _ => (),
            }
        }
        assert_eq!(
            SessionView::replay(f.session, &f.envelopes()).is_ok(),
            ok,
            "{criterion:?}"
        );
    }
}

#[test]
fn worker_protocol_rejected_command_capture_preserves_revision_and_continues() {
    for cause in [
        PrivateCaptureRejection::OutOfLease,
        PrivateCaptureRejection::Overflow,
        PrivateCaptureRejection::UnsupportedEntry,
    ] {
        let mut f = command_fixture();
        f.command(PrivateCapture::Rejected { cause });
        f.call(
            f.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::Finished,
            None,
        );
        let v = f.view();
        let w = v.runs[0].agents[1].assignment.as_ref().unwrap();
        assert_eq!(w.revision, 0);
        assert!(w.overlay.is_empty());
    }
}

#[test]
fn worker_protocol_handover_requires_current_private_evidence_and_gate_is_bounded() {
    let mut f = fixture();
    f.mutation();
    let data = f.finish_worker();
    let h = f.handover(data);
    let event = f.events.pop().unwrap();
    for evidence in [
        Vec::new(),
        vec![1],
        vec![h],
        vec![h - 1, h - 1],
        (1..=9).collect(),
    ] {
        let mut bad = event.clone();
        let Event::WorkerHandoverRecorded { record, .. } = &mut bad else {
            unreachable!()
        };
        record.criterion_evidence = evidence;
        refused(&f, bad, "missing/stale/foreign/oversized private evidence");
    }
    for case in ["fresh", "stale revision", "nonzero", "rejected"] {
        let mut f = command_fixture();
        set_command_criterion(&mut f);
        f.command(if case == "rejected" {
            PrivateCapture::Rejected {
                cause: PrivateCaptureRejection::OutOfLease,
            }
        } else {
            PrivateCapture::None {}
        });
        if case == "nonzero" {
            let Event::ToolFinished { observation, .. } = f.events.last_mut().unwrap() else {
                unreachable!()
            };
            let mut result: WorkerCommandResult =
                serde_json::from_str(&observation.output).unwrap();
            result.exit_code = Some(1);
            observation.output = serde_json::to_string(&result).unwrap();
        }
        if case == "stale revision" {
            f.mutation();
        } else {
            f.changes.files.clear();
        }
        let evidence = private_criterion_evidence(&f.view().runs[0], f.child).unwrap();
        assert_eq!(evidence.is_empty(), case != "fresh", "{case}");
        f.finish_worker();
        let replay = SessionView::replay(f.session, &f.envelopes());
        assert_eq!(replay.is_ok(), case == "fresh", "{case}");
    }
    let mut f = fixture();
    for attempt in 0..=3 {
        f.call(
            f.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::InvalidResponse,
            None,
        );
        let Event::ProviderCallRecorded { record, .. } = f.events.last_mut().unwrap() else {
            unreachable!()
        };
        record.failure_reason = Some(crate::provider::ProviderFailureReason::CriterionNotMetInView);
        assert_eq!(
            f.view().runs[0].agents[1]
                .assignment
                .as_ref()
                .unwrap()
                .gate_continuations,
            attempt
        );
    }
    refused(
        &f,
        Event::GoalProviderCallReserved {
            goal_id: f.goal,
            run_id: f.run,
            agent_run_id: f.child,
            active_ceiling_ms: 0,
        },
        "fourth gate continuation",
    );
}

#[test]
fn worker_child_unguarded_grant_can_only_narrow_explicit_parent_permission() {
    use crate::tools::UnguardedGrant::{Denied, HumanOnly};
    for parent in [Denied, HumanOnly] {
        for requested in [Denied, HumanOnly] {
            let mut f = fixture();
            for event in &mut f.events {
                match event {
                    Event::RunStarted { config, .. } => {
                        config.tool_policy.as_mut().unwrap().child_unguarded = parent;
                    }
                    Event::AgentSpawned {
                        role: AgentRole::Child,
                        grant,
                        ..
                    } => {
                        *grant = Some(ChildGrant::EditOnly {
                            unguarded: requested,
                        });
                    }
                    _ => {}
                }
            }
            assert_eq!(
                SessionView::replay_open(f.session, &f.envelopes()).is_ok(),
                requested == Denied && parent == Denied
            );
        }
    }
}
