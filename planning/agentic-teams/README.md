# Agentic coding teams

## Metadata

- Status: `In progress`
- Owner: Fernando Mirabile
- Last updated: 2026-10-17

## Outcome

Arany is a harness specialized in multi-task agentic teams. A team must be able
to do ordinary coding work end to end: inspect, edit, build, test and integrate
changes inside a project, while the harness detects agent misbehavior and
narrows authority before it causes harm.

This initiative holds the remaining Beta 1 team design.
[Next steps](../../NEXT_STEPS.md#pursue-in-order) owns delivery priority, freezes
and user-owned checks; this plan defines the remaining slices.

## Using this plan

- This plan owns remaining proposals. The [design rationale](./design-tree.md)
  explains choices that still constrain that scope; implemented contracts live
  in the specs linked below.
- Follow the repository workflow in [AGENTS.md](../../AGENTS.md): load the
  triggered rules, apply Ponytail, and specify each slice in its owning
  contract under the [spec workflow](../../docs/specs/README.md) before
  implementation.
- Follow roadmap priority using the [slice boundaries](#execution). When a slice starts, set
  Status to `In progress`; when it lands, move its accepted behavior to the
  owning spec, remove it from this plan and update the next action, as the
  [planning lifecycle](../README.md#lifecycle) requires.
- Resolve material open decisions with the owner before their slice starts;
  accepted behavior belongs in its spec when implemented.

## Current boundary and owners

C1 read-only researchers are implemented; writable workers remain C2/C3 scope.
The [execution contract](../../docs/specs/execution.md#researcher-children)
owns current child identity, grants, limits, failure and synthesis ordering.
The [Goals contract](../../docs/specs/goals.md) owns lifecycle, plan contracts,
success receipts, budgets, writer exclusion, steering and pause. The
[Tools contract](../../docs/specs/tools.md) owns command changesets, runtime
roots, build cache, finite resource profiles and typed Git reads on Linux and
macOS. These are prerequisites, not remaining implementation slices.

[Per-role routing and fallback](../../docs/specs/provider.md#per-role-selection-and-fallback)
are implemented for primary, child, review and compaction. Roster presets,
assignment-selected profiles and judge-only adapters remain below. A child
inherits the primary chain only when its role has no configured override.

Current human approval and per-action AI review narrow individual effects;
trajectory evaluation and writable-team handover are still proposed here.
[Detection is separate from enforcement](../../docs/research/deterministic-harness-protection.md#33-detection-and-observability-are-not-enforcement):
Guard remains the effect boundary.

## Chosen direction

Five workstreams (A–E). Sections are grouped by subject; the
[execution](#execution) list defines remaining slices under roadmap priority; each leaves the
harness coherent and safer than before. Every slice keeps the existing invariants: one router mediates
effects, Policy only narrows, Guard enforces, replay never re-executes, and
unsupported enforcement fails closed. Child authority remains
`global policy ∩ parent effective authority ∩ explicit task grant`
([security rules](../../agents/security.md)).

### A. Registry fetch

6. **Registry fetch (A6).** Lets an agent add dependencies to the project it works
   on (`npm install zod`, `cargo add serde`, `pip install requests`) without
   giving project code the network.
   - **Two phases.** A separate fetch phase reaches only an allowlist of
     public registries through a Guard-owned proxy and writes only into the
     persistent [cache](../../docs/specs/tools.md#persistent-workspace-build-cache);
     lockfile and manifest changes return as a validated
     [changeset](../../docs/specs/tools.md#command-changesets). Build and test
     phases stay offline.
   - **No project code with network.** Fetch runs only the fixed fetch
     command of the admitted toolchain profile with install scripts
     disabled, for example `cargo fetch`, `npm install --ignore-scripts` and
     `pip download --only-binary=:all:`. Scripts and builds run later,
     offline.
   - **Proxy.** It admits HTTPS `CONNECT` only to allowlisted hosts, resolves
     DNS itself and rejects loopback, private and link-local addresses
     (SSRF), bounds bytes, connections and time per fetch, and carries no
     credentials. Default allowlist: crates.io, the npm registry and PyPI
     hosts; the user can add hosts in global settings. Private registries
     with tokens are out of scope.
   - **Approval.** Fetch is an effect and follows the mode: Ask and Accept
     edits ask; Auto lets the Supervisor approve it inside the plan contract.
   - **Platforms.** Linux routes a private network namespace only to the
     proxy; macOS uses Seatbelt rules that allow only the proxy port. A
     dedicated security pass under the
     [network rules](../../agents/security.md) precedes this slice.

### B. Supervisor: active misbehavior detection

The Engine owns the Supervisor: it collects evidence from journaled Events,
runs configured evaluators and applies their verdicts. It can only narrow:
annotate, require human approval, revoke a capability, pause the agent for a
decision or cancel. It never grants, retries or approves.

Evaluators are configurable, not hard-wired. Built-in deterministic signals
are always available; the user can add model-based judges for decisions that
are not deterministic. A judge is any roster profile with the judge
capability (D): an agent-capable Provider answering a strict structured
verdict, or a typed-judge Provider (for example TypeSafe AI's Jev) answering
Boolean, Choice or Score questions with calibrated probabilities. No judge
vendor is built into the Supervisor; each typed-judge adapter ships only after
a research note verifies its API from primary sources.

- **Configuration.** A private, owner-only, versioned `supervisor.json` beside
  `tools.json` with a closed schema selects evaluators, thresholds and
  responses. Project files never configure the Supervisor. Without the file,
  built-in signals run in their default mode.
- **Triggers.** Judges run at review points (before changeset integration and
  before Finish) and when a deterministic signal fires. Judging every effect
  is an explicit per-evaluator opt-in.
- **Verdicts.** Each evaluator maps probability bands to a response (default:
  ≥0.65 applies its configured narrowing response, ≤0.35 clears, between asks
  the user). A judge selects only among narrowing responses. Verdicts are
  journaled observations; replay uses the recorded verdict and never calls
  the judge again. Failure or timeout asks the user.
- **Budget.** Judge calls have their own bounded per-Goal budget; exhaustion
  asks the user instead of skipping evaluation.

1. **Worker plan contracts.** Extend the existing
   [Goal plan contract](../../docs/specs/goals.md#initial-plan-contract-and-success-receipt)
   with the primary's mission, direction and scope per worker; a worker can only
   narrow its assignment. Approval of prospective revisions follows B6 without
   widening the pinned grant.
2. **Deterministic signals.** Each signal has a fixed threshold and response:

   | Signal | Detection | Default response |
   |---|---|---|
   | Scope drift | Effect outside declared paths/commands or team lease | Require approval |
   | Verification tampering | Edits to test files, assertions, ignore/skip markers, lint or CI configuration when the plan did not declare them | Require approval |
   | False completion | Finish claims success while the latest success-criterion receipt failed or is absent | Block Finish and report |
   | Boundary probing | Repeated Guard or Policy rejections (network, excluded paths, unknown executables) | Revoke capability, then pause |
   | Loop | Same failing intent repeated, or edit/revert oscillation on the same file | Pause |
   | Disproportion | Changeset size or deletions far above the declared scope | Require approval |
   | Injection suspicion | An observation containing instructions followed by an out-of-plan effect | Pause |

   Signals start in shadow mode: they are recorded without a response until
   corpus and real Runs calibrate their thresholds. False completion and the
   Finish gate (B5) enforce from the first release because they are exact.
3. **Graded escalation.** Annotate → switch the agent to Ask → revoke the offending capability → pause and ask the user → cancel the
   agent or Goal. Escalation is monotonic within a Goal.
4. **Review-point review.** Extend the existing AI review to judge the whole
   trajectory and changeset against the task at review points (before
   integration and before Finish), using the reviewer role's roster profile
   (D2), which can differ from the worker's. Uncertainty or failure asks the
   user, as today.
5. **Worker Finish gate.** Extend the existing Goal success-receipt rule to
   worker handover in C3; C2 requires primary verification after integration.
6. **Modes.** Replace the current Auto edits / Request approvals / Auto
   approval cycle with **RPI → Ask → Accept edits → Auto**, cycled with
   Shift+Tab. No bypass mode exists in attached sessions; unattended work
   uses `arany exec` with an upfront grant. The default mode is a user
   setting; a fresh installation defaults to RPI.
   - **RPI** runs Research → Plan → Implementation and the harness drives it
     to completion, like a goal that knows how to work: it divides the work,
     waits for whoever must act (the user at the Plan gate, workers,
     escalations) and continues. Each phase is its own Run in the Session,
     linked by its approved artifact, so approval waits hold no Run deadline
     and recovery resumes at a phase boundary.
     - Research is read-only and may use a team of read-only researchers; the
       primary may skip it for a trivial task.
     - Plan is read-only and may grill the user: bounded rounds of numbered
       questions, each with a recommended answer, resolve open decisions
       before the plan. Plan presents the plan contract and proposed team;
       this is the only mandatory gate, and approval chooses the
       implementation mode (Ask, Accept edits or Auto).
     - Rejecting a plan with feedback returns to Plan without repeating
       Research; asking for more research returns to Research. Revisions
       count toward budgets.
     - The research brief and plan are bounded Session artifacts, journaled
       and visible in transcript and inspection, never repository files; the
       user can export them.
     - Extend the existing durable Goal with automatic phase advancement,
       stopping at the Plan gate, grilling questions, Supervisor escalations,
       paused profiles or cancellation. Split Implementation Runs by approved
       milestone or team wave, preserving the existing recovery boundaries.
     - The approved plan stays within the Goal's pinned budgets and resource
       settings. Plan revisions and phase transitions consume bounded capacity.
       The proposed pause-on-exhaustion behavior needs owner resolution against
       the current terminal exhaustion contract before implementation (see
       [open decisions](#open-questions)).
     - Grilling is a typed "ask the user" outcome in the Plan phase: at most
       12 questions per round and 6 rounds per Goal, each question with a
       recommendation; answers are journaled Goal facts.
     - Keep the existing Completed/Failed report, cancellation pause and
       explicit resume semantics from the Goals contract. RPI may add its own
       confirmation at a phase boundary.
   - **Ask** asks before every effect.
   - **Accept edits** integrates typed edits and in-scope changesets without
     prompts; commands and MCP calls ask.
   - **Auto** lets the Supervisor approve effects inside the plan contract;
     out-of-scope, borderline or failed evaluation asks the user. It replaces
     today's per-action AI review.

   The proposed team (roles, profiles, missions, leases, budget) needs
   approval in RPI and Ask; Accept edits and Auto start it without a prompt
   when it fits the roster preset and budgets.

### C. Coding teams

The primary composes the team from the user's request: roles, profiles (D2),
missions, direction and path leases. Use the admitted child role chain;
assignment-specific selection waits for D.

Topology is one level: workers cannot spawn workers or message each other,
and everything flows through the primary. Concurrent workers are capped by a
setting (default 4) and by each Provider's concurrency. A worker's grant is the
intersection of the Goal grant and its assignment, which names its commands,
Skills and MCP tools; workers never receive Git or network capabilities. The
worker slices grant catalog listing only; MCP invocation is excluded
([rationale](./design-tree.md#worker-ownership)).

C2 adds edit-only workers to the existing researchers; C3 adds command-capable
views. The [ownership rationale](./design-tree.md#worker-ownership) explains
the cut. Until C3, the primary verifies each worker criterion after integration
because edit-only workers cannot execute it inside a view.

**Worker assignment identity (C2).** One assignment is one child AgentRun with a
UUIDv7 ID and an ordinal. Its spawn fact pins mission, lease, grant, success
criterion and assignment base (digests of the leased paths). Reassignment is
always a new AgentRun; an AgentRun never takes a second assignment. The AgentRun
Events gain a new current payload version, with no compatibility readers.

- **C2 Edit-only workers.** Workers are writable but run no
   commands. Each worker's edits accumulate into one changeset against its
   assignment base, which it hands to the primary; it never writes to the
   Workspace directly. No Git worktrees.
   - **Leases.** A lease is fixed at assignment for its Run, does not overlap
     another live assignment, and is a subset of the Goal path scope. It is held
     until its handover is integrated or discarded, or until its AgentRun ends
     without one, and never crosses a Run boundary. A write outside the lease
     rejects at changeset validation and leaves the Workspace unchanged. Reads
     stay within the Goal grant; the Goal's Workspace writer lock already
     excludes other writers.
   - **Handover.** A handover is a journaled fact bound to its AgentRun and
     changeset digest. The primary integrates only the handover of a Finished
     worker AgentRun of an open Run (no `RunFinished`), with no cancellation
     observed, that no reassignment superseded and that was not already
     integrated or discarded. Until C3, the worker's own criterion is not part of
     this test; the primary verifies the criterion after integration. Any other
     handover is journaled as discarded with its reason, including a conflicted
     handover the primary resolves by hand (its resolution is the primary's own
     new change), and a handover whose Run has no terminal fact when the Goal
     resumes, which is never carried over. Replay rejects an integration without
     an eligible handover.
   - **Conflicts.** A failed digest precondition is a conflict: the primary
     receives both versions and resolves it itself or reassigns the work on the
     current base. No automatic text merge.
   - **Failure and loss.** A failed or cancelled worker's changeset is discarded
     and reported; the primary may issue a new assignment within budget; no
     automatic retry. A result lost inside a worker's private storage fails that
     worker only and discards it; the Workspace is not marked Uncertain.
     Integration stays the primary's `ApplyChangeset`: a missing receipt after GO
     is Uncertain and stops the Run without retry. A worker failure is not fatal
     to the Run, which replaces the execution rule that child failure drains
     siblings before primary failure.
   - **Worker storage lifetime.** The worker's private storage is disposable
     Guard-owned storage, lost on cancellation, crash or process exit and never
     recovered. A resumed Goal reports the discarded worker work; the primary may
     reassign within budget. There is no automatic restart.
   - **Integration order and bounds.** Changesets integrate in completion order,
     recorded in the journal so replay stays deterministic. Dependencies are
     waves: a later wave starts after the earlier wave integrates; there is no
     general task DAG. A changeset holds at most 2,048 files and 32 MiB;
     oversized changesets reject whole with a count report and are never split.
- **C3 Command-capable views.** Each worker gets one persistent
   private Guard view for its whole assignment: the Workspace at assignment plus
   its own work. Its commands run there, and it must pass its own success
   criterion inside the view before handover; the primary re-verifies after
   integration. The build cache is read-only for workers: they read the archive
   as of assignment and never publish; only primary commands publish.
- **C4 Reviewer role.** A read-only reviewer child evaluates each changeset
   before integration, feeding the Supervisor review point (B4).

C2 will change the current researcher-only collaboration contract. Its gates
and evidence owners are in [execution](#execution).

### D. Provider roster and per-agent routing

Named private presets now pin and admit all reachable routes for the four current roles (`primary`, `child`, `review`, `compaction`), with explicit `--profile`, bounded immutable Run metadata, same-selection Goal recovery and pinned compaction. The [Provider contract](../../docs/specs/provider.md#named-roster-presets) owns that behavior and its limits; current selected-account loaders and Tool-history adapter/account restrictions still apply.

The primary can select an eligible, already admitted profile per typed child assignment, with journaled spawn references and actor-specific call/switch validation. Cancellation, crash, budget-refusal and Guard-continuity journeys are proven through both entry paths. Children remain read-only and bounded by the parent grant; model output cannot introduce a destination or another preset.

Separately proposed D work, excluded from item 9 by decision O1: judge-only adapters and a durable per-agent pause/retry/switch lifecycle after exhausted routes. Those need accepted Events, dependency blocking, active-time and user-command contracts before implementation. Existing ordered HTTP-5xx fallback, failed-child → sibling drain → failed Run, shared budgets and explicit new-Run recovery remain authoritative. Worker/reviewer/judge taxonomy, writable workers, leases and handover belong to C2/C3 rather than this preset slice.

### E. Goal lifecycle and terminal

- **E2 Context.** Each phase Run receives the approved artifacts, Goal facts and
   its own capsule, not the full transcript; workers receive only their
   assignment capsule.
- **E4 Checkpoints and rewind.** Cancellation keeps integrated changes, as in
   other harnesses. A checkpoint is taken at every user prompt and Goal phase
   boundary; `/rewind` restores code, conversation (through the existing
   fork) or both. Pre-images are stored in Arany's private StateRoot, per
   Session, only for files changed through Arany's validated changesets
   (`ApplyChangeset`). Code rewind applies reverse changesets with digest
   preconditions; drifted files reject and are listed. Command and MCP effects
   are not rewindable, and rewind says so explicitly. Checkpoints are never Git
   ghost commits in the user's repository. Rewind is never automatic.
   Checkpoints are kept 30 days, bounded per file and per Goal, and pruned
   automatically with a visible notice; exact numbers are decided in the E4
   implementation change.
- **E5 Supervisor override.** The user can override any Supervisor response
   with a one-use approval; overrides are journaled, feed threshold
   calibration and never change configuration.
- **E6 Git writes.** Commit, branch and push run the user's host `git` with
   fixed argument arrays, after Arany admits `.git/config` against a closed
   allowlist of inert keys; any other key rejects with an explanation. Hooks
   never run on the host (`--no-verify`); a per-folder setting can run the
   declared hook scripts as Guard commands before the write. A dedicated
   security pass precedes this slice.
   - The model commits on its own only when the project rules ask for it and
     the user enabled "project rules may authorize commits" for that trusted
     folder; otherwise only on an explicit user request. Project rules always
     guide conventions. Trusting a hostile repository is the user's decision.
   - Push, branch deletion and anything touching a remote always need human
     approval, even in Auto.
   - Force-push, history rewriting and similar disruptive operations are
     disabled; a global setting can enable them, and each one still needs
     human approval in every mode, showing the exact remote, branch and lost
     commits.
   - The final report always includes a change summary and a suggested
     commit message.
- **E7 Goal view.** The terminal tells the story of the Goal instead of showing
   a checklist (checklists spend tokens without adding much): what the team
   is doing now, agents with their profiles, occasional progress messages
   from each agent, budget use, Supervisor events and pending questions as a
   picker. The [terminal contract](../../docs/specs/terminal.md) owns it.
   - Activity lines are composed by the Engine from journaled events and
     cost no tokens. Each agent outcome may carry a progress note of at most
     160 characters; the primary adds a one-paragraph narration at phase
     transitions.
   - Each agent shows one live line with its latest note, replaced as new
     notes arrive; the full history stays in agent inspection.
   - Usage is shown as tokens and calls per profile, without cost estimates.
   - Linear and screen-reader presentations carry the same facts as labeled
     lines, as the terminal contract already requires for Runs.
- **E8 Phase-artifact compaction.** Extend the current Goal-context reinjection
   with approved RPI artifacts and grilling answers. Keep requests, decisions,
   touched files and errors in the summary; drop raw Tool output.
- **E9 Memory.** Each Goal starts from Session history and the repository;
   cross-Session Memory stays an expansion decision.
- **E10 Headless use.** `arany exec` stays as the scriptable product surface
   (CI, scripts, API-style integration). It runs RPI only with an explicit
   flag that approves the plan within the upfront grant and budgets; grilling
   questions resolve to their recommended answers, recorded as assumptions in
   the final report. Without the flag it goes straight to Implementation. Goal
   progress and the final report use the existing JSONL channel.
- **E12 Settings.** Extend the existing private, owner-only, versioned
    user preferences through `/settings`; per-folder values live with
    the folder-trust record; project files never set them. `tools.json`,
    `supervisor.json` and the roster stay separate private files.
    - Per folder: hook execution, commit authorization, toolchain profiles.
    - Global: default mode, timeouts and maxima, worker cap, roster
      selection and disruptive Git operations. Existing usage and timeout
      settings retain their current contract.
- **E13 Telemetry.** Extend opt-in OTLP for new phase, worker and Supervisor
    facts under the existing [telemetry spec](../../docs/specs/telemetry.md);
    current Goal/Agent spans already have that owner.

## OS impact

C2/C3 need equivalent disposable worker storage, isolation, cleanup and lease
validation on Linux and macOS. C3 adds offline commands and read-only cache
mounts in those views. A6 needs network-namespace routing to the proxy on Linux
and proxy-only Seatbelt rules on macOS. Unsupported enforcement fails closed;
[remaining native checks](../../NEXT_STEPS.md#beta-2-and-broader-release) precede
support claims. Native Windows is out of scope; WSL2 needs its own evidence.

## Security and resource model

Each slice that crosses one of these boundaries runs the
[security pass](../../agents/security.md#security-pass-triggers) before merge.

- **Registry fetch (A6).** The only network path from a Tool: allowlisted
  public registries through the Guard-owned proxy, with SSRF rejection,
  install scripts disabled and no credentials.
- **Git writes (E6).** Proposed host writes require closed allowlist admission
  of `.git/config` with hooks disabled. Existing typed reads remain owned by
  the Tools contract; this plan grants no host write path.
- **Providers and judges (D, B).** Every roster profile and judge is an egress
  destination with its own credential, usage consent and disclosure scope,
  admitted before Workspace disclosure. Multiple credentials stay inside the
  Engine and are never passed to Guard payloads.
- **Authority.** The Supervisor, judges and project rules can only narrow,
  except the explicit per-folder commit authorization and global
  disruptive-Git setting, which the user owns.
- **Bounds.** Changesets, checkpoints (30-day retention, per-file and per-Goal bounds), progress
  notes (160 characters), grilling (12 questions × 6 rounds), judge budgets,
  concurrent workers (default 4) and settings maxima are declared with
  overflow behavior. User-chosen unlimited usage applies only to Goal usage
  dimensions; per-request Provider caps and Guard resource ceilings stay
  finite, as [AGENTS.md rule 4](../../AGENTS.md#global-rules) already requires.

## Execution

[Next steps](../../NEXT_STEPS.md#pursue-in-order) sets priority and freezes;
these are the remaining slice boundaries and dependencies. Each slice updates
its owning contract and uses existing evidence under [testing rules](../../agents/testing.md).

- **C2 edit-only workers (roadmap 11).** Build on C1 and the shared Tool step.
  Add worker assignment pins, leases, handover eligibility, conflicts, loss,
  storage lifetime, waves and completion-order integration. Update Execution,
  Goals and Tools. Use the [Tool journeys](../../crates/arany-cli/tests/session_run/tools.rs)
  for out-of-lease rejection, post-GO Uncertain and digest conflict;
  [Goal corpus](../../crates/arany/tests/engine_run/goals.rs) for scope, writer
  exclusion and resume reporting discarded work;
  [reducer corpus](../../crates/arany/src/session/reducer/tests.rs) for spawn
  pins, lease overlap/lifetime and ineligible integration; and
  [Engine corpus](../../crates/arany/tests/engine_run.rs) for local worker loss
  and cancellation. Keep worker commands, MCP invocation, cache publication
  and in-view criteria out until their slice. Child write encodings need the
  owner-run [live coding check](../../NEXT_STEPS.md#beta-1-final-user-owned-checks).
- **C3 command-capable views (roadmap 12).** After C2, add persistent private
  assignment views, worker commands, the in-view success criterion and read-only
  assignment-time build cache. Tools owns cache behavior; Tool journeys cover
  worker cache outcomes and the Engine corpus covers cancellation/disposal.
  MCP invocation and worker cache publication remain excluded.
- **D roster (roadmap 9).** Presets and assignment selection have
  cancellation, crash and Guard-continuity evidence through both entry paths.
  Provider and credential contracts own admission. Judge-only adapters and
  paused-agent recovery are separate work requiring accepted contracts and
  primary-source API research.
- **B6/E7/E12 modes and RPI (roadmap 6).** Extend permission modes and hook
  wiring with phase Runs, grilling, plan/team approval, Goal presentation and
  settings. Goals, Tools and Terminal own approval, recovery, bounds and
  accessible presentation. Verify rejected/revised plans, phase cancellation,
  explicit resume and bounded headless RPI.
- **B2–B5/E5/C4 Supervisor and reviewer (roadmap 12).** Implement configurable
  evaluators as narrowing hook adapters, shadow-mode signals, escalation,
  worker Finish gate, one-use overrides and read-only changeset review. Keep
  thresholds data-dependent; verify detection, journaled verdicts, timeout,
  budget exhaustion and replay without another judge call.
- **E2/E4/E8/E10 remaining Goal workflows.** Add phase capsules/artifacts,
  checkpoint/rewind and headless RPI. Checkpoints remain frozen under the
  roadmap. Goals, Tools and Terminal own drift rejection, retained integrated
  changes, non-rewindable effects and explicit recovery.
- **E6 Git writes.** A separate security-reviewed slice after the freeze;
  verify inert repository configuration, exact approval, protected remote
  operations and no authority from repository text alone.
- **A6 registry fetch.** Remains frozen. Tools owns the two-phase proxy grant;
  verify public-registry success followed by offline build, SSRF/host refusal,
  disabled install scripts and byte/connection/time overflow.

Next action: follow the roadmap prerequisites, then implement C2. Dogfooding is
owner-run and supplies Supervisor calibration; pending live/native checks do
not block independent implementation. E13 telemetry lands with each new
subject under the existing telemetry contract.

## Open questions

Owner: Fernando Mirabile. [Roadmap item 6](../../NEXT_STEPS.md#pursue-in-order)
owns RPI exhaustion and Auto authority decisions; item 12 owns Supervisor
threshold calibration. The [E4 freeze entry](../../NEXT_STEPS.md#freeze-until-item-11-lands)
owns checkpoint quotas. Resolve each before its corresponding slice.
The [design rationale](./design-tree.md) explains the choices that remain live.

## Contract owners

- [Goals](../../docs/specs/goals.md): lifecycle, phase artifacts, budgets, checkpoints and Goal facts.
- [Execution](../../docs/specs/execution.md): Run admission, coordination, teams and recovery.
- [Tools](../../docs/specs/tools.md): grants, effects, changesets, Guard and native differences.
- [Terminal](../../docs/specs/terminal.md): interaction, Supervisor prompts and accessibility.
- [Credentials](../../docs/specs/credentials.md): account scope, storage, recovery and roster accounts.
- [Provider spec](../../docs/specs/provider.md): profile admission, capability classes and conformance.
- [Diagnostics](../../docs/specs/diagnostics.md): bounded private failure inspection.
- [System overview](../../docs/architecture/system-overview.md): ownership and data flow.

## Exit criteria

- A team of a primary and writable workers completes a scripted coding task
  (edit, format, build, test, integrate) on Linux and macOS with closed replay.
- An RPI Goal runs Research → Plan (with grilling) → approved multi-Run
  Implementation to Completed, and to Failed with a final report; Ctrl+C
  pauses it until `/goal resume`, and restart resumes only through the
  explicit user action in the Goal contract.
- One Goal mixes at least two Providers across roles; an unadmittable or
  failing profile names itself, pauses its agents or switches within the
  admitted fallback boundary, without re-dispatching uncertain effects.
- Teams beat or match a single-agent baseline on a local task corpus
  (success, usage, elapsed time) before any claim that teams help.
- Each Supervisor signal has a corpus case proving detection, the narrowing
  response and replay equality; no signal ever widens authority.
- A fetch adds a dependency through the proxy on Linux and macOS, then builds
  offline; non-allowlisted hosts, private addresses and install-script
  execution during fetch reject.
- Rejected, oversized, conflicting and cancelled changesets leave the Workspace
  unchanged or in a recorded uncertain state.
- Unperformed native, live and user checks are reported without claiming
  support. Accepted behavior moves to specs and tests, and completed
  execution work is removed from this plan.
