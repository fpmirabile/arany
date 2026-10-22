use super::*;

mod goal_success_tests {
    use super::*;
    use crate::tools::{
        EffectIntent, GuardReceipt, ToolCall, ToolDisposition, ToolLimits, ToolObservation,
    };

    fn matches_success<'a>(
        contract: &GoalPlanContract,
        observations: impl IntoIterator<Item = &'a ToolObservation>,
    ) -> bool {
        let mut state = contract.initial_success_state();
        for observation in observations {
            contract.observe_success(&mut state, observation);
        }
        contract.success_state_satisfied(&state)
    }

    fn observation(
        call: ToolCall,
        disposition: ToolDisposition,
        output: String,
    ) -> ToolObservation {
        let intent = EffectIntent {
            target: crate::session::worker::EffectTarget::Workspace,
            id: Uuid::now_v7(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        ToolObservation {
            enforcement_cause: None,
            limit_cause: (disposition == crate::tools::ToolDisposition::Limit)
                .then_some(crate::tools::LimitCause::Resource),
            attestation: Some(crate::tools::Attestation::Guard(GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            })),
            intent,
            disposition,
            output,
            cache_update: crate::tools::CacheUpdate::NotApplicable,
        }
    }

    fn criterion(criterion: GoalSuccessCriterion) -> GoalPlanContract {
        GoalPlanContract {
            mission: "Verify the requested result".into(),
            paths: Vec::new(),
            commands: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: criterion,
            budget: GoalBudget::default(),
        }
    }

    #[test]
    fn later_failed_receipt_invalidates_an_earlier_matching_call() {
        let call = ToolCall::Read {
            path: "src/main.rs".into(),
            offset: None,
            limit: Some(1024),
        };
        let contract = criterion(GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(call.clone()),
        });
        let observations = [
            observation(call.clone(), ToolDisposition::Succeeded, "{}".into()),
            observation(call, ToolDisposition::Failed, "".into()),
        ];
        assert!(!matches_success(&contract, &observations));
    }

    #[test]
    fn a_later_git_switch_invalidates_a_satisfied_command_criterion() {
        let call = ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd: String::new(),
        };
        let contract = criterion(GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(call.clone()),
        });
        let passed = observation(
            call,
            ToolDisposition::Succeeded,
            serde_json::json!({"exit_code":0,"stdout":"","stderr":"","stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}}).to_string(),
        );
        assert!(matches_success(&contract, std::slice::from_ref(&passed)));
        let switch = observation(
            ToolCall::GitSwitch {
                branch: "topic".into(),
            },
            ToolDisposition::Succeeded,
            "{}".into(),
        );
        assert!(!matches_success(&contract, &[passed, switch]));
    }

    #[test]
    fn skill_only_plan_requires_an_exact_binding_and_successful_receipt() {
        let call = ToolCall::Skill {
            name: "review".into(),
            resource: None,
        };
        let contract = GoalPlanContract {
            mission: "Read the requested Skill guidance".into(),
            paths: Vec::new(),
            commands: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: vec![GoalSkillBinding {
                name: "review".into(),
                resource: None,
            }],
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(call.clone()),
            },
            budget: GoalBudget::default(),
        };
        assert!(contract.validate().is_ok());
        assert!(contract.allows_call(&call));
        assert!(contract.allows_call(&ToolCall::Skill {
            name: "review".into(),
            resource: Some("SKILL.md".into()),
        }));
        let success = observation(
            call.clone(),
            ToolDisposition::Succeeded,
            "review guidance".into(),
        );
        assert!(matches_success(&contract, std::slice::from_ref(&success)));
        assert!(!matches_success(
            &contract,
            &[
                success,
                observation(call.clone(), ToolDisposition::Failed, "failed".into()),
            ]
        ));
        let resource_call = ToolCall::Skill {
            name: "review".into(),
            resource: Some("references/checklist.md".into()),
        };
        let mut resource_contract = contract.clone();
        resource_contract.skills[0].resource = Some("references/checklist.md".into());
        resource_contract.success_criterion = GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(resource_call.clone()),
        };
        assert!(resource_contract.validate().is_ok());
        assert!(matches_success(
            &resource_contract,
            &[observation(
                resource_call,
                ToolDisposition::Succeeded,
                "checklist".into(),
            )]
        ));
        let mut missing_resource = serde_json::to_value(&contract).unwrap();
        missing_resource["skills"][0]
            .as_object_mut()
            .unwrap()
            .remove("resource");
        assert!(serde_json::from_value::<GoalPlanContract>(missing_resource).is_err());

        let wrong_resource = ToolCall::Skill {
            name: "review".into(),
            resource: Some("references/other.md".into()),
        };
        let undeclared_skill = ToolCall::Skill {
            name: "other".into(),
            resource: None,
        };
        assert!(!contract.allows_call(&wrong_resource));
        assert!(!contract.allows_call(&undeclared_skill));
        let mut invalid = contract;
        invalid.success_criterion = GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(wrong_resource),
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn success_criterion_rejects_inspection_and_internal_tools() {
        let mut contract = GoalPlanContract {
            mission: "Verify a declared result".into(),
            paths: vec!["src".into()],
            commands: vec!["shell".into()],
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: vec![GoalMcpTool {
                server: "local".into(),
                tool: "inspect".into(),
            }],
            success_criterion: GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(ToolCall::Command {
                    program: "shell".into(),
                    args: Vec::new(),
                    cwd: String::new(),
                }),
            },
            budget: GoalBudget::default(),
        };
        for call in [
            ToolCall::List { path: "src".into() },
            ToolCall::Read {
                path: "src/main.rs".into(),
                offset: None,
                limit: Some(1024),
            },
            ToolCall::Search {
                path: "src".into(),
                query: "main".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
            },
            ToolCall::McpList {
                server: "local".into(),
            },
            ToolCall::GitStatus { path: "src".into() },
            ToolCall::GitDiff {
                path: "src".into(),
                base: None,
                head: None,
                staged: false,
            },
            ToolCall::GitLog {
                path: "src".into(),
                revision: None,
                max_count: 1,
            },
        ] {
            contract.success_criterion = GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(call),
            };
            assert!(contract.validate().is_err());
        }
    }

    #[test]
    fn plan_contract_scopes_git_reads_by_path() {
        use crate::tools::ToolCall;
        let contract = |paths: Vec<&str>| GoalPlanContract {
            mission: "Inspect Git state".into(),
            paths: paths.into_iter().map(str::to_owned).collect(),
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/main.rs".into(),
                content: "ok".into(),
            },
            budget: GoalBudget::default(),
        };
        let calls = |path: &str| {
            [
                ToolCall::GitStatus { path: path.into() },
                ToolCall::GitDiff {
                    path: path.into(),
                    base: None,
                    head: None,
                    staged: false,
                },
                ToolCall::GitLog {
                    path: path.into(),
                    revision: None,
                    max_count: 1,
                },
            ]
        };
        let scoped = contract(vec!["src"]);
        for path in ["src", "src/lib.rs"] {
            for call in calls(path) {
                assert!(scoped.allows_call(&call), "{call:?}");
            }
        }
        for path in [".", "docs", ""] {
            for call in calls(path) {
                assert!(!scoped.allows_call(&call), "{call:?}");
            }
        }
        for call in calls(".") {
            assert!(contract(vec!["."]).allows_call(&call), "{call:?}");
        }
    }

    #[test]
    fn later_file_edit_invalidates_an_earlier_matching_read() {
        let expected = crate::tools::types::hex_digest(b"before");
        let contract = criterion(GoalSuccessCriterion::WorkspaceFileDigest {
            path: "src/main.rs".into(),
            sha256: expected,
        });
        let observations = [
            observation(
                ToolCall::Read {
                    path: "src/main.rs".into(),
                    offset: None,
                    limit: Some(1024),
                },
                ToolDisposition::Succeeded,
                serde_json::json!({
                    "sha256": crate::tools::types::hex_digest(b"before")
                })
                .to_string(),
            ),
            observation(
                ToolCall::Write {
                    path: "src/main.rs".into(),
                    expected_digest: None,
                    content: "after".into(),
                },
                ToolDisposition::Succeeded,
                serde_json::json!({
                    "sha256": crate::tools::types::hex_digest(b"after")
                })
                .to_string(),
            ),
        ];
        assert!(!matches_success(&contract, &observations));
    }

    #[test]
    fn child_steering_event_round_trips_with_exact_scope() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let event = Event::AgentSteeringNoteRecorded {
            goal_id,
            run_id,
            agent_run_id,
            text: "literal /grant all".into(),
        };
        assert_eq!(event.version(), 1);
        assert_eq!(event.scope(), (Some(run_id), Some(agent_run_id)));
        let payload = event.payload().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
            serde_json::json!({"goal_id":goal_id,"text":"literal /grant all"})
        );
        assert_eq!(
            Event::decode(event.kind(), 1, &payload, Some(run_id), Some(agent_run_id)).unwrap(),
            event
        );
        for (run, actor) in [
            (None, None),
            (Some(run_id), None),
            (None, Some(agent_run_id)),
        ] {
            assert!(Event::decode(event.kind(), 1, &payload, run, actor).is_err());
        }
        for payload in [
            serde_json::json!({"goal_id":goal_id,"text":null}),
            serde_json::json!({"goal_id":goal_id,"text":" "}),
            serde_json::json!({"goal_id":null,"text":"x"}),
            serde_json::json!({"goal_id":uuid::Uuid::nil(),"text":"x"}),
            serde_json::json!({"goal_id":goal_id,"text":"x","agent_run_id":agent_run_id}),
            serde_json::json!({"goal_id":goal_id,"text":"x".repeat(crate::session::MAX_STEERING_NOTE_BYTES+1)}),
        ] {
            assert!(
                Event::decode(
                    event.kind(),
                    1,
                    &payload.to_string(),
                    Some(run_id),
                    Some(agent_run_id)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn goal_steering_note_round_trips_and_rejects_bad_scope_and_bounds() {
        let (goal_id, run_id) = (GoalId::new(), RunId::new());
        let note = |text: &str| Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: text.into(),
        };
        let event = note("use the smaller fixture");
        assert_eq!(event.scope(), (Some(run_id), None));
        let payload = event.payload().unwrap();
        assert_eq!(
            Event::decode(event.kind(), event.version(), &payload, Some(run_id), None).unwrap(),
            event
        );

        assert!(note("").validate().is_err());
        assert!(note(" \n\t").validate().is_err());
        let limit = crate::session::MAX_STEERING_NOTE_BYTES;
        assert!(note(&"x".repeat(limit)).validate().is_ok());
        assert!(note(&"x".repeat(limit + 1)).validate().is_err());

        let kind = event.kind();
        let agent_run_id = Some(AgentRunId::new());
        assert!(
            Event::decode(
                kind,
                payload_version(kind).expect("payload version") - 1,
                &payload,
                Some(run_id),
                agent_run_id
            )
            .is_err()
        );
        assert!(
            Event::decode(
                kind,
                payload_version(kind).expect("payload version"),
                &payload,
                None,
                None
            )
            .is_err()
        );
        assert!(Event::decode(kind, 3, &payload, Some(run_id), None).is_err());

        let mut extra: serde_json::Value = serde_json::from_str(&payload).unwrap();
        extra["unexpected"] = serde_json::Value::Bool(true);
        assert!(
            Event::decode(
                kind,
                payload_version(kind).expect("payload version"),
                &extra.to_string(),
                Some(run_id),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn plan_contract_bounds_are_enforced_without_provider_schema_keywords() {
        let base = || GoalPlanContract {
            mission: "Verify".into(),
            paths: vec!["src".into()],
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/main.rs".into(),
                content: "ok".into(),
            },
            budget: GoalBudget::default(),
        };
        let long = |byte: char, len: usize| byte.to_string().repeat(len);
        let mut cases: Vec<(&str, GoalPlanContract, bool)> = vec![
            (
                "mission at limit",
                GoalPlanContract {
                    mission: long('m', 2048),
                    ..base()
                },
                true,
            ),
            (
                "mission over limit",
                GoalPlanContract {
                    mission: long('m', 2049),
                    ..base()
                },
                false,
            ),
            (
                "empty mission",
                GoalPlanContract {
                    mission: String::new(),
                    ..base()
                },
                false,
            ),
            (
                "path count at limit",
                GoalPlanContract {
                    paths: (0..63)
                        .map(|i| format!("p{i:02}"))
                        .chain(["src".into()])
                        .collect(),
                    ..base()
                },
                true,
            ),
            (
                "path count over limit",
                GoalPlanContract {
                    paths: (0..64)
                        .map(|i| format!("p{i:02}"))
                        .chain(["src".into()])
                        .collect(),
                    ..base()
                },
                false,
            ),
            (
                "path length at limit",
                GoalPlanContract {
                    paths: vec![long('a', 512)],
                    success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                        path: long('a', 512),
                        content: "ok".into(),
                    },
                    ..base()
                },
                true,
            ),
            (
                "path length over limit",
                GoalPlanContract {
                    paths: vec![long('a', 513)],
                    success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                        path: long('a', 513),
                        content: "ok".into(),
                    },
                    ..base()
                },
                false,
            ),
            (
                "command count over limit",
                GoalPlanContract {
                    commands: (0..9).map(|i| format!("c{i}")).collect(),
                    ..base()
                },
                false,
            ),
            (
                "command length over limit",
                GoalPlanContract {
                    commands: vec![long('a', 65)],
                    ..base()
                },
                false,
            ),
            (
                "skill count over limit",
                GoalPlanContract {
                    skills: (0..65)
                        .map(|i| GoalSkillBinding {
                            name: format!("s{i:02}"),
                            resource: None,
                        })
                        .collect(),
                    ..base()
                },
                false,
            ),
            (
                "skill name over limit",
                GoalPlanContract {
                    skills: vec![GoalSkillBinding {
                        name: long('a', 65),
                        resource: None,
                    }],
                    ..base()
                },
                false,
            ),
            (
                "skill resource over limit",
                GoalPlanContract {
                    skills: vec![GoalSkillBinding {
                        name: "review".into(),
                        resource: Some(long('r', 513)),
                    }],
                    ..base()
                },
                false,
            ),
            (
                "mcp count over limit",
                GoalPlanContract {
                    mcp_tools: (0..65)
                        .map(|i| GoalMcpTool {
                            server: format!("m{i:02}"),
                            tool: "t".into(),
                        })
                        .collect(),
                    ..base()
                },
                false,
            ),
            (
                "mcp server over limit",
                GoalPlanContract {
                    mcp_tools: vec![GoalMcpTool {
                        server: long('a', 65),
                        tool: "t".into(),
                    }],
                    ..base()
                },
                false,
            ),
            (
                "mcp tool over limit",
                GoalPlanContract {
                    mcp_tools: vec![GoalMcpTool {
                        server: "local".into(),
                        tool: long('a', 129),
                    }],
                    ..base()
                },
                false,
            ),
            (
                "content over limit",
                GoalPlanContract {
                    success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                        path: "src/main.rs".into(),
                        content: long('x', 8193),
                    },
                    ..base()
                },
                false,
            ),
            (
                "digest length mismatch",
                GoalPlanContract {
                    success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
                        path: "src/main.rs".into(),
                        sha256: "a".repeat(63),
                    },
                    ..base()
                },
                false,
            ),
        ];
        cases.push(("base", base(), true));
        for (name, contract, admitted) in cases {
            assert_eq!(contract.validate().is_ok(), admitted, "{name}");
        }
    }
}

