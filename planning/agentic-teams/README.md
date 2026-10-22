# Team supervision and changeset review

## Metadata

- Status: `Draft`
- Owner: Fernando Mirabile
- Last updated: 2026-10-22

## Outcome and scope

Detect trajectory drift before integration or Finish and apply a journaled,
narrowing response, with a read-only reviewer child at review points.
The [Supervisor adapter](../../docs/specs/tools.md#supervisor-adapter) already
implements pure shadow evaluation. This plan covers enforcement and review,
not that completed implementation or worker command execution.

## Research and current boundary

- `crates/arany/src/supervisor.rs` and the Tools contract own the fixed,
  Run-pinned evaluator. Production has no configuration file, model judge or
  enforcement; replay recomputes verdicts without effects. Existing signals
  are loop, probing and disproportion. Broader detection below is proposed.
- Worker commands and the in-view Finish gate landed in `3243ac72` (#169),
  following protocol `bf92c26f` (#151) and native targets `b8d27f7c` (#162).
  [Execution](../../docs/specs/execution.md#worker-journal-protocol),
  [Tools](../../docs/specs/tools.md#private-worker-targets-and-handover-transfer)
  and [Goals](../../docs/specs/goals.md#worker-accounting-and-verification)
  own handover, fresh per-command images and primary verification. The separate
  [persistent assignment-view plan](../worker-assignment-views/README.md) retains
  the future C3 lifetime/cache proposal; it does not change current behavior.
- [Detection versus enforcement](../../docs/research/deterministic-harness-protection.md#33-detection-and-observability-are-not-enforcement)
  explains why a judge cannot become an authority source. Peer/source research
  for new judges remains with [judge adapters](../judge-adapters/README.md).

## Proposed direction

Extend the existing coordinator and narrowing hook adapters; add no daemon or
subscriber. Scope drift, undeclared verification changes and injection suspicion
are candidate signals requiring corpus evidence, not existing guarantees.
Existing deterministic success receipts remain enforced independently.

Responses escalate monotonically: annotate, require human approval, revoke the
relevant capability, pause for a decision, then cancel. At integration and Finish,
a read-only reviewer compares the trajectory/changeset with the approved task.
A missing, uncertain, timed-out or over-budget evaluation asks the user.
Per-effect judging requires explicit cost consent rather than running by default.

A private versioned `supervisor.json` selecting evaluators/thresholds and default
probability bands (≥0.65 narrow, ≤0.35 clear, otherwise ask) are **future proposals**,
not the current file-free evaluator contract. They require accepted schemas,
finite judge budgets and Run-pinned policy before implementation. Model verdicts
are observations; replay never repeats a judge call. Repository text cannot
select evaluators or thresholds.

A one-use user response follows the existing permission route, is journaled and
can inform calibration without changing configuration or overriding Guard denial.
The Supervisor itself never grants, retries or approves effects.

## Contract, scenarios and evidence

Tools owns policy, evaluator admission and narrowing; Execution owns Event order,
reviewer scheduling and replay; Goals owns pause/recovery; Terminal owns prompts
and accessible notices; Telemetry owns privacy-reduced projection.
Use the existing Supervisor corpus, Engine/reducer and Guard journeys under
[testing rules](../../agents/testing.md), extending them for detection, exact
response, reviewer refusal, exhausted budgets, cancellation, interrupted prefixes
and replay without a second judge call. Linux/macOS share product policy; native
Guard, terminal and OTLP claims require the [owner checks](../../NEXT_STEPS.md#pursue-in-order).

## Remaining slices

1. After owner-accepted calibration, enforce a single existing signal end to end:
   pinned policy → committed verdict → narrowing response → visible pause/approval
   → explicit recovery. Update Tools/Execution/Terminal and replay evidence.
2. Add bounded read-only changeset review at integration/Finish through admitted
   roster profiles, with journaled verdicts and failure-to-human handling. Typed
   judge vendors depend on the separate adapter plan.
3. Add explicit one-use responses and their calibration facts, with no automatic
   policy rewrite. Extend opt-in telemetry with the existing privacy contract.

## Open questions

Owner: Fernando Mirabile. Accept thresholds only after the [owner-run calibration
criteria](../../NEXT_STEPS.md#pursue-in-order) pass. Decide evaluator
configuration/schema, review budget/timeout/call caps, response eligibility and
any new signal before its slice. Proposed probability bands are not calibrated
thresholds. No enforcement slice is implementation-ready yet.

## Related plans

<a id="e-goal-lifecycle-and-terminal"></a>

Git authority belongs exclusively to the [Git write workflow](../git-write-workflow/README.md)
and [typed Git contract](../../docs/specs/tools.md#typed-git-writes).
[Goal RPI](../goal-rpi/README.md), [Goal progress](../goal-progress/README.md),
[agent route recovery](../agent-route-recovery/README.md) and
[registry fetch](../registry-fetch/README.md) have independent lifecycles.
Cross-Session memory and multiple supervisors require a demonstrated need.

## Exit criteria

Each enabled signal has accepted calibration and corpus proof of detection,
narrowing, failure/recovery and replay equality; reviewers add no effect authority.
Compare team success, usage and elapsed time with a single-agent corpus baseline
before claiming a quality benefit. Keep missing native/live/assistive evidence in
NEXT_STEPS and move accepted behavior to specs as slices land.
