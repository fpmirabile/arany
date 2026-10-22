# Durable Research → Plan → Implementation Goals

## Metadata

- Status: `Draft`
- Owner: Fernando Mirabile
- Last updated: 2026-10-22

## Outcome and current boundary

Let the harness advance a Goal through research, an approved plan and multi-Run
implementation. This is proposed behavior. [Goals](../../docs/specs/goals.md)
already owns durable budgets, plan contracts, success receipts and recovery;
[Terminal](../../docs/specs/terminal.md) and
[permissions](../../docs/specs/tools.md#folder-trust-and-approvals) own current
modes. This plan does not change those contracts until accepted and implemented.

## Proposed direction and rationale

Use separate Runs for phases so approval waits hold no Run deadline and recovery
stops at a durable phase boundary. Research is read-only, may delegate researchers
and may be skipped for a trivial task. Plan is read-only, presents the proposed
team/leases/budget and has the mandatory approval gate. Feedback returns to Plan;
an explicit research request returns to Research. Revisions consume budget.

Store bounded research/plan artifacts and grilling answers as journaled Session
facts with transcript/inspection/export, rather than writing proposal files into
the repository. Grilling is a typed user-question outcome: at most 12 questions
per round and 6 rounds per Goal, each with a recommended answer. Phase requests
carry approved artifacts, Goal facts and their own capsule; workers receive only
their assignment capsule. Compaction retains requests, decisions, touched files
and errors while dropping raw Tool output.

The proposed Shift+Tab cycle is RPI → Ask → Accept edits → Auto, with a private
user-selected default (proposed fresh-install default RPI). Plan approval selects
an implementation mode. Ask approves effects individually; Accept edits follows
the current typed-edit/changeset boundary; Auto must use an accepted approval
policy within the pinned plan. [Supervision](../agentic-teams/README.md) can only
narrow and cannot itself grant Auto authority. Proposed team startup asks in RPI
and Ask; other modes must fit admitted presets and budgets. No attached bypass
mode is proposed.

Headless `exec` enters RPI only through an explicit flag approving the plan within
its upfront grant/budget; grilling recommendations become recorded assumptions in
the final report. Without it, retain current direct implementation behavior.
Reuse Goal continuation, cancellation, explicit resume and final reports.

## Contract, scenarios and remaining slices

1. Goals/Execution: phase transitions, bounded artifact storage and one Plan
   gate across Runs; Terminal/Serve: approve, revise, request research, cancel
   and explicitly resume after restart. Extend the existing Goal/reducer and
   product Session journeys for each successful and interrupted path.
2. Add typed bounded grilling and phase-specific context/compaction through
   those same surfaces. Verify overflow, omitted answers, stale approvals and
   exhausted budgets without widening authority or losing prior artifacts.
3. Terminal/Tools: accepted mode cycle and private default setting; `exec`:
   explicit headless flag and visible recorded assumptions. Verify rejected
   plans, read-only research, exact grant enforcement and no interactive wait
   in headless mode. JSONL follows the existing output contract.

Policy stays in Engine, durable state in the journal, and presentation in the
CLI. Linux/macOS have common behavior and existing Guard adapters. The
[security pass](../../agents/security.md#security-pass-triggers) must cover
artifact disclosure, configuration and approval; all new artifact/question/
transition/call/storage limits reject overflow before dispatch.

## Open questions

Owner: Fernando Mirabile. Resolve pause-on-exhaustion against today's terminal
budget-exhaustion contract, Auto approval authority, phase/milestone boundaries,
artifact byte/count/retention limits, and private setting admission before
implementation. Peer RPI/mode research remains required by
[PLANNING.md](../PLANNING.md); the earlier proposal supplies no peer equivalence
or authority to change the current contract.

## Exit criteria

A scripted Goal reaches Completed and Failed with honest reports, survives
phase-boundary restart, handles plan rejection/research feedback and cancellation,
and never repeats an uncertain effect. Headless RPI stays within its upfront
grant. Require native accessibility and live-model evidence separately in
[NEXT_STEPS](../../NEXT_STEPS.md); close the plan as behavior moves into specs.