mod account_default_tests {
    use super::*;

    #[test]
    fn saved_account_binding_requires_a_native_selected_provider() {
        let id = Uuid::now_v7();
        let mut defaults = SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: None,
            account_id: Some(id),
            policy: CollaborationPolicy::Single,
        };
        assert!(defaults.validate().is_ok());
        defaults.provider = Some("custom:local".into());
        assert!(defaults.validate().is_err());
        defaults.provider = Some("openai".into());
        defaults.model = None;
        assert!(defaults.validate().is_err());
        defaults.model = Some("gpt-5.4".into());
        defaults.account_id = Some(Uuid::nil());
        assert!(defaults.validate().is_err());
        defaults.account_id = Some(id);
        defaults.provider = Some("chatgpt".into());
        defaults.model = None;
        assert!(defaults.validate().is_ok());
        defaults.account_id = None;
        assert!(defaults.validate().is_err());
    }

    #[test]
    fn current_defaults_require_explicit_nullable_fields() {
        let defaults = SessionDefaults::default();
        let encoded = serde_json::to_value(&defaults).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionDefaults>(encoded.clone()).unwrap(),
            defaults
        );
        for field in ["provider", "model", "effort", "account_id"] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<SessionDefaults>(missing).is_err());
        }
    }

    #[test]
    fn goal_budget_requires_explicit_nullable_token_limit() {
        let budget = GoalBudget::default();
        let encoded = serde_json::to_value(budget).unwrap();
        assert!(encoded["max_tokens"].is_null());
        assert_eq!(
            serde_json::from_value::<GoalBudget>(encoded.clone()).unwrap(),
            budget
        );
        for field in [
            "max_tokens",
            "max_provider_calls",
            "max_active_ms",
            "max_runs",
        ] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<GoalBudget>(missing).is_err());
        }
        let mut unlimited = encoded;
        unlimited["max_provider_calls"] = serde_json::Value::Null;
        unlimited["max_active_ms"] = serde_json::Value::Null;
        let decoded = serde_json::from_value::<GoalBudget>(unlimited).unwrap();
        assert_eq!(decoded.max_provider_calls, None);
        assert_eq!(decoded.max_active_ms, None);
    }

    #[test]
    fn run_account_provenance_separates_api_keys_and_subscription_bounds() {
        let mut config = RunConfig {
            supervisor: None,
            worker_policy: None,
            agent_definitions: None,
            roster: None,
            providers: None,
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            effort: None,
            custom_profile_provenance: None,
            saved_api_account_id: Some(Uuid::now_v7()),
            chatgpt_provenance: None,
            output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
            context_window_tokens: None,
            policy: CollaborationPolicy::Single,
            output_token_cap: 4096,
            provider_concurrency: 1,
            workspace_device: 1,
            workspace_inode: 1,
            user_instruction_digest: None,
            instruction_sources: Vec::new(),
            launch_directory: String::new(),
            include_digests: Vec::new(),
            history_run_ids: Vec::new(),
            excluded_history_runs: 0,
            context_usage: None,
            compaction_event_sequence: None,
            compaction_content_digest: None,
            tool_policy: None,
        };
        assert!(config.validate().is_ok());
        let mut inherited = config.clone();
        inherited.saved_api_account_id = None;
        let event = Event::RunStarted {
            run_id: RunId::new(),
            config: inherited,
            active_deadline_ms: Some(MAX_GOAL_ACTIVE_MS),
        };
        assert_eq!(
            event.payload().unwrap(),
            r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"user_instruction_digest":null,"instruction_sources":[],"launch_directory":"","include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null},"active_deadline_ms":1800000}"#
        );
        let Event::RunStarted {
            config: ordinary, ..
        } = &event
        else {
            unreachable!()
        };
        let mut roster_config = ordinary.clone();
        let mut roster = crate::provider::RosterSnapshot {
            preset: "coding".into(),
            selection_digest: [0; 32],
            profiles: vec![crate::provider::NamedProviderRoutes {
                name: "main".into(),
                routes: vec![ordinary.identity().unwrap()],
            }],
            providers: crate::provider::Providers::all("main".into()),
            child_profiles: vec!["main".into()],
        };
        roster.selection_digest = roster.digest().unwrap();
        roster_config.roster = Some(Box::new(roster));
        assert!(roster_config.validate().is_ok());
        for change in 0..4 {
            let mut forged = roster_config.clone();
            match change {
                0 => forged.roster.as_mut().unwrap().selection_digest[0] ^= 1,
                1 => forged.model = "changed".into(),
                2 => forged.roster.as_mut().unwrap().profiles[0].routes[0].model = "changed".into(),
                _ => forged.roster.as_mut().unwrap().child_profiles.clear(),
            }
            assert!(forged.validate().is_err());
        }
        let mut other_identity = ordinary.identity().unwrap();
        other_identity.model = "gpt-5.4-mini".into();
        let mut two = roster_config.roster.as_deref().unwrap().clone();
        two.profiles.push(crate::provider::NamedProviderRoutes {
            name: "other".into(),
            routes: vec![other_identity.clone()],
        });
        two.child_profiles.push("other".into());
        two.selection_digest = two.digest().unwrap();
        let mut pinned_config = roster_config.clone();
        pinned_config.roster = Some(Box::new(two));
        let main_routes = pinned_config
            .roster
            .as_ref()
            .unwrap()
            .role_routes()
            .unwrap();
        let mut pinned_base = pinned_config.clone();
        let roster = pinned_base.roster.as_mut().unwrap();
        roster.providers.review = "other".into();
        roster.selection_digest = roster.digest().unwrap();
        pinned_base.providers = Some(roster.role_routes().unwrap());
        for base in [pinned_config.clone(), pinned_base] {
            assert!(base.validate().is_ok());
            for role in 0..3 {
                let mut forged = base.clone();
                let roster = forged.roster.as_mut().unwrap();
                let name = match role {
                    0 => &mut roster.providers.child,
                    1 => &mut roster.providers.review,
                    _ => &mut roster.providers.compaction,
                };
                *name = if name == "other" { "main" } else { "other" }.into();
                roster.selection_digest = roster.digest().unwrap();
                assert!(forged.validate().is_err(), "role {role}");
            }
        }
        let mut forged = pinned_config.clone();
        forged.providers = Some(main_routes);
        forged.providers.as_mut().unwrap().review = vec![other_identity];
        assert!(forged.validate().is_err());
        let mut missing_supervisor = serde_json::to_value(ordinary).unwrap();
        missing_supervisor
            .as_object_mut()
            .unwrap()
            .remove("supervisor");
        assert!(serde_json::from_value::<RunConfig>(missing_supervisor).is_err());
        let mut missing_user = serde_json::to_value(ordinary).unwrap();
        missing_user
            .as_object_mut()
            .unwrap()
            .remove("user_instruction_digest");
        assert!(serde_json::from_value::<RunConfig>(missing_user).is_err());
        let mut pinned_user = ordinary.clone();
        pinned_user.user_instruction_digest = Some([7; 32]);
        assert!(pinned_user.validate().is_ok());
        let mut short_user = serde_json::to_value(&pinned_user).unwrap();
        short_user["user_instruction_digest"] = serde_json::json!(vec![7; 31]);
        assert!(serde_json::from_value::<RunConfig>(short_user).is_err());
        let mut missing_roster = serde_json::to_value(ordinary).unwrap();
        missing_roster.as_object_mut().unwrap().remove("roster");
        assert!(serde_json::from_value::<RunConfig>(missing_roster).is_err());
        let mut missing_window = serde_json::to_value(ordinary).unwrap();
        missing_window
            .as_object_mut()
            .unwrap()
            .remove("context_window_tokens");
        assert!(serde_json::from_value::<RunConfig>(missing_window).is_err());
        assert_eq!(payload_version("RunStarted"), Some(20));
        assert_eq!(payload_version("ProviderCallRecorded"), Some(15));
        assert_eq!(payload_version("GoalProviderCallReserved"), Some(7));
        config.output_token_cap = crate::provider::MAX_OUTPUT_TOKEN_CAP;
        assert!(config.validate().is_ok());
        for (cap, valid) in [
            (0, false),
            (4096, true),
            (crate::provider::MAX_OUTPUT_TOKEN_CAP, true),
            (crate::provider::MAX_OUTPUT_TOKEN_CAP + 1, false),
        ] {
            config.output_token_cap = cap;
            assert_eq!(config.validate().is_ok(), valid, "output cap {cap}");
        }
        config.output_token_cap = 4096;
        let source = |directory: &str| crate::session::InstructionSource {
            directory: directory.into(),
            file: Some(crate::session::InstructionFile::Agents),
            digest: [3; 32],
        };
        let deep = vec!["d"; 31].join("/");
        let too_deep = format!("{deep}/d");
        for (case, launch, sources, valid) in [
            (
                "root and nested",
                "a/b",
                vec![source(""), source("a/b")],
                true,
            ),
            ("nested only", "a/b", vec![source("a")], true),
            ("deepest allowed", deep.as_str(), vec![source(&deep)], true),
            ("too deep", too_deep.as_str(), vec![], false),
            ("deeper first", "a/b", vec![source("a"), source("")], false),
            ("repeated", "a", vec![source("a"), source("a")], false),
            ("off the path", "a/b", vec![source("c")], false),
            ("below launch", "a", vec![source("a/b")], false),
            ("traversal", "a/..", vec![], false),
            ("empty component", "a//b", vec![], false),
        ] {
            config.launch_directory = launch.into();
            config.instruction_sources = sources;
            assert_eq!(config.validate().is_ok(), valid, "instruction scope {case}");
        }
        config.launch_directory.clear();
        config.instruction_sources.clear();
        config.saved_api_account_id = Some(Uuid::nil());
        assert!(matches!(
            config.validate(),
            Err(ReplayError::InvalidPayload)
        ));
        config.saved_api_account_id = Some(Uuid::now_v7());
        config.provider = "scripted".into();
        assert!(matches!(
            config.validate(),
            Err(ReplayError::InvalidPayload)
        ));
        config.saved_api_account_id = None;
        assert!(config.validate().is_ok());
        let decoded: RunConfig =
            serde_json::from_value(serde_json::to_value(&config).expect("current JSON"))
                .expect("current Run config");
        assert_eq!(decoded.saved_api_account_id, None);
        assert_eq!(decoded.chatgpt_provenance, None);
        assert_eq!(
            decoded.output_token_bound,
            crate::provider::OutputTokenBound::ProviderEnforced
        );

        config.provider = "chatgpt".into();
        config.model = "visible-model".into();
        config.effort = Some(crate::provider::Effort::High);
        config.chatgpt_provenance = Some(crate::provider::ChatGptProvenance {
            account_id: Uuid::now_v7(),
            evidence_fingerprint: [7; 32],
            admission: crate::provider::ChatGptAdmission::AccountConsent,
        });
        assert!(config.validate().is_err());
        config.output_token_bound = crate::provider::OutputTokenBound::LocalAcceptanceOnly;
        assert!(config.validate().is_ok());
        config.saved_api_account_id = Some(Uuid::now_v7());
        assert!(config.validate().is_err());
        config.saved_api_account_id = None;
        config.chatgpt_provenance.as_mut().unwrap().account_id = Uuid::nil();
        assert!(config.validate().is_err());
        config.chatgpt_provenance.as_mut().unwrap().account_id = Uuid::now_v7();
        config
            .chatgpt_provenance
            .as_mut()
            .unwrap()
            .evidence_fingerprint = [0; 32];
        assert!(config.validate().is_err());
        config
            .chatgpt_provenance
            .as_mut()
            .unwrap()
            .evidence_fingerprint = [7; 32];
        config.provider_concurrency = 2;
        assert!(config.validate().is_err());
        config.provider_concurrency = 1;
        assert!(config.validate().is_ok());
        for (provider, window, budget, valid) in [
            ("chatgpt-like", None, 376 * 1024, true),
            ("chatgpt-like", None, 376 * 1024 + 1, false),
            ("chatgpt", Some(272_000), 734_400, true),
            ("chatgpt", Some(272_000), 734_401, false),
            ("chatgpt", Some(2_000_000), 1024 * 1024, true),
            ("chatgpt", Some(2_000_001), 64 * 1024, false),
            ("chatgpt", Some(32_767), 64 * 1024, false),
            ("chatgpt-like", Some(272_000), 64 * 1024, false),
            ("openai", Some(272_000), 64 * 1024, false),
            ("anthropic", Some(272_000), 64 * 1024, false),
        ] {
            let mut pinned = if provider == "chatgpt" {
                config.clone()
            } else {
                ordinary.clone()
            };
            pinned.provider = provider.into();
            pinned.context_window_tokens = window;
            pinned.context_usage = Some(ContextUsage {
                used_bytes: 0,
                budget_bytes: budget,
                compactable_bytes: 0,
                tool_history_bytes: 0,
            });
            assert_eq!(
                pinned.validate().is_ok(),
                valid,
                "{provider} {window:?} {budget}"
            );
        }
        let mut fallback = config.identity().unwrap();
        fallback.model = "fallback-model".into();
        let mut custom = ordinary.identity().unwrap();
        custom.profile = "custom:fallback".into();
        custom.provenance =
            crate::provider::Provenance::Custom(crate::provider::CustomProfileProvenance {
                endpoint: "https://example.invalid/v1".into(),
                evidence_digest: [1; 32],
                capability_evidence_version: 2,
            });
        for (fallback, window_valid) in [
            (fallback, true),
            (ordinary.identity().unwrap(), false),
            (custom, false),
        ] {
            let mut pinned = config.clone();
            pinned.providers = Some(crate::provider::Providers::all(vec![
                config.identity().unwrap(),
                fallback,
            ]));
            assert!(pinned.validate().is_ok(), "no window: {pinned:?}");
            pinned.context_window_tokens = Some(272_000);
            assert_eq!(pinned.validate().is_ok(), window_valid, "{pinned:?}");
        }
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(encoded["output_token_bound"], "local_acceptance_only");
        assert_eq!(encoded["chatgpt_provenance"]["evidence_fingerprint"][0], 7);
        assert_eq!(
            encoded["chatgpt_provenance"]["admission"],
            "account_consent"
        );
        assert_eq!(
            serde_json::from_value::<RunConfig>(encoded.clone()).unwrap(),
            config
        );
        for field in [
            "effort",
            "custom_profile_provenance",
            "saved_api_account_id",
            "chatgpt_provenance",
            "output_token_bound",
            "provider_concurrency",
            "context_usage",
            "compaction_event_sequence",
            "compaction_content_digest",
            "tool_policy",
        ] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<RunConfig>(missing).is_err());
        }
        let mut missing_admission = encoded.clone();
        missing_admission["chatgpt_provenance"]
            .as_object_mut()
            .unwrap()
            .remove("admission");
        assert!(serde_json::from_value::<RunConfig>(missing_admission).is_err());
        for (field, value) in [
            ("admission", serde_json::json!("unchecked")),
            ("admission", serde_json::Value::Null),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut invalid = encoded.clone();
            invalid["chatgpt_provenance"][field] = value;
            assert!(serde_json::from_value::<RunConfig>(invalid).is_err());
        }
    }
}

