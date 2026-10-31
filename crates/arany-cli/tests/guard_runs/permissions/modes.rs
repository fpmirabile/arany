use super::*;

#[tokio::test]
async fn permission_dispatch_modes_rules_command_mcp_and_edit() {
    for mode in ["default", "acceptEdits", "plan", "auto", "dontAsk"] {
        for rule in ["allow", "ask", "deny"] {
            for (kind, pattern) in [
                ("command", "Bash"),
                ("mcp", "mcp__server__echo"),
                ("edit", "Edit"),
            ] {
                let settings = json!({"permissions":{"defaultMode":mode,(rule):[pattern]}});
                let fixture = Fixture::new(Some(&settings), true);
                let call = call(kind);
                let denied = mode == "plan" || rule == "deny" || mode == "dontAsk" && rule == "ask";
                let human = !denied && rule == "ask";
                let mut outcomes = vec![
                    arany::test_support::scenario::plan_outcome(plan(&call)),
                    tool_outcome(call.clone()),
                ];
                outcomes.push(finish("answer"));
                let (result, requests, humans) = fixture
                    .run(
                        outcomes,
                        human.then_some(arany::ApprovalMode::Request),
                        false,
                    )
                    .await;
                let case = format!("{mode}/{rule}/{kind}");
                assert_eq!(humans, usize::from(human), "{case}");
                assert!(
                    requests
                        .iter()
                        .all(|request| request.phase != AgentPhase::ToolReview),
                    "{case}: user rules resolve before any review"
                );
                assert_eq!(
                    fixture.workspace.join("notes/new.txt").exists(),
                    kind == "edit" && !denied,
                    "{case}"
                );
                let (events, view) = fixture.reopen(result.session_id).await;
                let tool = &view.runs[0].tools[0];
                let verdict = tool.permission_verdict.as_ref().unwrap();
                assert_eq!(verdict.approved, !denied, "{case}");
                assert_eq!(
                    verdict.source,
                    if human {
                        PermissionSource::Human
                    } else if mode == "plan" {
                        PermissionSource::Mode
                    } else {
                        PermissionSource::Rule
                    },
                    "{case}"
                );
                let observation = tool.observation.as_ref().unwrap();
                assert_eq!(
                    observation.disposition,
                    if denied {
                        arany::ToolDisposition::Denied
                    } else {
                        arany::ToolDisposition::Succeeded
                    },
                    "{case}"
                );
                assert_eq!(observation.attestation.is_some(), !denied, "{case}");
                let started = events
                    .iter()
                    .position(|event| matches!(event.event, Event::ToolStarted { .. }))
                    .unwrap();
                let proposed = events
                    .iter()
                    .position(|event| matches!(event.event, Event::ToolProposed { .. }));
                let finished = events
                    .iter()
                    .position(|event| matches!(event.event, Event::ToolFinished { .. }))
                    .unwrap();
                if human {
                    assert!(
                        proposed.is_some_and(|proposed| proposed < started),
                        "{case}"
                    );
                } else {
                    assert!(proposed.is_none(), "{case}");
                }
                assert!(started < finished, "{case}");
            }
        }
    }
}

