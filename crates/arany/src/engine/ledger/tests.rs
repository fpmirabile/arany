use super::*;
use crate::session::{
    GoalPhase, GoalStatus, GoalSuccessCriterion, GoalSuccessState, RunId, SessionDefaults,
    SessionId, WorkspaceIdentity,
};

fn goal_view(goal_id: GoalId, budget: GoalBudget) -> GoalView {
    GoalView {
        id: goal_id,
        objective: String::new(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        budget,
        resource_settings: crate::tools::ToolResourceSettings::default(),
        phase: GoalPhase::Implementation,
        autonomous: false,
        status: GoalStatus::Running,
        run_ids: vec![RunId::new()],
        active_run: None,
        plan_contract: None,
        success_state: GoalSuccessState::NoContract,
        success_criterion_satisfied: false,
        awaiting_plan_contract: false,
        active_ms: 0,
        uncertain_active_ms: 0,
        active_deadline: None,
        provider_calls: 0,
        unobserved_provider_calls: Vec::new(),
        compaction_call_reserved: None,
        uncertain_compaction_calls: 0,
        known_tokens: 0,
        known_cached_tokens: 0,
        token_usage_unknown: false,
        report: None,
        started_sequence: 0,
        finished_sequence: None,
        active_ceiling_ms: 0,
    }
}

fn ledger(budget: GoalBudget) -> GoalLedger {
    let goal_id = GoalId::new();
    let view = SessionView {
        id: SessionId::new(),
        title: String::new(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: Some(WorkspaceIdentity {
            device: 1,
            inode: 2,
        }),
        defaults: SessionDefaults::default(),
        created_sequence: 0,
        last_sequence: 0,
        lineage: None,
        runs: Vec::new(),
        compactions: Vec::new(),
        goals: vec![goal_view(goal_id, budget)],
    };
    GoalLedger::new(view, Some(goal_id))
}

fn edit(ledger: &GoalLedger, change: impl FnOnce(&mut GoalView, &mut InFlight)) {
    let mut state = ledger.state.lock().expect("state");
    let State { view, in_flight } = &mut *state;
    change(&mut view.goals[0], in_flight);
}

fn contract(budget: GoalBudget) -> GoalPlanContract {
    GoalPlanContract {
        mission: "complete the task".into(),
        paths: vec!["src".into()],
        commands: Vec::new(),
        resource_class: crate::tools::ToolResourceClass::Default,
        command_timeout_ms: None,
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/main.rs".into(),
            content: "done".into(),
        },
        budget,
    }
}

fn unlimited_time() -> GoalBudget {
    GoalBudget {
        max_active_ms: None,
        ..GoalBudget::default()
    }
}

#[test]
fn poisoned_state_fails_closed() {
    let ledger = ledger(GoalBudget::default());
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = ledger.state.lock().expect("fresh lock");
        panic!("poison the ledger state");
    }));
    assert!(poisoned.is_err());
    assert!(matches!(
        ledger.plan_contract(),
        Err(EngineError::CoordinatorFailed)
    ));
    assert!(!ledger.allows_tool(&crate::tools::ToolCall::Read {
        path: "src/main.rs".into(),
        offset: 0,
        limit: 32,
    }));
    assert!(!ledger.admits(Action::ProviderCall {
        previews: false,
        steering: false,
    }));
    assert_eq!(ledger.remaining_calls(), Some(0));
}

#[test]
fn active_plan_narrowing_uses_settled_action_usage() {
    let ledger = ledger(unlimited_time());
    let narrowing = contract(GoalBudget {
        max_active_ms: Some(10_000),
        ..GoalBudget::default()
    });
    // The live Run's settled actions count although no Event has recorded them yet.
    edit(&ledger, |_, in_flight| in_flight.run_active_ms = 10_000);
    assert!(!ledger.can_narrow(&narrowing));
    edit(&ledger, |_, in_flight| {
        in_flight.run_active_ms = 100;
        in_flight.reserved_ms = Some(20_000);
    });
    assert!(
        ledger.can_narrow(&narrowing),
        "an open reservation is not yet spent"
    );
    edit(&ledger, |_, in_flight| in_flight.reserved_ms = None);
    edit(&ledger, |goal, _| goal.plan_contract = Some(narrowing));
    assert!(
        ledger
            .active_remaining_ms()
            .is_some_and(|remaining| remaining <= 9_900)
    );
}

#[test]
fn unlimited_goal_admits_a_finite_build_action_above_the_default_usage_ceiling() {
    let ledger = ledger(unlimited_time());
    let (_, reserved_ms) = ledger
        .reserve_active(1_845_000)
        .expect("maximum macOS Build action plus Store tail");
    assert_eq!(reserved_ms, 1_850_000);
}