mod subscription_compaction_tests {
    use super::*;

    #[test]
    fn subscription_compaction_pins_account_and_weaker_bound() {
        let mut record = CompactionRecord {
            covered_run_id: RunId::new(),
            covered_sequence: 1,
            source_digest: [1; 32],
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            prompt_version: 1,
            schema_version: 1,
            compiler_version: 3,
            source_bytes: 4,
            source_tokens_estimate: 1,
            input_tokens: None,
            output_tokens: None,
            response_id: None,
            wire_provenance: None,
            chatgpt_provenance: None,
            output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
            status: CompactionStatus::Failed {
                reason: CompactionFailure::ProviderUnavailable,
            },
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert!(record.validate().is_ok());
        let encoded = serde_json::to_value(&record).unwrap();
        assert!(encoded["chatgpt_provenance"].is_null());
        assert_eq!(encoded["output_token_bound"], "provider_enforced");
        for field in [
            "wire_provenance",
            "chatgpt_provenance",
            "output_token_bound",
        ] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<CompactionRecord>(missing).is_err());
        }
        for (field, current) in [
            ("prompt_version", 1),
            ("schema_version", 1),
            ("compiler_version", 3),
        ] {
            assert_eq!(encoded[field], current);
            for version in (0..=4).filter(|version| *version != current) {
                let mut invalid = encoded.clone();
                invalid[field] = serde_json::json!(version);
                let invalid: CompactionRecord = serde_json::from_value(invalid).unwrap();
                assert!(invalid.validate().is_err(), "{field}={version}");
            }
        }
        record.provider = "chatgpt".into();
        record.chatgpt_provenance = Some(crate::provider::ChatGptProvenance {
            account_id: Uuid::now_v7(),
            evidence_fingerprint: [7; 32],
            admission: crate::provider::ChatGptAdmission::AccountConsent,
        });
        assert!(record.validate().is_err());
        record.output_token_bound = crate::provider::OutputTokenBound::LocalAcceptanceOnly;
        assert!(record.validate().is_ok());
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(
            encoded["chatgpt_provenance"]["admission"],
            "account_consent"
        );
        assert_eq!(
            serde_json::from_value::<CompactionRecord>(encoded.clone()).unwrap(),
            record
        );
        for (field, value) in [
            ("admission", serde_json::json!("unchecked")),
            ("admission", serde_json::Value::Null),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut invalid = encoded.clone();
            invalid["chatgpt_provenance"][field] = value;
            assert!(serde_json::from_value::<CompactionRecord>(invalid).is_err());
        }
        record
            .chatgpt_provenance
            .as_mut()
            .unwrap()
            .evidence_fingerprint = [0; 32];
        assert!(record.validate().is_err());
    }
}