#[tokio::test]
async fn permission_dispatch_settings_and_terminal_modes_only_narrow() {
    for (settings, kind, terminal, human, ai, denied) in [
        (
            json!({"permissions":{"defaultMode":"acceptEdits"}}),
            "edit",
            arany::ApprovalMode::Request,
            true,
            false,
            false,
        ),
        (
            json!({"permissions":{"allow":["Bash(sh -c true)"]}}),
            "command",
            arany::ApprovalMode::Request,
            false,
            false,
            false,
        ),
        (
            json!({"permissions":{"defaultMode":"auto"}}),
            "edit",
            arany::ApprovalMode::Request,
            true,
            false,
            false,
        ),
        (
            json!({"permissions":{"defaultMode":"auto"}}),
            "protected_edit",
            arany::ApprovalMode::Auto,
            false,
            true,
            false,
        ),
        (
            json!({"permissions":{"ask":["Edit"]}}),
            "edit",
            arany::ApprovalMode::Auto,
            true,
            false,
            false,
        ),
        (
            json!({"permissions":{"allow":["Edit"]}}),
            "edit",
            arany::ApprovalMode::Auto,
            false,
            false,
            false,
        ),
        (
            json!({"permissions":{"deny":["Edit"]}}),
            "edit",
            arany::ApprovalMode::Auto,
            false,
            false,
            true,
        ),
    ] {
        let fixture = Fixture::new(Some(&settings), true);
        let mut call = call(kind);
        if let arany::ToolCall::Command { args, .. } = &mut call {
            *args = vec!["-c".into(), "true".into()];
        }
        if settings["permissions"]["allow"].is_array() {
            assert_eq!(
                arany::permissions::PermissionConfig::parse(
                    &serde_json::to_vec(&settings).unwrap()
                )
                .unwrap()
                .decision(&call),
                Decision::Allow
            );
        }
        let mut outcomes = vec![
            arany::test_support::scenario::plan_outcome(plan(&call)),
            tool_outcome(call.clone()),
        ];
        if ai {
            outcomes.push(verdict("approve"));
        }
        outcomes.push(finish("answer"));
        let (result, requests, humans) = fixture.run(outcomes, Some(terminal), false).await;
        assert_eq!(humans, usize::from(human), "{settings}/{terminal:?}");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ToolReview)
                .count(),
            usize::from(ai)
        );
        let (_, view) = fixture.reopen(result.session_id).await;
        let verdict = view.runs[0].tools[0].permission_verdict.as_ref().unwrap();
        let allowed = !human && !ai && !denied;
        assert_eq!(
            verdict.decision,
            if denied {
                Decision::Deny
            } else if allowed {
                Decision::Allow
            } else {
                Decision::Ask
            }
        );
        assert_eq!(verdict.approved, !denied);
        assert_eq!(
            verdict.basis,
            if denied || allowed || settings["permissions"]["ask"].is_array() {
                PermissionSource::Rule
            } else {
                PermissionSource::Mode
            }
        );
        assert_eq!(
            verdict.source,
            if denied || allowed {
                PermissionSource::Rule
            } else if human {
                PermissionSource::Human
            } else {
                PermissionSource::AiReview
            }
        );
        assert_eq!(
            view.runs[0].tools[0]
                .observation
                .as_ref()
                .unwrap()
                .disposition,
            if denied {
                arany::ToolDisposition::Denied
            } else {
                arany::ToolDisposition::Succeeded
            }
        );
    }
}

#[tokio::test]
async fn permission_dispatch_missing_settings_preserves_existing_approval_flow() {
    for mode in [
        None,
        Some(arany::ApprovalMode::AutoEdits),
        Some(arany::ApprovalMode::Request),
        Some(arany::ApprovalMode::Auto),
    ] {
        let fixture = Fixture::new(None, true);
        private_file(
            &fixture.workspace.join("permissions.json"),
            br#"{"permissions":{"deny":["Edit"]}}"#,
        );
        let call = call("edit");
        let mut outcomes = vec![
            arany::test_support::scenario::plan_outcome(plan(&call)),
            tool_outcome(call.clone()),
        ];
        outcomes.push(finish("answer"));
        let (result, _, humans) = fixture.run(outcomes, mode, false).await;
        assert_eq!(
            humans,
            usize::from(mode == Some(arany::ApprovalMode::Request))
        );
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("notes/new.txt")).unwrap(),
            "done"
        );
        let (events, view) = fixture.reopen(result.session_id).await;
        assert!(!events.iter().any(|event| matches!(
            event.event,
            Event::ToolStarted {
                verdict: Some(_),
                ..
            }
        )));
        assert_eq!(
            view.runs[0]
                .config
                .as_ref()
                .unwrap()
                .tool_policy
                .as_ref()
                .unwrap()
                .permissions_digest,
            None
        );
    }
}

