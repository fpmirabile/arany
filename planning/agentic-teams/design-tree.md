# Agentic coding teams: design tree

Grilling record for the [agentic coding teams plan](./README.md#chosen-direction). Each node is a
decision with its recommendation and the user's answer. Settled answers are
applied to the plan; this file keeps the decision trail and open nodes.

## Round 1 — settled 2026-10-13

| # | Decision | Answer | Applied to |
|---|---|---|---|
| Q1 | Single-agent foundation (A) and Supervisor core before writable teams (C) | Yes | Plan execution order |
| Q2 | Command writes return as Engine-validated changesets, not a writable Workspace mount | Yes | A1 |
| Q3 | Supervisor owned by the Engine over journaled Events | Yes, amended: evaluators must be configurable and not tied to Engine-only deterministic rules; typed model judges such as TypeSafe AI's Jev can decide non-deterministic cases | B intro, execution step 4 |
| Q4 | Plan contract inside the first Tool outcome, mandatory for every Run with effects | Yes | B1 |
| Q5 | Shadow mode for fuzzy signals; false completion and Finish gate enforce immediately | Yes | B2 |
| Q6 | Registry fetch with network leaves this plan | Yes | Moved to [next steps](../../NEXT_STEPS.md#next-functional-priority) |
| Q7 | Toolchains mount exact subtrees read-only with identity pins, never whole home/tool directories | Yes | A2 |
| Q8 | README goal now; replace D-03 when the teams contract is accepted | Yes | [README](../../README.md) updated |

Q3 context: Jev returns calibrated probabilities for Boolean, Choice or Score
questions rather than generated text
([deepeval integration](https://deepeval.com/integrations/models/typesafe-ai),
[AI SDK provider](https://ai-sdk.dev/providers/ai-sdk-providers/typesafe-ai)).
Its API contract still needs primary-source verification before an adapter.

User directive (2026-10-13): Jev is one example, not a fixed choice. Providers
must be interchangeable per agent: one Run can mix a ChatGPT subscription, an
Anthropic API key, a typed-judge Provider and any of their models in any role.
Applied as workstream D in the plan.

## Round 2 — settled 2026-10-13

| # | Decision | Answer | Applied to |
|---|---|---|---|
| R2-1 | Fresh snapshot per command; immediate changeset integration; persistent views only with C | Yes | A1 |
| R2-2 | Changeset integration follows the approval mode | Yes, amended: redesign the approval modes after Claude Code's permission modes; the current modes are unsatisfying | A1, B6, Round 3 |
| R2-3 | Private versioned `supervisor.json` with closed schema; never project-configured | Yes | B configuration |
| R2-4 | Judges at checkpoints and on deterministic signals; per-effect judging opt-in | Yes | B triggers |
| R2-5 | Judges are roster profiles; typed-judge adapters need a primary-source research note | Yes | B intro |
| R2-6 | Probability bands (≥0.65 / ≤0.35 / ask between); judges only narrow | Yes | B verdicts |
| R2-7 | Separate judge budget; exhaustion asks the user | Yes | B budget |
| R2-8 | Private roster file with `agent` and `judge` capability classes | Yes | D1 |
| R2-9 | Per-role defaults; primary picks only among eligible profiles | Yes, amended: the primary composes the team from the user's request and gives each worker its mission and direction | D2, C intro, B1 |
| R2-10 | One explicit roster-preset choice at start | Yes | D3 |
| R2-11 | Unadmittable profile rejects the start | Yes, amended: name the failing profile; if agents are already working, pause them until the user acts | D3, D4 |
| R2-12 | No automatic fallback | Amended: keep no fallback by default, plus a user-enabled setting naming a fallback profile (a default reasoning model) | D4 |

## Round 3 — settled 2026-10-13

| # | Decision | Answer | Applied to |
|---|---|---|---|
| R3-1 | Plan → Ask → Accept edits → Auto, no bypass | Amended: no standalone Plan mode. Arany works on RPI (Research → Plan → Implementation); RPI is the mode in that slot | B6 |
| R3-2 | Plan mode read-only, ending in plan/team approval | Covered by R3-1: it describes RPI's Plan phase | B6 |
| R3-3 | Auto: Supervisor approves inside the plan contract; replaces per-action AI review | Yes | B6 |
| R3-4 | Accept edits: edits and in-scope changesets without prompts; commands/MCP ask | Yes | B6 |
| R3-5 | New trust starts in Ask; last mode remembered per folder | Yes (R4-6 revisits the default given RPI) | B6 |
| R3-6 | Team approval in RPI/Ask; automatic in Accept edits/Auto within preset and budgets | Yes | B6 |
| R3-7 | Fallback only on Provider-call failure; usage notice; no Tool re-dispatch | Yes | D4 |
| R3-8 | One fallback profile per capability class | Yes | D4 |
| R3-9 | Pause bound agents at the next call boundary; others continue; paused time excluded from Run wall clock | Yes | D5 |

## Round 4 — settled 2026-10-13

| # | Decision | Answer | Applied to |
|---|---|---|---|
| R4-1 | RPI is one mode in the Shift+Tab cycle; Implementation runs under the mode chosen at plan approval | Yes | B6 |
| R4-2 | Each RPI phase is its own Run linked by its approved artifact | Yes, amended: the harness divides the work, waits for whoever must act and continues; RPI is driven to completion like a goal that knows how to work | B6, Round 5 |
| R4-3 | Read-only Research with optional read-only team; skippable for trivial tasks; Plan gate never skipped | Yes | B6 |
| R4-4 | Plan is the only mandatory gate | Yes | B6 |
| R4-5 | Research brief and plan are Session artifacts, exportable | Yes | B6 |
| R4-6 | Plan rejection returns to Plan; "research more" returns to Research | Yes, amended: Arany's Plan phase should grill the user like this design session | B6, R5-4 |
| R4-7 | New trust starts in RPI | Amended: the default mode is a user setting; a fresh installation defaults to RPI | B6, R5-7 |

## Round 5 — settled 2026-10-13

| # | Decision | Answer | Applied to |
|---|---|---|---|
| R5-1 | Durable Engine-owned Goal spanning phase Runs | Yes | B6 |
| R5-2 | Goal budget set by the approved plan; exhaustion pauses | Yes, amended: the user may remove usage ceilings (tokens and similar); CPU and other resource ceilings are not opened for now | B6, R6-4 |
| R5-3 | Implementation spans several Runs split by milestone | Yes, amended: the Engine must be flexible with these values for larger tasks; follow what Claude Code and Codex do rather than reinventing | A4, Round 6 |
| R5-4 | Typed grilling outcome in Plan, 12 questions × 6 rounds | Yes | B6 |
| R5-5 | Completed/Failed/Cancelled with a final report | Yes | B6 |
| R5-6 | Resume only after user confirmation | Yes | B6 |
| R5-7 | Default-mode setting always applies; per-folder memory dropped | Yes | B6 |

## Round 6 — settled 2026-10-13

Fact: `MAX_TOOL_CALLS = 16`, a 64 KiB Tool context and the 300-second Run are
compile-time constants ([src/tools/types.rs](../../src/tools/types.rs)).
Mainstream coding harnesses do not cap tool calls per turn: their loop runs
until the model finishes, context is managed by compaction, commands have a
default timeout with a configurable maximum (Claude Code: 2-minute default,
10-minute maximum via `BASH_DEFAULT_TIMEOUT_MS`/`BASH_MAX_TIMEOUT_MS`), and
headless use offers an optional turn limit.

| # | Decision | Recommendation | Answer |
|---|---|---|---|
| R6-1 | Remove the 16-proposal and 300-second caps as product constants: a Run's loop continues until Finish, bounded by context compaction, the Goal budget and cancellation; the Tool context grows to the model's admitted context with compaction instead of a fixed 64 KiB | Yes. Matches established harnesses; Goal budgets and Supervisor signals replace arbitrary small caps as the safety mechanism | Yes; applied to A4/B6 |
| R6-2 | Command timeout: 2-minute default, per-command request up to a user-configurable maximum (default 10 minutes) | Yes. Proven defaults from Claude Code; large builds raise the maximum in settings | Yes; applied to A4/B6 |
| R6-3 | The plan contract may request a larger resource profile or timeout; Plan-gate approval grants it; in Accept edits and Auto, requests within settings maxima are granted without a prompt; beyond the maxima always asks. The Engine never exceeds user-set maxima on its own | Yes. Gives the Engine the flexibility you asked for while the user keeps the outer bound | Yes; applied to A4/B6 |
| R6-4 | Settings can set Goal usage dimensions (tokens, calls, time) to unlimited; Arany then shows running usage visibly; per-request Provider caps and finite Guard resource ceilings (CPU, memory, storage, processes) always remain. AGENTS.md rule 4 gains this documented exception | Yes. Honors the user's choice without removing the bounds that protect the machine and parsers | Yes; applied to A4/B6 |
| R6-5 | Runs remain the durable checkpoint unit: the Engine closes a Run at a milestone or wave boundary, never because an arbitrary cap was hit | Yes. Keeps recovery granularity independent of limits | Yes; applied to A4/B6 |

## Round 7 — settled 2026-10-13

| # | Decision | Answer | Applied to |
|---|---|---|---|
| R7-1 | Steering notes at the next call boundary; `/pause` | Yes | E1 |
| R7-2 | Headless RPI behind an explicit flag | First answered "remove `exec`", then reversed: keep `exec` as the scriptable product surface for CI, scripts and API-style use. The headless RPI question returns as R8-1 | E10, R8-1 |
| R7-3 | Phase Runs get artifacts, Goal facts and capsules, not transcripts | Yes | E2 |
| R7-4 | One writing Goal per Workspace | Yes | E3 |
| R7-5 | Explicit undo through reverse changesets | Yes | E4 |
| R7-6 | One-use override of Supervisor responses | Yes | E5 |
| R7-7 | One-level topology, no peer messages, default 4 concurrent workers | Yes for now; multiple supervisors are an expansion to experiment with | C intro, expansion options |
| R7-8 | Worker grant = Goal grant ∩ assignment; no Git or network | Yes | C intro |
| R7-9 | Never commit or push | Amended: commit or push when the user asks, as other harnesses do | E6, Round 8 |
| R7-10 | Single-agent baseline before claiming teams help | Yes | Exit criteria |
| R7-11 | Goal view in the terminal | Yes, modelled on Claude Code for now | E7 |
| R7-12 | Cross-Goal memory out of scope | Yes | E9 |

## Round 8 — settled 2026-10-13

| # | Decision | Answer | Applied to |
|---|---|---|---|
| R8-1 | `exec` runs RPI only with an explicit plan-approval flag; grilling resolves to recommendations recorded as assumptions | Yes | E10 |
| R8-2 | `exec` emits Goal progress and the final report on its JSONL channel | Yes | E10 |
| R8-3 | Git writes only on explicit user request, approved in every mode | Amended: behave like Claude Code and Codex; if the project rules (AGENTS.md) say to commit, respect them; otherwise never | R9-1, R9-2 |
| R8-4 | Host `git` with fixed argv after a closed allowlist of `.git/config` keys; security pass | Yes | E6 |
| R8-5 | Hooks never run on the host; optional Guard-confined hook run | Yes | E6 |
| R8-6 | Claude Code–style Goal view with a checklist | Amended: no checklist (it spends tokens needlessly); tell the story of what the team is doing and show occasional progress messages from each agent | E7, R9-3, R9-4 |
| R8-7 | Every task is a Goal | Yes | E11 |
| R8-8 | Goal facts and artifacts survive compaction | Yes, pending validation against other harnesses | R9-5 |
| R8-9 | Cancel keeps integrated changes and offers undo | Amended: do what other harnesses do | R9-6, R9-7 |
| R8-10 | One active Goal per Session; prompts during it are steering notes | Yes | E11 |

## Round 9 — settled 2026-10-13

Research (2026-10-13, mostly secondary sources; official pages where linked):

- **Commits.** Claude Code's guidance is to commit only when asked, but many
  user reports show commits and pushes despite CLAUDE.md rules forbidding
  them; permission `ask`/`deny` rules are the dependable control
  ([permissions](https://code.claude.com/docs/en/permissions),
  [issue 13009](https://github.com/anthropics/claude-code/issues/13009)).
  In Arany, AGENTS.md is repository text: a hostile repository could
  instruct "push after every change", so it can guide but not authorize.
- **Compaction.** Claude Code re-injects project CLAUDE.md, the plan-mode plan
  and task lists from disk after compaction; the summary keeps requests,
  decisions, touched files and errors, and drops raw tool output
  ([what survives compaction](https://claudefa.st/blog/guide/mechanics/what-survives-compaction)).
  Codex keeps a summary plus recent user messages and treats AGENTS.md as
  instructions outside history
  ([Codex compaction architecture](https://codex.danielvaughan.com/2026/03/31/codex-cli-context-compaction-architecture/)).
- **Cancel and undo.** Claude Code keeps changes on interrupt; every user
  prompt creates a checkpoint and `/rewind` restores code, conversation or
  both, but bash-command edits are not tracked
  ([checkpointing](https://code.claude.com/docs/en/checkpointing)). Codex
  removed its ghost-commit `/undo`
  ([openai/codex#8454](https://github.com/openai/codex/pull/8454)).

| # | Decision | Answer | Applied to |
|---|---|---|---|
| R9-1 | Model commits on its own only when project rules ask and the user enabled it for the trusted folder | Yes; trusting a hostile repository is the user's responsibility, not Arany's | E6 |
| R9-2 | Remote-touching writes always need human approval | Yes; force-push and similar disruptive operations are forbidden unless a user setting enables them | E6, R10-1 |
| R9-3 | Engine activity lines plus bounded agent progress notes and phase narration | Yes | E7 |
| R9-4 | One live line per agent; history in inspection | Yes | E7 |
| R9-5 | Re-inject instructions, plan, Goal facts and answers after compaction | Yes | E8 |
| R9-6 | Checkpoints per prompt and phase; `/rewind` code, conversation or both | Yes | E4 |
| R9-7 | 30-day bounded checkpoint retention | Yes | E4 |

## Round 10 — settled 2026-10-13

| # | Decision | Answer | Applied to |
|---|---|---|---|
| R10-1 | Enabled disruptive Git operations still need per-operation approval | Yes | E6 |
| R10-2 | One private user settings file; per-folder values with folder trust | Yes | E12 |
| R10-3 | Per-folder versus global settings split | Yes | E12 |
| R10-4 | Goal, phase, agent and Supervisor telemetry spans | Yes | E13 |
| R10-5 | Usage as tokens and calls per profile, no cost estimates | Yes | E7 |
| R10-6 | New `docs/specs/goals.md` domain spec | Yes | Contract owners, execution step 2 |
| R10-7 | Own initiative folder `planning/agentic-teams/` | Yes | This folder; [beta plan](../arany-beta/README.md) links here |
| R10-8 | Beta 1 closes as is; this initiative headlines Beta 2 | First yes, then reversed by the user: all of this initiative is Beta 1 scope | Plan outcome, [beta plan](../arany-beta/README.md), [next steps](../../NEXT_STEPS.md) |

## Round 11 — settled 2026-10-13

The team nodes were marked blocked on starting C, but their prerequisites (Q1,
Q2) are settled, so they are decidable now. R11-7 also closes the plan's
changeset-limit open question.

| # | Decision | Recommendation | Answer |
|---|---|---|---|
| R11-1 | Each worker gets one persistent private Guard view for its whole assignment (the Workspace at assignment plus its own work); its commands run there and it hands the primary one cumulative changeset against its assignment base. No Git worktrees | Yes. Workers accumulate build state and edits without touching the Workspace; worktrees bring `.git` and no confinement | Yes; applied to C |
| R11-2 | Path leases are file or directory prefixes in the assignment and are enforced: a worker's writes outside its lease reject at changeset validation, not just as a Supervisor signal. Reads stay within the Goal grant | Yes. Ownership must be a guarantee, not a hint | Yes; applied to C |
| R11-3 | Changesets integrate in completion order (leases do not overlap), replacing the earlier assignment-order wording; the journal records the order, so replay stays deterministic | Yes. Faster feedback; ordering is a recorded fact rather than a schedule | Yes; applied to C |
| R11-4 | A failed digest precondition is a conflict: the primary receives both versions and resolves it itself or reassigns the work on the current base; no automatic text merge | Yes. Silent merges are where team edits get lost | Yes; applied to C |
| R11-5 | A worker must pass its own success criterion inside its view before handing over (per-worker Finish gate); the primary re-verifies after integration | Yes. Catches breakage before integration and interaction effects after | Yes; applied to C |
| R11-6 | A failed or cancelled worker's changeset is discarded and reported; the primary may issue a new assignment, which counts against the budget; no automatic retry | Yes. Consistent with the no-implicit-retry rule | Yes; applied to C |
| R11-7 | A changeset holds at most 2,048 files and 32 MiB, matching snapshot bounds; oversized changesets reject whole with a count report; no splitting | Yes. Reuses existing bounds; splitting would integrate partial work | Yes; applied to C |
| R11-8 | Dependencies are expressed as waves: a later wave starts after the earlier wave integrates; no general task DAG | Yes. Covers sequential dependencies without the lease and fencing machinery a DAG needs | Yes; applied to C |

## Round 12 — settled 2026-10-13

Plan review after the grilling, with stated confidence. Repository pace (first
commit 2026-09-29, 45 commits, PRs of thousands of lines) lowered the weight
of time-based arguments.

| # | Change | Confidence | Answer | Applied to |
|---|---|---|---|---|
| R12-1 | Reorder: toolchains, cache and limits right after A1, then a dogfooding milestone ("Arany develops Arany") before Goal, modes and Supervisor work | High (~80%): the current limits block real coding, and shadow-mode calibration needs real Runs | Yes | Execution steps 2–3 |
| R12-2 | Minimal Beta 2 cut | Medium (~55%): weak given the observed pace | Not applied | — |
| R12-3 | RPI default may add friction to small tasks | Low (~40%): speculative | Observe during dogfooding | Execution note |
| R12-4 | Per-slice process cost may slow delivery | Low (~35%): agents absorb most of it at the observed pace | Observe during dogfooding | Execution note |

## User directives after Round 12

| Date | Directive | Applied to |
|---|---|---|
| 2026-10-13 | The whole initiative is Beta 1 scope (reverses R10-8) | Plan outcome, beta plan, next steps |
| 2026-10-13 | No native Windows: Windows users run the Linux build under WSL2 | AGENTS.md priority 2, README, CONTRIBUTING, planning template, security rules, decision D-17, next steps, plan OS impact |
| 2026-10-13 | Registry fetch joins Beta 1 as A6 (reverses Q6) | Plan A6, security model, OS impact, execution step 8; removed from next steps as a separate initiative |

## Expansion options

Not part of this plan; recorded so they are not lost.

- Multiple supervisors (for example, one per worker or competing judges)
  after shadow-mode data shows where a single Supervisor falls short.

## Frontier status

Rounds 1–6 settled the decisions opened during the plan review, Rounds 7–10
the lifecycle and cross-cutting sweeps, Round 11 the team mechanics and
Round 12 the execution order. The
nodes below wait on implementation data; they are not silently assumed.

## Blocked nodes

- After A2 lands: `build` resource profile values per OS and default settings
  maxima, from measured builds on named hosts.
- After shadow-mode data: thresholds for loop, probing and disproportion.
