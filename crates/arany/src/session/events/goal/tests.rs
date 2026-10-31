use crate::session::events::*;
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

fn observation(call: ToolCall, disposition: ToolDisposition, output: String) -> ToolObservation {
    let intent = EffectIntent {
        host: None,
        wrap: None,
        rewrite: Vec::new(),
        host_command: None,
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
        use_count: 1,
    };
    ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: (disposition == crate::tools::ToolDisposition::Limit)
            .then_some(crate::tools::LimitCause::Resource),
        attestation: Some(crate::tools::Attestation::for_site(&intent)),
        intent,
        disposition,
        output,
        approval_cause: None,
        guard_stage: None,
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
    assert!(matches_success(&contract, &observations[..1]));
    assert!(!matches_success(&contract, &observations));
}

#[test]
fn a_later_git_switch_invalidates_a_satisfied_command_criterion() {
    let call = ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), "true".into()],
        cwd: String::new(),
        resource_class: None,
        timeout_ms: None,
        additional_reads: None,
    };
    let contract = criterion(GoalSuccessCriterion::ToolCallSucceeded {
        call: Box::new(call.clone()),
    });
    let passed = observation(
        call,
        ToolDisposition::Succeeded,
        serde_json::json!({"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":"","stderr":"","stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}}).to_string(),
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
    assert!(success.valid());
    assert!(matches!(
        success.attestation,
        Some(crate::tools::Attestation::InProcess { .. })
    ));
    let mut guarded = success.clone();
    guarded.attestation = Some(crate::tools::Attestation::Guard(GuardReceipt {
        contract_version: 1,
        intent_digest: guarded.intent.digest(),
        enforcement_digest: guarded.intent.enforcement_digest,
        limits: guarded.intent.limits.clone(),
    }));
    assert!(!matches_success(&contract, &[guarded]));
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
                resource_class: None,
                timeout_ms: None,
                additional_reads: None,
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
fn delete_and_move_scope_and_file_success_follow_both_paths() {
    let digest = crate::validate::hex_digest(b"original");
    let mut contract = criterion(GoalSuccessCriterion::WorkspaceFileDigest {
        path: "src/source".into(),
        sha256: digest.clone(),
    });
    contract.paths = vec!["src".into()];
    let read = observation(
        ToolCall::Read {
            path: "src/source".into(),
            offset: None,
            limit: None,
        },
        ToolDisposition::Succeeded,
        serde_json::json!({"sha256":digest}).to_string(),
    );
    assert!(matches_success(&contract, std::slice::from_ref(&read)));
    for call in [
        ToolCall::Delete {
            path: "src/source".into(),
            expected_digest: Some(digest.clone()),
        },
        ToolCall::Move {
            source: "src/source".into(),
            destination: "src/target".into(),
            expected_digest: Some(digest.clone()),
        },
    ] {
        assert!(contract.allows_call(&call));
        let removed = observation(
            call,
            ToolDisposition::Succeeded,
            serde_json::json!({"removed":{"sha256":digest,"bytes":8,"executable":false}})
                .to_string(),
        );
        assert!(!matches_success(&contract, &[read.clone(), removed]));
    }
    for (source, destination) in [
        ("src/source", "other/target"),
        ("other/source", "src/target"),
    ] {
        assert!(!contract.allows_call(&ToolCall::Move {
            source: source.into(),
            destination: destination.into(),
            expected_digest: Some(digest.clone())
        }));
    }
    contract.success_criterion = GoalSuccessCriterion::WorkspaceFileDigest {
        path: "src/target".into(),
        sha256: digest.clone(),
    };
    let moved = observation(
        ToolCall::Move {
            source: "src/source".into(),
            destination: "src/target".into(),
            expected_digest: Some(digest.clone()),
        },
        ToolDisposition::Succeeded,
        serde_json::json!({"removed":{"sha256":digest,"bytes":8,"executable":false}}).to_string(),
    );
    assert!(matches_success(&contract, &[moved]));
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
    let expected = crate::validate::hex_digest(b"before");
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
                "sha256": crate::validate::hex_digest(b"before")
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
                "sha256": crate::validate::hex_digest(b"after")
            })
            .to_string(),
        ),
    ];
    assert!(matches_success(&contract, &observations[..1]));
    assert!(!matches_success(&contract, &observations));
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