#[tokio::test]
async fn permission_dispatch_children_inherit_parent_restrictions_and_review() {
    for (settings, kind, denied, human) in [
        (
            json!({"permissions":{"allow":["Read"],"deny":["Read(./notes/**)"]}}),
            "read",
            true,
            false,
        ),
        (
            json!({"permissions":{"defaultMode":"auto","allow":["mcp__server"]}}),
            "catalog",
            false,
            false,
        ),
        (
            json!({"permissions":{"defaultMode":"auto","ask":["Read"]}}),
            "read",
            false,
            true,
        ),
    ] {
        let fixture = Fixture::new(Some(&settings), false);
        let mut outcomes = vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect the facts".into(),
                    profile: None,
                }],
            }),
            tool_outcome(call(kind)),
        ];
        outcomes.extend([finish("report"), finish("answer")]);
        let (result, requests, humans) = fixture
            .run(outcomes, Some(arany::ApprovalMode::Auto), true)
            .await;
        assert_eq!(humans, usize::from(human));
        let (events, view) = fixture.reopen(result.session_id).await;
        let child = &view.runs[0].agents[1];
        assert_eq!(
            child.grant,
            Some(arany::ChildGrant::ReadOnly {
                unguarded: arany::UnguardedGrant::Denied
            })
        );
        assert_eq!(
            child.tools[0].observation.as_ref().unwrap().disposition,
            if denied {
                arany::ToolDisposition::Denied
            } else {
                arany::ToolDisposition::Succeeded
            }
        );
        assert_eq!(
            child.tools[0].permission_verdict.as_ref().unwrap().approved,
            !denied
        );
        assert!(
            requests
                .iter()
                .all(|request| request.phase != AgentPhase::ToolReview)
        );
        if denied {
            let continuation = requests
                .iter()
                .rev()
                .find(|request| request.phase == AgentPhase::ChildWork)
                .unwrap();
            assert_eq!(
                continuation
                    .input
                    .tools
                    .as_ref()
                    .unwrap()
                    .observations()
                    .last()
                    .unwrap()
                    .output,
                "permission_rule_denied: no operation was dispatched"
            );
            let synthesis = requests
                .iter()
                .find(|request| request.phase == AgentPhase::RootSynthesis)
                .unwrap();
            assert!(synthesis.input.child_results.iter().any(|report| {
                report
                    .result
                    .contains("permission_rule_denied: no operation was dispatched")
            }));
        }
        assert!(events.iter().any(|event| matches!(&event.event, Event::ToolStarted { agent_run_id, verdict: Some(_), .. } if *agent_run_id == child.id)));
    }
}

#[tokio::test]
async fn researcher_decline_reaches_child_continuation_and_primary_report() {
    let mut fixture = Fixture::new(Some(&json!({"permissions":{"ask":["Read"]}})), false);
    fixture.deny_humans = true;
    let (result, requests, humans) = fixture
        .run(
            vec![
                ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        editor: None,
                        objective: "Inspect the facts".into(),
                        profile: None,
                    }],
                }),
                tool_outcome(call("read")),
                finish("report"),
                finish("answer"),
            ],
            Some(arany::ApprovalMode::Request),
            true,
        )
        .await;
    assert_eq!(humans, 1);
    let (_, view) = fixture.reopen(result.session_id).await;
    let child = &view.runs[0].agents[1];
    assert_eq!(
        child.tools[0].model_denial_output(),
        Some("user_declined: no operation was dispatched")
    );
    let continuation = requests
        .iter()
        .rev()
        .find(|request| request.phase == AgentPhase::ChildWork)
        .unwrap();
    assert_eq!(
        continuation
            .input
            .tools
            .as_ref()
            .unwrap()
            .observations()
            .last()
            .unwrap()
            .output,
        "user_declined: no operation was dispatched"
    );
    let synthesis = requests
        .iter()
        .find(|request| request.phase == AgentPhase::RootSynthesis)
        .unwrap();
    assert!(synthesis.input.child_results.iter().any(|report| {
        report
            .result
            .contains("user_declined: no operation was dispatched")
    }));
}

