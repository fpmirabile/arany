use super::*;
use crate::session::{
    GoalPhase, GoalStatus, GoalSuccessCriterion, GoalSuccessState, RunId, SessionDefaults,
    SessionId, WorkspaceIdentity,
};

#[tokio::test(start_paused = true)]
async fn active_deadline_pauses_idle_time_and_counts_overlapping_work_once() {
    let clock = Arc::new(ActivityClock::new());
    let deadline = ActiveDeadline {
        started_at: clock.elapsed(),
        clock: clock.clone(),
        allowance_ms: 1_000,
        cause: ActiveDeadlineCause::Delegation,
    };

    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(deadline.remaining_ms(), 1_000);

    let provider = clock.provider().start();
    tokio::time::advance(Duration::from_millis(400)).await;
    let effect = clock.effect();
    tokio::time::advance(Duration::from_millis(300)).await;
    drop(provider);
    tokio::time::advance(Duration::from_millis(100)).await;
    assert_eq!(deadline.remaining_ms(), 200);

    drop(effect);
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(deadline.remaining_ms(), 200);
    let provider = clock.provider().start();
    tokio::time::advance(Duration::from_millis(100)).await;
    drop(provider);
    tokio::time::advance(Duration::from_millis(60)).await;
    let next_effect = clock.effect();
    tokio::time::advance(Duration::from_millis(100)).await;
    assert!(deadline.reached());
    drop(next_effect);
}

