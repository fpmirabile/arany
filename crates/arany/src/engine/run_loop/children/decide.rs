use super::*;

pub(super) fn decide_child_call(
    result: CallResult,
    valid: bool,
    gate: Option<GateFacts>,
    abort: Option<WaveStop>,
    facts: &ChildFacts,
) -> ChildDecision {
    let Some(gate) = gate else {
        return decide_researcher_call(result, valid, abort, facts);
    };
    let open = abort.is_none();
    if gate.unmet {
        return match result {
            CallResult::Response(ProviderResponse { turn, .. })
                if editor_may_continue_after_gate(gate, abort, facts) =>
            {
                ChildDecision::Reprompt(RepromptReason::CriterionNotMetInView, turn)
            }
            CallResult::Response(_) | CallResult::Failed(_) | CallResult::Cancelled => {
                ChildDecision::End(ChildEnd::Editor(WorkerCause::CriterionUnverified))
            }
        };
    }
    match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Answer(answer),
            turn,
            ..
        }) if valid && open => ChildDecision::Finish {
            answer,
            turn,
            stop_hook: true,
        },
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tools(requests),
            turn,
            ..
        }) if valid && open && !gate.worker_stopped => ChildDecision::ToolStep(requests, turn),
        other => ChildDecision::End(ChildEnd::Editor(editor_failure(
            &other,
            valid,
            facts.turns_left,
        ))),
    }
}

fn editor_may_continue_after_gate(
    gate: GateFacts,
    abort: Option<WaveStop>,
    facts: &ChildFacts,
) -> bool {
    abort.is_none()
        && !gate.worker_stopped
        && facts.turns_left
        && facts.tool_calls_left
        && gate.stop_room
        && facts.before_deadline
        && facts.tokens_open
        && facts.calls_left
        && gate.continuations_left
}

fn editor_failure(result: &CallResult, valid: bool, turns_left: bool) -> WorkerCause {
    match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tools(_),
            ..
        }) if !turns_left => WorkerCause::TurnLimit,
        CallResult::Response(_) if !valid => WorkerCause::InvalidOutcome,
        CallResult::Failed(CallFailure::InvalidResponse(_) | CallFailure::OutputLimit) => {
            WorkerCause::InvalidOutcome
        }
        CallResult::Response(_) | CallResult::Failed(_) | CallResult::Cancelled => {
            WorkerCause::ToolFailed
        }
    }
}

fn decide_researcher_call(
    result: CallResult,
    valid: bool,
    abort: Option<WaveStop>,
    facts: &ChildFacts,
) -> ChildDecision {
    if abort == Some(WaveStop::Boundary(StopCause::HookStop)) || facts.hook.stop().is_some() {
        return ChildDecision::End(ChildEnd::Researcher(AgentDisposition::Cancelled));
    }
    match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Answer(answer),
            turn,
            ..
        }) if valid => ChildDecision::Finish {
            answer,
            turn,
            stop_hook: facts.has_tools && !facts.cancelled && abort.is_none(),
        },
        // An exhausted Goal budget stops the child before its Tool starts, like a pause.
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tools(requests),
            turn,
            ..
        }) if valid || requests.len() > usize::from(crate::session::MAX_STEP_TOOL_CALLS) => {
            let budget = !facts.tokens_open || !facts.calls_left;
            if facts.cancelled || facts.paused || abort.is_some() || budget {
                ChildDecision::End(ChildEnd::Skipped {
                    disposition: researcher_end(facts.actor_hook_stop, true),
                    budget,
                })
            } else if valid {
                ChildDecision::ToolStep(requests, turn)
            } else if facts.has_tools
                && facts.turns_left
                && facts.tool_calls_left
                && facts.before_deadline
                && !facts.refusal_limit_reached
            {
                ChildDecision::RefusedTools(requests, turn)
            } else {
                ChildDecision::End(ChildEnd::Researcher(AgentDisposition::Failed))
            }
        }
        CallResult::Cancelled => {
            ChildDecision::End(ChildEnd::Researcher(AgentDisposition::Cancelled))
        }
        CallResult::Response(_) | CallResult::Failed(_) => {
            ChildDecision::End(ChildEnd::Researcher(AgentDisposition::Failed))
        }
    }
}