#[tokio::test]
async fn researcher_enforcement_cause_reaches_primary_report() {
    let fixture = Fixture::new(None, false);
    std::fs::create_dir(fixture.workspace.join("notes/.gitignore")).unwrap();
    let (result, requests, humans) = fixture
        .run(
            vec![
                ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        editor: None,
                        objective: "Inspect the notes".into(),
                        profile: None,
                    }],
                }),
                tool_outcome(arany::ToolCall::List {
                    path: "notes".into(),
                }),
                finish("report"),
                finish("answer"),
            ],
            None,
            true,
        )
        .await;
    assert_eq!(humans, 0);
    let (_, view) = fixture.reopen(result.session_id).await;
    let tool = &view.runs[0].agents[1].tools[0];
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Denied);
    assert_eq!(
        observation.enforcement_cause,
        Some(arany::EnforcementCause::FilesystemDenied)
    );
    assert!(observation.attestation.is_some());
    assert_eq!(tool.model_denial_output(), None);
    let synthesis = requests
        .iter()
        .find(|request| request.phase == AgentPhase::RootSynthesis)
        .unwrap();
    assert!(synthesis.input.child_results.iter().any(|report| {
        report
            .result
            .contains(arany::EnforcementCause::FilesystemDenied.diagnostic())
            && !report.result.contains("cause unavailable")
            && !report.result.contains("no operation was dispatched")
    }));
}

#[tokio::test]
async fn review_approval_dispatches_within_a_short_goal_budget() {
    let fixture = Fixture::new(Some(&json!({"permissions":{"defaultMode":"auto"}})), true);
    let short = arany::GoalBudget {
        max_active_ms: Some(10 * 60 * 1000),
        ..budget()
    };
    let call = call("protected_edit");
    let (result, _, humans) = fixture
        .run_budgeted(
            vec![
                arany::test_support::scenario::plan_outcome(plan(&call)),
                tool_outcome(call.clone()),
                verdict("approve"),
                finish("answer"),
            ],
            Some(arany::ApprovalMode::Auto),
            false,
            short,
        )
        .await;
    assert_eq!(humans, 0);
    assert_eq!(result.run.status, RunStatus::Finished);
    let (events, view) = fixture.reopen(result.session_id).await;
    let tool = &view.runs[0].tools[0];
    let verdict = tool.permission_verdict.as_ref().unwrap();
    assert!(verdict.approved);
    assert_eq!(verdict.source, PermissionSource::AiReview);
    assert_eq!(
        tool.observation.as_ref().unwrap().disposition,
        arany::ToolDisposition::Succeeded
    );
    let proposed = events
        .iter()
        .position(|event| matches!(event.event, Event::ToolProposed { .. }))
        .expect("review proposal");
    let ceiling = |event: &arany::EventEnvelope| match &event.event {
        Event::GoalProviderCallReserved {
            active_ceiling_ms, ..
        } => Some(*active_ceiling_ms),
        _ => None,
    };
    let call_ceiling = events[..proposed]
        .iter()
        .rev()
        .find_map(ceiling)
        .expect("tool call reserved");
    let review_ceiling = events[proposed..]
        .iter()
        .find_map(ceiling)
        .expect("review reserved");
    assert!(
        review_ceiling <= call_ceiling,
        "the review's crash ceiling adds no Tool action time: {call_ceiling} then {review_ceiling}"
    );
}