#[test]
fn carried_time_joins_the_next_reservation_and_the_runs_active_time() {
    let ledger = ledger(unlimited_time());
    edit(&ledger, |_, in_flight| {
        in_flight.origin = Some(Instant::now() - Duration::from_secs(11));
    });
    let (_, reserved_ms) = ledger
        .reserve_active(1_845_000)
        .expect("Build action after carried time");
    assert!(reserved_ms >= 1_861_000, "{reserved_ms}");
    assert!(
        matches!(
            ledger.reserve_active(1),
            Err(EngineError::CoordinatorFailed)
        ),
        "one open reservation at a time"
    );
    ledger.settle_active(20_000, None).expect("settled");
    assert!(!ledger.active_reserved());
    assert_eq!(ledger.run_active_ms().expect("Run time"), 20_000);
}

#[tokio::test]
async fn finite_token_plan_reduces_parallelism_and_rejects_more_calls_at_the_cap() {
    let ledger = ledger(GoalBudget::default());
    edit(&ledger, |goal, _| {
        goal.plan_contract = Some(contract(GoalBudget {
            max_tokens: Some(10),
            ..GoalBudget::default()
        }));
        goal.known_tokens = 10;
    });
    assert_eq!(ledger.provider_concurrency(4), 1);
    assert!(
        ledger.tokens_within_limit(),
        "an exact-cap Finish is still valid"
    );
    assert!(!ledger.can_start_provider_call());
    assert!(!ledger.admits(Action::ProviderCall {
        previews: false,
        steering: false,
    }));
    assert!(matches!(
        ledger
            .reserve(Action::ProviderCall {
                previews: false,
                steering: false,
            })
            .await,
        Err(EngineError::GoalBudgetExhausted)
    ));
}

#[test]
fn an_unrecorded_result_counts_toward_the_token_cap() {
    let ledger = ledger(GoalBudget {
        max_tokens: Some(10),
        ..GoalBudget::default()
    });
    ledger.settle(Some(6), Some(4)).unwrap();
    assert!(ledger.tokens_within_limit());
    assert!(!ledger.can_start_provider_call());
    ledger.settle(Some(1), None).unwrap();
    assert!(!ledger.tokens_within_limit(), "unknown usage blocks");
}

#[tokio::test(start_paused = true)]
async fn child_idle_approval_is_excluded_once_from_the_shared_segment() {
    let ledger = ledger(unlimited_time());
    let (started, _) = ledger.reserve_active(10_000).unwrap();
    tokio::time::advance(Duration::from_millis(100)).await;
    ledger
        .exclude_idle_approval(Duration::from_millis(80))
        .unwrap();
    ledger
        .settle_active(duration_ms(started.elapsed()), None)
        .unwrap();
    assert_eq!(ledger.run_active_ms().unwrap(), 20);
    let (started, _) = ledger.reserve_active(10_000).unwrap();
    tokio::time::advance(Duration::from_millis(50)).await;
    ledger
        .settle_active(duration_ms(started.elapsed()), None)
        .unwrap();
    assert_eq!(ledger.run_active_ms().unwrap(), 70);
}

#[test]
fn acknowledged_active_time_overrun_is_recorded_within_the_action_bound() {
    let ledger = ledger(GoalBudget {
        max_active_ms: Some(100),
        ..GoalBudget::default()
    });
    edit(&ledger, |_, in_flight| in_flight.reserved_ms = Some(100));
    assert!(ledger.active_time_exhausted());
    edit(&ledger, |goal, in_flight| {
        goal.active_ms = 101;
        in_flight.reserved_ms = None;
        in_flight.origin = Some(Instant::now());
    });
    assert!(ledger.active_time_exhausted());
}

#[test]
fn open_delegation_reservation_does_not_exhaust_its_own_review() {
    let ledger = ledger(GoalBudget {
        max_active_ms: Some(600_000),
        ..GoalBudget::default()
    });
    let (_started, reserved_ms) = ledger.reserve_active(600_000).unwrap();
    assert!(reserved_ms > 0);
    assert!(ledger.active_time_exhausted());
    assert!(!ledger.spent_active_time_exhausted());
    edit(&ledger, |goal, _| goal.active_ms = 600_000);
    assert!(ledger.spent_active_time_exhausted());
}