#[tokio::test(start_paused = true)]
async fn active_deadline_wakes_on_work_and_expires_after_its_allowance() {
    let clock = Arc::new(ActivityClock::new());
    let deadline = ActiveDeadline {
        started_at: clock.elapsed(),
        clock: clock.clone(),
        allowance_ms: 250,
        cause: ActiveDeadlineCause::Goal,
    };
    let waiter = tokio::spawn({
        let deadline = deadline.clone();
        async move { deadline.elapsed().await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(!waiter.is_finished());

    let _work = clock.effect();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(250)).await;
    waiter.await.unwrap();
    assert!(deadline.reached());
}

fn goal_view(goal_id: GoalId, budget: GoalBudget) -> GoalView {
    GoalView {
        include_paths: Some(Vec::new()),
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
        continues: None,
        budget_closed: false,
        budget_refusal: None,
        run_start_usage: Default::default(),
    }
}

fn ledger(budget: GoalBudget) -> GoalLedger {
    let goal_id = GoalId::new();
    let view = SessionView {
        usage: Default::default(),
        rewinds: Default::default(),
        id: SessionId::new(),
        title: String::new(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: Some(WorkspaceIdentity {
            device: 1,
            inode: 2,
        }),
        defaults: SessionDefaults::default(),
        host_epoch_floor: 0,
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
    let State {
        view, in_flight, ..
    } = &mut *state;
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
        offset: None,
        limit: Some(32),
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
    let Admission::Reserved((_, reserved_ms, _)) = ledger
        .reserve_active(1_845_000, BudgetAction::Provider, None)
        .expect("maximum macOS Build action plus Store tail")
    else {
        panic!("expected active reservation")
    };
    assert_eq!(reserved_ms, 1_850_000);
}

#[test]
fn active_reservation_records_which_budget_bound_it() {
    let unlimited = ledger(unlimited_time());
    let Admission::Reserved((_, _, limit)) = unlimited
        .reserve_active(20_000, BudgetAction::Delegation, None)
        .expect("unlimited Goal reservation")
    else {
        panic!("expected active reservation")
    };
    assert_eq!(limit, ActiveReservationLimit::ActionBound);

    let constrained = ledger(GoalBudget {
        max_active_ms: Some(20_000),
        ..Default::default()
    });
    let Admission::Reserved((_, _, limit)) = constrained
        .reserve_active(20_000, BudgetAction::Delegation, None)
        .expect("Goal-limited reservation")
    else {
        panic!("expected active reservation")
    };
    assert_eq!(limit, ActiveReservationLimit::Goal);
}

#[tokio::test(start_paused = true)]
async fn active_admission_distinguishes_store_headroom_from_elapsed_exhaustion() {
    for (used, carried, action_ms, action, admitted, elapsed) in [
        (4_999, 0, 600_000, BudgetAction::Provider, true, false),
        (5_000, 0, 600_000, BudgetAction::Provider, false, false),
        (5_000, 10, 600_000, BudgetAction::Provider, false, false),
        (10_000, 0, 600_000, BudgetAction::Provider, false, true),
        (10_000, 0, 0, BudgetAction::Delegation, false, true),
        (9_990, 10, 0, BudgetAction::Delegation, false, true),
        (10_000, 0, 0, BudgetAction::Tool, false, true),
        (9_990, 0, 0, BudgetAction::Delegation, true, false),
    ] {
        let ledger = ledger(GoalBudget {
            max_active_ms: Some(10_000),
            ..Default::default()
        });
        edit(&ledger, |goal, in_flight| {
            goal.active_ms = used;
            in_flight.origin = Some(Instant::now());
        });
        tokio::time::advance(Duration::from_millis(carried)).await;
        assert_eq!(ledger.spent_active_time_exhausted(), elapsed);
        assert_eq!(
            matches!(
                ledger.reserve_active(action_ms, action, None),
                Ok(Admission::Reserved(_))
            ),
            admitted
        );
        let refusal = ledger.budget_refusal().unwrap();
        assert_eq!(refusal.is_none(), admitted);
        if let Some(refusal) = refusal {
            assert_eq!(refusal.used, used + carried);
            assert_eq!(refusal.limit, 10_000);
            assert_eq!(refusal.requested, Some(action_ms + 5_000 + carried));
            assert_eq!(
                refusal.required_minimum,
                Some(carried + if action_ms == 0 { 1 } else { 5_001 })
            );
            refusal.validate().expect("valid captured reservation");
            if action_ms == 0 {
                let mut forged = refusal;
                forged.action = BudgetAction::Provider;
                assert!(forged.validate().is_err());
            }
            assert_eq!(
                refusal.cause,
                if elapsed {
                    crate::BudgetRefusalCause::Exhausted
                } else {
                    crate::BudgetRefusalCause::InsufficientHeadroom
                }
            );
        }
    }
}

#[test]
fn refusal_snapshot_uses_pinned_dimensions_and_canonical_simultaneous_order() {
    for (calls, tokens, unknown, active, runs, expected) in [
        (0, 0, false, 0, 1, None),
        (4, 0, false, 0, 1, Some(GoalLimit::Calls)),
        (4, 100, false, 60_000, 2, Some(GoalLimit::Tokens)),
        (4, 0, false, 60_000, 2, Some(GoalLimit::Calls)),
        (0, 0, false, 55_000, 2, Some(GoalLimit::ActiveTime)),
        (0, 0, false, 0, 2, Some(GoalLimit::Runs)),
        (0, 0, true, 0, 1, Some(GoalLimit::Tokens)),
    ] {
        let ledger = ledger(GoalBudget {
            max_provider_calls: Some(4),
            max_tokens: Some(100),
            max_active_ms: Some(60_000),
            max_runs: 2,
        });
        edit(&ledger, |goal, _| {
            goal.provider_calls = calls;
            goal.known_tokens = tokens;
            goal.token_usage_unknown = unknown;
            goal.active_ms = active;
            goal.run_ids.resize_with(runs, RunId::new);
        });
        ledger
            .capture_budget_refusal(BudgetAction::Run, None)
            .unwrap();
        let refusal = ledger.budget_refusal().unwrap();
        assert_eq!(refusal.map(|r| r.dimension), expected);
        if let Some(refusal) = refusal {
            refusal.validate().unwrap();
            let error = EngineError::GoalBudgetUncommitted {
                refusal: Box::new(refusal),
                source: Box::new(EngineError::GoalBudgetExhausted),
            }
            .to_string();
            assert!(error.starts_with("Goal budget refusal is uncommitted:"));
            assert!(
                !error.contains("Some(") && !error.contains("None") && !error.contains("Observed")
            );
            assert!(error.contains(&format!(
                "used at refusal {}, pinned limit {}",
                refusal.used, refusal.limit
            )));
            assert_eq!(
                error.contains("token usage unknown"),
                refusal.token_usage_unknown
            );
        }
    }
}

#[test]
fn carried_time_joins_the_next_reservation_and_the_runs_active_time() {
    let ledger = ledger(unlimited_time());
    edit(&ledger, |_, in_flight| {
        in_flight.origin = Some(Instant::now() - Duration::from_secs(11));
    });
    let Admission::Reserved((_, reserved_ms, _)) = ledger
        .reserve_active(1_845_000, BudgetAction::Provider, None)
        .expect("Build action after carried time")
    else {
        panic!("expected active reservation")
    };
    assert!(reserved_ms >= 1_861_000, "{reserved_ms}");
    assert!(
        matches!(
            ledger.reserve_active(1, BudgetAction::Provider, None),
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
        "an exact-cap Answer is still valid"
    );
    assert!(!ledger.can_start_provider_call());
    assert!(!ledger.admits(Action::ProviderCall {
        previews: false,
        steering: false,
    }));
    assert!(matches!(
        ledger
            .reserve(
                Action::ProviderCall {
                    previews: false,
                    steering: false,
                },
                AgentRunId::new()
            )
            .await,
        Ok(Admission::Exhausted)
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
    let Admission::Reserved((started, _, _)) = ledger
        .reserve_active(10_000, BudgetAction::Provider, None)
        .unwrap()
    else {
        panic!("expected active reservation")
    };
    tokio::time::advance(Duration::from_millis(100)).await;
    ledger
        .exclude_idle_approval(Duration::from_millis(80))
        .unwrap();
    ledger
        .settle_active(duration_ms(started.elapsed()), None)
        .unwrap();
    assert_eq!(ledger.run_active_ms().unwrap(), 20);
    let Admission::Reserved((started, _, _)) = ledger
        .reserve_active(10_000, BudgetAction::Provider, None)
        .unwrap()
    else {
        panic!("expected active reservation")
    };
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
    let Admission::Reserved((_started, reserved_ms, _)) = ledger
        .reserve_active(600_000, BudgetAction::Provider, None)
        .unwrap()
    else {
        panic!("expected active reservation")
    };
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
fn provider_action_reserves_plan_recording_and_terminal_events() {
    assert_eq!(
        Action::ProviderCall {
            previews: false,
            steering: false
        }
        .max_events(),
        7
    );
    assert_eq!(
        Action::Child {
            in_flight: 2,
            research: false
        }
        .max_events(),
        14
    );
    assert_eq!(
        Action::Child {
            in_flight: 2,
            research: true
        }
        .max_events(),
        19
    );
    assert_eq!(Action::Tool.max_events(), 8);
    assert_eq!(Action::Review.max_events(), 8);
    assert_eq!(Action::Tool.verdict_slots(), 2);
    assert_eq!(Action::Compaction.verdict_slots(), 0);
    assert_eq!(Action::Compaction.max_events(), 6);
    assert_eq!(Action::BoundaryCompaction { goal: true }.max_events(), 2);
}

#[test]
fn action_bounds_reserve_each_calls_switches_and_three_retry_facts() {
    let r = usize::from(crate::provider::MAX_PROVIDER_RETRIES);
    let inherited = Providers::all(vec![()]);
    let configured = Providers {
        primary: vec![(); 3],
        child: vec![(); 2],
        review: vec![(); 3],
        compaction: vec![()],
    };
    // Per-call attempt facts: switches of the role's chain plus the three retries.
    let (primary, child, review, compaction) = (2 + r, 1 + r, 2 + r, r);
    let calls = MAX_CHILD_TOOL_CALLS + 1;
    // (action, configured extra, inherited extra)
    for (action, extra, single) in [
        (
            Action::ProviderCall {
                previews: false,
                steering: false,
            },
            primary,
            r,
        ),
        (
            Action::Child {
                in_flight: 2,
                research: false,
            },
            3 * child,
            3 * r,
        ),
        (
            Action::Child {
                in_flight: 2,
                research: true,
            },
            3 * child + review,
            4 * r,
        ),
        (
            Action::Worker { chunks: 1093 },
            review + 8 * child + primary,
            10 * r,
        ),
        (Action::Tool, review, r),
        (Action::Review, review, r),
        (Action::Compaction, compaction, r),
        (Action::BoundaryCompaction { goal: true }, compaction, r),
        (
            Action::Delegation {
                children: 2,
                research: false,
            },
            2 * child + primary,
            3 * r,
        ),
        (
            Action::Delegation {
                children: 2,
                research: true,
            },
            2 * calls * child + 2 * MAX_CHILD_TOOL_CALLS * review + primary,
            (2 * calls + 2 * MAX_CHILD_TOOL_CALLS + 1) * r,
        ),
    ] {
        assert_eq!(
            action.max_events_for(&inherited),
            action.max_events() + single,
            "{action:?}"
        );
        assert_eq!(
            action.max_events_for(&configured),
            action.max_events() + extra,
            "{action:?}"
        );
    }
}

#[test]
fn action_headroom_covers_eligible_chains_longer_than_the_child_default() {
    use crate::test_support::scenario::ScriptedProvider;
    use crate::{AdmittedProviders, AdmittedRoster, NamedProviderRoutes, Provider, RosterSnapshot};
    use std::sync::Arc;
    let primary: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(vec![]));
    let mut providers = AdmittedProviders::single(primary.clone());
    let chain: Vec<Arc<dyn Provider>> = vec![
        primary.clone(),
        Arc::new(ScriptedProvider::new(vec![]).with_profile("scripted", "fallback-a")),
        Arc::new(ScriptedProvider::new(vec![]).with_profile("scripted", "fallback-b")),
    ];
    let mut snapshot = RosterSnapshot {
        preset: "coding".into(),
        selection_digest: [0; 32],
        profiles: vec![
            NamedProviderRoutes {
                name: "main".into(),
                routes: vec![primary.identity().clone()],
            },
            NamedProviderRoutes {
                name: "spare".into(),
                routes: chain
                    .iter()
                    .map(|provider| provider.identity().clone())
                    .collect(),
            },
        ],
        providers: Providers::all("main".into()),
        child_profiles: vec!["main".into(), "spare".into()],
    };
    snapshot.selection_digest = snapshot.digest().unwrap();
    assert!(snapshot.valid());
    providers.roster = Some(AdmittedRoster {
        snapshot,
        profiles: vec![("main".into(), vec![primary]), ("spare".into(), chain)],
    });
    let r = usize::from(crate::provider::MAX_PROVIDER_RETRIES);
    // The longest eligible child chain has two switches; every other role has a single route.
    for (action, extra) in [
        (
            Action::Delegation {
                children: 8,
                research: true,
            },
            8 * (MAX_CHILD_TOOL_CALLS + 1) * (2 + r) + 8 * MAX_CHILD_TOOL_CALLS * r + r,
        ),
        (
            Action::Child {
                in_flight: 2,
                research: true,
            },
            3 * (2 + r) + r,
        ),
        (Action::Review, r),
    ] {
        assert_eq!(
            action.max_events_for_admitted(&providers),
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
    assert_eq!(call.max_events(), 7 + 1 + MAX_DISPLAY_CHUNKS);
    let providers = Providers::all(vec![()]);
    let r = usize::from(crate::provider::MAX_PROVIDER_RETRIES);
    assert_eq!(
        call.remaining(1, &providers),
        call.max_events_for(&providers) - 1
    );
    assert_eq!(Action::Tool.remaining(0, &providers), 8 + r);
    assert_eq!(Action::Tool.remaining(1, &providers), 7 + r);
    assert_eq!(Action::Tool.remaining(usize::MAX, &providers), 0);
    for children in 1..=8 {
        let delegation = Action::Delegation {
            children,
            research: false,
        };
        // Each earlier child appends its call, its result and at most one steering note.
        let last_call_prefix = children + 2 * (children - 1);
        let last_child = Action::Child {
            in_flight: u32::try_from(children - 1).unwrap(),
            research: false,
        };
        // The delegation also covers the synthesis call's retry facts.
        assert_eq!(
            delegation.remaining(last_call_prefix, &providers),
            last_child.max_events_for(&providers) + r
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

#[test]
fn drain_headroom_reserves_four_events_per_lease_holding_editor_and_a_verdict_slot_when_supervised()
{
    let fixture = crate::session::worker_protocol_fixture();
    let events = fixture.envelopes();
    let base = (1..=events.len())
        .filter_map(|end| {
            crate::session::SessionView::replay(fixture.session, &events[..end])
                .ok()
                .flatten()
        })
        .find(|view| {
            view.runs.last().is_some_and(|run| {
                run.agents.iter().any(|agent| {
                    agent
                        .assignment
                        .as_ref()
                        .is_some_and(|worker| worker.released.is_none())
                })
            })
        })
        .expect("a prefix with a lease-holding editor");
    let editor = base
        .runs
        .last()
        .and_then(|run| run.agents.iter().find(|agent| agent.assignment.is_some()))
        .expect("editor")
        .clone();
    let researcher = {
        let mut agent = editor.clone();
        agent.assignment = None;
        agent
    };
    for (name, run, held, released, researchers, supervised, expected) in [
        ("no Run", false, 0, 0, 0, false, 0),
        ("researchers only", true, 0, 0, 2, false, 0),
        ("one held editor", true, 1, 0, 0, false, 4),
        ("released editor", true, 0, 1, 0, false, 0),
        ("three held, one released", true, 3, 1, 0, false, 12),
        ("three held under a Supervisor", true, 3, 0, 0, true, 15),
    ] {
        let mut view = base.clone();
        if run {
            let last = view.runs.last_mut().expect("Run");
            last.agents.retain(|agent| agent.assignment.is_none());
            for _ in 0..held {
                last.agents.push(editor.clone());
            }
            for _ in 0..released {
                let mut agent = editor.clone();
                agent.assignment.as_mut().expect("assignment").released = Some(1);
                last.agents.push(agent);
            }
            for _ in 0..researchers {
                last.agents.push(researcher.clone());
            }
            last.config.as_mut().expect("config").supervisor =
                supervised.then(crate::supervisor::SupervisorPolicy::default);
        } else {
            view.runs.clear();
        }
        let ledger = GoalLedger::new(view, Some(fixture.goal));
        assert_eq!(ledger.drain_headroom(), expected, "{name}");
    }
}