mod provider_call_tests {
    use super::*;

    #[test]
    fn provider_call_record_rejects_old_versions_and_inconsistent_cache_fields() {
        let current = payload_version("ProviderCallRecorded").expect("payload version");
        let run = RunId::new();
        let agent = AgentRunId::new();
        let mut record = ProviderCallRecord {
            delegation: None,
            phase: crate::provider::AgentPhase::RootPlan,
            disposition: ProviderCallDisposition::Finished,
            requested_tools: 0,
            response_id: Some("response-1".into()),
            input_tokens: Some(100),
            output_tokens: Some(10),
            cache_read_input_tokens: Some(40),
            cache_creation_input_tokens: Some(20),
            wire_provenance: None,
            refusal: None,
            failure_reason: None,
        };
        assert!(record.validate().is_ok());
        record.cache_read_input_tokens = Some(81);
        assert!(
            record.validate().is_err(),
            "cache parts exceed the input total"
        );
        record.cache_read_input_tokens = Some(40);
        record.input_tokens = None;
        assert!(
            record.validate().is_err(),
            "cache without a reported input total"
        );
        record.input_tokens = Some(100);
        record.requested_tools = 1;
        assert!(
            record.validate().is_err(),
            "a finished call declares no Tool"
        );
        record.disposition = ProviderCallDisposition::ToolRequested;
        record.requested_tools = 0;
        assert!(
            record.validate().is_err(),
            "a Tool request declares at least one Tool"
        );
        record.requested_tools = MAX_STEP_TOOL_CALLS + 1;
        assert!(record.validate().is_err(), "above the step ceiling");
        record.requested_tools = MAX_STEP_TOOL_CALLS;
        assert!(record.validate().is_ok());

        let event = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: agent,
            record: record.clone(),
        };
        let payload = event.payload().expect("current payload");
        assert_eq!(
            Event::decode(
                "ProviderCallRecorded",
                current,
                &payload,
                Some(run),
                Some(agent),
            )
            .expect("current replay"),
            event
        );
        assert!(
            Event::decode(
                "ProviderCallRecorded",
                current - 1,
                &payload,
                Some(run),
                Some(agent),
            )
            .is_err(),
            "payload version 5 is not current"
        );
    }

    #[test]
    fn rejects_unbounded_or_unearned_provider_metadata() {
        let current = payload_version("ProviderCallRecorded").expect("payload version");
        let mut record = ProviderCallRecord {
            delegation: None,
            phase: crate::provider::AgentPhase::RootPlan,
            disposition: ProviderCallDisposition::Finished,
            response_id: Some("response-1".into()),
            input_tokens: Some(100),
            output_tokens: Some(10),
            wire_provenance: None,
            refusal: None,
            failure_reason: None,
            requested_tools: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert!(record.validate().is_ok());
        record.response_id = Some("bad\nidentifier".into());
        assert!(record.validate().is_err());
        record.response_id = Some("r".repeat(129));
        assert!(record.validate().is_err());
        record.response_id = None;
        record.input_tokens = Some(crate::provider::MAX_REPORTED_INPUT_TOKENS + 1);
        assert!(record.validate().is_err());
        record.input_tokens = Some(100);
        // A local-acceptance total includes reasoning; the reducer applies each route's cap.
        record.output_tokens = Some(crate::provider::MAX_REPORTED_OUTPUT_TOKENS + 1);
        assert!(record.validate().is_err());
        record.output_tokens = Some(crate::provider::MAX_REPORTED_OUTPUT_TOKENS);
        assert!(record.validate().is_ok());
        record.output_tokens = None;
        record.disposition = ProviderCallDisposition::Unavailable;
        record.input_tokens = Some(100);
        assert!(record.validate().is_err());
        record.input_tokens = None;
        assert!(record.validate().is_ok());
        record.disposition = ProviderCallDisposition::Finished;
        record.response_id = Some("response-1".into());
        record.wire_provenance =
            Some(crate::provider::ProviderWireProvenance::MessagesEndTurnStorageUnspecified);
        assert!(record.validate().is_err());
        record.input_tokens = Some(100);
        record.output_tokens = Some(10);
        assert!(record.validate().is_ok());
        record.disposition = ProviderCallDisposition::TimedOut;
        assert!(record.validate().is_err());
        record.disposition = ProviderCallDisposition::Finished;
        record.wire_provenance = None;
        let encoded = serde_json::to_value(&record).expect("current payload");
        assert!(encoded["wire_provenance"].is_null());
        assert!(encoded["failure_reason"].is_null());
        let decoded: ProviderCallRecord = serde_json::from_value(encoded).expect("current replay");
        assert_eq!(decoded, record);

        use crate::provider::ProviderFailureReason;
        let run = RunId::new();
        let agent = AgentRunId::new();
        let run_id = Some(run);
        let agent_run_id = Some(agent);
        let event = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: agent,
            record: decoded,
        };
        let payload = event.payload().expect("current payload");
        assert_eq!(event.version(), current);
        assert_eq!(
            Event::decode(
                "ProviderCallRecorded",
                current,
                &payload,
                run_id,
                agent_run_id
            )
            .unwrap(),
            event
        );
        for version in [0, 1, 2, 3, 4, 5, current + 1] {
            assert!(
                Event::decode(
                    "ProviderCallRecorded",
                    version,
                    &payload,
                    run_id,
                    agent_run_id
                )
                .is_err()
            );
        }
        for (reason, expected) in [
            (
                ProviderFailureReason::AccountAccess,
                ProviderCallDisposition::Rejected,
            ),
            (
                ProviderFailureReason::UsageLimit,
                ProviderCallDisposition::Rejected,
            ),
            (
                ProviderFailureReason::UsageTemporarilyUnavailable,
                ProviderCallDisposition::Unavailable,
            ),
            (
                ProviderFailureReason::ServiceUnavailable,
                ProviderCallDisposition::Unavailable,
            ),
            (
                ProviderFailureReason::StreamProtocol,
                ProviderCallDisposition::InvalidResponse,
            ),
            (
                ProviderFailureReason::ResponseContract,
                ProviderCallDisposition::InvalidResponse,
            ),
            (
                ProviderFailureReason::OutcomeContract,
                ProviderCallDisposition::InvalidResponse,
            ),
            (
                ProviderFailureReason::LocalOutputLimit,
                ProviderCallDisposition::InvalidResponse,
            ),
        ] {
            for disposition in [
                ProviderCallDisposition::Finished,
                ProviderCallDisposition::Delegated,
                ProviderCallDisposition::Unavailable,
                ProviderCallDisposition::Rejected,
                ProviderCallDisposition::InvalidResponse,
                ProviderCallDisposition::OutputLimit,
                ProviderCallDisposition::TimedOut,
                ProviderCallDisposition::Cancelled,
                ProviderCallDisposition::TaskPanic,
            ] {
                let record = ProviderCallRecord {
                    delegation: None,
                    phase: crate::provider::AgentPhase::RootPlan,
                    disposition,
                    response_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    wire_provenance: None,
                    refusal: None,
                    failure_reason: Some(reason),
                    requested_tools: 0,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                };
                assert_eq!(
                    record.validate().is_ok(),
                    disposition == expected,
                    "{disposition:?}/{reason:?}"
                );
                if disposition != expected {
                    continue;
                }
                let event = Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: agent,
                    record,
                };
                let payload = event.payload().expect("current payload");
                assert_eq!(event.version(), current);
                assert_eq!(
                    Event::decode(
                        "ProviderCallRecorded",
                        current,
                        &payload,
                        run_id,
                        agent_run_id,
                    )
                    .expect("current replay"),
                    event
                );
                for version in [0, 1, 2, 3, 4, 5, current + 1] {
                    assert!(
                        Event::decode(
                            "ProviderCallRecorded",
                            version,
                            &payload,
                            run_id,
                            agent_run_id
                        )
                        .is_err(),
                        "version {version}"
                    );
                }
                for scope in [(None, agent_run_id), (run_id, None)] {
                    assert!(
                        Event::decode("ProviderCallRecorded", current, &payload, scope.0, scope.1)
                            .is_err()
                    );
                }
                let mut invalid: serde_json::Value =
                    serde_json::from_str(&payload).expect("v4 JSON");
                invalid["failure_reason"] = "unknown_reason".into();
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        current,
                        &invalid.to_string(),
                        run_id,
                        agent_run_id
                    )
                    .is_err()
                );
                invalid["failure_reason"] = serde_json::Value::Null;
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        current,
                        &invalid.to_string(),
                        run_id,
                        agent_run_id
                    )
                    .is_ok()
                );
                invalid.as_object_mut().unwrap().remove("failure_reason");
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        current,
                        &invalid.to_string(),
                        run_id,
                        agent_run_id
                    )
                    .is_err()
                );
            }
        }
        record.disposition = ProviderCallDisposition::InvalidResponse;
        record.failure_reason = Some(ProviderFailureReason::CriterionNotMetInView);
        for phase in [
            crate::provider::AgentPhase::RootPlan,
            crate::provider::AgentPhase::RootSynthesis,
            crate::provider::AgentPhase::ToolReview,
            crate::provider::AgentPhase::ChildWork,
        ] {
            record.phase = phase;
            let payload = serde_json::to_string(&record).expect("scoped failure payload");
            assert_eq!(
                Event::decode(
                    "ProviderCallRecorded",
                    current,
                    &payload,
                    run_id,
                    agent_run_id,
                )
                .is_ok(),
                phase == crate::provider::AgentPhase::ChildWork,
                "criterion failure phase: {phase:?}"
            );
        }
    }
}