#[tokio::test]
async fn review_503_falls_back_and_the_run_completes() {
    use arany::test_support::engine::run_routed;
    use arany::test_support::routing::{input, route, routes};
    let fixture = Fixture::new(Some(&json!({"permissions":{"defaultMode":"auto"}})), true);
    let call = call("protected_edit");
    let primary = route(
        ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(plan(&call)),
            tool_outcome(call.clone()),
            finish("answer"),
        ])
        .with_profile("openai", "primary"),
    );
    // The clock pauses only for the review's backoff; the edit runs after it resumes.
    let mut review = ScriptedProvider::new(vec![])
        .with_profile("openai", "review")
        .with_before_call(0, tokio::time::pause);
    review.responses = Mutex::new(
        (0..=arany::MAX_PROVIDER_RETRIES)
            .map(|_| Err(ProviderError::RemoteHttp(503)))
            .collect(),
    );
    let review = route(review);
    let fallback = route(
        ScriptedProvider::new(vec![verdict("approve")])
            .with_profile("openai", "review-fallback")
            .with_before_call(0, tokio::time::resume),
    );
    let mut bindings = routes(&primary);
    bindings.roles.review = vec![review.clone(), fallback.clone()];
    let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
    let mut request = input(&fixture.workspace);
    request.goal.budget = Some(budget());
    let result = run_routed(
        &fixture.engine,
        bindings,
        request,
        RunControls {
            tools: ToolGrant::Configured,
            approvals: Some(approvals),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    let (_events, view) = fixture.reopen(result.session_id).await;
    let switches = &view.runs[0].provider_fallbacks;
    assert_eq!(switches.len(), 1);
    assert_eq!(switches[0].0, view.runs[0].agents[0].id);
    assert_eq!(switches[0].1.role, arany::ProviderRole::Review);
    assert_eq!(view.runs[0].provider_retries.len(), 3);
    assert_eq!(review.requests.lock().unwrap().len(), 4);
    assert_eq!(fallback.requests.lock().unwrap().len(), 1);
    assert_eq!(
        view.runs[0].tools[0]
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        arany::ToolDisposition::Succeeded
    );
}

#[tokio::test]
async fn permission_dispatch_headless_auto_reviews_and_unanswered_asks_deny() {
    for (mode, explicit_ask, review, approved) in [
        ("default", false, None, false),
        ("auto", false, Some("approve"), true),
        ("auto", false, Some("ask"), false),
        ("auto", true, None, false),
    ] {
        let mut settings = json!({"permissions":{"defaultMode":mode}});
        if explicit_ask {
            settings["permissions"]["ask"] = json!(["Edit"]);
        }
        let fixture = Fixture::new(Some(&settings), true);
        let call = call("protected_edit");
        let mut outcomes = vec![
            arany::test_support::scenario::plan_outcome(plan(&call)),
            tool_outcome(call.clone()),
        ];
        if let Some(review) = review {
            outcomes.push(verdict(review));
        }
        outcomes.push(finish("answer"));
        let (result, requests, humans) = fixture.run(outcomes, None, false).await;
        assert_eq!(humans, 0);
        assert_eq!(requests.len(), 3 + usize::from(review.is_some()));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ToolReview)
                .count(),
            usize::from(review.is_some())
        );
        assert_eq!(fixture.workspace.join("notes/AGENTS.md").exists(), approved);
        let (_, view) = fixture.reopen(result.session_id).await;
        let verdict = view.runs[0].tools[0].permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.approved, approved);
        assert_eq!(
            verdict.source,
            if approved {
                PermissionSource::AiReview
            } else {
                PermissionSource::UnavailableApproval
            }
        );
    }
    for terminal in [None, Some(arany::ApprovalMode::Auto)] {
        let fixture = Fixture::new(Some(&json!({"permissions":{"defaultMode":"auto"}})), true);
        let call = call("protected_edit");
        let mut outcomes = vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect the task".into(),
                    profile: None,
                }],
            }),
            finish("report"),
            arany::test_support::scenario::plan_outcome(plan(&call)),
            tool_outcome(call.clone()),
        ];
        outcomes.push(finish("answer"));
        let (result, requests, humans) = fixture.run(outcomes, terminal, true).await;
        assert_eq!(humans, usize::from(terminal.is_some()));
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[3].phase, AgentPhase::RootSynthesis);
        assert!(
            !requests
                .iter()
                .any(|request| request.phase == AgentPhase::ToolReview)
        );
        let (_, view) = fixture.reopen(result.session_id).await;
        let verdict = view.runs[0].tools[0].permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.approved, terminal.is_some());
        assert_eq!(
            verdict.source,
            if terminal.is_some() {
                PermissionSource::Human
            } else {
                PermissionSource::UnavailableApproval
            }
        );
    }
}

