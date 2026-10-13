# Agentic coding teams

## Metadata

- Status: `In progress`
- Owner: Fernando Mirabile
- Last updated: 2026-10-13

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
  Runs and ends Completed, Failed or Cancelled with a final report (E11).
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
  (C3).
- **Steering note.** A user message delivered to a running Goal's primary at
  its next call boundary (E1).

## Current facts

- Children are direct, read-only, one-call reasoning tasks without Tools; only
  the primary mutates files or runs commands
  ([execution](../../docs/specs/execution.md), decision
  [D-03](../../docs/research/next-step-decision-register.md#d-03--generic-bounded-agent-protocol)).
  This is a fan-out of advisors, not a coding team.
- Commands return bounded changesets from a private offline snapshot; a separate
  validated mutation publishes them to the Workspace. Scratch data and build
  caches do not survive calls
  ([Tools](../../docs/specs/tools.md#command-changesets)).
- Folder trust pins only the system `sh`; home-installed toolchains need manual
  executable pins, and the Linux runtime mount is `/usr` only.
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

2. **Toolchain runtime profiles.** Named read-only runtime profiles (`rust`,
   `node`, `python`) mount exact toolchain subtrees as declared runtime, like
   `/usr` today, for example `~/.rustup/toolchains/<pinned>` and registry
   caches. Whole home or tool directories are never mounted: `~/.cargo` holds
   `credentials.toml`, and `~/.npmrc`/`~/.pypirc` hold tokens. The profile pins
   toolchain identity (version, path and directory identity) rather than every
   file digest; identity drift rejects dispatch. Folder trust can enable a
   detected profile on the same consent screen.
3. **Persistent build cache.** A private per-Workspace cache directory under
   StateRoot, outside the Workspace, is mounted writable for build outputs and
   dependency caches. It never carries credentials, is bounded by quota, and
   the user can clear it. Its contents are build data, never authority.
4. **Flexible limits.** Today's ceilings are compile-time constants
   (`MAX_TOOL_CALLS = 16` and a 64 KiB Tool context in
   [src/tools/types.rs](../../src/tools/types.rs), plus the 300-second Run).
   Follow the established harness pattern instead of inventing one:
   - These caps stop being product constants. A Run's loop continues until
     Finish, bounded by context compaction, the Goal budget and
     cancellation. Tool observations use the model's admitted context with
     compaction instead of a fixed 64 KiB.
   - Commands default to a 2-minute timeout and may request up to a
     user-configurable maximum (default 10 minutes).
   - The plan contract may request a larger resource profile or timeout;
     Plan-gate approval grants it. In Accept edits and Auto, requests within
     the settings maxima are granted without a prompt; beyond them always
     asks. The Engine never exceeds user-set maxima.
   - Runs remain the durable recovery unit, closed at milestone or wave
     boundaries, never because an arbitrary cap was hit.
   - Named, closed resource profiles (current `default` plus a larger
     `build`) remain bound into intents and receipts, and Guard resource
     ceilings (CPU, memory, storage, processes) stay finite.
5. **Typed Git reads.** Read-only `status`, `diff` and `log` operations
   implemented in-process over the repository, without executing Git hooks,
   filters or configuration from the project. Commits stay with the user.
6. **Registry fetch.** Lets an agent add dependencies to the project it works
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
   and scope in its assignment; a worker can only narrow it. Each agent's
   first Tool outcome carries its scope
   (paths and command names), success criterion (an explicit check such as a
   named test command) and budget, without an extra model call. It is
   mandatory for every agent that performs effects, bounded, validated and journaled.
   Changing it requires approval under the current approval mode.
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
       Failed or Cancelled with the same report.
     - After a crash or restart, an unfinished Goal resumes at its last phase
       or Run boundary only after the user confirms.
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
missions, direction and path leases.

Topology is one level: workers cannot spawn workers or message each other,
and everything flows through the primary. Concurrent workers are capped by a
setting (default 4) and by each Provider's concurrency. A worker's grant is the
intersection of the Goal grant and its assignment, which names its commands,
Skills and MCP tools; workers never receive Git or network capabilities.

1. **Writable workers.** Children can receive an explicit Tool grant. Each
   worker gets one persistent private Guard view for its whole assignment
   (the Workspace at assignment plus its own work); its commands run there,
   and it hands the primary one cumulative changeset against its assignment
   base, never writing to the Workspace directly. No Git worktrees.
2. **Path leases.** The primary assigns each worker file or directory prefix
   leases. They are enforced: writes outside the lease reject at changeset
   validation. Reads stay within the Goal grant.
3. **Single integrator.** The primary integrates changesets in completion
   order, recorded in the journal so replay stays deterministic, and remains
   the only publisher and final answer owner.
   - A failed digest precondition is a conflict: the primary receives both
     versions and resolves it itself or reassigns the work on the current
     base. No automatic text merge.
   - A worker must pass its own success criterion inside its view before
     handing over; the primary re-verifies after integration.
   - A failed or cancelled worker's changeset is discarded and reported; the
     primary may issue a new assignment against the budget. No automatic
     retry.
   - A changeset holds at most 2,048 files and 32 MiB; oversized changesets
     reject whole with a count report and are never split.
   - Dependencies are expressed as waves: a later wave starts after the
     earlier wave integrates. There is no general task DAG.
4. **Reviewer role.** A read-only reviewer child evaluates each changeset
   before integration, feeding the Supervisor review point (B4).

This replaces decision D-03 for coding Runs and requires the attempt, lease,
stale-result and lost-effect reconciliation rules listed in
[multi-agent supervision](../../docs/research/multi-agent-loop-and-user-feedback.md#ownership-before-scheduling)
before implementation.

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
   fork) or both. Code rewind applies reverse changesets with digest
   preconditions, so command edits are rewindable too; drifted files reject
   and are listed. Never automatic. Checkpoints are kept 30 days and bounded
   per Session; overflow drops the oldest with a visible notice.
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
    Auto go straight to Implementation with the plan contract in the first
    outcome. A Session holds at most one active Goal; a prompt while it runs
    is a steering note, and the next prompt after it ends starts a new Goal.
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

- A3–A5, B and D are platform-independent Engine/Guard logic over
  existing private storage on Linux (tmpfs) and macOS (native images); judge
  adapters are Provider HTTP.
- A2 needs native mounts on Linux and Seatbelt read grants on macOS.
- A6 needs network-namespace routing to the proxy on Linux and proxy-only
  Seatbelt network rules on macOS.
- Native Windows is out of scope; Windows users run the Linux build under
  WSL2, whose support needs its own evidence.

## Security and resource model

Each slice that crosses one of these boundaries runs the
[security pass](../../agents/security.md#security-pass-triggers) before merge.

- **Runtime mounts and cache (A2, A3).** Toolchain subtrees and the build cache
  must exclude credential files and never overlap Workspace, StateRoot or
  account roots; cache contents are data, never authority.
- **Registry fetch (A6).** The only network path from a Tool: allowlisted
  public registries through the Guard-owned proxy, with SSRF rejection,
  install scripts disabled and no credentials.
- **Git (A5, E6).** Reads are in-process without executing repository
  configuration, hooks or filters. Writes run host `git` only after closed
  allowlist admission of `.git/config`, with hooks disabled on the host.
- **Providers and judges (D, B).** Every roster profile and judge is an egress
  destination with its own credential, usage consent and disclosure scope,
  admitted before Workspace disclosure. Multiple credentials stay inside the
  Engine and are never passed to Guard payloads.
- **Authority.** The Supervisor, judges and project rules can only narrow,
  except the explicit per-folder commit authorization and global
  disruptive-Git setting, which the user owns.
- **Bounds.** Changesets, checkpoints (30 days, per-Session cap), progress
  notes (160 characters), grilling (12 questions × 6 rounds), judge budgets,
  concurrent workers (default 4) and settings maxima are declared with
  overflow behavior. User-chosen unlimited usage applies only to Goal usage
  dimensions; per-request Provider caps and Guard resource ceilings stay
  finite, and [AGENTS.md rule 4](../../AGENTS.md#global-rules) gains this
  exception when the slice lands.

## Execution

Each item is one PR with its own contract update, security pass where
triggered and evidence under [testing rules](../../agents/testing.md).

2. A2–A4 Toolchain profiles, build cache and flexible limits.
3. **Dogfooding milestone: Arany develops Arany.** Use Arany for this
   repository's own work from here on. Requires Beta 1 check 7 (live
   validation of the chosen model's strict Tool encoding) in
   [next steps](../../NEXT_STEPS.md#beta-1-final-user-owned-checks). Every
   later slice is exercised through real use, which supplies the shadow-mode
   calibration data B2 needs.
4. Goal core: E11 every task is a Goal, B1 plan contract, E1–E4 steering,
   context, Workspace lock and checkpoints, E8 compaction and E10 `exec`
   integration. Creates the `docs/specs/goals.md` domain spec.
5. B6 modes including RPI and grilling, E7 Goal view and E12 settings.
6. B2, B3, B5 and E5 Supervisor signals, escalation, Finish gate and override.
7. A5 Typed Git reads and E6 Git writes.
8. A6 Registry fetch.
9. D1–D5 Provider roster and per-agent routing.
10. Configurable judges over the roster (agent-capable judges first, then a
   typed-judge adapter after its API research note).
11. C1–C3 Writable workers, leases and integrator.
12. B4 and C4 Review-point review and reviewer role.

Observe during dogfooding, without changing the plan unless evidence demands
it: whether RPI's Plan gate adds friction to small tasks, and whether the
per-slice specification and evidence cost slows delivery.

E13 telemetry spans land with the slice that introduces each span's subject.

Next action: specify A2–A4 toolchain profiles, build cache and flexible limits
in the [Tools contract](../../docs/specs/tools.md), including native admission,
credential exclusion, cache quotas and larger offline build scenarios.

## Open questions

The [design tree](./design-tree.md) records every grilling round. Only
data-dependent questions remain:

- Default thresholds for loop, probing and disproportion signals. Decided from
  shadow-mode corpus and real Runs before those signals enforce.
- `build` resource profile values per OS and default settings maxima. Decided
  from measured builds on named hosts after A2–A4.

## Contract owners

- `docs/specs/goals.md` (created in execution step 4): Goal lifecycle, RPI, steering, checkpoints and Goal view facts.
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
  Implementation to Completed, and to Failed/Cancelled with a final report;
  restart resumes only after confirmation.
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
