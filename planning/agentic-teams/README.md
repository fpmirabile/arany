# Agentic coding teams

## Metadata

- Status: `In progress`
- Owner: Fernando Mirabile
- Last updated: 2026-10-16

## Outcome

Arany is a harness specialized in multi-task agentic teams. A team must be able
to do ordinary coding work end to end: inspect, edit, build, test and integrate
changes inside a project, while the harness detects agent misbehavior and
narrows authority before it causes harm.

This initiative is part of Beta 1 and is its remaining implementation work. The
[beta plan](../arany-beta/README.md) tracks the milestone as a whole, including
the user-owned checks in [next steps](../../NEXT_STEPS.md).

## Using this plan

- This file is authoritative and self-contained: every accepted decision is
  written here. The [design tree](./design-tree.md) only records the rationale
  and the user's answers; read it when a decision's reason matters, and never
  treat it as a second source of requirements.
- Follow the repository workflow in [AGENTS.md](../../AGENTS.md): load the
  triggered rules, apply Ponytail, and specify each slice in its owning
  contract under the [spec workflow](../../docs/specs/README.md) before
  implementation.
- Work the [execution](#execution) list in order. When a slice starts, set
  Status to `In progress`; when it lands, move its accepted behavior to the
  owning spec, remove it from this plan and update the next action, as the
  [planning lifecycle](../README.md#lifecycle) requires.
- A new material decision goes to the user; record it in the design tree and
  apply it here in the same change.

## Terms

- **Goal.** A durable, Engine-owned unit of user intent that spans one or more
  Runs and ends Completed or Failed with a final report (E11). Cancellation
  (Ctrl+C) pauses it as Interrupted until `/goal resume`; `/pause` pauses it at
  its next step.
- **RPI.** Research → Plan → Implementation, the default mode (B6).
- **Plan contract.** An agent's declared scope (paths, command names), success
  criterion and budget; the anchor for every Supervisor signal (B1).
- **Plan gate.** The user's approval of the plan contract and proposed team in
  RPI; it chooses the implementation mode (B6).
- **Finish gate.** No agent reports success without a successful receipt for
  its success criterion (B5).
- **Changeset.** A bounded set of created, modified and deleted regular files
  that the Engine validates before publishing to the Workspace ([command changesets](../../docs/specs/tools.md#command-changesets)).
- **Review point.** A Supervisor evaluation moment: before changeset
  integration and before Finish (B, B4).
- **Checkpoint.** A rewind point taken at every user prompt and Goal phase
  boundary (E4). Distinct from a review point.
- **Supervisor.** Engine logic that evaluates journaled evidence through
  configured evaluators and can only narrow authority (B).
- **Judge.** A roster profile of class `judge` that returns verdicts for the
  Supervisor (B, D1).
- **Roster, profile, preset.** A profile is an account, model and effort; the
  roster lists profiles; a preset is a named set of role bindings chosen at
  start (D).
- **Lease.** The file or directory prefixes a worker may write (C2).
- **Wave.** A group of workers that starts after the previous group integrates
  (C2).
- **Steering note.** A user message delivered to a running Goal's primary at
  its next call boundary (E1, landed).

## Current facts

- Children are direct, read-only, one-call reasoning tasks without Tools; only
  the primary mutates files or runs commands
  ([execution](../../docs/specs/execution.md), decision
  [D-03](../../docs/research/next-step-decision-register.md#d-03--generic-bounded-agent-protocol)).
  This is a fan-out of advisors, not a coding team.
- Commands return bounded changesets from a private offline snapshot; a separate
  validated mutation publishes them to the Workspace. A private per-Workspace
  StateRoot archive reuses bounded build outputs and dependency data inside each
  command's quota-backed scratch view. Explicitly selected host registry caches
  remain read-only and separate from Arany's archive
  ([Tools](../../docs/specs/tools.md#persistent-workspace-build-cache)).
- Folder trust pins the system `sh` and can explicitly select exact Rust, Node
  and Python runtime roots or known host-cache subtrees. Linux uses private
  mount propagation plus bounded namespace mount-table checks; macOS uses exact
  Seatbelt grants and native mount-table checks. The owning Tools spec records
  disclosure, exclusions and the remaining native evidence.
- Fixed ceilings block real builds: 60 seconds per unit, 300 seconds per Run,
  16 Tool effects per Run, 512 MiB memory, 64 MiB scratch and writable
  project storage, 2,048-entry snapshots and 8 KiB edit strings
  ([limits](../../docs/specs/tools.md#limits-and-failure-behavior)).
- A Run pins one ProviderProfile: the primary and every child share one
  account, model and effort
  ([system overview](../../docs/architecture/system-overview.md)). Mixed
  saved access is chosen on every new start
  ([D-20](../../docs/research/next-step-decision-register.md#d-20--mixed-saved-access-is-chosen-on-every-new-start)).
- `.git` is excluded from every grant, so agents cannot inspect history or diffs.
- Approval exists as human Allow once/Deny and per-action AI review. Nothing
  evaluates the trajectory of a Run against what the agent was asked to do.
- Detection is not enforcement: observers and classifiers run after the OS
  allowed an effect ([deterministic protection §3.3](../../docs/research/deterministic-harness-protection.md#33-detection-and-observability-are-not-enforcement)).
  Guard stays the effect boundary.

## Chosen direction

Five workstreams (A–E). Sections are grouped by subject; the
[execution](#execution) list sets the delivery order, and each slice leaves the
harness coherent and safer than before. Every slice keeps the existing invariants: one router mediates
effects, Policy only narrows, Guard enforces, replay never re-executes, and
unsupported enforcement fails closed. Child authority remains
`global policy ∩ parent effective authority ∩ explicit task grant`
([security rules](../../agents/security.md)).

### A. Coding foundation

3. **Flexible limits.** Before A4, the ceilings were compile-time constants
   (`MAX_TOOL_CALLS = 16` and a 64 KiB Tool context in
   [src/tools/types.rs](../../src/tools/types.rs), plus the 300-second Run).
   Follow the established harness pattern instead of inventing one:
   - These caps stop being product constants. A Run's loop continues until
     Finish, bounded by context compaction, the Goal budget and
     cancellation. Tool observations use the model's admitted context with
     compaction instead of a fixed 64 KiB.
   - Commands default to 120 seconds and may request up to the approved
     user-configurable maximum of 600 seconds.
   - The plan contract may declare a Build profile and command timeout within
     the saved Goal settings. The Engine binds the exact selected resource
     values into each immutable Tool intent and receipt. A request above the
     pinned timeout maximum gets a durable non-dispatched `Limit` with clear
     guidance to lower the request or change `/settings`; approval never
     exceeds a Goal's pinned settings. Prospective approved revisions belong
     to the later RPI/modes slice.
   - Runs remain the durable recovery unit, closed at milestone or wave
     boundaries, never because an arbitrary cap was hit.
   - Named, closed resource profiles (current `default` plus a larger
     `build`) remain bound into intents and receipts. The `build` profile uses
     a Linux CPU quota of 100% of one core; macOS retains per-process CPU
     supervision with its threshold derived from the admitted command timeout,
     up to 600 seconds, and makes no aggregate CPU-rate claim. Memory,
     storage, task and wall-time ceilings stay finite.
4. **Typed Git reads.** Read-only `status`, `diff` and `log` operations run the
   system `git` CLI as a read-only command inside the Guard and parse its
   output. No Git library dependency. Repository configuration, hooks, filters,
   textconv and external diff programs stay inert; the exact neutralizing
   arguments are part of the A5 security pass. `.git` stays excluded from file
   grants, so history is reachable only through these typed operations. Git is
   required on the host: without it the capability reports as unavailable and
   fails closed, never emulated. Commits stay with the user.
5. **Registry fetch.** Lets an agent add dependencies to the project it works
   on (`npm install zod`, `cargo add serde`, `pip install requests`) without
   giving project code the network.
   - **Two phases.** A separate fetch phase reaches only an allowlist of
     public registries through a Guard-owned proxy and writes only into the
     persistent cache (A3); lockfile and manifest changes return as a
     changeset (A1). Build and test phases stay offline.
   - **No project code with network.** Fetch runs only the fixed fetch
     command of the active toolchain profile (A2) with install scripts
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

1. **Plan contract.** The primary declares each worker's mission, direction
   and scope in its assignment; a worker can only narrow it. Before its plan,
   an agent may use bounded Workspace List/Read/Search, an admitted Skill read,
   and configured bounded MCP catalog listing to inspect the task. Before any
   other Tool, a bounded contract covers paths, command names, exact Skill
   resources and MCP tools, a typed success criterion and budget, without an
   extra model call. Catalog listing discovers names only; every MCP tool
   invocation requires a contract even if the catalog labels it read-only. A
   contract may accompany pre-plan inspection and is always validated and
   journaled. A task limited to read-only inspection can finish without
   inventing effect scope. Contract changes require approval under the current
   approval mode.
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
5. **Finish gate.** An agent cannot report success unless its declared success
   criterion has a matching successful receipt.
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
     - A durable, Engine-owned **Goal** spans the phase Runs. The Engine
       advances phases automatically and stops only at the Plan gate,
       grilling questions, Supervisor escalations, paused profiles or
       cancellation. Implementation spans several Runs split by plan
       milestone or team wave, each Run boundary a durable recovery point.
     - The approved plan sets a Goal budget (calls, tokens, time, plan
       revisions, implementation Runs); exhaustion pauses and asks. Settings
       can make usage dimensions (tokens, calls, time) unlimited; Arany then
       shows running usage, and per-request Provider caps and Guard resource
       ceilings remain. The slice that lands this adds the exception to
       [AGENTS.md rule 4](../../AGENTS.md#global-rules).
     - Grilling is a typed "ask the user" outcome in the Plan phase: at most
       12 questions per round and 6 rounds per Goal, each question with a
       recommendation; answers are journaled Goal facts.
     - A Goal ends Completed when the Finish gate passes and a final report
       (changes, evidence, unresolved items) is committed; otherwise it ends
       Failed with the same report. Cancellation (Ctrl+C) pauses it as
       Interrupted until `/goal resume`.
     - After a crash or restart, an unfinished Goal resumes at its last phase
       or Run boundary only through an explicit user action: `/goal resume` or
       `--resume-goal`, with no further confirmation prompt in the Foundation
       contract. An RPI flow may add its own confirmation step.
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
missions, direction and path leases. Until D lands, every worker shares the
primary's Provider profile.

Topology is one level: workers cannot spawn workers or message each other,
and everything flows through the primary. Concurrent workers are capped by a
setting (default 4) and by each Provider's concurrency. A worker's grant is the
intersection of the Goal grant and its assignment, which names its commands,
Skills and MCP tools; workers never receive Git or network capabilities. The
first C slices grant workers catalog listing only; MCP invocation is excluded
(R14-7).

Delivery is staged (R14-8, which revisits Q1): read-only researchers, then
edit-only workers, then command-capable views. The rationale is in
[Round 14](./design-tree.md#round-14--settled-2026-10-16). Until C3 lands, C1
and C2 deviate from R11-1 and R11-5: edit-only workers cannot run their success
criterion inside a view, so the primary verifies each criterion after
integration, and the handover rule below omits the criterion condition.

**Assignment identity (from C1).** One assignment is one child AgentRun with a
UUIDv7 ID and an ordinal. Its spawn fact pins mission, lease, grant, success
criterion and assignment base (digests of the leased paths). Reassignment is
always a new AgentRun; an AgentRun never takes a second assignment. The AgentRun
Events gain a new current payload version, with no compatibility readers.

1. **Read-only researchers (stage 1, C1).** Children may receive a read-only
   Tool grant: Workspace List/Read/Search, admitted Skill reads and MCP catalog
   listing. They write nothing, run no command and return a bounded report to
   the primary. The Goal plan contract, delivered in Goal Foundation, scopes
   child Tool calls.
2. **Edit-only workers (stage 2, C2).** Workers are writable but run no
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
3. **Command-capable views (stage 3, C3).** Each worker gets one persistent
   private Guard view for its whole assignment: the Workspace at assignment plus
   its own work. Its commands run there, and it must pass its own success
   criterion inside the view before handover; the primary re-verifies after
   integration. The build cache is read-only for workers: they read the archive
   as of assignment and never publish; only primary commands publish. With C3,
   C again matches R11-1 and R11-5.
4. **Reviewer role.** A read-only reviewer child evaluates each changeset
   before integration, feeding the Supervisor review point (B4).

This replaces decision D-03 for coding Runs. Stage-specific gates, owners and
exclusions are in the [execution](#execution) list.

### D. Provider roster and per-agent routing

Providers are interchangeable per agent. One Run can combine, for example, a
ChatGPT subscription, an Anthropic API key and a typed-judge Provider, using
any admitted model of each in any role.

1. **Roster.** The user keeps a private roster of admitted profiles (account,
   model and effort), each with a capability class: `agent` (Finish, Delegate
   and Tool outcomes) or `judge` (typed verdicts only). Judge-only Providers
   never act as agents. Per-profile Tool capability still follows
   [Provider rules](../../agents/provider.md); exact custom profiles stay
   read-only until tool-capable conformance exists.
   The roster is a private, owner-only file outside the Workspace.
2. **Role binding.** The user binds a default profile per role (primary,
   worker, reviewer, judge, compaction). The primary composes the team from
   the user's request and may pick a profile per assignment only among roster
   entries the user marked eligible for that role. Model output can never add
   a profile or reach an ineligible one.
3. **Admission.** At start the user picks a named roster preset in one
   explicit choice listing every route and its billing; this replaces today's
   single mixed-access choice. Every profile in the preset is admitted
   (credential, usage consent, conformance) before Workspace disclosure. An
   unadmittable profile rejects the start and names the failing profile.
   `RunStarted` and each AgentRun record the exact profile provenance.
4. **Failure and fallback.** By default a failing profile is never replaced
   automatically: Arany tells the user which profile failed and pauses the
   agents bound to it until the user acts. The user can enable a setting
   naming a fallback profile (a default reasoning model) that replaces a
   failing profile with a visible, journaled switch. There is one fallback
   profile per capability class (agent, judge), admitted at start like any
   other. Fallback applies only to Provider-call failure (credential,
   unavailable, rate limit, transport, invalid outcome); the notice states
   that the failed call may have consumed usage. Tool uncertainty rules are
   unchanged: fallback never re-dispatches an uncertain effect.
5. **Pause.** Without fallback, agents bound to a failed profile pause at their
   next call boundary; other agents continue until they need a paused result.
   The user can retry, switch to an eligible profile or cancel. Paused time
   does not count against the Goal time budget; in-flight Guard units keep
   their own deadlines.

This changes D-04, D-20 and the execution contract's no-fallback rule when its
contract is accepted; the [credential](../../docs/specs/credentials.md) and
execution contracts own the resulting behavior.

### E. Goal lifecycle and terminal

1. **Steering.** While a Goal works, user messages become steering notes
   delivered to the primary at its next call boundary; they never cancel.
   Escape/Ctrl+C keep cancellation; `/pause` pauses at the next boundary.
2. **Context.** Each phase Run receives the approved artifacts, Goal facts and
   its own capsule, not the full transcript; workers receive only their
   assignment capsule.
3. **Workspace lock.** One active writing Goal per Workspace across Sessions
   and processes; read-only work is unrestricted.
4. **Checkpoints and rewind.** Cancellation keeps integrated changes, as in
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
5. **Supervisor override.** The user can override any Supervisor response
   with a one-use approval; overrides are journaled, feed threshold
   calibration and never change configuration.
6. **Git writes.** Commit, branch and push run the user's host `git` with
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
7. **Goal view.** The terminal tells the story of the Goal instead of showing
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
8. **Compaction.** After compaction Arany re-injects project instructions,
   the approved plan, Goal facts and grilling answers. The summary keeps
   requests, decisions, touched files and errors; raw Tool outputs are
   dropped.
9. **Memory.** Each Goal starts from Session history and the repository;
   cross-Session Memory stays an expansion decision.
10. **Headless use.** `arany exec` stays as the scriptable product surface
   (CI, scripts, API-style integration). It runs RPI only with an explicit
   flag that approves the plan within the upfront grant and budgets; grilling
   questions resolve to their recommended answers, recorded as assumptions in
   the final report. Without the flag it goes straight to Implementation. Goal
   progress and the final report use the existing JSONL channel.
11. **Every task is a Goal.** RPI adds Research and Plan; Ask, Accept edits and
    Auto go straight to Implementation. In every mode, bounded read-only
    inspection may precede the plan contract; every other Tool requires it
    before dispatch. A Session holds at most one active Goal. Steering a running
    Goal is deferred beyond the Foundation slice; until then a prompt while it
    runs is not a steering note, and the next prompt after it ends starts a new
    Goal.
12. **Settings.** New settings live in one private, owner-only, versioned
    user settings file edited through `/settings`; per-folder values live with
    the folder-trust record; project files never set them. `tools.json`,
    `supervisor.json` and the roster stay separate private files.
    - Per folder: hook execution, commit authorization, toolchain profiles.
    - Global: default mode, timeouts and maxima, worker cap, fallback
      profiles, unlimited usage, disruptive Git operations.
13. **Telemetry.** Opt-in OTLP gains Goal, phase, agent and Supervisor spans
    under the existing [telemetry rules](../../agents/telemetry.md): no
    content, bounded attributes.

## OS impact

- A4 needs native Linux and macOS resource enforcement and evidence. The
  approved configurable ceilings do not establish build support.
- A2's exact runtime roots, private Linux mount bootstrap, namespace topology
  checks, macOS Seatbelt grants and host-cache disclosure rules are owned by the
  [Tools spec](../../docs/specs/tools.md).
- A6 needs network-namespace routing to the proxy on Linux and proxy-only
  Seatbelt network rules on macOS.
- Native Windows is out of scope; Windows users run the Linux build under
  WSL2, whose support needs its own evidence.

## Security and resource model

Each slice that crosses one of these boundaries runs the
[security pass](../../agents/security.md#security-pass-triggers) before merge.

- **Runtime mounts and cache (A2, A3).** Runtime and cache grants never include
  parent configuration or credential paths and never overlap Workspace,
  StateRoot or account roots. Explicitly selected host cache contents may be
  private data including embedded credential URLs, can be emitted through
  bounded Tool output and can reach a later Provider request; the consent
  screen warns and discovery never reads the contents. Arany's writable cache is separately quota-bounded and carries
  no authority.
- **Registry fetch (A6).** The only network path from a Tool: allowlisted
  public registries through the Guard-owned proxy, with SSRF rejection,
  install scripts disabled and no credentials.
- **Git (A5, E6).** Reads run the system `git` inside Guard with repository
  configuration, hooks, filters and external diff programs neutralized; without
  `git` the capability is unavailable. Writes run host `git` only after closed
  allowlist admission of `.git/config`, with hooks disabled on the host.
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
  finite, and [AGENTS.md rule 4](../../AGENTS.md#global-rules) gains this
  exception when the slice lands.

## Execution

Each item is one PR with its own contract update, security pass where
triggered and evidence under [testing rules](../../agents/testing.md).

4. **A4 Flexible limits.** Replace the Run's fixed tool/model/context ceilings
   with Finish, compaction, cancellation and Goal-budget bounds; add the
   agreed command timeout and finite build resource profile settings. Guard
   resource ceilings remain finite. This slice builds on the landed
   [Goal Foundation](../../docs/specs/goals.md) and must not make the current
   fixed Run loop unlimited by itself. Landed in #32.
5. **Dogfooding milestone: Arany develops Arany.** Use Arany for this
   repository's own work from here on. Requires Beta 1 check 7 (live
   validation of the chosen model's strict Tool encoding) in
   [next steps](../../NEXT_STEPS.md#beta-1-final-user-owned-checks). The user
   runs it later; it runs alongside implementation and does not block C1 or
   later slices (user directive 2026-10-16). Once it starts, every later slice
   is exercised through real use, which supplies the shadow-mode calibration
   data B2 needs.
6. **C1 Read-only researchers (stage 1 of C, R14-8 (b)).** Child AgentRun
   identity and pins, read-only grants, bounded reports, catalog listing only,
   and every child on the primary's profile. Gate: the Goal plan contract
   (landed); Round 14 rules R14-1 and R14-7 land in the execution and Tools
   specs with this stage. Beta 1 check 7 is run by the user later. This stage may
   be implemented before it, and its new child Tool encodings stay marked
   unvalidated live until check 7 passes. Evidence: [Engine Run corpus](../../tests/engine_run.rs)
   for live admission and for a worker `McpCall` rejected before dispatch
   (R14-1, R14-7); [reducer replay corpus](../../src/session/reducer.rs) for
   spawn pins (R14-1). Stays out: writes, commands, MCP invocation and build
   cache publication.
7. **C2 Edit-only workers (stage 2 of C).** Leases, handover eligibility,
   conflicts, loss and worker storage lifetime, waves and completion-order
   integration, as specified in C2. Gate: C1 landed; Round 14 rules R14-2 to
   R14-5 land in the execution, Goals and Tools specs with this stage; the new
   child write Tool encodings follow the same check 7 rule as C1. Evidence:
   [Tool journeys](../../tests/session_run/tools.rs) for out-of-lease rejection
   with the Workspace unchanged, post-GO Uncertain and digest conflict
   (R14-3, R14-5); [Goal corpus](../../tests/engine_run/goals.rs) for the
   Goal-scope subset, writer lock and resume reporting discarded work (R14-3,
   R14-4); [reducer replay corpus](../../src/session/reducer.rs) for overlapping
   live leases, lease lifetime and integration without an eligible handover
   (R14-2, R14-3); [Engine Run corpus](../../tests/engine_run.rs) for a
   worker-local loss failing only that worker and for cancellation discarding
   unintegrated worker work (R14-4, R14-5). Stays out: command execution in
   workers, build cache publication, MCP invocation, and the criterion inside a
   view (deviation until C3).
8. **C3 Command-capable views (stage 3 of C).** The persistent per-worker Guard
   view, worker commands in it, the own-criterion handover condition and the
   read-only build cache for workers (R14-4, R11-1, R11-5, R14-6). Gate: C2
   landed; A4's command timeout and Build profile in place; R14-6 lands in the
   Tools build-cache section with this stage. Evidence: [Tool journeys](../../tests/session_run/tools.rs)
   for the worker command's cache outcome (R14-6); [Engine Run corpus](../../tests/engine_run.rs)
   for cancellation discarding views (R14-4). Stays out: MCP invocation (R14-7)
   and worker cache publication.
9. **Goal completion.** E2–E4 context reinjection, Workspace
   checkpoints and rewind, E8 compaction and E10 `exec` integration. This
   extends Goal Foundation with the remaining durable workflows and UI.
10. B6 modes including RPI and grilling, E7 Goal view and E12 settings.
11. B2, B3, B5 and E5 Supervisor signals, escalation, Finish gate and override.
12. A5 Typed Git reads and E6 Git writes.
13. A6 Registry fetch.
14. D1–D5 Provider roster and per-agent routing.
15. Configurable judges over the roster (agent-capable judges first, then a
    typed-judge adapter after its API research note).
16. B4 and C4 Review-point review and reviewer role.

Observe during dogfooding, without changing the plan unless evidence demands
it: whether RPI's Plan gate adds friction to small tasks, and whether the
per-slice specification and evidence cost slows delivery.

E13 telemetry spans land with the slice that introduces each span's subject.

Next action: land C1 read-only researchers. Dogfooding
starts once the user runs Beta 1 check 7. The A3 per-Workspace
cache retains its 64 MiB default and shares an 8 GiB StateRoot aggregate; A4
derives the effective admitted cache quota from remaining aggregate headroom,
so the 8 GiB requested maximum is not guaranteed usable capacity. Approved
Build maxima are 4 GiB memory, 8 GiB requested cache, 1 GiB writable project
storage and 256 processes/threads. These are finite policy ceilings, not
support evidence. Native build verification and named-host measurements remain
required before support claims.

## Open questions

The [design tree](./design-tree.md) records every grilling round.
[Round 14](./design-tree.md#round-14--settled-2026-10-16) records the C
ownership rules and delivery order; the remaining questions are data-dependent:

- Default thresholds for loop, probing and disproportion signals. Decided from
  shadow-mode corpus and real Runs before those signals enforce.
- Native build-resource enforcement and measured builds on named hosts. The
  approved finite ceilings do not prove a build works or establish support on
  a named host.

## Contract owners

- `docs/specs/goals.md` (created by Goal Foundation): Goal lifecycle, RPI, steering, checkpoints, budgets and Goal view facts.
- [Execution](../../docs/specs/execution.md): Run admission, coordination, teams and recovery.
- [Tools](../../docs/specs/tools.md): grants, effects, changesets, Guard and native differences.
- [Terminal](../../docs/specs/terminal.md): interaction, Supervisor prompts and accessibility.
- [Credentials](../../docs/specs/credentials.md): account scope, storage, recovery and roster accounts.
- [Provider rules](../../agents/provider.md): profile admission, capability classes and conformance.
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
  failing profile names itself, pauses its agents or switches to the enabled
  fallback, without re-dispatching uncertain effects.
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
  support. Accepted behavior moves to specs and module rules, and completed
  execution work is removed from this plan.