#[tokio::test]
async fn human_denial_on_the_only_allowed_run_is_a_failure_not_a_budget_close() {
    let mut fixture = Fixture::new(None, true);
    fixture.deny_humans = true;
    let call = call("edit");
    let (result, requests, humans) = fixture
        .run(
            vec![
                arany::test_support::scenario::plan_outcome(plan(&call)),
                tool_outcome(call.clone()),
                finish("The proposed edit was declined"),
            ],
            Some(arany::ApprovalMode::Request),
            false,
        )
        .await;
    assert_eq!(humans, 1);
    assert_eq!(requests.len(), 3);
    assert_eq!(result.run.failure, Some(arany::RunFailure::CriterionUnmet));
    assert_eq!(result.run.status, RunStatus::Failed);
    let (_, view) = fixture.reopen(result.session_id).await;
    let goal = view.goal(result.goal_id).unwrap();
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert!(!goal.budget_closed);
    assert!(view.continuable_goal().is_none());
}

#[tokio::test]
async fn agent_definition_guard_modes_preserve_parent_asks_and_add_review() {
    for case in ["parent-ask", "review", "dont-ask"] {
        let review = case == "review";
        let dont_ask = case == "dont-ask";
        let settings = if review {
            json!({"permissions":{"allow":["mcp__server"]}})
        } else {
            json!({"permissions":{"ask":["Read"]}})
        };
        let fixture = Fixture::new(Some(&settings), false);
        agent_file(
            &fixture,
            "reader.md",
            if review {
                "tools: mcp_list\npermissionMode: auto\n"
            } else if dont_ask {
                "tools: Read\npermissionMode: dontAsk\n"
            } else {
                "tools: Read\npermissionMode: acceptEdits\n"
            },
            "Body",
        );
        let mut responses = vec![
            ProviderOutcome::Delegate(Delegate {
                agent: Some("reader".into()),
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect".into(),
                    profile: None,
                }],
            }),
            tool_outcome(call(if review { "catalog" } else { "read" })),
        ];
        responses.extend([finish("report"), finish("answer")]);
        let (result, requests, humans) = fixture
            .run(responses, Some(arany::ApprovalMode::Auto), true)
            .await;
        assert_eq!(result.run.status, RunStatus::Finished);
        assert_eq!(
            requests.len(),
            4,
            "the child's allow rule resolves before review"
        );
        assert_eq!(humans, usize::from(!review && !dont_ask));
        let (_, view) = fixture.reopen(result.session_id).await;
        let verdict = view.runs[0].agents[1].tools[0]
            .permission_verdict
            .as_ref()
            .unwrap();
        assert_eq!(verdict.approved, !dont_ask);
        assert_eq!(
            verdict.source,
            if dont_ask {
                PermissionSource::Mode
            } else if review {
                PermissionSource::Rule
            } else {
                PermissionSource::Human
            }
        );
        assert!(
            view.runs[0].tools[0]
                .permission_verdict
                .as_ref()
                .unwrap()
                .approved
        );
    }
}

#[tokio::test]
async fn auto_approves_workspace_edits_and_reviews_protected_paths() {
    for kind in ["edit", "protected_edit"] {
        let fixture = Fixture::new(Some(&json!({"permissions":{"defaultMode":"auto"}})), true);
        let call = call(kind);
        let reviewed = kind == "protected_edit";
        let mut outcomes = vec![
            arany::test_support::scenario::plan_outcome(plan(&call)),
            tool_outcome(call.clone()),
        ];
        if reviewed {
            outcomes.push(verdict("approve"));
        }
        outcomes.push(finish("answer"));
        let (result, requests, humans) = fixture
            .run(outcomes, Some(arany::ApprovalMode::Auto), false)
            .await;
        assert_eq!(humans, 0, "{kind}");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ToolReview)
                .count(),
            usize::from(reviewed),
            "{kind}"
        );
        let (_, view) = fixture.reopen(result.session_id).await;
        let verdict = view.runs[0].tools[0].permission_verdict.as_ref().unwrap();
        assert!(verdict.approved, "{kind}");
        assert_eq!(
            verdict.source,
            if reviewed {
                PermissionSource::AiReview
            } else {
                PermissionSource::Mode
            },
            "{kind}"
        );
    }
}