mod child_grant_tests {
    use super::*;

    #[test]
    fn child_grant_round_trips_and_rejects_missing_or_misplaced_pins() {
        let (run_id, agent_run_id) = (RunId::new(), AgentRunId::new());
        let spawned = |role, ordinal, objective: Option<&str>, grant| Event::AgentSpawned {
            profile: None,
            assignment: None,
            definition: None,
            run_id,
            agent_run_id,
            role,
            ordinal,
            objective: objective.map(String::from),
            grant,
        };
        let decode = |event: &Event, payload: &str| {
            Event::decode(
                event.kind(),
                payload_version(event.kind()).expect("payload version"),
                payload,
                Some(run_id),
                Some(agent_run_id),
            )
        };
        for grant in [
            ChildGrant::NoTools,
            ChildGrant::ReadOnly {
                unguarded: crate::tools::UnguardedGrant::Denied,
            },
        ] {
            let event = spawned(AgentRole::Child, 1, Some("inspect"), Some(grant));
            let payload = event.payload().expect("child payload");
            assert_eq!(decode(&event, &payload).expect("child replay"), event);
        }
        let primary = spawned(AgentRole::Primary, 0, None, None);
        let payload = primary.payload().expect("primary payload");
        assert_eq!(decode(&primary, &payload).expect("primary replay"), primary);

        let child = spawned(
            AgentRole::Child,
            1,
            Some("inspect"),
            Some(ChildGrant::ReadOnly {
                unguarded: crate::tools::UnguardedGrant::Denied,
            }),
        );
        assert_eq!(payload_version(child.kind()), Some(12));
        let payload = child.payload().expect("child payload");
        let mut named = child.clone();
        if let Event::AgentSpawned { profile, .. } = &mut named {
            *profile = Some("inspect".into());
        }
        assert_eq!(decode(&named, &named.payload().unwrap()).unwrap(), named);
        let mut value: serde_json::Value = serde_json::from_str(&payload).expect("JSON");
        for profile in [
            serde_json::json!("../inspect"),
            serde_json::json!(""),
            serde_json::json!("x".repeat(65)),
            serde_json::json!(1),
        ] {
            let mut invalid = value.clone();
            invalid["profile"] = profile;
            assert!(decode(&child, &invalid.to_string()).is_err());
        }
        assert!(
            Event::decode(
                child.kind(),
                payload_version(child.kind()).expect("payload version") - 1,
                &payload,
                Some(run_id),
                Some(agent_run_id)
            )
            .is_err_and(|error| matches!(error, ReplayError::UnknownEvent))
        );
        let mut missing = value.clone();
        missing.as_object_mut().expect("object").remove("profile");
        assert!(decode(&child, &missing.to_string()).is_err());
        missing = value.clone();
        missing.as_object_mut().expect("object").remove("grant");
        assert!(decode(&child, &missing.to_string()).is_err());
        for invalid in [
            serde_json::json!("read_only"),
            serde_json::json!({"read_only":{}}),
            serde_json::json!({"read_only":{"unguarded":"human_only","automatic":true}}),
        ] {
            let mut forged = value.clone();
            forged["grant"] = invalid;
            assert!(decode(&child, &forged.to_string()).is_err());
        }
        value["grant"] = serde_json::Value::Null;
        assert!(decode(&child, &value.to_string()).is_err());
        value["grant"] = "write".into();
        assert!(decode(&child, &value.to_string()).is_err());

        let primary_payload = primary.payload().expect("primary payload");
        let mut primary_value: serde_json::Value =
            serde_json::from_str(&primary_payload).expect("JSON");
        primary_value["grant"] = "no_tools".into();
        assert!(decode(&primary, &primary_value.to_string()).is_err());
    }
}

