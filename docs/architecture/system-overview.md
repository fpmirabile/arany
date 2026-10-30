# Arany beta architecture

**Design rule:** build a familiar durable harness, then deepen it without replacing its Engine

This document owns physical architecture, data model and responsibility boundaries. [Product specs](../specs/README.md) own observable contracts. [The documentation map](../README.md) locates those owners, and [next steps](../../NEXT_STEPS.md) owns current defects and verification debt. Research and the [decision register](../research/next-step-decision-register.md) retain source evidence and rationale.

[D-17](../research/next-step-decision-register.md#d-17--product-identity-platforms-and-license)
owns product identity, platform scope and the distribution license.

Read the section for the affected boundary:
[runtime ownership](#2-runtime-architecture), [module entry points](#3-physical-beta-shape),
[Session lifecycle](#5-session-run-and-collaboration-lifecycle),
[context and effects](#6-context-and-compaction), [persistence](#7-canonical-persistence),
[presentation](#8-terminal-presentation), [Providers](#9-provider-profiles-and-custom-endpoints),
[telemetry](#10-telemetry-and-diagnostics), or [security](#11-security-boundary).

## 1. What the beta must prove

Arany is an interactive Session-oriented CLI, not a one-shot team demo. Bare `arany` opens a local conversation and creates its durable Session on the first task submission; every accepted user Message starts one bounded Run; each Run has one accountable primary AgentRun and may admit a bounded mixed team of direct researchers and EditOnly workers.

The full product contract is successful when it proves all of these together:

1. a Session survives process exit, explicit resume, multiple Runs, provider changes between Runs, compaction, and deterministic replay;
2. `single`, `auto`, and `team` policies all use the primary coordination loop and a generic ordered child collection rather than a fixed two-worker shape;
3. the primary cannot finish team work before every required child has a persisted terminal disposition;
4. the default fullscreen terminal keeps a navigable committed transcript on the alternate screen (Classic inserts it into native scrollback), conditional agent activity above a bottom composer, a slash menu below input when open, and a compact status row;
5. keyboard access is complete, while mouse reporting exists only in fullscreen or transiently inside an open picker or detail panel;
6. native OpenAI and Anthropic plus every admitted exact custom endpoint/model profile satisfy the same semantic Provider contract;
7. bounded instruction discovery follows the [Session admission contract](../specs/session.md#workspace-admission-and-authority);
8. `exec` and `show` expose the same committed facts without terminal initialization; and
9. cancellation reaches the primary and every admitted child without fabricating completion; and
10. explicitly enabled primary Tools use typed one-use intents, restrict-only Policy, native Guard enforcement for programs and capability-scoped file publication, bounded continuation and durable non-executing replay.

Startup admits trusted user and process configuration, a private StateRoot outside the Workspace, [per-role Provider routes and fallback chains](../specs/provider.md#per-role-selection-and-fallback) and the caller-selected Workspace before reading project-controlled input; [Session admission](../specs/session.md#workspace-admission-and-authority) owns that order and the instruction and include snapshots. Untrusted and ordinary headless Runs are read-only. Attached [folder trust](../specs/tools.md#folder-trust-and-approvals) or an explicit `--tools` grant feeds the separate native Guard, and unsupported native enforcement fails closed. The [primary host-command exception](../specs/tools.md#primary-host-commands) is the only execution outside it. Engine owns live epochs and journaled scope and reuse validation; Tools owns executable admission and supervision.

Tools owns one private model-input projection shared by the native adapters: bounded catalog and output data and ordered call, disposition and Workspace-effect facts, without journal intent identities, enforcement digests, limits or receipts. Canonical observations keep all enforcement evidence, so the projection changes neither authority nor replay. Adapters pair model action and result items with request-local correlation labels and carry each observation only in its result item; Engine integration outcomes are separate untrusted user-data records with no fabricated model function. [Provider encoding](../specs/provider.md#structured-outcomes-and-tool-encoding) owns the wire.

## 2. Runtime architecture

The workspace has two crates: the `arany` library owns the harness, and
`arany-cli` produces the `arany` executable. One long-lived Engine serves each
process. The CLI calls `Engine::open(state, EngineConfig)` once, then
`submit(providers, request, controls) -> RunHandle` for each submission. The
handle exposes cancel, steer, approvals and events; completion yields
`RunResult { run, stopped }`. The CLI/account edge constructs an admitted Provider bundle per submission: four role chains plus an optional immutable named roster. JSON file types and credentials stay at that edge; the Engine independently validates bounded semantic snapshots, named identities and fixed role defaults. RunStarted pins the cross-validated roster/digest with role provenance. Children carry typed objective/profile assignments; the Engine resolves eligible references and commits them in AgentSpawned before any dependent call. Routing derives each child chain from its actor and immutable profile, keeping review/compaction at their fixed roles; no new process or privilege boundary is added.
A concurrent submission or compaction is refused with `EngineError::Busy`;
Session and ancestor locks remain scoped to a Run, while the Store thread lives
until the Engine is closed or dropped. The [execution contract](../specs/execution.md#admission-persistence-and-recovery)
owns Engine lifetime and admission.
The binary's `serve` Server owns a bounded submission/response channel loop over
that Engine. Its stdio adapter, ACP translator, attached terminal, headless `exec` and in-process conformance
adapter use the same loop. ACP maps its private wire subset into the serve
vocabulary at the binary edge; its translator owns the family-specific Tool
projection and preserves serve's Move FileChange path while giving GhPrList an
explicit read projection. Each active submission has an owned task driving
its RunHandle while the Server projects its committed Event subscription; bounded client backpressure
cannot hold Journal appends. The library has no protocol or transport dependency. Supervised stdio avoids
listener and discovery ownership; browser or multi-client transports require
the separate admission and recovery design in the [Serve contract](../specs/serve.md).

Production entry points poll the Engine on the process main thread's current-thread Tokio runtime.
Any new production thread hosting the Engine, including a multi-thread runtime worker,
must set an explicit stack size of at least 8 MiB.

The Engine owns admission, scheduling, context, Goal accounting and Run
transitions. `GoalLedger` admits, reserves and settles actions over the reducer's
`GoalView`, with `Action::max_events` owning Event headroom. Primary, researcher and editor Tool steps share `execute_step`: reservation, committed intent,
approval, dispatch, committed observation and continuation have one owner.
Tools owns the `ToolGrant` value supplied for a submission; the Engine consumes
it during admission, so the source dependency points from Engine to Tools.
Engine permission selection, host session reuse and remembering eligibility live
in `crates/arany/src/engine/run_loop/step/permission.rs`; its result enters the
shared ask and dispatch flow. The Engine composes per-dispatch queue progress,
active-action and checkpoint senders through consuming ToolRuntime builders.
The Session reducer owns shared editor-request validation, and the Engine consumes its typed causes. The [execution contract](../specs/execution.md) owns refusal and failure behavior.
The Engine coordinator schedules Run-scoped editors through the shared Tool step. Each editor works in an isolated copy-on-write workspace owned by Tools, and the Engine is the one integration owner: when the editor finishes, its lease-bounded project changes publish into the Workspace as one checkpointed changeset under the child identity, or nothing does. The reducer owns leases, workspace targets, the single integration, terminal dispositions, causal release and independent primary verification. Tools owns admitted-capability file publication and the editor workspaces, while Guard retains process enforcement. Journal acknowledgment precedes continuation. The [execution contract](../specs/execution.md)
and [Goal contract](../specs/goals.md) own lifecycle, budgets and recovery.

The RunLoop owns wave-primary request assembly; the [Execution collaboration contract](../specs/execution.md#collaboration) owns its preparation and admission ordering.

The Engine owns the Supervisor's bounded evaluation state and pure evaluator over committed reducer facts. The RunLoop explicitly decides whether to journal a shadow verdict and commits that verdict as its own Event before continuation. The reducer owns only the recorded-verdict projection and shape/placement validation; replay accepts recorded decisions without reevaluating signals. The Supervisor does not subscribe to the lossy tap and has no enforcement authority. The [Supervisor contract](../specs/tools.md#supervisor-adapter) owns signals, bounds and replay.

`Journal::append` publishes only committed Events to a bounded broadcast ring of
256 envelopes. Clients, telemetry and diagnostics each subscribe to that tap;
a lagging receiver gets `Resync { missed }` and rebuilds from the committed view.
Observers never block control flow or authorize work. Provider previews remain
separate provisional data. The tap belongs to Journal rather than the Store
thread so persistence does not own observer delivery. [Execution](../specs/execution.md#committed-event-stream),
[telemetry](../specs/telemetry.md) and [diagnostics](../specs/diagnostics.md) own
their observable contracts.
`provider/diagnostics.rs` owns Provider diagnostic types (stages, counts,
response shape, Tool argument shape) and stream, failure and context record
construction; the Engine calls its context-exhaustion entry point. Root
`diagnostics.rs` keeps the bounded private sink (activation, locking,
sanitization, file admission) with no Provider dependency or re-export; Provider
depends on it one way, and other Engine, Event and Tool producers stay at root.

```mermaid
flowchart LR
    User([User or script]) --> CLI[CLI + accounts]

    subgraph Edge[arany-cli: binary arany]
        CLI --> Terminal[Terminal or deterministic output]
        Presentation[Pure presentation] --> Terminal
    end

    subgraph Harness[arany: library]
        CLI --> Engine[Engine: open / submit]
        Engine --> Loop[Primary loop + children]
        Engine --> Ledger[GoalLedger over GoalView]
        Loop --> Step[Shared execute_step]
        Loop --> Provider[Provider interface]
        Step --> Tools[ToolRuntime + Policy]
        Engine --> Journal[Journal: append + subscribe]
        Step --> Journal
        Journal --> Store[(SQLite owner thread)]
        Store --> Views[Reducer + committed views]
        Views --> Ledger
        Views --> Presentation
        Journal -. committed Events .-> Tap[256-envelope broadcast ring + Resync]
        Tap --> Terminal
        Tap --> OTLP[Telemetry]
        Tap --> Diagnostics[Diagnostics]
        Tools --> Reads[In-process List / Read / Search / Glob]
    end

    Provider --> Adapters[OpenAI / Anthropic / custom / scripted]
    Tools --> Guard[Same-binary native Guard]
    Guard -. Linux .-> Systemd[systemd + Bubblewrap]
    Guard -. macOS .-> Launchd[launchd + Seatbelt]
    OTLP -. opt-in .-> Collector[Loopback Collector]
    CLI --> CredentialHelper[Same-binary credential helper]
    CredentialHelper --> Keyring[OS keyring]

    classDef deep fill:#173b34,color:#fff,stroke:#5eead4,stroke-width:2px;
    classDef canonical fill:#2b2346,color:#fff,stroke:#c4b5fd,stroke-width:2px;
    class Engine,Loop,Ledger,Step deep;
    class Journal,Store,Views canonical;
```

Provider is the substitutable semantic inference interface. Wire adapters stay
behind it; SQLite, replay, reducers and Tool dispatch remain concrete owners.
`ProviderKind` owns native profile names, credential classification, catalog
dispatch and custom-profile syntax; public strings are parsed at those edges. Telemetry
maps those closed categories to its existing fixed labels.
The CLI depends on `arany`; the library never depends on CLI, terminal,
presentation or accounts, and has no ratatui, crossterm or keyring dependency.
`accounts/` owns ChatGPT OAuth, saved API accounts and the credential helper.
Terminal rendering and pure view projection belong to the binary.

Tool operations remain a closed enum with one facts table and per-family
dispatch: a `dyn Tool` registry would duplicate a closed set without adding
substitutability. Per-variant operation structs wait until a family payload
gains a third caller. Model-facing schemas remain hand-written to preserve
descriptions and byte-stable ordering; a derive needs caching evidence and a
byte snapshot before reconsideration. Guard adapters remain `cfg`-selected
functions because the host kernel, not runtime configuration, selects them.

Researcher children are direct read-only AgentRuns: without Tools they make one reasoning
call; with Tools they may make up to 16 read-only Tool calls before reporting.
Run-scoped editors edit private workspaces through the shared Tool step, integrate once within their leases,
report once and wait for fresh primary criterion verification.
Explicit ID reassignment creates a fresh attempt; the reducer projects historical dispositions and lineage,
while native Guard admission alone owns abandoned-storage containment and cleanup.
The [researcher contract](../specs/execution.md#researcher-children) owns their
catalog, budgets, ordering and failure rules; children never inherit a broader
grant than their parent.

Exact Read, typed discovery (List, Glob, Search) and Skill text use the admitted
directory capability in process.
Observations carry `Attestation::{InProcess, Guard, Git, Host, Unguarded}`; the reducer validates the
execution site. Typed Workspace mutations and changeset publication use the same admitted handle
in process, with digest preconditions and journaled File receipts. Typed Git runs
natively under a minimal cage with its own `Git` attestation; Skill loading, programs
and MCP stay behind the separate Guard. The [Tool contract](../specs/tools.md#in-process-file-operations) owns
path checks, bounds and attestation; its [Linux](../specs/tools.md#linux-enforcement-and-compatibility)
and [macOS](../specs/tools.md#macos-enforcement-and-compatibility) sections own
native launch, supervision and recovery. Short-lived same-binary Guard and
credential helpers are privilege boundaries, not Clients or daemons. On Linux, a
retained same-binary namespace supervisor owns bootstrap tmpfs, per-directory command
lower/overlay/mask construction, read-only file/parent mounts and its bwrap child's pidfd. It clears every capability set and sets no-new-privileges before
forking bwrap; the inner helper owns payload dispatch/capture, while the controller
retains READY/GO and unit/cgroup cleanup authority. A short-lived
same-binary host launcher establishes a fresh OS session before executing an
authorized host command; the Engine owns approval and the parent owns supervision.

The current-thread Tokio coordinator owns async lifecycle. One named
standard-library thread owns the synchronous SQLite connection; a bounded
worker owns OTLP export. Store and Guard share `private_fs` for directory-handle
and ACL admission without sharing history or Tool policy. Native capacity and
storage recovery remain private to Guard admission, outside canonical Events.
The macOS Tool controller awaits a blocking lifecycle owner with a private
reactor, keeping native teardown off the terminal reactor. Abandoning the await
cancels that owner; the dispatch marker distinguishes pre-GO refusal from
uncertain execution.

## 3. Physical beta shape

These are responsibility entry points; private file layout follows the source
modules.

| Responsibility | Owner | Contract |
|---|---|---|
| CLI grammar and composition | `crates/arany-cli/src/cli/`, `main.rs` | [Terminal](../specs/terminal.md) |
| ACP translator over the channel Server | `crates/arany-cli/src/cli/acp/` | [ACP](../specs/acp.md) |
| Stdio protocol and channel Server | `crates/arany-cli/src/cli/serve/`, `protocol/` | [Serve](../specs/serve.md) |
| Terminal and pure view projection | `crates/arany-cli/src/terminal/`, `presentation/` | [Terminal](../specs/terminal.md) |
| Saved API accounts, OAuth and credential helper | `crates/arany-cli/src/accounts/` | [Credentials](../specs/credentials.md), [ChatGPT](../specs/chatgpt.md) |
| Run admission, Goal ledger and shared Tool step | `crates/arany/src/engine/` | [Execution](../specs/execution.md), [Goals](../specs/goals.md) |
| Events, reducer, context and replay | `crates/arany/src/session/`, `store/` | [Session](../specs/session.md) |
| Committed Event tap | `crates/arany/src/journal.rs` | [Execution](../specs/execution.md#committed-event-stream) |
| Semantic inference and wire adapters | `crates/arany/src/provider/` | [Provider](../specs/provider.md) |
| Tool admission, Policy, dispatch and native Guard | `crates/arany/src/tools/` | [Tools](../specs/tools.md) |
| SQLite, private records and shared filesystem admission | `crates/arany/src/store/`, `private_fs/` | [Execution](../specs/execution.md#admission-persistence-and-recovery), [Credentials](../specs/credentials.md#account-scope-and-identity) |
| Lossy export, bounded private sink and Provider stream/failure/context records | `crates/arany/src/telemetry/`, `diagnostics.rs`, `provider/diagnostics.rs` | [Telemetry](../specs/telemetry.md), [Diagnostics](../specs/diagnostics.md) |

[Testing](../../agents/testing.md#corpus-placement) owns corpus placement and
shared support. [Library](../../agents/arany.md) and
[binary](../../agents/arany-cli.md) editing notes route this work.
Guard helper entry points remain reachable from the binary's `main.rs`.

## 4. Public command contract

[The terminal spec](../specs/terminal.md#entry-modes-and-channels) owns attached entry, interactive controls, deterministic channels and their acceptance scenarios. [Account setup](../setup.md) and [local tools](../tools.md) are user procedures; the compiled CLI/command registry owns exact syntax. The spec also owns admission ordering, typed error classification and command-specific composition.

`main.rs` selects attached chat, setup, `exec`, `show` or Provider operations. Private CLI callers resolve trusted inputs and call Engine operations; they do not implement Run transitions or wire decoding. Attached new/clear entry keeps a bounded empty conversation projection and its selected defaults/title local, with zero creation and last sequence; the first task submission delegates creation to the Engine before Run admission. Exit and resume discard that unsaved projection without deleting canonical state. No Events or resumable ID are invented. Continue/resume/fork delegate to the Engine, which revalidates the pinned Workspace and canonical lineage. Headless `exec` creates one Run in a new Session by default; append requires an explicit Session ID. `show` uses strict read-only replay. There is no `run` alias.

The attached composer, slash handlers and quick selector share the compiled local controls and existing defaults/admission owners. Provider selection, saved-account identity, billing-route choice, catalog discovery and Run authority remain separate operations. Typed busy/defaults rejection can recover in chat, while State/replay/terminal failures retain their fatal boundary. A failed operation is not a rollback or retry grant.

Attached setup (`cli/attached/setup.rs`) owns entry routing and setup-journal coordination; its private children split the flows. `setup/error.rs` owns error classification, conversion and diagnostic recording. `setup/native.rs` owns native account lookup, inspection, key and workspace-ID entry, catalog defaults, and save, removal and reset settlement. `setup/chatgpt.rs` owns the ChatGPT account menu, selection, local removal, connect and reconnect through publication, risk review and catalog defaults. `setup/routes.rs` and `setup/custom.rs` own custom-route menus, snapshot-bound edits and checks; `setup/routing.rs` owns role routing and `setup/roster.rs` roster profiles; `setup/prompts.rs` owns shared prompts, confirmation, paging and field entry. Each flow settles its own journal disposition. Credential storage and key access stay in `accounts/credentials/`, OAuth and ChatGPT storage in `accounts/chatgpt/`; the [credential contract](../specs/credentials.md#storage-replacement-and-recovery) owns setup ordering and recovery.

Explicit `--tools` reaches the same private Engine/Policy/Guard admission from attached and headless composition. The CLI supplies the shipped executable identity and private configuration root, not an alternative Tool runtime or per-effect approval service.

## 5. Session, Run, and collaboration lifecycle

The lifecycle hierarchy is:

```text
Session
├── durable messages, title, fork lineage, defaults, compaction snapshots
└── Goals[] in accepted-task order
    ├── accepted objective, Workspace identity, bounded budgets, plan and terminal report
    └── Runs[] linked to exactly one Goal
        └── Run
            ├── pinned ProviderProfile/model/policies/budgets
            └── AgentRuns[]
                ├── one primary
                └── ordered 0..N direct children
```

A Session has at most one active Run in beta. Every accepted task is a durable Goal linked to one or more Runs; Goal budgets, the initial plan contract and terminal report are canonical Events projected by Session replay. Write-capable Goals also hold the OS-user Workspace identity lock, so separate Session StateRoots cannot concurrently publish to one Workspace. The Engine holds a private per-Session state lock across each Run, source fork, and manual compaction; a brief private namespace lock coordinates file creation and removal while other Sessions can proceed independently during work. At most 65,536 Session lock files may exist, including crash-stranded pre-commit files. Completed, failed, cancelled, and interrupted Runs remain immutable history. Resume reconstructs the same Session; fork creates a new Session with a single `SessionForked` first Event recording source Session, boundary Run, boundary sequence, and canonical Event-prefix digest. Store replay validates bounded source ancestry; the context compiler selects successful inherited turns only through that boundary. Historical children remain inspectable but are never recreated as live work.

Before each Run, Arany pins one collaboration policy:

```text
single
  maximum total direct children = 0

auto(max_active_children = N)
  primary may delegate only when independent work is useful

team(max_active_children = N)
  primary must propose a non-empty team decomposition
```

The product default is `single`; opt-in `auto` allows at most three total direct children. The historical field name `max_active_children` bounds total delegation size, not only simultaneous dispatch. `N` is a numeric input and bounded by policy, process safety and remaining call/token/byte/time budgets. The separate Provider concurrency ceiling limits simultaneous calls; admitted children can wait in the ordered queue. Children cannot create nested teams in beta.

The semantic Provider outcome is:

- `Answer { text }`, a plain text answer;
- `Delegate { children: bounded ordered objective/profile assignments }`; or
- `Tools { calls: bounded ordered list of closed typed operations }`, under the existing per-role Tool restrictions ([execution contract](../specs/execution.md#accepted-outcomes)).

In `single`, the primary finishes without delegating. In `auto`, it delegates only when substantial independent read-only work justifies the extra calls and duplicated context; routine edits, direct questions and tightly coupled work stay with the primary. Shared guidance asks for the smallest useful team with distinct evidence-backed assignments; history and Skills never require a team. This is model guidance within the enforced ceiling, not a task classifier. In `team`, the first non-Tool outcome must be a nonempty bounded delegation. Researcher children make one reasoning call without Tools, or up to 16 read-only Tool calls in a Tool-enabled Run; after required children finish, primary synthesis uses the remaining Goal budget. Without Tools, a team of `k` children uses `k + 2` Provider calls and a direct answer one. [Goals](../specs/goals.md#lifecycle-budgets-and-recovery) owns defaults and finite limits. The Engine reserves each call or effect atomically with its bounded terminal tail; read-only inspection and Tool history compact within the admitted request budget instead of stopping at a fixed step count. Every public production adapter rejects reported input usage above 1,000,000 tokens or output above the exact request cap; Engine validation is defense in depth. [Provider reliability](../specs/provider.md#usage-provenance-and-reliability) owns same-route retries and [fallback chains](../specs/provider.md#per-role-selection-and-fallback) own switches.

Context is isolated per AgentRun. Children receive explicit assignment capsules and narrower-or-equal authority, never a shared mutable prompt. Only the primary writes the final assistant Message.

## 6. Context and compaction

### Local effect ownership

The [Tool contract](../specs/tools.md) owns behavior. [Guard research](../research/deterministic-harness-protection.md#linux-launch-and-syscall-rationale) and [MCP/Skill research](../research/mcp-and-runtime-skills-2026-10.md) keep alternatives and rationale; [dependency review](../security/local-tool-dependency-review.md) owns scoped security evidence. `tools.json` is private host configuration, not repository configuration; its pinned scope and resource digests produce immutable primary-only one-use intents. The Engine commits each Provider request fact and intent before dispatch and a correlated observation before inferring again; stops follow the [effect outcomes](../specs/execution.md#effect-outcomes). Replay validates payloads and transitions, accepts only the current Goal-linked format and never grants authority from an older or incomplete journal.

In-process file operations use live no-follow handles and digest- or create-preconditioned replacements. The Linux Guard verifies namespace, capability, seccomp and cgroup limits before payload release, owns the invocation and retained cgroup through cleanup, and returns a digest-bound receipt. Linux primary commands see supervisor-owned overlays for admitted directories and read-only mounts for exact files; macOS commands and MCP see fresh private snapshots of selected files. Effects get no accounts, bounded resources and no network beyond a Command's pinned [egress proxy grant](../specs/tools.md#network-grants-and-egress-proxy) and the proxied web and remote-MCP calls. Commands return bounded changesets after descendant termination; the Engine journals the result and routes a separate receipt-bound integration through the in-process publisher, which validates every precondition first. MCP writes stay disposable. Skills are pinned guidance and resources, never permissions; YAML and schema work happens only inside the killable boundary. MCP supports the 2025-11-25 tools profile over local stdio or, on Linux, [streamable HTTP with OAuth](../specs/tools.md#remote-mcp-and-oauth) through the proxy. The [tool guide](../tools.md) explains setup.

On Linux the retained same-binary supervisor owns primary command launch. It verifies its own executable pin, builds the view (per-directory lower clones, bounded inventory, masks, private tmpfs upper and work, read-only file and parent mounts) before clearing capabilities, and reattests before relaying GO; raw layers and their descriptors never reach the payload. The inner helper attests its own pin and mount topology. The controller keeps READY/GO, the admitted phase envelope and deadline, and unit and cgroup cleanup. Pidfd wait and reap precede upper validation and capture, and only the primary publisher applies captured candidates. Default and Build cache overlays have their own bounded tmpfs; cache application waits for supervisor namespace and unit teardown, and Store owns cache admission, reuse and explicit-clear recovery. [Linux overlay publication](../specs/tools.md#linux-overlay-publication) and the [build cache](../specs/tools.md#persistent-workspace-build-cache) own capture, selection and archive rules. A typed setup refusal can explain unavailable enforcement before GO; missing completion after GO is [Uncertain](../specs/execution.md#effect-outcomes). Neither grants host authority or picks another launcher.

Past effect records enter context/compaction only as bounded untrusted facts, including interrupted attempts and observed failures, never renewed permission or execution. The tool-enabled compiler reserves current observations from the remaining finite context budget. An active Goal may summarize only an acknowledged Tool prefix at a Run boundary; it retains every observation in canonical Events and sends the latest untrusted summary plus a bounded recent tail. Researcher children receive a read-only catalog and only their own observations; the primary receives their bounded reports.

### Conversation context

Session history is local canonical state. Provider conversation IDs, cache keys, and opaque compaction items are optional accelerators.

The deterministic context compiler selects trusted instructions, the current user Message, bounded recent Session history, valid compaction material, explicit include snapshots, and required child results under the local finite content and encoded-request byte limits. It records what was included and excluded. Exact context-window metadata is used only when trusted for the admitted route/model; when unavailable, Arany retains conservative byte limits and makes no expanded-window claim. Provider switching between Runs recompiles from Arany state.

Provider-neutral textual compaction uses the Provider's separate semantic `compact` operation rather than inventing an AgentRun. Accepted conversation outcomes, including unanswered objectives, and an optional prior summary enter that call; the model-authored result remains untrusted derived data. A later `RunStarted` pins the selected snapshot Event and content digest.

Compaction never replaces or deletes canonical history. `/compact` creates a derived snapshot for an exact committed Session prefix and records:

- covered Run/sequence and source digest;
- Provider, model, prompt/schema/compiler versions;
- bounded summary or compatible opaque Provider item;
- byte/token estimates and content digest; and
- completion/failure provenance.

Manual and automatic compaction run in the Engine under the Session lock, using the pinned Provider, model, effort and account source of the Run whose committed boundary they cover; the private CLI passes that committed config rather than newer Session defaults. An absent saved account stays the original environment-backed or custom source, a recorded API or ChatGPT account is re-admitted by identity and current evidence, and replay grants no credential authority. Compaction composition keeps committed and admission failures (labelled chat feedback), explicit interruption (an uncertain-commitment warning) and terminal-owner errors (restoration and exit) apart; none changes the confirmed Run outcome. [Session compaction](../specs/session.md#context-and-compaction) owns triggers, bounds, failure and invalidation. Cross-Session Memory is deferred and distinct.

## 7. Canonical persistence

One strict SQLite Event journal remains product truth. Store owns the separate derived checkpoint SQLite store and pre-image durability, directory-cache admission, publication and unavailable conversion/application recovery for default and Build Commands; the controller retains durable Guard ownership until native cleanup is confirmed, including recovery before cache clear; Guard owns Workspace publications; the payload receives no StateRoot path or handle, while only the retained Linux supervisor receives the narrow admitted cache-directory identity. Session, Run, AgentRun, SessionView, and RunView are reduced state rather than duplicate authoritative tables. SessionView retains canonical title text, explicit-title provenance and a bounded prefix-derived inherited title; its conversation-title projection is shared by status, resume feedback and human exports. These facts add no Event schema or persisted display cache.

```sql
CREATE TABLE events (
    sequence       INTEGER PRIMARY KEY,
    session_id     TEXT NOT NULL,
    run_id         TEXT,
    agent_run_id   TEXT,
    kind           TEXT NOT NULL,
    payload        TEXT NOT NULL CHECK (json_valid(payload)),
    created_at_ms  INTEGER NOT NULL
) STRICT;

CREATE INDEX events_by_session ON events (session_id, sequence);
CREATE INDEX events_by_run ON events (run_id, sequence) WHERE run_id IS NOT NULL;
```

The initial vocabulary is intentionally small:

- `SessionStarted`, `SessionRenamed`, `SessionDefaultChanged`, `SessionForked`, `ContextCompacted`;
- `MessageAccepted`, `MessageCommitted`;
- `RunStarted`, `RunFinished`;
- `AgentSpawned`, `ProviderCallRecorded`, `AgentFinished`; and
- `ToolStarted`, `ToolFinished`.

The [execution contract](../specs/execution.md#admission-persistence-and-recovery) owns the one journal format version and transition validation. Goal Events pin the approved usage and resource settings and record reservations beside terminal reports; Tool Events correlate Run, acting AgentRun, one-use intent, pinned policy and enforcement digest, Workspace identity and effective limits. Event encoding borrows from in-memory facts, and decoding keeps owned strict payload records; both preserve canonical serialized bytes. A started attempt without a terminal observation stays a past interrupted or uncertain fact, never an approval or retry instruction.

`SessionStarted` pins the admitted Workspace device and inode atomically with the title, including for an empty Session. Replay rejects a later Run or fork whose identity disagrees with the pinned Session. The SQLite owner admits only the current registered SQLite schema and Event payloads; unsupported formats and missing fields reject without conversion or deletion, and history at another version is archived intact at startup ([incompatible history](../specs/execution.md#incompatible-history)).

`RunStarted` fixes Provider and profile, collaboration policy, limits, instruction digest and Workspace snapshot facts. Provider owns the closed `ProviderRecord` kind/name representation; the Engine translates its admitted runtime identity into it and Session validates it before reduction. The CLI account and route owners persist the same tagged record, translating only at runtime edges; keyring read upgrades stay in memory, and StateRoot owns atomic private-file publication. Each observed call records phase, local disposition, bounded response ID, reported usage and accepted wire provenance before its AgentRun finishes; compaction records carry the same optional provenance. A cancelled or timed-out call has unknown usage and provenance, not zero; cancelling after a call returned keeps its record. Sticky cancellation wins before a primary terminal commitment starts, without rewriting earlier child outcomes; once started, the commitment completes or leaves an interrupted prefix. A logical transition commits before reduction or feedback, and a crash leaves a valid prefix that replay labels `Interrupted`. [Execution](../specs/execution.md#admission-persistence-and-recovery) owns these rules.

Typed Provider failures keep an explicit nullable closed reason on every call ([ChatGPT reasons](../specs/chatgpt.md#catalog-and-runs)); strict admission rejects a reason inconsistent with its disposition, and raw status, codes, upstream text and request IDs are excluded. Presentation translates the last call of a failed AgentRun into compiled recovery guidance; cancelled siblings and successful calls cannot supply its cause, and linear modes append it after the confirmed receipt. The reason never enters Provider context or traces, proves a billing diagnosis, triggers retry or switches account routes. Compaction keeps its own failure categories.

Idle rename and default changes after an interrupted prefix seal the unfinished predecessor under the per-Session lock, as the [Session reducer contract](../specs/session.md#persistence-facts) defines; no interruption Event or synthetic terminal outcome is added.

Store capacity and byte preflight follow [execution admission](../specs/execution.md#admission-persistence-and-recovery). The Engine holds operation locks for the Session and all ancestors because replay counts their full journals, including suffixes outside a fork boundary. Compaction also checks its direct Session's lifetime attempt quota; inherited summaries do not charge it. Independent lineages stay concurrent.

The sole connection opens in a private owner-verified local state directory with the settings the [Session spec](../specs/session.md#persistence-facts) owns. Rebuildable indexes may accelerate Session listing; they never become canonical.

## 8. Terminal presentation

[The terminal spec](../specs/terminal.md) owns modes, layout, keys, draft retention, selectors, setup presentation, paste, accessibility, process channels and acceptance scenarios. Clipboard and image rules live in its [paste and attachments](../specs/terminal.md#paste-and-attachments) section; implementation invariants live in tests.

`presentation.rs` maps committed SessionView/RunView facts to bounded semantic rows and append-only lines. Tool summaries and approval actions dispatch over the library's closed Tool family and execution site; the CLI owns their per-family rendering, escaping and host disclosures. Read-only `ToolCall::display_facts()` data (family, site, verb, raw subject and paths) crosses into presentation; authority stays with typed calls and admission. Consumers own formatting, escaping and truncation, and the journaled report formatter remains separate. The private terminal module owns the primary-screen viewport, Composer, history index, raw/canonical readers, keys, focus, mouse and one RAII lifecycle owner. Its `Panels` groups selection, copy, cue, picker geometry and help state; private `Transcript` groups the distinct Fullscreen and Classic history indexes, native emission tracking, pager/dump state, Classic adjacency and kept-Tool suppression across release/reacquisition. Linear output, signals, secret echo, clipboard and graphics cleanup retain their separate owners and lifecycle order. None of these presentation types enters the Engine.

The attached Run's private `Drafts` owns offered steering text, linear offers and the held draft, shared by Run input and settlement. Composer owns editing and capacity; the Server owns steering delivery and withdrawal.

The presentation layer owns the committed budget-refusal lookup and Run feedback projection. `exec` retains its looser admission-refusal fallback, while committed receipts render the exact Run being reported.

History indexes row counts/source anchors over the already loaded Session view without retaining another full transcript, and materializes only visible message rows. Terminal-local feedback interleaves with those rows but has no canonical or Provider-context authority. The Composer owns the bounded Provider/account-scoped catalog and draft; the CLI's picker owns its cancellable refresh and staged default publication.

The private attached model owner persists token-free `model-preferences.json` under the checked OS-user account root with the account replacement lock and atomic file transport: at most eight exact Provider/account UUID sources, each with its selected model and effort, last catalog read time and up to 4,096 catalog IDs with their context windows, within 1 MiB with oldest-source eviction. It holds no compatibility evidence, grant or credential. New bare Sessions reuse matching preferences and seed the Composer cache; explicit flags and resumed defaults win. A first upgrade may copy only a matching tuple from the latest admitted-Workspace Session, without its history. Selection and catalog writes stay separate from account, consent and response admission.

The attached command owner persists token-free `ui-preferences.json` at the admitted StateRoot through the private replacement lock and atomic file transport. Attached startup reads it read-only before terminal acquisition without creating State. Engine snapshots only the selected execution values into a Goal before Workspace disclosure; they never widen Tool grants, and the record never enters Events, account authority or Provider context. [Terminal settings](../specs/terminal.md) own its fields, defaults and invalid-record behavior. The Composer owns tab and focus state, and the frame places the bounded list below input, sharing history-capacity geometry with the lifecycle owner.

The channel Server forwards provisional display and capacity waits from the Engine hints handle and committed Events from its Run subscription to the attached terminal over bounded in-process channels. The terminal reduces those canonical Events into its local SessionView and reconciles gaps through a Server snapshot request. The Server owns Run submission, approvals, steering, cancellation and settlement; idle Session operations retain their existing direct Engine calls. The Session shares its Engine with each connection and closes it only after the connection has settled and released its tasks. Before Engine admission, preparing feedback remains presentation-local; afterward activity derives solely from committed current-Run facts. Closed replay and typed CLI classification precede receipt/answer output. Linear output restores ownership first; visual modes retain their owned viewport while inserting classic transcript or emitting redirected confirmed stdout.

Guard's shared capacity wait is a separate one-slot intent-ID hint, validated
against the committed pending Tool before display. It cannot enter history or
authorize/replay work. Linux/macOS Guard own user-level private FIFO/reservations
and validated settings independently of Session roots; Engine owns immutable
intent/approval and aggregate Run deadlines. No native lock or process type
crosses into Engine. [Shared capacity](../specs/tools.md#shared-native-capacity-and-settings)
owns defaults, configuration, resource bounds and interruption semantics.

A terminal-owned clipboard worker has bounded trusted OS-client ownership distinct from the reader and Engine. Immutable image values cross semantic Provider/Session boundaries; human drawing receives metadata. Canonical image bytes, multimodal context and compaction remain under the image/Session/Store owners.

`OwnedTerminal` acquires and owns the active renderer, reader and terminal-mode guards, and restores them in lifecycle order. `AttachedTerminal` keeps signal listeners through committed output and transfers them during reacquisition; secret echo and clipboard ownership stay separate. Suspension retains the pinned Run future and bounded draft; lifecycle failures cannot manufacture a committed outcome. [Terminal spec](../specs/terminal.md#interruption-suspension-and-restoration) owns restoration behavior; [next steps](../../NEXT_STEPS.md) owns unverified claims.

### Display observations and terminal continuity

Provider adapters may publish a bounded typed preview to attached primary execution. The Engine coalesces transient observations, checkpoints safe text through canonical Events and validates the complete outcome before accepting an answer. Display-only checkpoints never enter future Provider context or compaction. Terminal owns provisional labels, sanitized Tool details, reading anchors and classic/fullscreen mode changes. Classic uses Ratatui Inline and native insertion while fullscreen owns an alternate screen; a narrow backend routes cursor observation through the sole input reader and scopes primary-screen clearing. This adds no process, terminal engine or second canonical history store.

## 9. Provider profiles and custom endpoints

Provider is the library's semantic inference boundary. The [Provider](../specs/provider.md), [ChatGPT](../specs/chatgpt.md) and [credential](../specs/credentials.md) contracts own behavior; this section records where state and authority live.

- **Profiles and routes.** One ProviderProfile binds protocol family, normalized endpoint and base path, credential reference, model, outcome encoding and privacy claim. Native profiles are compiled into `provider/`. Custom profiles (`provider-profiles.json`), role routing (`provider-routing.json`) and roster presets (`provider-roster.json`) are closed private StateRoot records, never Workspace files.
- **Admission flow.** The CLI and account edge reads those records and credentials, admits every configured route and hands the Engine an admitted Provider bundle before Workspace input. The custom adapter resolves and pins its destination for the Run before reading its key. `RunStarted` pins identity, account UUID, effort, endpoint and declaration digest as provenance; replay never turns provenance into credential authority.
- **Credentials.** The binary's `accounts/` owns saved native API accounts, ChatGPT OAuth and renewal, and the same-binary credential helper. Account records, the ChatGPT registration and account index, `model-preferences.json` and the replacement lock live in the effective OS user's account root, separate from every Session StateRoot. Keyring items stay in the OS store (Secret Service on Linux, Keychain Services on macOS), reached only through the helper. Environment references serve `exec` and explicit selections. The library receives validated credential values, never storage handles.
- **Validation.** There are no conformance pre-checks or stored Provider evidence; every real response passes the strict decoder. The ChatGPT catalog sizes budgets and never grants account authority.
- **Wire adapters.** OpenAI Responses (API key, and the ChatGPT plan in `provider/openai/subscription`), Anthropic Messages, custom Responses, Messages and Chat Completions, and scripted test adapters sit behind the Provider interface. [Configuration and egress](../specs/provider.md#configuration-and-egress) owns transport rules.

## 10. Telemetry and diagnostics

The private telemetry owner consumes committed Journal facts without blocking
appends. Its SDK batch worker owns the HTTP reactor; construction and synchronous
shutdown sit outside the Engine coordinator. [Telemetry](../specs/telemetry.md)
owns endpoint admission, span topology, privacy, bounds and failure behavior;
[D-14](../research/next-step-decision-register.md#d-14--otlp-is-optional-lossy-output)
owns the transport rationale.

Development diagnostics have a separate private StateRoot log owner. Engine and
CLI boundaries supply typed failure stages; optimized builds leave it disabled.
[Diagnostics](../specs/diagnostics.md) owns content and retention.

## 11. Security boundary

Policy admits authority before project input; the separate native Guard enforces
program effects and returns evidence for validated publication. Typed file effects
use admitted directory capabilities in process and return intent-bound receipts. The Engine journals the
transition and never treats replay as permission to dispatch. Account setup and
the credential helper own browser/callback and OS-store access respectively.

[Session admission](../specs/session.md#workspace-admission-and-authority),
[Tool enforcement](../specs/tools.md), [credentials](../specs/credentials.md) and
[Provider egress](../specs/provider.md#configuration-and-egress) own the contracts;
[security rules](../../agents/security.md) own the development review. Native and
human evidence gaps remain in [next steps](../../NEXT_STEPS.md).

## 14. Explicitly deferred

| Trigger | Introduce then |
|---|---|
| Local tool workloads exceed the proven base | Reviewed runtime/cache/artifact adapters, bounded larger profiles and actual workload evidence |
| Interactive or background jobs need network effects | Exact protocol/auth/data-disclosure grants and enforcing transport adapters |
| Children need nested delegation | Bounded recursive supervision and depth budgets; beta remains flat |
| Work needs dependencies/ownership transfer | Assignment DAG, attempts, leases, fencing |
| Knowledge must improve another Session | Scoped Memory plus retrieval/deletion evaluation |
| Event payloads exceed their bound | Content-addressed Artifacts |
| Replay misses a measured budget | Rebuildable snapshots |
| Labelled evaluation proves recent/lexical context insufficient | FTS/vector or hybrid derived retrieval |
| Second Client or detached execution exists | Per-user daemon and versioned local protocol |
| Built-in OpenRouter support is claimed | Exact route, privacy, provenance, strict-output, and no-fallback gates |
| Anthropic explicitly approves third-party subscription auth | Reassess native Claude subscription profile |
| Browser/remote/multi-tenant product exists | New identity, authorization, quota, encryption, retention, threat model |