#[test]
fn carried_acknowledgment_time_counts_toward_active_exhaustion() {
    let ledger = ledger(GoalBudget {
        max_active_ms: Some(100),
        ..GoalBudget::default()
    });
    edit(&ledger, |goal, in_flight| {
        goal.active_ms = 98;
        in_flight.origin = Some(Instant::now() - Duration::from_millis(10));
    });

    assert!(ledger.active_time_exhausted());
    assert!(
        ledger
            .run_active_ms()
            .is_ok_and(|active_ms| active_ms >= 10)
    );
}

#[test]
fn default_provider_action_retains_its_original_event_admission() {
    assert_eq!(
        Action::ProviderCall {
            previews: false,
            steering: false
        }
        .max_events(),
        6
    );
    assert_eq!(
        Action::Child {
            in_flight: 2,
            research: false
        }
        .max_events(),
        13
    );
    assert_eq!(
        Action::Child {
            in_flight: 2,
            research: true
        }
        .max_events(),
        18
    );
    assert_eq!(Action::Tool.max_events(), 8);
    assert_eq!(Action::Review.max_events(), 8);
    assert_eq!(Action::Compaction.max_events(), 6);
    assert_eq!(Action::BoundaryCompaction { goal: true }.max_events(), 2);
}

#[test]
fn action_bounds_reserve_only_the_selected_roles_switches() {
    let inherited = Providers::all(vec![()]);
    let configured = Providers {
        primary: vec![(); 3],
        child: vec![(); 2],
        review: vec![(); 3],
        compaction: vec![()],
    };
    for (action, extra) in [
        (
            Action::ProviderCall {
                previews: false,
                steering: false,
            },
            2,
        ),
        (
            Action::Child {
                in_flight: 2,
                research: false,
            },
            3,
        ),
        (
            Action::Child {
                in_flight: 2,
                research: true,
            },
            5,
        ),
        (Action::Tool, 2),
        (Action::Review, 2),
        (Action::Compaction, 0),
        (Action::BoundaryCompaction { goal: true }, 0),
        (
            Action::Delegation {
                children: 2,
                research: false,
            },
            4,
        ),
        (
            Action::Delegation {
                children: 2,
                research: true,
            },
            2 * (MAX_CHILD_TOOL_CALLS + 1) + 4 * MAX_CHILD_TOOL_CALLS + 2,
        ),
    ] {
        assert_eq!(action.max_events_for(&inherited), action.max_events());
        assert_eq!(
            action.max_events_for(&configured),
            action.max_events() + extra
        );
    }
}

#[test]
fn staged_bounds_never_exceed_the_first_admission() {
    let call = Action::ProviderCall {
        previews: true,
        steering: true,
    };
    assert_eq!(call.max_events(), 6 + 1 + MAX_DISPLAY_CHUNKS);
    let providers = Providers::all(vec![()]);
    assert_eq!(call.remaining(1, &providers), call.max_events() - 1);
    assert_eq!(Action::Tool.remaining(0, &providers), 8);
    assert_eq!(Action::Tool.remaining(1, &providers), 7);
    assert_eq!(Action::Tool.remaining(usize::MAX, &providers), 0);
    for children in 1..=8 {
        let delegation = Action::Delegation {
            children,
            research: false,
        };
        let last_call_prefix = children + (children - 1);
        let last_child = Action::Child {
            in_flight: u32::try_from(children - 1).unwrap(),
            research: false,
        };
        assert_eq!(
            delegation.remaining(last_call_prefix, &providers),
            last_child.max_events()
        );
    }
}

#[test]
fn tool_observation_slots_fit_within_each_action_bound() {
    for action in [
        Action::ProviderCall {
            previews: false,
            steering: false,
        },
        Action::Child {
            in_flight: 7,
            research: false,
        },
        Action::Child {
            in_flight: 7,
            research: true,
        },
        Action::Tool,
        Action::Review,
        Action::Compaction,
        Action::BoundaryCompaction { goal: true },
        Action::Delegation {
            children: 8,
            research: true,
        },
        Action::Delegation {
            children: 8,
            research: false,
        },
    ] {
        assert!(
            action.tool_observations() <= action.max_events(),
            "{action:?}"
        );
    }
    assert_eq!(
        Action::Delegation {
            children: 3,
            research: true
        }
        .tool_observations(),
        3 * MAX_CHILD_TOOL_CALLS
    );
    assert_eq!(Action::Tool.tool_observations(), 2);
    assert_eq!(
        Action::Child {
            in_flight: 2,
            research: false
        }
        .tool_observations(),
        0
    );
    assert_eq!(
        Action::Child {
            in_flight: 2,
            research: true
        }
        .tool_observations(),
        3
    );
    assert_eq!(Action::Compaction.tool_observations(), 0);
}