mod payload_version_tests {
    use super::*;

    #[test]
    fn decode_accepts_the_current_version_and_rejects_its_neighbours() {
        let run = RunId::new();
        let agent = AgentRunId::new();
        let event = Event::SessionRenamed {
            title: "renamed".into(),
        };
        let payload = event.payload().expect("payload");
        let version = payload_version(event.kind()).expect("known kind");
        assert_eq!(
            Event::decode(event.kind(), version, &payload, None, None).expect("current replay"),
            event
        );
        for stale in [version - 1, version + 1] {
            assert!(matches!(
                Event::decode(event.kind(), stale, &payload, None, None),
                Err(ReplayError::UnknownEvent)
            ));
        }
        for kind in [
            "ToolStarted",
            "ToolFinished",
            "RunStarted",
            "HookStarted",
            "HookFinished",
        ] {
            let version = payload_version(kind).expect("known kind");
            for stale in [version - 1, version + 1] {
                assert!(matches!(
                    Event::decode(kind, stale, "{}", Some(run), Some(agent)),
                    Err(ReplayError::UnknownEvent)
                ));
            }
        }
        assert!(matches!(
            Event::decode("NotAKind", 6, "{}", Some(run), Some(agent)),
            Err(ReplayError::UnknownEvent)
        ));
        assert_eq!(payload_version("NotAKind"), None);
    }

    fn read_observation(run: RunId, agent: AgentRunId) -> crate::tools::ToolObservation {
        let intent = crate::tools::EffectIntent {
            target: crate::session::worker::EffectTarget::Workspace,
            id: Uuid::now_v7(),
            run_id: run,
            agent_run_id: agent,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: crate::tools::ToolCall::Read {
                path: "src/lib.rs".into(),
                offset: None,
                limit: Some(16),
            },
            limits: crate::tools::ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        crate::tools::ToolObservation {
            enforcement_cause: None,
            limit_cause: None,
            attestation: Some(crate::tools::Attestation::InProcess {
                policy_digest: intent.policy_digest,
                workspace_device: intent.workspace_device,
                workspace_inode: intent.workspace_inode,
            }),
            intent,
            disposition: crate::tools::ToolDisposition::Succeeded,
            output: "{}".into(),
            cache_update: crate::tools::CacheUpdate::NotApplicable,
        }
    }

    #[test]
    fn tool_events_reject_internal_rewind_sources() {
        let run = RunId::new();
        let agent = AgentRunId::new();
        let mut observation = read_observation(run, agent);
        observation.disposition = crate::tools::ToolDisposition::Denied;
        observation.attestation = None;
        let events = [
            Event::ToolStarted {
                run_id: run,
                agent_run_id: agent,
                intent: observation.intent.clone(),
                active_ceiling_ms: 1,
            },
            Event::ToolFinished {
                checkpoint: None,
                run_id: run,
                agent_run_id: agent,
                observation,
            },
        ];
        let call = crate::tools::ToolCall::ApplyChangeset {
            source: ChangesetSource::Rewind {
                plan_digest: [3; 32],
                bytes: 1,
                sha256: [4; 32],
            },
        };
        for mut event in events {
            let mut payload: serde_json::Value =
                serde_json::from_str(&event.payload().unwrap()).unwrap();
            let intent = match &mut event {
                Event::ToolStarted { intent, .. } => {
                    payload["intent"]["call"] = serde_json::to_value(&call).unwrap();
                    intent
                }
                Event::ToolFinished { observation, .. } => {
                    payload["observation"]["intent"]["call"] = serde_json::to_value(&call).unwrap();
                    &mut observation.intent
                }
                _ => unreachable!(),
            };
            intent.call = call.clone();
            assert!(intent.valid());
            if let Event::ToolFinished { observation, .. } = &event {
                assert!(observation.valid());
            }
            assert!(matches!(event.validate(), Err(ReplayError::InvalidPayload)));
            assert!(matches!(
                Event::decode(
                    event.kind(),
                    event.version(),
                    &payload.to_string(),
                    Some(run),
                    Some(agent),
                ),
                Err(ReplayError::InvalidPayload)
            ));
        }
    }

    #[test]
    fn a_tool_finished_bump_rejects_its_previous_version() {
        let run = RunId::new();
        let agent = AgentRunId::new();
        let finished = Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation: read_observation(run, agent),
        };
        let payload = finished.payload().expect("payload");
        assert_eq!(payload_version("ToolFinished"), Some(21));
        assert_eq!(
            Event::decode("ToolFinished", 21, &payload, Some(run), Some(agent)).expect("current"),
            finished
        );
        let mut missing: serde_json::Value = serde_json::from_str(&payload).unwrap();
        missing.as_object_mut().unwrap().remove("checkpoint");
        assert!(
            Event::decode(
                "ToolFinished",
                21,
                &missing.to_string(),
                Some(run),
                Some(agent)
            )
            .is_err()
        );
        for invalid in [
            payload.replacen("\"checkpoint\":null", "\"checkpoint\":null,\"checkpoint\":null", 1),
            payload.replacen("\"checkpoint\":null", "\"checkpoint\":null,\"unknown\":null", 1),
            payload.replacen("\"checkpoint\":null", "\"checkpoint\":{\"status\":\"captured\",\"reason\":\"quota\",\"pruned_before_run\":null}", 1),
        ] {
            assert!(Event::decode("ToolFinished", 21, &invalid, Some(run), Some(agent)).is_err());
        }
        let mut missing: serde_json::Value = serde_json::from_str(&payload).unwrap();
        missing.as_object_mut().unwrap().remove("limit_cause");
        assert!(
            Event::decode(
                "ToolFinished",
                21,
                &missing.to_string(),
                Some(run),
                Some(agent)
            )
            .is_err()
        );
        let mut unknown: serde_json::Value = serde_json::from_str(&payload).unwrap();
        unknown["disposition"] = "limit".into();
        unknown["limit_cause"] = "invented".into();
        assert!(
            Event::decode(
                "ToolFinished",
                21,
                &unknown.to_string(),
                Some(run),
                Some(agent)
            )
            .is_err()
        );
        for previous in [20, 19, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7] {
            assert!(matches!(
                Event::decode("ToolFinished", previous, &payload, Some(run), Some(agent)),
                Err(ReplayError::UnknownEvent)
            ));
        }
    }