// A researcher that ends without an answer.
pub(super) fn researcher_end(actor_hook_stop: bool, interrupted: bool) -> AgentDisposition {
    if actor_hook_stop {
        AgentDisposition::StoppedByHook
    } else if interrupted {
        AgentDisposition::Cancelled
    } else {
        AgentDisposition::Failed
    }
}

pub(super) fn decide_child_stop(
    run: StopHookRun,
    abort: Option<WaveStop>,
    facts: &ChildFacts,
) -> ChildStop {
    match run {
        StopHookRun::Cancelled => ChildStop::End {
            disposition: AgentDisposition::Cancelled,
            cause: WorkerCause::Cancelled,
            stops_wave: true,
        },
        StopHookRun::Passed if facts.cancelled => ChildStop::End {
            disposition: AgentDisposition::Cancelled,
            cause: WorkerCause::Cancelled,
            stops_wave: true,
        },
        StopHookRun::Passed if facts.actor_hook_stop => ChildStop::End {
            disposition: AgentDisposition::StoppedByHook,
            cause: WorkerCause::StoppedByHook,
            stops_wave: false,
        },
        StopHookRun::Passed => ChildStop::Proceed,
        // A cut-off or cancelled hook ends this child only, unless its outcome stops the Run.
        StopHookRun::CutOffByActiveTime => ChildStop::End {
            disposition: if facts.cancelled {
                AgentDisposition::Cancelled
            } else {
                AgentDisposition::Failed
            },
            cause: WorkerCause::Budget,
            stops_wave: !facts.hook.settled && abort.is_none(),
        },
        StopHookRun::CutOffByContext => ChildStop::End {
            disposition: if facts.cancelled {
                AgentDisposition::Cancelled
            } else {
                AgentDisposition::Failed
            },
            cause: WorkerCause::ToolFailed,
            stops_wave: !facts.hook.settled && abort.is_none(),
        },
        StopHookRun::Uncertain => ChildStop::End {
            disposition: AgentDisposition::Failed,
            cause: WorkerCause::ToolFailed,
            stops_wave: false,
        },
        StopHookRun::Blocked => {
            let room = facts.before_deadline && facts.tokens_open && facts.calls_left;
            if facts.turns_left && room {
                ChildStop::Reprompt
            } else {
                ChildStop::End {
                    disposition: AgentDisposition::Failed,
                    cause: if room {
                        WorkerCause::TurnLimit
                    } else {
                        WorkerCause::Budget
                    },
                    stops_wave: false,
                }
            }
        }
    }
}

