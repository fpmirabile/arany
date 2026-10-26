use super::super::test_support::*;
use super::super::*;
use crate::provider::OutputTokenBound;
use crate::session::events::CollaborationPolicy;

#[test]
fn run_started_host_epochs_follow_live_runs_and_resume_boundaries() {
    let old = uuid::Uuid::now_v7();
    let newer = uuid::Uuid::now_v7();
    for (name, previous, resumed, next, accepted) in [
        ("consecutive", vec![old], false, old, true),
        ("resumed", vec![old], true, old, false),
        ("older", vec![old, newer], false, old, false),
        ("new", vec![old], false, newer, true),
        ("new-after-resume", vec![old], true, newer, true),
    ] {
        let (session, goal) = (SessionId::new(), GoalId::new());
        let mut events = vec![goal_started(
            goal,
            GoalBudget {
                max_runs: 8,
                ..Default::default()
            },
        )];
        for epoch in previous {
            let (run, agent) = (RunId::new(), AgentRunId::new());
            let mut config = config(CollaborationPolicy::Single, true);
            config.tool_policy.as_mut().unwrap().host_epoch = Some(epoch);
            events.extend(start_run(goal, run, agent, config));
            events.extend(cancel_run(run, agent));
        }
        if resumed {
            events.push(Event::GoalResumed { goal_id: goal });
        }
        let mut config = config(CollaborationPolicy::Single, true);
        config.tool_policy.as_mut().unwrap().host_epoch = Some(next);
        let run = RunId::new();
        let mut started = start_run(goal, run, AgentRunId::new(), config);
        started.truncate(3);
        let event = started.pop().unwrap();
        assert!(matches!(event, Event::RunStarted { .. }));
        events.extend(started);
        let envelopes = session_with(session, events);
        let mut view = SessionView::replay_open(session, &envelopes)
            .unwrap()
            .unwrap();
        assert!(view.runs.last().unwrap().config.is_none());
        let boundary = envelopes
            .iter()
            .find(|envelope| {
                if resumed {
                    matches!(envelope.event, Event::GoalResumed { .. })
                } else {
                    matches!(envelope.event, Event::SessionStarted { .. })
                }
            })
            .unwrap();
        assert_eq!(view.host_epoch_floor, boundary.sequence, "{name}");
        assert_eq!(
            view.runs[0].accepted_sequence > view.host_epoch_floor,
            !resumed,
            "{name}"
        );
        let sequence = view.last_sequence + 1;
        let (run_id, agent_run_id) = event.scope();
        let result = view.apply_committed(&EventEnvelope {
            session_id: session,
            sequence,
            run_id,
            agent_run_id,
            event,
            created_at_ms: 0,
        });
        assert_eq!(result.is_ok(), accepted, "{name}: {result:?}");
    }
}

#[test]
fn run_failure_is_present_exactly_on_failed_terminal_events() {
    use crate::session::RunFailure;
    let session = SessionId::new();
    let goal = GoalId::new();
    let run = RunId::new();
    let agent = AgentRunId::new();
    for (disposition, failure, accepted) in [
        (
            RunDisposition::Failed,
            Some(RunFailure::ProposalRefused),
            true,
        ),
        (
            RunDisposition::Failed,
            Some(RunFailure::RefusalLimit),
            false,
        ),
        (RunDisposition::Failed, None, false),
        (RunDisposition::Cancelled, None, true),
        (
            RunDisposition::Cancelled,
            Some(RunFailure::ProviderCall),
            false,
        ),
        (
            RunDisposition::Finished,
            Some(RunFailure::ProviderCall),
            false,
        ),
        (
            RunDisposition::StoppedByHook,
            Some(RunFailure::ProviderCall),
            false,
        ),
    ] {
        let mut events = vec![goal_started(goal, GoalBudget::default())];
        events.extend(start_run(
            goal,
            run,
            agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.push(Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: agent,
            active_ceiling_ms: 0,
        });
        events.push(Event::AgentFinished {
            worker: None,
            run_id: run,
            agent_run_id: agent,
            disposition: if disposition == RunDisposition::Failed {
                AgentDisposition::Failed
            } else {
                AgentDisposition::Cancelled
            },
            summary: None,
            result: None,
        });
        events.push(Event::RunFinished {
            run_id: run,
            disposition,
            failure,
            active_ms: 0,
        });
        let result = SessionView::replay(session, &session_with(session, events));
        assert_eq!(result.is_ok(), accepted, "{disposition:?} {failure:?}");
        if accepted {
            let view = result.unwrap().unwrap();
            assert_eq!(view.runs[0].failure, failure);
            assert_eq!(view.usage.pending, [(session, run, Some(agent))]);
            assert!(view.usage.groups.is_empty());
        }
    }
}

#[test]
fn run_output_caps_replay_inside_the_declared_range_only() {
    let session = SessionId::new();
    let (goal, run, agent) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let ceiling = crate::provider::MAX_OUTPUT_TOKEN_CAP;
    for (cap, accepted) in [
        (4096, true),
        (16_384, true),
        (ceiling, true),
        (ceiling + 1, false),
        (0, false),
    ] {
        let mut config = config(CollaborationPolicy::Single, false);
        config.output_token_cap = cap;
        let mut events = vec![goal_started(goal, GoalBudget::default())];
        events.extend(start_run(goal, run, agent, config));
        let result = SessionView::replay_open(session, &session_with(session, events));
        assert_eq!(result.is_ok(), accepted, "cap {cap}");
    }
}

#[test]
fn role_chains_are_canonical_and_tool_history_stays_in_one_adapter_and_account() {
    use crate::provider::{Provenance, Providers};
    let mut config = config(CollaborationPolicy::Single, true);
    let primary = config.identity().unwrap();
    config.providers = Some(Providers::all(vec![primary.clone()]));
    assert!(config.validate().is_err());
    for child in [false, true] {
        for account_kind in 0..3 {
            let mut alternate = primary.clone();
            alternate.model = "alternate".into();
            if account_kind == 0 {
                alternate.profile = "anthropic".into();
            } else if account_kind == 1 {
                alternate.provenance = Provenance::Native {
                    saved_api_account_id: Some(uuid::Uuid::now_v7()),
                };
            } else {
                alternate.profile = "chatgpt".into();
                alternate.effort = Some(crate::provider::Effort::High);
                alternate.limits.output_token_bound = OutputTokenBound::LocalAcceptanceOnly;
                alternate.provenance = Provenance::ChatGpt(crate::provider::ChatGptProvenance {
                    account_id: uuid::Uuid::now_v7(),
                    evidence_fingerprint: [1; 32],
                    admission: crate::provider::ChatGptAdmission::AccountConsent,
                });
            }
            let mut routes = Providers::all(vec![primary.clone()]);
            if child {
                routes.child.push(alternate);
            } else {
                routes.primary.push(alternate);
            }
            config.providers = Some(routes);
            assert!(config.validate().is_err());
            let mut no_tools = config.clone();
            no_tools.tool_policy = None;
            no_tools.supervisor = None;
            assert!(no_tools.validate().is_ok());
        }
    }
}