    #[test]
    fn the_unguarded_format_bumps_reject_their_previous_versions() {
        let run = RunId::new();
        let agent = AgentRunId::new();
        let mut intent = read_observation(run, agent).intent;
        intent.call = crate::tools::ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
            cwd: String::new(),
            justification: "Run the failed command once".into(),
        };
        let started = Event::ToolStarted {
            run_id: run,
            agent_run_id: agent,
            intent,
            active_ceiling_ms: 1,
        };
        let verdict = Event::ToolPermissionDecided {
            run_id: run,
            agent_run_id: agent,
            verdict: crate::tools::permissions::PermissionVerdict {
                intent_digest: [3; 32],
                decision: crate::tools::permissions::Decision::Allow,
                mode: None,
                basis: crate::tools::permissions::PermissionSource::ExactCommandRule,
                source: crate::tools::permissions::PermissionSource::ExactCommandRule,
                approved: true,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            },
        };
        for event in [&started, &verdict] {
            let payload = event.payload().expect("payload");
            let version = payload_version(event.kind()).expect("known kind");
            assert_eq!(
                Event::decode(event.kind(), version, &payload, Some(run), Some(agent))
                    .expect("current"),
                *event
            );
            assert!(matches!(
                Event::decode(event.kind(), version - 1, &payload, Some(run), Some(agent)),
                Err(ReplayError::UnknownEvent)
            ));
        }
        assert_eq!(payload_version("ToolStarted"), Some(15));
        assert_eq!(payload_version("ToolPermissionDecided"), Some(5));
    }

    #[test]
    fn an_in_process_attestation_never_validates_a_guard_site_operation() {
        let run = RunId::new();
        let agent = AgentRunId::new();
        let read = read_observation(run, agent);
        assert!(read.valid());
        let mut drifted = read.clone();
        drifted.attestation = Some(crate::tools::Attestation::InProcess {
            policy_digest: [1; 32],
            workspace_device: 1,
            workspace_inode: 3,
        });
        assert!(!drifted.valid());

        let mut write = read.clone();
        write.intent.call = crate::tools::ToolCall::Write {
            path: "src/lib.rs".into(),
            expected_digest: None,
            content: "x".into(),
        };
        assert!(
            !write.valid(),
            "a read attestation cannot authorize a write"
        );
        assert!(matches!(
            Event::ToolFinished {
                checkpoint: None,
                run_id: run,
                agent_run_id: agent,
                observation: write.clone(),
            }
            .validate(),
            Err(ReplayError::InvalidPayload)
        ));

        let mut guarded_read = read;
        guarded_read.attestation = Some(crate::tools::Attestation::Guard(
            crate::tools::GuardReceipt {
                contract_version: 1,
                intent_digest: guarded_read.intent.digest(),
                enforcement_digest: guarded_read.intent.enforcement_digest,
                limits: guarded_read.intent.limits.clone(),
            },
        ));
        assert!(!guarded_read.valid(), "reads attest only in process");
    }

    #[test]
    fn superseded_hook_and_terminal_versions_are_rejected() {
        for (kind, old) in [
            ("HookFinished", 2),
            ("HookFinished", 3),
            ("HookFinished", 4),
            ("HookFinished", 5),
            ("HookFinished", 6),
            ("HookFinished", 7),
            ("AgentFinished", 8),
            ("AgentFinished", 9),
            ("ProviderCallRecorded", 12),
            ("ProviderCallRecorded", 8),
            ("RunStarted", 14),
            ("RunStarted", 15),
            ("RunFinished", 7),
            ("ProviderCallRecorded", 7),
        ] {
            assert!(matches!(
                Event::decode(kind, old, "{}", None, None),
                Err(ReplayError::UnknownEvent)
            ));
        }
    }

    #[test]
    fn ledger_diet_kinds_reject_their_stale_version_while_unbumped_kinds_decode() {
        let run = RunId::new();
        let agent = AgentRunId::new();
        let goal_id = GoalId::new();
        let config = RunConfig {
            supervisor: None,
            worker_policy: None,
            agent_definitions: None,
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
            policy: CollaborationPolicy::Single,
            output_token_cap: 4096,
            provider_concurrency: 1,
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
            tool_policy: None,
        };
        let bumped = [
            Event::RunStarted {
                run_id: run,
                config: config.clone(),
                active_deadline_ms: Some(MAX_GOAL_ACTIVE_MS),
            },
            Event::RunFinished {
                run_id: run,
                disposition: RunDisposition::Finished,
                failure: None,
                active_ms: 1_234,
            },
            Event::GoalProviderCallReserved {
                goal_id,
                run_id: run,
                agent_run_id: agent,
                active_ceiling_ms: 605_000,
            },
            Event::GoalCompactionCallReserved {
                goal_id,
                run_id: run,
                active_ceiling_ms: 125_000,
            },
            Event::ToolStarted {
                run_id: run,
                agent_run_id: agent,
                intent: read_observation(run, agent).intent,
                active_ceiling_ms: 610_000,
            },
        ];
        for event in bumped {
            let (run_id, agent_run_id) = event.scope();
            let payload = event.payload().expect("payload");
            let version = match event {
                Event::RunStarted { .. } => 20,
                Event::ToolStarted { .. } => 15,
                Event::RunFinished { .. } => 9,
                _ => 7,
            };
            assert_eq!(event.version(), version, "{}", event.kind());
            assert_eq!(
                Event::decode(event.kind(), version, &payload, run_id, agent_run_id)
                    .expect("current"),
                event
            );
            assert!(matches!(
                Event::decode(event.kind(), version - 1, &payload, run_id, agent_run_id),
                Err(ReplayError::UnknownEvent)
            ));
        }
        let unbumped = [
            Event::SessionRenamed {
                title: "renamed".into(),
            },
            Event::GoalSteeringNoteRecorded {
                goal_id,
                run_id: run,
                text: "steer".into(),
            },
        ];
        for event in unbumped {
            let (run_id, agent_run_id) = event.scope();
            let payload = event.payload().expect("payload");
            assert_eq!(
                Event::decode(event.kind(), 6, &payload, run_id, agent_run_id).expect("unbumped"),
                event
            );
        }
        let finished = Event::GoalFinished {
            goal_id,
            status: GoalStatus::Completed,
            active_ms: 1_234,
            report: GoalReport {
                summary: "done".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
            exhausted: false,
        };
        let (run_id, agent_run_id) = finished.scope();
        let payload = finished.payload().expect("payload");
        assert_eq!(
            Event::decode("GoalFinished", 7, &payload, run_id, agent_run_id).expect("bumped"),
            finished
        );
        let started = Event::GoalStarted {
            goal_id,
            objective: "continue".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: GoalBudget::default(),
            phase: GoalPhase::Implementation,
            autonomous: false,
            resource_settings: crate::tools::ToolResourceSettings::default(),
            continues: Some(GoalId::new()),
        };
        let started_payload = started.payload().expect("payload");
        assert_eq!(
            Event::decode("GoalStarted", 7, &started_payload, None, None).expect("v7"),
            started
        );
        for (kind, payload) in [
            ("GoalStarted", &started_payload),
            ("GoalFinished", &payload),
        ] {
            assert!(matches!(
                Event::decode(kind, 6, payload, None, None),
                Err(ReplayError::UnknownEvent)
            ));
        }
        for deleted in ["GoalActiveTimeReserved", "GoalActiveTimeSettled"] {
            assert_eq!(payload_version(deleted), None);
            let payload = format!(r#"{{"goal_id":"{goal_id}","active_ms":1}}"#);
            assert!(matches!(
                Event::decode(deleted, 6, &payload, Some(run), None),
                Err(ReplayError::UnknownEvent)
            ));
        }
        let unbounded = Event::RunStarted {
            run_id: run,
            config,
            active_deadline_ms: Some(MAX_GOAL_ACTIVE_MS + 1),
        };
        assert!(
            unbounded.payload().is_err(),
            "a deadline beyond the Goal maximum"
        );
    }
}

#[test]
fn worker_command_protocol_requires_pins_capture_fields_and_exact_versions() {
    use crate::session::worker::*;
    let f = crate::session::worker_command_protocol_fixture();
    for (kind, pointer, field) in [
        (
            "RunStarted",
            "/config/worker_policy/parent_grant",
            "commands",
        ),
        (
            "ProviderCallRecorded",
            "/delegation/0/assignment",
            "commands",
        ),
        ("AgentSpawned", "/assignment", "cache_generation"),
        ("AgentSpawned", "/assignment/grant", "commands"),
        ("ToolStarted", "/intent/target", "cache_generation"),
        ("ToolFinished", "/intent/target", "cache_generation"),
        ("WorkerHandoverRecorded", "", "criterion_evidence"),
    ] {
        let e = f
            .envelopes()
            .into_iter()
            .find(|e| {
                e.event.kind() == kind && {
                    let v: serde_json::Value =
                        serde_json::from_str(&e.event.payload().unwrap()).unwrap();
                    v.pointer(pointer).and_then(|v| v.get(field)).is_some()
                }
            })
            .expect("command fixture includes the field");
        let mut payload: serde_json::Value =
            serde_json::from_str(&e.event.payload().unwrap()).unwrap();
        payload
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            Event::decode(
                kind,
                e.event.version(),
                &payload.to_string(),
                e.run_id,
                e.agent_run_id
            )
            .is_err(),
            "{kind} {field}"
        );
        assert!(
            Event::decode(
                kind,
                e.event.version() - 1,
                &e.event.payload().unwrap(),
                e.run_id,
                e.agent_run_id
            )
            .is_err(),
            "old {kind}"
        );
    }
    for commands in [
        vec!["x".into(); 9],
        vec!["b".into(), "a".into()],
        vec!["x".into(), "x".into()],
        vec!["x".repeat(65)],
    ] {
        let e = f
            .envelopes()
            .into_iter()
            .find(|e| {
                matches!(
                    e.event,
                    Event::AgentSpawned {
                        assignment: Some(_),
                        ..
                    }
                )
            })
            .unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&e.event.payload().unwrap()).unwrap();
        let decode = |v: &serde_json::Value| {
            Event::decode(
                e.event.kind(),
                e.event.version(),
                &v.to_string(),
                e.run_id,
                e.agent_run_id,
            )
        };
        assert!(decode(&value).is_ok(), "unmutated command payload decodes");
        *value
            .pointer_mut("/assignment/request/commands")
            .expect("fixture carries request commands") = serde_json::json!(["a", "b"]);
        assert!(decode(&value).is_ok(), "a valid command list decodes");
        *value.pointer_mut("/assignment/request/commands").unwrap() = serde_json::json!(commands);
        assert!(decode(&value).is_err(), "commands {commands:?}");
    }
    let result = WorkerCommandResult {
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
        stdout_total: Default::default(),
        stderr_total: Default::default(),
        private_capture: PrivateCapture::None {},
    };
    for field in [
        "exit_code",
        "stdout_total",
        "stderr_total",
        "private_capture",
    ] {
        let mut value = serde_json::to_value(&result).unwrap();
        value.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkerCommandResult>(value).is_err(),
            "missing {field}"
        );
    }
    for capture in [
        serde_json::json!({"kind":"unknown"}),
        serde_json::json!({"kind":"none","extra":true}),
        serde_json::json!({"kind":"rejected","cause":"unknown"}),
    ] {
        let mut value = serde_json::to_value(&result).unwrap();
        value["private_capture"] = capture;
        assert!(serde_json::from_value::<WorkerCommandResult>(value).is_err());
    }
}

