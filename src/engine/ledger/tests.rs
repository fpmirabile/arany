use super::*;
use crate::session::{
    GoalPhase, GoalStatus, GoalSuccessCriterion, GoalSuccessState, RunId, SessionDefaults,
    SessionId,
};

fn goal_view(goal_id: GoalId, budget: GoalBudget) -> GoalView {
    GoalView {
        id: goal_id,
        objective: String::new(),
        workspace_identity: (1, 2),
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
        reserved_active_run: None,
        reserved_active_ms: None,
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
    }
}

fn ledger(budget: GoalBudget) -> GoalLedger {
    let goal_id = GoalId::new();
    let view = SessionView {
        id: SessionId::new(),
        title: String::new(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: Some((1, 2)),
        defaults: SessionDefaults::default(),
        created_sequence: 0,
        last_sequence: 0,
        lineage: None,
        runs: Vec::new(),
        compactions: Vec::new(),
        goals: vec![goal_view(goal_id, budget)],
    };
    GoalLedger::new(view, goal_id)
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
    assert!(!ledger.admits(Action::ProviderCall { previews: false }));
    assert_eq!(ledger.remaining_calls(), Some(0));
}

#[test]
fn active_plan_narrowing_uses_settled_action_usage() {
    let ledger = ledger(unlimited_time());
    edit(&ledger, |goal, _| goal.active_ms = 100);
    let narrowing = contract(GoalBudget {
        max_active_ms: Some(10_000),
        ..GoalBudget::default()
    });
    assert!(ledger.can_narrow(&narrowing));
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
        .active_reservation(1_845_000)
        .expect("maximum macOS Build action plus Store tail");
    assert_eq!(reserved_ms, 1_850_000);
}

#[test]
fn unlimited_goal_checkpoints_carried_time_before_a_maximum_build_action() {
    let ledger = ledger(unlimited_time());
    edit(&ledger, |_, in_flight| {
        in_flight.origin = Some(Instant::now() - Duration::from_secs(11));
    });
    assert!(ledger.needs_carried_time_checkpoint(1_845_000).unwrap());

    let (_, carried_ms) = ledger
        .pending_active_reservation()
        .unwrap()
        .expect("carried active time before the Build action");
    edit(&ledger, |goal, in_flight| {
        goal.active_ms += carried_ms;
        in_flight.origin = None;
    });

    assert!(!ledger.needs_carried_time_checkpoint(1_845_000).unwrap());
    let (_, build_reservation) = ledger
        .active_reservation(1_845_000)
        .expect("maximum Build interval after the carried checkpoint");
    assert_eq!(build_reservation, 1_850_000);
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
    assert!(!ledger.admits(Action::ProviderCall { previews: false }));
    assert!(matches!(
        ledger
            .reserve(Action::ProviderCall { previews: false })
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
    ledger.settle(Some(6), Some(4));
    assert!(ledger.tokens_within_limit());
    assert!(!ledger.can_start_provider_call());
    ledger.settle(Some(1), None);
    assert!(!ledger.tokens_within_limit(), "unknown usage blocks");
}

#[test]
fn acknowledged_active_time_overrun_is_recorded_within_the_action_bound() {
    let ledger = ledger(GoalBudget {
        max_active_ms: Some(100),
        ..GoalBudget::default()
    });
    edit(&ledger, |goal, _| goal.reserved_active_ms = Some(100));
    assert!(ledger.active_time_exhausted());
    edit(&ledger, |goal, in_flight| {
        goal.active_ms = 101;
        goal.reserved_active_ms = None;
        in_flight.origin = Some(Instant::now());
    });
    assert!(ledger.active_time_exhausted());
}

#[test]
fn carried_acknowledgment_time_is_checkpointed_before_active_exhaustion() {
    let ledger = ledger(GoalBudget {
        max_active_ms: Some(100),
        ..GoalBudget::default()
    });
    edit(&ledger, |goal, in_flight| {
        goal.active_ms = 98;
        in_flight.origin = Some(Instant::now() - Duration::from_millis(10));
    });

    assert!(ledger.active_time_exhausted());
    let (_, reserved_ms) = ledger
        .pending_active_reservation()
        .expect("bounded checkpoint")
        .expect("carried active segment");
    assert_eq!(reserved_ms, 2);
}

#[test]
fn max_events_covers_the_events_each_action_appends() {
    let tools = MAX_CHILD_TOOL_CALLS;
    let chunks = MAX_DISPLAY_CHUNKS;
    let pending = 3_u32;
    let children = 4;
    // Reserved and Settled active time, call reservation and record, steering note, display chunks.
    let primary_call = |previews| 6 + if previews { chunks } else { 0 };
    // Plan contract, ToolStarted, approval wait, queue wait, active segment, ToolFinished.
    let tool = 1 + 1 + 2 + 2 + 2 + 1;
    let table = [
        (
            Action::ProviderCall { previews: false },
            primary_call(false),
        ),
        (Action::ProviderCall { previews: true }, primary_call(true)),
        // Call reserved and recorded, Tool started and finished, AgentFinished, and the closing
        // pair of each other child in flight.
        (
            Action::Child { in_flight: pending },
            5 + 2 * pending as usize,
        ),
        (Action::Tool, tool),
        // Active time Reserved and Settled around a reserved and recorded call.
        (Action::Review, 4),
        (Action::Compaction, 4),
        (Action::BoundaryCompaction { goal: true }, 4),
        (Action::BoundaryCompaction { goal: false }, 1),
        (Action::Pause, 2 + 2 + 2 + 1 + 2),
        (Action::Stop, 4),
        // AgentSpawned, call pair and AgentFinished per child, a Tool pair per researcher step,
        // plus the delegation's own segment pair and the synthesis call pair.
        (
            Action::Delegation {
                children,
                research: false,
            },
            children * 4 + 2 + 2,
        ),
        (
            Action::Delegation {
                children,
                research: true,
            },
            children * (1 + 2 * (tools + 1) + 2 * tools + 1) + 2 + 2,
        ),
    ];
    for (action, observed) in table {
        assert!(
            action.max_events() >= observed,
            "{action:?} bounds {} Events but appends {observed}",
            action.max_events()
        );
        assert_eq!(
            action.checkpointed_events(),
            action.max_events() + CHECKPOINT_EVENTS
        );
    }
}