// What stops the wave after a child's call settles, when nothing stopped it yet.
pub(super) fn wave_stop_after(
    queue_stop: Option<StopCause>,
    cancelled: bool,
    paused: bool,
    end: Option<AgentDisposition>,
    hook: HookFacts,
) -> Option<WaveStop> {
    if let Some(cause) = queue_stop {
        Some(WaveStop::from_queue(cause))
    } else if cancelled {
        Some(WaveStop::Abort(StopCause::Cancelled))
    } else if paused {
        Some(WaveStop::Boundary(StopCause::Paused))
    } else if end == Some(AgentDisposition::StoppedByHook) {
        Some(WaveStop::Boundary(StopCause::HookStop))
    } else if let Some(stop) = hook.stop() {
        Some(stop)
    } else if end == Some(AgentDisposition::Failed) {
        Some(WaveStop::Abort(StopCause::ToolStopped))
    } else if end == Some(AgentDisposition::Cancelled) {
        Some(WaveStop::Abort(StopCause::Cancelled))
    } else {
        None
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;

    #[test]
    fn delayed_pre_wave_time_reduces_the_active_deadline_allowance() {
        let started = Instant::now();
        let carried_ms = duration_ms(Duration::from_secs(20));
        let delayed_ms = duration_ms(Duration::from_secs(21));
        let bound = Duration::from_secs(120);
        let requested_ms = duration_ms(bound)
            .saturating_add(duration_ms(crate::engine::STORE_ACTION_OVERHEAD))
            .saturating_add(carried_ms);
        let sampled_at = started + Duration::from_millis(delayed_ms);
        for (limit, reserved_ms, expected_cause) in [
            (
                ActiveReservationLimit::ActionBound,
                requested_ms,
                ActiveDeadlineCause::Delegation,
            ),
            (
                ActiveReservationLimit::Goal,
                requested_ms.saturating_sub(15_000),
                ActiveDeadlineCause::Goal,
            ),
        ] {
            let segment = ActiveSegment {
                started,
                reserved_ms,
                limit,
            };
            let (allowance_ms, cause) = active_deadline_allowance(&segment, bound, sampled_at);

            assert_eq!(
                allowance_ms,
                reserved_ms
                    .saturating_sub(delayed_ms)
                    .saturating_sub(duration_ms(crate::engine::STORE_ACTION_OVERHEAD))
            );
            assert_eq!(cause, expected_cause);
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Kind {
        ToolStep,
        RefusedTools,
        Finish { stop_hook: bool },
        Reprompt(RepromptReason),
        End(ChildEnd),
    }

    // (call, valid, gate (editors only), wave abort, facts, decision)
    type Row = (
        CallResult,
        bool,
        Option<GateFacts>,
        Option<WaveStop>,
        ChildFacts,
        Kind,
    );

    fn kind(decision: &ChildDecision) -> Kind {
        match decision {
            ChildDecision::ToolStep(..) => Kind::ToolStep,
            ChildDecision::RefusedTools(..) => Kind::RefusedTools,
            ChildDecision::Finish { stop_hook, .. } => Kind::Finish {
                stop_hook: *stop_hook,
            },
            ChildDecision::Reprompt(reason, _) => Kind::Reprompt(*reason),
            ChildDecision::End(end) => Kind::End(*end),
        }
    }

    fn response(outcome: ProviderOutcome) -> CallResult {
        CallResult::Response(ProviderResponse {
            outcome,
            turn: ProviderTurn::new("test", "[{}]", "absent-secret", 4096).expect("minimal turn"),
            response_id: None,
            usage: crate::provider::Usage::default(),
            wire_provenance: None,
        })
    }

    fn answer() -> CallResult {
        response(ProviderOutcome::Answer(Answer {
            text: "done".into(),
        }))
    }

    fn tools() -> CallResult {
        response(ProviderOutcome::Tools(vec![ToolRequest {
            id: crate::provider::ToolUseId::new("call").expect("id"),
            call: ToolCall::List { path: ".".into() },
        }]))
    }

    fn timeout() -> CallResult {
        CallResult::Failed(CallFailure::Timeout)
    }

    const OPEN: ChildFacts = ChildFacts {
        has_tools: true,
        refusal_limit_reached: false,
        turns_left: true,
        tool_calls_left: true,
        before_deadline: true,
        cancelled: false,
        paused: false,
        tokens_open: true,
        calls_left: true,
        hook: HookFacts {
            uncertain: false,
            stop_requested: false,
            settled: true,
        },
        actor_hook_stop: false,
    };

    const GATE: GateFacts = GateFacts {
        unmet: false,
        worker_stopped: false,
        stop_room: true,
        continuations_left: true,
    };

    fn with(change: fn(&mut ChildFacts)) -> ChildFacts {
        let mut facts = OPEN;
        change(&mut facts);
        facts
    }

    fn gate(change: fn(&mut GateFacts)) -> GateFacts {
        let mut gate = GATE;
        gate.unmet = true;
        change(&mut gate);
        gate
    }

    #[test]
    fn researcher_over_cap_proposals_continue_only_with_refusal_and_budget_room() {
        for (facts, abort, expected) in [
            (OPEN, None, Kind::RefusedTools),
            (
                with(|facts| facts.refusal_limit_reached = true),
                None,
                Kind::End(ChildEnd::Researcher(AgentDisposition::Failed)),
            ),
            (
                with(|facts| facts.turns_left = false),
                None,
                Kind::End(ChildEnd::Researcher(AgentDisposition::Failed)),
            ),
            (
                with(|facts| facts.tool_calls_left = false),
                None,
                Kind::End(ChildEnd::Researcher(AgentDisposition::Failed)),
            ),
            (
                with(|facts| facts.before_deadline = false),
                None,
                Kind::End(ChildEnd::Researcher(AgentDisposition::Failed)),
            ),
            (
                with(|facts| facts.cancelled = true),
                None,
                Kind::End(ChildEnd::Skipped {
                    disposition: AgentDisposition::Cancelled,
                    budget: false,
                }),
            ),
            (
                with(|facts| facts.paused = true),
                None,
                Kind::End(ChildEnd::Skipped {
                    disposition: AgentDisposition::Cancelled,
                    budget: false,
                }),
            ),
            (
                with(|facts| facts.calls_left = false),
                None,
                Kind::End(ChildEnd::Skipped {
                    disposition: AgentDisposition::Cancelled,
                    budget: true,
                }),
            ),
            (
                with(|facts| facts.tokens_open = false),
                None,
                Kind::End(ChildEnd::Skipped {
                    disposition: AgentDisposition::Cancelled,
                    budget: true,
                }),
            ),
            (
                OPEN,
                Some(WaveStop::BUDGET),
                Kind::End(ChildEnd::Skipped {
                    disposition: AgentDisposition::Cancelled,
                    budget: false,
                }),
            ),
        ] {
            let requests = (0..=crate::session::MAX_STEP_TOOL_CALLS)
                .map(|index| ToolRequest {
                    id: crate::provider::ToolUseId::new(format!("call_{index}")).unwrap(),
                    call: ToolCall::List { path: ".".into() },
                })
                .collect();
            assert_eq!(
                kind(&decide_child_call(
                    response(ProviderOutcome::Tools(requests)),
                    false,
                    None,
                    abort,
                    &facts
                )),
                expected
            );
        }
    }

    #[test]
    fn child_calls_decide_by_kind_outcome_and_facts() {
        use AgentDisposition::{Cancelled, Failed, StoppedByHook};
        let editor = |cause| Kind::End(ChildEnd::Editor(cause));
        let researcher = |disposition| Kind::End(ChildEnd::Researcher(disposition));
        let skipped = |disposition, budget| {
            Kind::End(ChildEnd::Skipped {
                disposition,
                budget,
            })
        };
        let unverified = editor(WorkerCause::CriterionUnverified);
        let reprompt = Kind::Reprompt(RepromptReason::CriterionNotMetInView);
        let torn = Some(WaveStop::Abort(StopCause::ToolStopped));
        let hook_stop = Some(WaveStop::Boundary(StopCause::HookStop));
        let invalid = || response(ProviderOutcome::Answer(Answer { text: " ".into() }));
        let rows: Vec<Row> = vec![
            (
                answer(),
                true,
                Some(GATE),
                None,
                OPEN,
                Kind::Finish { stop_hook: true },
            ),
            (
                answer(),
                true,
                Some(GATE),
                torn,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (answer(), true, Some(gate(|_| {})), None, OPEN, reprompt),
            (
                answer(),
                true,
                Some(gate(|g| g.worker_stopped = true)),
                None,
                OPEN,
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|g| g.stop_room = false)),
                None,
                OPEN,
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|g| g.continuations_left = false)),
                None,
                OPEN,
                unverified,
            ),
            (answer(), true, Some(gate(|_| {})), torn, OPEN, unverified),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.turns_left = false),
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.tool_calls_left = false),
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.before_deadline = false),
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.tokens_open = false),
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.calls_left = false),
                unverified,
            ),
            (tools(), true, Some(GATE), None, OPEN, Kind::ToolStep),
            (
                tools(),
                true,
                Some(GateFacts {
                    worker_stopped: true,
                    ..GATE
                }),
                None,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (
                tools(),
                true,
                Some(GATE),
                hook_stop,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (
                tools(),
                false,
                Some(GATE),
                None,
                with(|f| f.turns_left = false),
                editor(WorkerCause::TurnLimit),
            ),
            (
                tools(),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::InvalidOutcome),
            ),
            (
                invalid(),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::InvalidOutcome),
            ),
            (
                CallResult::Failed(CallFailure::OutputLimit),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::InvalidOutcome),
            ),
            (
                CallResult::Failed(CallFailure::InvalidResponse(None)),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::InvalidOutcome),
            ),
            (
                timeout(),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (
                CallResult::Cancelled,
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (
                answer(),
                true,
                None,
                None,
                OPEN,
                Kind::Finish { stop_hook: true },
            ),
            (
                answer(),
                true,
                None,
                None,
                with(|f| f.has_tools = false),
                Kind::Finish { stop_hook: false },
            ),
            (
                answer(),
                true,
                None,
                None,
                with(|f| f.cancelled = true),
                Kind::Finish { stop_hook: false },
            ),
            (
                answer(),
                true,
                None,
                Some(WaveStop::BUDGET),
                OPEN,
                Kind::Finish { stop_hook: false },
            ),
            (answer(), true, None, hook_stop, OPEN, researcher(Cancelled)),
            (
                answer(),
                true,
                None,
                None,
                with(|f| f.hook.uncertain = true),
                researcher(Cancelled),
            ),
            (
                answer(),
                true,
                None,
                None,
                with(|f| f.hook.stop_requested = true),
                researcher(Cancelled),
            ),
            (
                tools(),
                true,
                None,
                None,
                with(|f| f.hook.settled = false),
                researcher(Cancelled),
            ),
            (tools(), true, None, None, OPEN, Kind::ToolStep),
            (
                tools(),
                true,
                None,
                None,
                with(|f| f.calls_left = false),
                skipped(Cancelled, true),
            ),
            (
                tools(),
                true,
                None,
                None,
                with(|f| f.tokens_open = false),
                skipped(Cancelled, true),
            ),
            (
                tools(),
                true,
                None,
                None,
                with(|f| f.paused = true),
                skipped(Cancelled, false),
            ),
            (tools(), true, None, torn, OPEN, skipped(Cancelled, false)),
            (
                tools(),
                true,
                None,
                None,
                with(|f| {
                    f.cancelled = true;
                    f.actor_hook_stop = true;
                }),
                skipped(StoppedByHook, false),
            ),
            (invalid(), false, None, None, OPEN, researcher(Failed)),
            (timeout(), false, None, None, OPEN, researcher(Failed)),
            (
                CallResult::Cancelled,
                false,
                None,
                None,
                OPEN,
                researcher(Cancelled),
            ),
        ];
        for (index, (result, valid, gate, abort, facts, expected)) in rows.into_iter().enumerate() {
            assert_eq!(
                kind(&decide_child_call(result, valid, gate, abort, &facts)),
                expected,
                "row {index}: {gate:?} {abort:?} {facts:?}"
            );
        }
    }

    #[test]
    fn subagent_stop_runs_decide_the_child_end() {
        use AgentDisposition::{Cancelled, Failed, StoppedByHook};
        let end = |disposition, cause, stops_wave| ChildStop::End {
            disposition,
            cause,
            stops_wave,
        };
        let unsettled = with(|f| f.hook.settled = false);
        let budget = StopHookRun::CutOffByActiveTime;
        let rows = [
            (
                StopHookRun::Cancelled,
                None,
                with(|f| f.cancelled = true),
                end(Cancelled, WorkerCause::Cancelled, true),
            ),
            (StopHookRun::Passed, None, OPEN, ChildStop::Proceed),
            (
                StopHookRun::Passed,
                None,
                with(|f| f.cancelled = true),
                end(Cancelled, WorkerCause::Cancelled, true),
            ),
            (
                StopHookRun::Passed,
                None,
                with(|f| {
                    f.cancelled = true;
                    f.actor_hook_stop = true;
                }),
                end(Cancelled, WorkerCause::Cancelled, true),
            ),
            (
                StopHookRun::Passed,
                None,
                with(|f| f.actor_hook_stop = true),
                end(StoppedByHook, WorkerCause::StoppedByHook, false),
            ),
            (budget, None, OPEN, end(Failed, WorkerCause::Budget, false)),
            (
                budget,
                None,
                unsettled,
                end(Failed, WorkerCause::Budget, true),
            ),
            (
                budget,
                Some(WaveStop::BUDGET),
                unsettled,
                end(Failed, WorkerCause::Budget, false),
            ),
            (
                StopHookRun::CutOffByContext,
                None,
                with(|f| f.cancelled = true),
                end(Cancelled, WorkerCause::ToolFailed, false),
            ),
            (
                StopHookRun::Uncertain,
                None,
                OPEN,
                end(Failed, WorkerCause::ToolFailed, false),
            ),
            (
                StopHookRun::Uncertain,
                None,
                with(|f| f.cancelled = true),
                end(Failed, WorkerCause::ToolFailed, false),
            ),
            (StopHookRun::Blocked, None, OPEN, ChildStop::Reprompt),
            (
                StopHookRun::Blocked,
                None,
                with(|f| f.turns_left = false),
                end(Failed, WorkerCause::TurnLimit, false),
            ),
            (
                StopHookRun::Blocked,
                None,
                with(|f| f.before_deadline = false),
                end(Failed, WorkerCause::Budget, false),
            ),
            (
                StopHookRun::Blocked,
                None,
                with(|f| f.tokens_open = false),
                end(Failed, WorkerCause::Budget, false),
            ),
            (
                StopHookRun::Blocked,
                None,
                with(|f| {
                    f.turns_left = false;
                    f.calls_left = false;
                }),
                end(Failed, WorkerCause::Budget, false),
            ),
        ];
        for (index, (run, abort, facts, expected)) in rows.into_iter().enumerate() {
            assert_eq!(
                decide_child_stop(run, abort, &facts),
                expected,
                "row {index}: {run:?} {abort:?} {facts:?}"
            );
        }
    }

    #[test]
    fn queue_stops_settle_the_run_with_their_own_cause() {
        use super::super::super::stop::{PrimaryEnd, RunFacts, Settlement, settle};
        use crate::session::RunFailure;
        let failed = |failure| Settlement::Fail {
            failure,
            exhausted: false,
        };
        let refusal = StopCause::ToolRefused(crate::session::ToolRefusal::INVALID_TRANSITION);
        for (cause, wave, settlement) in [
            (
                StopCause::ContextFull,
                WaveStop::Boundary(StopCause::ContextFull),
                failed(RunFailure::ToolContextFull),
            ),
            (
                StopCause::Cancelled,
                WaveStop::Boundary(StopCause::Cancelled),
                Settlement::Cancel,
            ),
            (
                StopCause::HookStop,
                WaveStop::Boundary(StopCause::HookStop),
                Settlement::HookStop,
            ),
            (
                StopCause::Budget,
                WaveStop::BUDGET,
                Settlement::Fail {
                    failure: RunFailure::BudgetExhausted,
                    exhausted: true,
                },
            ),
            (
                StopCause::Capacity,
                WaveStop::Boundary(StopCause::Capacity),
                failed(RunFailure::ToolStopped),
            ),
            (
                StopCause::DelegationDeadline,
                WaveStop::Boundary(StopCause::DelegationDeadline),
                failed(RunFailure::ToolStopped),
            ),
            (
                StopCause::Uncertain,
                WaveStop::Abort(StopCause::Uncertain),
                Settlement::Unknown,
            ),
            (
                refusal,
                WaveStop::Abort(refusal),
                failed(RunFailure::ProposalRefused),
            ),
        ] {
            assert_eq!(
                wave_stop_after(Some(cause), false, false, None, OPEN.hook),
                Some(wave),
                "{cause:?}"
            );
            assert_eq!(
                settle(PrimaryEnd::Stopped(wave.cause()), &RunFacts::ready()),
                settlement,
                "{cause:?}"
            );
        }
    }

    #[test]
    fn wave_stops_after_a_child_by_one_order() {
        use AgentDisposition::{Cancelled, Failed, Finished, StoppedByHook};
        let settled = OPEN.hook;
        let uncertain = HookFacts {
            uncertain: true,
            ..settled
        };
        let requested = HookFacts {
            stop_requested: true,
            ..settled
        };
        let unsettled = HookFacts {
            settled: false,
            ..settled
        };
        let abort = |cause| Some(WaveStop::Abort(cause));
        let boundary = |cause| Some(WaveStop::Boundary(cause));
        // (queue stop, cancelled, paused, child end, hook state, wave stop)
        let rows = [
            (
                Some(StopCause::Uncertain),
                false,
                false,
                Some(Failed),
                settled,
                abort(StopCause::Uncertain),
            ),
            (
                Some(StopCause::ToolRefused(
                    crate::session::ToolRefusal::INVALID_TRANSITION,
                )),
                false,
                false,
                Some(Finished),
                settled,
                abort(StopCause::ToolRefused(
                    crate::session::ToolRefusal::INVALID_TRANSITION,
                )),
            ),
            (
                Some(StopCause::Budget),
                true,
                true,
                Some(Failed),
                uncertain,
                Some(WaveStop::BUDGET),
            ),
            (
                None,
                true,
                true,
                Some(StoppedByHook),
                uncertain,
                abort(StopCause::Cancelled),
            ),
            (
                None,
                false,
                true,
                Some(StoppedByHook),
                uncertain,
                boundary(StopCause::Paused),
            ),
            (
                None,
                false,
                false,
                Some(StoppedByHook),
                uncertain,
                boundary(StopCause::HookStop),
            ),
            (
                None,
                false,
                false,
                Some(Failed),
                uncertain,
                abort(StopCause::Uncertain),
            ),
            (
                None,
                false,
                false,
                Some(Failed),
                requested,
                boundary(StopCause::HookStop),
            ),
            (
                None,
                false,
                false,
                Some(Failed),
                unsettled,
                abort(StopCause::ToolStopped),
            ),
            (
                None,
                false,
                false,
                Some(Failed),
                settled,
                abort(StopCause::ToolStopped),
            ),
            (
                None,
                false,
                false,
                Some(Cancelled),
                settled,
                abort(StopCause::Cancelled),
            ),
            (None, false, false, Some(Finished), settled, None),
            (None, false, false, None, settled, None),
            (
                None,
                false,
                false,
                None,
                unsettled,
                abort(StopCause::ToolStopped),
            ),
        ];
        for (index, (queue_stop, cancelled, paused, end, hook, expected)) in
            rows.into_iter().enumerate()
        {
            assert_eq!(
                wave_stop_after(queue_stop, cancelled, paused, end, hook),
                expected,
                "row {index}"
            );
        }
        for cause in [
            StopCause::ActiveTime,
            StopCause::DelegationDeadline,
            StopCause::Capacity,
        ] {
            assert_eq!(
                wave_stop_after(Some(cause), false, false, Some(Failed), settled),
                boundary(cause)
            );
        }
        for (result, reason) in [
            (answer(), Some(RepromptReason::AnswerDeferred)),
            (
                response(ProviderOutcome::Delegate(crate::provider::Delegate {
                    agent: None,
                    children: Vec::new(),
                })),
                Some(RepromptReason::DelegateRefused),
            ),
            (tools(), Some(RepromptReason::ToolRefused)),
            (timeout(), None),
            (CallResult::Cancelled, None),
        ] {
            assert_eq!(RepromptReason::deferral(&result), reason);
        }
    }
}