#[test]
fn supervisor_event_v1_is_closed_bounded_and_run_started_requires_the_pin() {
    use crate::supervisor::*;
    let fixture = crate::session::worker_protocol_fixture();
    let start = fixture
        .events
        .iter()
        .find(|e| matches!(e, Event::RunStarted { .. }))
        .unwrap();
    for case in 0..5 {
        let mut event = start.clone();
        if let Event::RunStarted { config, .. } = &mut event {
            let mut pin = SupervisorPolicy::default();
            match case {
                0 => (),
                1 => pin.evaluator = 2,
                2 => pin.window = 33,
                3 => pin.digest[0] ^= 1,
                _ => pin.floors.identical = 0,
            }
            config.supervisor = Some(pin);
        }
        assert_eq!(event.payload().is_ok(), case == 0, "policy pin {case}");
    }

    let verdict = SupervisorVerdict {
        subject_sequence: 10,
        site: Site::ToolOutcome,
        evaluator: 1,
        mode: Mode::Shadow,
        capped: false,
        signals: vec![Signal {
            signal: SignalKind::Loop,
            level: Level::Notice,
            would_response: None,
            features: Features::Loop(LoopFeatures {
                repeat_identical: 3,
                ..Default::default()
            }),
            evidence: vec![8, 9, 10],
        }],
    };
    let event = Event::SupervisorVerdictRecorded {
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        verdict,
    };
    assert_eq!(event.version(), 1);
    let (run, actor) = event.scope();
    let payload = event.payload().unwrap();
    assert_eq!(
        Event::decode(event.kind(), 1, &payload, run, actor).unwrap(),
        event
    );
    assert!(matches!(
        Event::decode(event.kind(), 0, &payload, run, actor),
        Err(ReplayError::UnknownEvent)
    ));
    for (field, value) in [
        ("evaluator", serde_json::json!(2)),
        ("site", serde_json::json!("unknown")),
        ("mode", serde_json::json!("enforce")),
        ("extra", serde_json::json!(null)),
        ("signals", serde_json::json!([])),
    ] {
        let mut value_payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        value_payload[field] = value;
        assert!(
            Event::decode(event.kind(), 1, &value_payload.to_string(), run, actor).is_err(),
            "{field}"
        );
    }
    for field in ["would_response", "features"] {
        let mut value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        value["signals"][0].as_object_mut().unwrap().remove(field);
        assert!(
            Event::decode(event.kind(), 1, &value.to_string(), run, actor).is_err(),
            "missing {field}"
        );
    }
    let padded = format!(
        "{payload}{}",
        " ".repeat(MAX_VERDICT_BYTES + 1 - payload.len())
    );
    assert!(matches!(
        Event::decode(event.kind(), 1, &padded, run, actor),
        Err(ReplayError::InvalidPayload)
    ));
    assert_eq!(payload_version("RunStarted"), Some(20));
}

#[test]
fn refusal_and_run_failure_payloads_require_current_versions_and_exact_pairings() {
    use crate::provider::ProviderFailureReason;
    let run_id = Some(RunId::new());
    let agent_id = Some(AgentRunId::new());
    assert_eq!(payload_version("ProviderCallRecorded"), Some(15));
    assert_eq!(payload_version("RunFinished"), Some(9));
    let record = ProviderCallRecord {
        delegation: None,
        phase: crate::provider::AgentPhase::RootPlan,
        disposition: ProviderCallDisposition::InvalidResponse,
        requested_tools: 0,
        response_id: None,
        input_tokens: None,
        output_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        wire_provenance: None,
        failure_reason: Some(ProviderFailureReason::ProposalRefused),
        refusal: Some(ProposalRefusal {
            operation: "arany_write".into(),
            cause: ProposalRefusalCause::PlanInvalid,
        }),
    };
    let valid = serde_json::to_value(record).unwrap();
    let decode_call = |value: &serde_json::Value, version| {
        Event::decode(
            "ProviderCallRecorded",
            version,
            &value.to_string(),
            run_id,
            agent_id,
        )
    };
    assert!(decode_call(&valid, 15).is_ok());
    assert!(decode_call(&valid, 14).is_err());
    for (field, value) in [
        ("refusal", serde_json::Value::Null),
        ("failure_reason", serde_json::Value::Null),
        ("failure_reason", serde_json::json!("response_contract")),
        ("disposition", serde_json::json!("finished")),
        (
            "refusal",
            serde_json::json!({"operation":"PRIVATE_ARGUMENT", "cause":"plan_invalid"}),
        ),
        (
            "refusal",
            serde_json::json!({"operation":"arany_write", "cause":"unknown"}),
        ),
        (
            "refusal",
            serde_json::json!({"operation":"arany_write", "cause":"plan_invalid", "arguments":"PRIVATE_ARGUMENT"}),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert!(decode_call(&invalid, 15).is_err(), "field {field}");
    }
    let mut worker_phase = valid.clone();
    worker_phase["phase"] = serde_json::json!("child_work");
    assert!(decode_call(&worker_phase, 15).is_err());
    let mut synthesis = valid.clone();
    synthesis["phase"] = serde_json::json!("root_synthesis");
    assert!(decode_call(&synthesis, 15).is_ok());
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("refusal");
    assert!(decode_call(&missing, 15).is_err());
    for (disposition, failure, accepted) in [
        ("failed", serde_json::json!("proposal_refused"), true),
        ("failed", serde_json::Value::Null, false),
        ("finished", serde_json::json!("proposal_refused"), false),
        ("cancelled", serde_json::json!("provider_call"), false),
        ("stopped_by_hook", serde_json::json!("provider_call"), false),
        ("failed", serde_json::json!("unknown"), false),
        ("failed", serde_json::json!("budget_exhausted"), true),
        ("failed", serde_json::json!("tool_stopped"), true),
        ("failed", serde_json::json!("definition_invalid"), true),
        ("finished", serde_json::Value::Null, true),
    ] {
        let value =
            serde_json::json!({"disposition":disposition, "failure":failure, "active_ms":0});
        assert_eq!(
            Event::decode("RunFinished", 9, &value.to_string(), run_id, None).is_ok(),
            accepted,
            "{disposition}"
        );
        assert!(Event::decode("RunFinished", 8, &value.to_string(), run_id, None).is_err());
    }
    assert!(
        Event::decode(
            "RunFinished",
            9,
            r#"{"disposition":"failed","active_ms":0}"#,
            run_id,
            None
        )
        .is_err()
    );
}

#[test]
fn plan_recorded_is_a_primary_control_call_without_effects_or_failure() {
    let record = ProviderCallRecord {
        delegation: None,
        phase: crate::provider::AgentPhase::RootPlan,
        disposition: ProviderCallDisposition::PlanRecorded,
        requested_tools: 0,
        response_id: Some("response_plan".into()),
        input_tokens: Some(8),
        output_tokens: Some(4),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        wire_provenance: None,
        failure_reason: None,
        refusal: None,
    };
    let value = serde_json::to_value(&record).unwrap();
    let decode = |payload: &serde_json::Value, version| {
        Event::decode(
            "ProviderCallRecorded",
            version,
            &payload.to_string(),
            Some(RunId::new()),
            Some(AgentRunId::new()),
        )
    };
    assert!(decode(&value, 15).is_ok());
    assert!(decode(&value, 14).is_err());
    for (key, invalid) in [
        ("phase", serde_json::json!("child_work")),
        ("phase", serde_json::json!("tool_review")),
        ("requested_tools", serde_json::json!(1)),
        ("failure_reason", serde_json::json!("response_contract")),
        (
            "refusal",
            serde_json::json!({"operation":"arany_plan","cause":"plan_invalid"}),
        ),
    ] {
        let mut malformed = value.clone();
        malformed[key] = invalid;
        assert!(decode(&malformed, 15).is_err(), "{key}");
    }
}

#[test]
fn accepted_message_requires_current_nullable_skill() {
    let run_id = Some(RunId::new());
    assert_eq!(payload_version("MessageAccepted"), Some(7));
    for skill in [serde_json::Value::Null, serde_json::json!("ask-matt")] {
        let payload = serde_json::json!({"text":"do X", "images":[], "skill":skill});
        let event =
            Event::decode("MessageAccepted", 7, &payload.to_string(), run_id, None).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&event.payload().unwrap()).unwrap(),
            payload
        );
        assert!(Event::decode("MessageAccepted", 6, &payload.to_string(), run_id, None).is_err());
    }
    for payload in [
        serde_json::json!({"text":"do X", "images":[]}),
        serde_json::json!({"text":"do X", "images":[], "skill":"../escape"}),
        serde_json::json!({"text":"do X", "images":[], "skill":false}),
    ] {
        assert!(Event::decode("MessageAccepted", 7, &payload.to_string(), run_id, None).is_err());
    }
}

#[test]
fn clearing_event_has_one_strict_current_payload() {
    let run = RunId::new();
    let event = Event::ToolResultsCleared {
        run_id: run,
        covered_tool_count: 4,
        cleared_bytes: 1234,
    };
    assert_eq!(payload_version(event.kind()), Some(1));
    let payload = event.payload().unwrap();
    assert_eq!(
        Event::decode(event.kind(), 1, &payload, Some(run), None).unwrap(),
        event
    );
    for (version, body, agent) in [
        (0, payload.clone(), None),
        (2, payload.clone(), None),
        (1, payload.clone(), Some(AgentRunId::new())),
        (1, r#"{"covered_tool_count":4}"#.into(), None),
        (
            1,
            r#"{"covered_tool_count":0,"cleared_bytes":1}"#.into(),
            None,
        ),
        (
            1,
            r#"{"covered_tool_count":4,"cleared_bytes":1,"extra":true}"#.into(),
            None,
        ),
    ] {
        assert!(Event::decode(event.kind(), version, &body, Some(run), agent).is_err());
    }
}
