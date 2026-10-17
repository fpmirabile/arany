# Architecture refactor

## Metadata

- Status: `Draft`
- Owner: `fpmirabile`
- Last updated: `2026-10-17`
- Baseline: `origin/main` at `8857765` (A2 merged)
- Supporting material: [diagrams.md](./diagrams.md) (module map, runtime flow, process and privilege map, data model, target architecture, PR graph) and [evidence.md](./evidence.md) (measured fan-in, depth audit, function sizes, findings F1-F13 with `file:line`).

## Outcome and scope

**Outcome.** The Engine interface, Goal bookkeeping, Tool dispatch and crate shape become deep enough that the next roadmap items (in-process reads, event stream, hooks, `serve --stdio`, C2 workers, ledger diet) plug into one implementation each instead of a second copy. The journal, reducer, strict replay, Guard seam and `private_fs` are already deep and are kept.

**Harness responsibility served.** Agent execution, context, Tools, permissions and durable Sessions: the refactor keeps product policy in the Engine and platform, Provider and presentation details at their boundaries, and evaluates each change against failure, cancellation and recovery.

**Work included.** Fourteen PRs in four lanes (Tools, Engine, Edges, Shape), in the order in [Execution](#execution). Twelve are behavior-preserving and proven by existing corpora. Three change observable behavior on purpose and say so: in-process reads (PR7), the ledger diet (PR9) and commit-time telemetry spans (PR11). PR12 changes the CLI's Store-thread lifetime and PR7 changes Read bounds.

**Exclusions.** These stay out of this plan:

- Prompt caching. It needs nothing from this plan and runs in parallel from day one.
- The macOS Guard internals (`macos/unit.rs`, about 3,200 lines). They are frozen until C2 and need a Mac for evidence.
- Build cache internals, ChatGPT OAuth and credential internals (kept, only moved in PR13).
- `apply_committed` transition rules. Only its layout changes (PR2); rule changes go through the [spec workflow](../../docs/specs/README.md).
- `provider/streaming.rs` `feed`, the SQLite thread, Provider conformance checks, and a hand-written schema replaced by a derive. See [Not refactored yet](#not-refactored-yet).
- Typed Git reads moving in-process (decided, see [Decisions](#decisions)).
- The per-command unguarded fallback after a sandbox failure. It is decided separately and tracked in [NEXT_STEPS.md](../../NEXT_STEPS.md).
- `serve --stdio` itself (roadmap item 8). This plan only prepares its seams.

**Lifecycle.** This folder is execution coordination. When the work lands, accepted behavior moves into the owning specs, unfinished defects go to [NEXT_STEPS.md](../../NEXT_STEPS.md), and this folder is deleted under the [planning lifecycle](../README.md#lifecycle).

## Current facts and owners

**Verdict (measured; [evidence.md](./evidence.md)).** Overall grade B-. Deep and correct: the journal and reducer, the A2 Provider seam, the Guard privilege seam, `private_fs`, `presentation`, and `tools/fs.rs` and `tools/git.rs` as leaves. Shallow or duplicated: the Engine interface, Goal bookkeeping (two state machines), the Tool step (implemented twice), the terminal toolbox, and the crate shape. The problems are depth problems, not wrong architecture.

**Owners.** Behavior is owned by the specs; implementation shape by the module notes and code.

| Area | Owner for behavior | Owner for implementation shape |
|---|---|---|
| Journal, Events, reducer, replay | [Session spec](../../docs/specs/session.md), [Execution spec](../../docs/specs/execution.md) | `src/session/`, `src/store/` |
| Goal lifecycle, budgets, recovery | [Goal spec](../../docs/specs/goals.md) | `src/engine/run_loop.rs`, `src/session/reducer/goal.rs` |
| Tools, grants, Guard, MCP, Skills | [Tools spec](../../docs/specs/tools.md) | `src/tools/` |
| Provider seam and adapters | [Provider spec](../../docs/specs/provider.md) | `src/provider/` |
| Engine lifetime and Runs | [Execution spec](../../docs/specs/execution.md) | `src/engine.rs`, `src/engine/` |
| Telemetry and diagnostics | [Telemetry spec](../../docs/specs/telemetry.md), [Diagnostics spec](../../docs/specs/diagnostics.md) | `src/telemetry.rs`, `src/diagnostics.rs` |
| Terminal and CLI | [Terminal spec](../../docs/specs/terminal.md) | `src/terminal.rs`, `src/cli/` |
| Credentials and saved accounts | [Credential spec](../../docs/specs/credentials.md) | `src/cli/credentials/`, `src/cli/chatgpt/` |
| Private directories | [Credential spec (private directories)](../../docs/specs/credentials.md) | `src/private_fs.rs` |
| System shape | [System overview](../../docs/architecture/system-overview.md) | n/a |

**Verified facts the PRs depend on.** Every line reference was read on `8857765`.

- **Goal state is duplicated.** `GoalRunBudget` (`src/engine/run_loop.rs:159-181`, 17 fields, six mutexes, about 460 lines of methods) is a live copy of `GoalView` (`src/session/reducer.rs:41-76`), which the reducer already maintains through `ProgressPublisher::committed` (`src/engine/progress.rs:213-239`). The copy is kept honest by seven ledger Events and Event-tail literals (`src/engine/run_loop.rs:966, 1850, 1983, 2065`; `src/engine/run_loop/children.rs:733`; `src/engine/compaction.rs:128`; `src/engine.rs:2169-2175`).
- **The Tool step is implemented twice.** `execute_tool` and `execute_intent` (`src/engine/run_loop.rs:1434-1665`, `:2024-2314`) and `execute_child_tool` (`src/engine/run_loop/children.rs:675-787`) repeat sizing, admission, `ToolStarted`/`ToolFinished`, cancellation and continuation with different rules. The child path builds a fake `GuardReceipt` only to size a reservation (`children.rs:701-716`).
- **Every read launches a Guard unit.** `guard::execute` (`src/tools/guard.rs:482-495`) routes every call, including `List`, `Read` and `Search`, into the Guard, although `src/tools/fs.rs` already implements them with `cap-std` (`fs.rs:812` native path). A read pays the preparation budget (5 s on Linux, `src/tools/guard/linux.rs:19`; launchd and disk image on macOS).
- **The Engine's interface is a mutation sequence.** `Engine` is built by `open_with_telemetry`, then mutated by `enable_tools`, `set_tool_approvals`, `set_steering` and `enable_trusted_workspace` (`src/engine.rs:1977-2005`), each writing an `Option` field (`:1801-1810`). `enable_tools` silently resets `workspace_permissions`. `run_turn` is 467 lines (`:2110-2577`). Both CLI paths open and close an Engine per submission (`src/cli/attached/run.rs:328-368`, `src/cli/exec.rs:247-290`).
- **Provider identity is reassembled.** The `Provider` trait has 13 methods, seven of them identity getters (`src/provider.rs:698-760`), reassembled into `ProviderSelection` (`src/engine.rs:390-400`) and checked by `ProviderIdentity::valid()` (`src/provider.rs:259-290`). `RunControls` carries ChatGPT fields only to stamp compaction records (`src/engine/run_loop.rs:143-144`).
- **Tool knowledge has nine owners.** One operation's facts are spread across `src/tools/types.rs` (enum at `:138-208`, predicates at `:224-345`, schemas at `:1030-1145`, `CALL_ORDER` at `:1148`), `src/tools/config.rs:225-269`, `src/tools/approval.rs:30-34`, `src/session/events.rs:1055`, `src/session/reducer.rs:690-697`, `src/provider/tool_specs.rs`, `src/tools.rs:368-420` and the Guard dispatch at `src/tools/guard.rs:1017-1070`. Typed Git reads (A5) touched 28 files for this reason.
- **The crate graph has a cycle and a wrong shape.** `tools` depends on `engine` only for `RunCancellation` (`src/tools/guard.rs:5`, `linux.rs:5`, `macos.rs:5`, `macos/unit.rs:9`). `terminal` has no library consumer, and `presentation` has one. `keyring` is used only under `cli/credentials`; `ratatui` and `crossterm` only under `terminal`. `lib.rs:58-67` re-exports terminal items for the binary.
- **The journal is the hub.** `session/events.rs` embeds `tools` and `provider` types (`src/session/events.rs:2361-2494`), with fan-in 8 for `session` and 6 for `tools`.
- **Store evidence lives in Store.** Six `Request` variants serve Provider and native evidence caches (`src/store.rs:201-224`), and `StateRoot` has 25 pass-through methods (`src/store/state.rs:249-375`).
- **Observers are inline.** `append_observed` fans one committed Event to diagnostics, telemetry and progress (`src/engine/progress.rs:242-288`). `RunLoop.trace` and `AgentTrace` are threaded through control flow (63 `trace` tokens in `run_loop.rs`). `TraceEventKind` re-enumerates 20 of 26 Event kinds (`src/telemetry.rs:122`). `RunProgress` is a coalescing `watch` (`progress.rs:14-23`), so a client cannot see every Event.
- **Payload versions are global.** `EVENT_PAYLOAD_VERSION` is `6` for all 26 kinds (`src/session/events.rs:101`).
- **Test fixtures re-implement production knowledge.** `tests/common/semantic.rs` (127 lines) inverts the renderer by matching prompt prefixes. `tests/session_run/tools/replay.rs:17-21` and `:376-385` hard-code Event indices that re-derive the tail arithmetic.
- **Spec drift.** `agents/provider.md:18` says `Provider` is the only Engine behavior seam; `docs/architecture/system-overview.md` §2 says one long-lived process and labels children "one-call read-only". See [evidence.md](./evidence.md#findings-f1-f13) for the full F1-F13 list.

**Roadmap facts.** [NEXT_STEPS.md](../../NEXT_STEPS.md) lists the roadmap items that this plan unblocks and the macOS architecture follow-ups that stay frozen until a Mac is available. The plan maps each roadmap item to its PR in [Roadmap fit](#roadmap-fit).

## Contract and scenarios

This plan changes behavior in three places and otherwise preserves contracts. Each behavior change names its owning spec.

- **Owning specs and proposed changes:**
  - [Tools spec](../../docs/specs/tools.md): PR7 adds in-process `List`, `Read` and `Search`, raises the Read page limit and `MAX_TOOL_RESULT_BYTES` to 64 KiB with a derived `ToolFinished` bound, and changes the `read.limit` schema text (this invalidates the cached prefix once, by design). PR13 keeps Guard behavior in this spec.
  - [Goal spec](../../docs/specs/goals.md) lifecycle, budgets and recovery: PR9 changes Event count and active-time accounting granularity. This is the one PR where spec work is unavoidable.
  - [Telemetry spec](../../docs/specs/telemetry.md): PR11 moves span start and end timestamps to commit time. The architecture document already promises "a projection of committed safe facts".
  - [System overview](../../docs/architecture/system-overview.md) §2 and the [AGENTS.md](../../AGENTS.md) routing table: PR13 corrects the long-lived Engine, the Journal tap, the two-crate shape and the children's Tool allowance (F12).
- **Journal format.** Per-kind payload versions (PR5) replace the global constant. PR7 bumps `ToolFinished`; PR9 bumps `RunStarted`, `RunFinished` and `GoalFinished` and deletes two kinds. Unknown or stale versions are rejected as today, with no compatibility readers and no migrations (pre-beta rule in [AGENTS.md](../../AGENTS.md#global-rules)).
- **Successful path and meaningful rejection, failure and recovery scenarios:**
  - An in-process `Read` returns a result with `Attestation::InProcess`, and the Run replays identically after process exit.
  - The reducer rejects an `InProcess` attestation on any site other than a read-only operation (for example, on `Write`).
  - A `Read` through an `.env` name is refused (`forbidden`), a symlink escape is refused, and an overlap or device/inode change is refused exactly as in the Guard path.
  - A crash between `ToolStarted` and `ToolFinished` replays as Interrupted with the full reservation charged, and `Uncertain` still stops continuation.
  - A lagging journal subscriber receives `Resync` and recovers from the view; observers never block control flow.
  - `submit` on a busy Engine returns `EngineError::Busy`; the Provider may change between two submissions on one Engine.
  - A catalog or schema byte change is caught by the catalog snapshot test before it reaches the wire corpora.
- **Evidence owners.** The existing corpora in [Verification](../../docs/development.md#verification) and [Testing](../../agents/testing.md) own the proof. Live, native and manual checks stay in [NEXT_STEPS.md](../../NEXT_STEPS.md) and are not blockers for day-to-day PRs.
- **Deferred checks.** Read p95 at or below 50 ms (roadmap item 2), the native Guard lane for mutations, the macOS release-build Tool journeys, and the manual native lanes for the workspace split (PR13).

## Chosen direction

### Simplest sufficient solution

Apply Ponytail (`.agents/skills/ponytail/SKILL.md`, installed per [project skill setup](../../agents/skills/README.md#install-for-a-harness)) to each PR. The plan adds only the complexity a measured constraint requires:

- One `const` facts table replaces nine owners of Tool knowledge.
- One `GoalLedger` replaces a state machine that the reducer already maintains.
- One `AgentStep` executor replaces two Tool paths.
- One `Journal` tap replaces three inline observers.
- One `RunHandle` replaces four setters and three `run_*` variants.
- One `ProviderIdentity` replaces seven getters.
- One `RecordKind` table replaces 25 methods.

No new trait is introduced in this plan except the one hook seam in roadmap item 6 (see [Traits, enums and functions](#traits-enums-and-functions-for-the-other-reuse-points)).

### Target architecture

The target shape is three crates at the end. Diagrams are in [diagrams.md §5](./diagrams.md#5-target-crates-modules-and-dependency-direction-after-pr14).

- **`arany-cli`** (binary `arany`): `cli/attached` (interactive flows), `cli/exec`, `terminal`, `presentation`, and `accounts` (today `cli/chatgpt` and `cli/credentials`). It takes `serve` in roadmap item 8.
- **`arany`** (library, name kept so `use arany::` in the corpora stays valid): `engine` (with `ledger`, `step`, `run_loop`, `children`, `compaction`, `lifecycle`), `journal` (the bounded tap), `provider`, `tools` (with `catalog`, `dispatch`, families, `guard`), `store` (SQLite thread and `StateRoot`), `telemetry`, `diagnostics`, `private_fs`.
- **`arany-journal`** (PR14, conditional): `events`, `reducer`, `views`, the vocabulary (`ToolCall`, `OperationFacts`, `EffectIntent`, `ToolObservation`, `Attestation`, `ProviderIdentity`, `Usage`, `ImageAttachment`) and `replay`. Its dependencies are serde, serde_json, sha2, uuid and thiserror only.

**Why this cut and not `engine` and `cli` alone.** Three measured facts decide it:

1. `terminal` and `presentation` have no library consumer. Moving them to the binary costs the library nothing and removes ratatui, crossterm and keyring from the library build.
2. The journal vocabulary is the real hub. A split that keeps `tools` whole in an engine crate drags bwrap and Seatbelt into every consumer of `Event`.
3. Crate creation needs measured pressure ([AGENTS.md](../../AGENTS.md#module-boundaries)). The binary split has compile pressure today (a reducer test rebuilds the macOS FFI, the systemd launcher and the OTLP exporter). The journal crate has release and ownership pressure only when `serve --stdio` starts, so PR14 waits for roadmap item 8.

**Naming (approved).** The library keeps `arany`. The binary package is `arany-cli` with `[[bin]] name = "arany"`. The roadmap's `arany-engine` name is not used. `cli/chatgpt` and `cli/credentials` become `accounts`, because they never construct an Engine.

**Interface changes, with the reason each seam exists:**

- `Engine::open(state, EngineConfig)` and `submit(providers, request, controls) -> RunHandle` (PR12). `ToolGrant` is `None`, `Configured` or `Trusted(WorkspacePermissions)`. `RunResult { run, stopped }` replaces the `StoppedAfterRun` error. The config is a value, and the per-role Provider chains may change per submission. Design A (submit returns a handle) is chosen now. Design B (an actor Engine over `mpsc`) waits until `serve --stdio` needs a second client.
- `Journal::append` and `subscribe` (PR11). Observers own `broadcast::Receiver`s with a 256-envelope ring and an explicit `Resync` on lag. Design A (a tap in `Journal`) is chosen over design B (a subscriber inside the Store thread), because B makes the Store the owner of a presentation concern.
- `Provider::identity() -> &ProviderIdentity` (PR3), with `Provenance` as an enum and limits in `CallLimits`. Adapters implement one method returning a value they already own. `invoke`, `invoke_with_preview` and `compact` remain. `RunConfig`'s serialized shape stays byte-identical in PR3; it is pinned from the identity in PR9.
- `StateRoot::record(RecordKind) -> Record` with `read`, `replace`, `remove` and `present` (PR4). Each kind carries its bound and file name in one table. Provider evidence becomes `Request::Evidence(EvidenceOp)` with a `provider::evidence` module that owns its records. Test faults become one `FaultPlan` behind a `test-support` feature.

**Tools design choice: closed enum, `const` facts table, per-family modules.** The design was tested three ways.

- *A. `trait Tool` with a `dyn` registry.* Rejected. Every method re-matches the enum to extract its payload, `dyn` dispatch buys nothing for a closed set, and the registry must agree with the enum (the same bug class as `CALL_ORDER`). Deleting the trait moves the complexity back into the enum, so it is not a deep seam.
- *B. Closed enum with a `const` facts table and per-family modules.* Chosen.
- *C. Enum of structs with a `trait Operation` per variant.* Deferred. It is the deepest option for a family that grows, but costs about 400 lines of churn and changes every pattern match on `ToolCall` (reducer, events, config, fs, guard, presentation, terminal). Adopt it per family only when a family's payload gains a third caller.

Chosen shape (PR6):

```rust
// src/tools/catalog.rs: the one place that knows what an operation is
pub struct OperationFacts {
    pub name: &'static str,      // wire `operation` and journal label
    pub family: Family,          // File | Process | Skill | Mcp | Git
    pub site: Site,              // InProcess | Guard: where it may execute
    pub mutates: bool,
    pub spawns_process: bool,
    pub prelude: bool,           // allowed before a Goal plan contract
    pub researcher: bool,        // offered to read-only children
    pub asks_approval: bool,
    pub order: u8,               // model-facing position; byte-stable prefix
}
impl ToolCall { pub const fn facts(&self) -> &'static OperationFacts { /* exhaustive match */ } }

// src/tools/dispatch.rs: one router for the Guard helper and the in-process path
pub(crate) async fn run(call: &ToolCall, site: &SiteContext<'_>) -> Result<Output, ToolError> {
    match call.facts().family {
        Family::File => fs::run(call, site), /* Process, Skill, Mcp, Git */
    }
}
```

Leverage: the reducer, approval, resource settings, `tool_specs`, the Guard helper, the in-process executor and the catalog prose all read one table. Locality: a new operation is one enum variant, one facts constant, one family arm and one schema fragment, and the compiler points at each missing arm. `ToolCall` serde, `deny_unknown_fields` and strict replay are unchanged. `Config::allows` keeps only scope checks per family.

#### Traits, enums and functions for the other reuse points

| Place | Adapters today | Shape | Reason |
|---|---|---|---|
| Provider | 5 (OpenAI, ChatGPT subscription, Anthropic, Custom, Scripted), chosen at runtime | `Arc<dyn Provider>` (exists since A2), four methods after PR3 | Runtime-selected and open to a sixth; one vtable call per network round trip is nil |
| Hooks (roadmap item 6) | 3 on day one: human approval, AI review, external command | `Vec<Box<dyn Hook>>` with a closed `Verdict` enum | The only new `dyn` seam in the plan: an open, user-configured list. Verdicts stay closed so journaling and "only narrow" are exhaustive. PR10 prepares the call site |
| Guard platform | 2 (`linux.rs`, `macos.rs`) | `cfg`-selected functions (as today) | The kernel chooses the adapter, never a runtime; a trait would be hypothetical |
| Tool execution site | 2 (in-process, Guard) | enum and `if` in `ToolRuntime::execute` | Site is a property of the operation, not configuration; a fake Guard would let Engine tests pass against enforcement that never ran |
| Journal observers | 3 (progress, telemetry, diagnostics) | `broadcast::Receiver<Arc<EventEnvelope>>`, no trait | The interface is a channel; a trait would re-invent `Receiver` |
| Store backend | 1 (SQLite thread) | concrete type | One adapter, deep and correct |
| Reducer | 26 Event kinds, 5 aggregates | one `match` dispatching to per-aggregate functions (PR2) | Exhaustiveness is the safety property |

**Documentation rule (approved): no `//!` module docs.** Module intent comes from names. Behavior comes from [`docs/specs/`](../../docs/specs/README.md). Implementation invariants come from tests. Code gets at most one short `//` comment where the reason is non-obvious, and no long doc blocks. `agents/` holds only general working practice. PR13 applies this to the documentation migration: [AGENTS.md](../../AGENTS.md) and the [documentation rules](../../agents/testing.md#documentation-and-instruction-changes) govern the rest.

**Readability bounds (apply from PR1).**

- Function length: 150 lines for control flow, enforced as `clippy::too_many_lines` warnings in the `engine` and `tools` modules. A state-machine match (the `apply_committed` arms, the SSE `feed`) may exceed it when it is one cohesive transition table, with a one-line `#[expect]` reason.
- File length: 1,500 non-test lines, with tests in a sibling `tests.rs`. PRs split the six files over the bound on the critical path: `engine.rs`, `run_loop.rs`, `reducer.rs`, `guard.rs`, `types.rs`, `events.rs`.
- Module names are nouns for what they own (`ledger`, `step`, `catalog`, `dispatch`, `journal`). No `util`, `helpers`, `common` or `misc`.
- Every public data type derives `Debug`; journal types derive `Serialize, Deserialize` with `deny_unknown_fields`.
- `pub` only for what the binary or tests use through `lib.rs`; `lib.rs` shrinks from about 160 re-exports to about 90.

#### Test corpora per crate (approved)

Each crate owns a `tests/` directory. A test set moves only where a crate boundary exists. Two rules decide placement:

- A corpus that launches the `arany` executable (`env!("CARGO_BIN_EXE_arany")`) or drives the terminal belongs to `arany-cli`.
- Anything else that exercises Engine, Session, Store, Provider or Tools in process stays with `arany`.

Placement:

- `arany`: `tests/engine_run.rs` and `tests/engine_run/` (`goals.rs`, `replay.rs`, `workspace_security.rs`, `performance/`), except for the Guard-launching cases below. In-module `tests.rs` files: `src/tools/tests.rs`, `src/store/journal/tests.rs`, `src/telemetry/tests.rs`, `src/provider/render/tests.rs`, `src/provider/{anthropic,openai}/wire/tests.rs`, `src/provider/openai/subscription/tests.rs`.
- `arany-cli`: `tests/session_run.rs` and `tests/session_run/` (layout unchanged), `tests/platform_tls.rs`, `tests/release_build_guard.rs`, the five `goals.rs` cases that open the Engine with `guard_executable: Some(env!("CARGO_BIN_EXE_arany"))` and submit with `ToolGrant::Configured` (lines 230, 1337, 1643, 3112, 3202) and the launch at `engine_run.rs:1022`, which move to `tests/guard_runs.rs`. This is the only split inside a corpus. In-module: `src/terminal/view/tests.rs` and `src/cli/chatgpt/**/tests.rs` (which become `accounts`).
- Shared support: `tests/common/process.rs` and `tests/common/git.rs` become `arany` modules behind the `test-support` feature, introduced in PR4. `tests/common/semantic.rs` moves to `src/provider/render/semantic.rs` in PR3, behind `cfg(any(test, feature = "test-support"))`. `arany-cli` takes `arany = { path = "../arany", features = ["test-support"] }` under `[dev-dependencies]`.
- `arany-journal` (PR14) gets its own `tests/` for the reducer and cold-replay tests that move out of `src/session`. SQLite admission tests stay in `arany`.

### Linux and macOS impact

Behavior is common to both targets. The only platform-specific code is the `cfg`-selected Guard adapters, which do not change shape.

- PR1 replaces a Linux-only 5 s constant with a per-site method. macOS's 30 s-derived overhead keeps its value. Both platforms need the Guard test corpus to pass on native lanes.
- PR7 adds in-process reads on both platforms through the same `cap-std` code. The native Guard lane keeps proving mutations on both platforms.
- PR13 moves code and changes CI test paths. The manual native lanes run once after the move.
- macOS Guard internals stay frozen (see [Not refactored yet](#not-refactored-yet)). Unavailable capabilities fail closed; no native support is claimed from the synthetic corpora. Native evidence is recorded in [NEXT_STEPS.md](../../NEXT_STEPS.md), not here.

### Decisions

These answers are owner decisions. They are final for this plan.

1. **Per-kind payload versions (PR5): approved.** Each Event kind has one current payload version in one table. A change bumps that kind only. No global bump, no compatibility readers, no migrations.
2. **Crates (PR13, PR14): approved.** The library `arany` keeps its name. The binary package `arany-cli` produces `arany`. `arany-journal` arrives with `serve --stdio`. Each crate owns a `tests/` corpus, and shared support sits behind `test-support`.
3. **Documentation: no `//!` docs** (see the rule above).
4. **Typed Git reads stay in the Guard.** NEXT_STEPS item 2 lists typed Git reads among in-process operations. The guarantee says every process runs in the Guard, and a Git read spawns `git` on a neutralized private view (`src/tools/git.rs:124, 405`). PR7 therefore moves only List, Read and Search. A warm per-Run Guard unit may cut the Git round trip later without weakening the boundary.
5. **Per-command unguarded fallback: outside this plan.** Tracked in NEXT_STEPS. The Tools spec owns it once implemented. Nothing here implements or depends on it.

### Not refactored yet

Each row names the trigger that reopens it.

| Area | Why not now | Revisit when |
|---|---|---|
| `AttachedTerminal` toolbox to screen objects; `cli/attached.rs run` (821 lines), `active.rs submit_objective` (523) | Roadmap item 8 makes the TUI a client of `serve --stdio`; those flows are rewritten against `RunHandle` then. PR13 does the free physical move only | Item 8 starts; adopt screens flow by flow as each is rewritten as a client |
| `provider/streaming.rs feed` (540 lines) | One cohesive SSE state machine with a large corpus; no roadmap item touches its framing | A second framing (for example WebSocket) appears |
| macOS Guard (`macos/unit.rs`, 3,184 lines; two `helper_main`s) | Frozen until C2; needs a Mac for evidence. PR6 already removes the dispatch duplication | The freeze lifts; then do the two NEXT_STEPS items (one Seatbelt profile builder, one executable-identity table) |
| Build cache (`store/state/build_cache.rs`, 2,795 lines) | Frozen; correct; no consumer pressure before a real build is measured | After native build evidence |
| ChatGPT OAuth and credential internals | Deep internally; PR13 moves them to `accounts` unchanged | A second OAuth route lands |
| `apply_committed` rules | The transition table is the product's safety property; rewriting rules is a behavior change | Only through the spec workflow, rule by rule |
| Store SQLite thread and `Request` enum (19 variants) | One adapter, deep and correct; PR4 trims six variants; a trait would be hypothetical | A second backend or a daemon (actor Engine) |
| Provider conformance checks (`native_check.rs`, `custom/check.rs`) | Large but a leaf; a breadth liability, not a shape problem | Routing (item 9) adds routes |
| Hand-written JSON Schema replaced by `schemars` | The schemas carry model-facing descriptions and byte-stable ordering a derive cannot guarantee; the cached prefix depends on the bytes | Caching has landed and a snapshot pins the bytes |

## Security and resource model

**Trust boundaries.** The Guard remains the mandatory boundary for every mutation, process, Skill script and MCP call. In-process reads are the one new execution site and run with the Engine's ambient privileges instead of bwrap's. [Security pass triggers](../../agents/security.md#security-pass-triggers) apply to PR7, and the pass must confirm the points below. Credential, permission, Tool, file, network, memory and untrusted-content rules are owned by [Security](../../agents/security.md).

- The in-process `cap-std` `Dir` is opened once, with ambient authority, at `admit`. No path joins happen outside the `Dir`.
- Path, overlap, no-follow and device/inode pin checks are the same as the Guard path (`fs.rs` entry points already enforce them). The runtime re-checks device and inode before each in-process call, the same `ChangedInput` rule the helper applies at `guard.rs:898-905`.
- Result bounds are enforced before serialization. `.env` names are refused by the existing `forbidden` rule.
- Symlink policy is identical. Disclosure does not widen: the same `workspace_paths` scope applies.
- The reducer rejects an `InProcess` attestation for any operation whose `site` is not `InProcess`, so a read receipt cannot authorize a write.
- `SystemBlock` stays a closed enum, so untrusted text cannot become system text by construction. `ProviderTurn` stays opaque and is never journaled.
- Approvals remain an `mpsc(1)` channel. Hook verdicts (roadmap item 6) stay a closed enum.
- Credential storage does not change: the account-root and `StateRoot` records and the OS keyring keep their current owners, and the credential helper stays the same binary under supervision. The library drops its keyring dependency.

**Bounds and backpressure.**

- Store request channel: `mpsc(64)`, unchanged.
- Journal tap (PR11): a 256-envelope broadcast ring. Lagging receivers get `Resync { missed }` and resync from the view; they never block control flow.
- Read page limit and `MAX_TOOL_RESULT_BYTES` (PR7): 64 KiB, with a derived `ToolFinished` bound.
- Children: at most 8 children and 16 read-only calls each, as today.
- MCP stdio servers: at most 4, pinned.
- Event cap: 10,000 per Session as today; after PR9 the same cap admits about 3,000 Tool steps instead of about 1,000.
- Reservation overhead comes from the runtime, per site (PR1). It is zero for in-process calls (PR7), which removes the 5 s from every read's deadline arithmetic.
- Build cache quotas and admission are unchanged.

**Failure containment.**

- `Uncertain` stops continuation and charges the full reservation.
- A crash between `ToolStarted` and `ToolFinished` replays as Interrupted with the reserved time charged.
- A concurrent `submit` is refused with `Busy`; session locks remain per Run, and the Store thread lives for the process (PR12).
- Telemetry and diagnostics are observers. Their failure or lag cannot change a Run's outcome.
- `unsafe` stays isolated in `tools/guard/macos/ffi.rs` and `private_fs/acl.rs`. No new `unsafe` is introduced by this plan.

## Risks and mitigations

| Risk | PR | Mitigation |
|---|---|---|
| Model-facing bytes change by accident (catalog prose, schema text, Tool order), breaking caching and the wire corpora | PR6, PR7 | Snapshot the rendered catalog and `tool_specs` output before the PR and assert equality. PR7's schema change is announced as a one-time prefix invalidation |
| In-process reads widen disclosure | PR7 | Security pass on `fs.rs` entry points (see above); the reducer rejects `InProcess` attestations for any non-read site; the native Guard lane keeps proving mutations |
| `GoalLedger` decisions drift from `GoalRunBudget` by one Event (the not-yet-appended window) | PR8 | `in_flight` is the only state outside the view; the Goals corpus asserts budgets under paused time; a table test asserts `max_events` at least the observed Events per action |
| Ledger diet changes crash semantics | PR9 | Explicit test: a crash between `ToolStarted` and `ToolFinished` replays as Interrupted with the full reservation charged; `Uncertain` still stops continuation |
| Telemetry assertions depend on in-loop timing | PR11 | The corpus asserts topology and attributes; the diff is expected to be timestamps only. A test that asserts wall-clock time is testing past the interface and is replaced |
| A long-lived Engine holds a Store thread across idle time in the TUI | PR12 | Session locks stay per Run; `close` on exit as today; `Busy` for concurrent `submit` |
| The workspace split breaks native CI lanes that name test paths | PR13 | Scaffold first (lints, `--workspace`); update the `ci.yml` test paths in the same PR; run the manual native lanes once |
| Two coders in `run_loop.rs` | PR7, PR8, PR10 | Land PR7 before PR8's `run_loop.rs` edits or rebase; PR10 is the joint deletion |
| A refactor reads as a behavior change and is reviewed as one | All | Each PR states whether it is behavior-preserving. Only PR7, PR9 and PR11 are not, and each names its spec change |

## Execution

Sizes: S is at most 2 days, M is at most 2 weeks, L is more than 2 weeks. Every PR stacks cleanly on `main`. "Corpus" names the existing tests that prove no behavior change.

**Critical path:** PR1, PR6, PR7, PR10, PR11, PR12, PR13, with PR8 feeding PR10 and PR9 between PR8 and PR11. One coder: about 12 to 14 weeks. Three coders on disjoint files: about 7 to 8 weeks. Caching runs in parallel from day one.

**Ordering rules.**

1. Vocabulary before paths. The facts table (PR6) lands before the second execution site (PR7) and the shared step (PR10). Per-kind versions (PR5) land before the two PRs that bump a kind.
2. One state machine before the Event shape changes. PR8 removes `GoalRunBudget` with byte-identical journals. Only then does PR9 change Events.
3. Interface last. PR12 reshapes `Engine` after the loop, ledger and tap are stable. PR13 splits crates after `lib.rs` stops churning.

**Lanes.**

| Lane | PRs | What it buys |
|---|---|---|
| Tools | PR1, PR6, PR7 | Roadmap item 2 (reads in milliseconds); one dispatch table; the vocabulary hooks and C2 need |
| Engine | PR2, PR8, PR9, PR10, PR11, PR12 | Items 3 and 5 (engine hygiene, event stream, ledger diet); the seams hooks (6) and `serve --stdio` (8) plug into |
| Edges | PR3, PR4, PR5 | Smaller fakes, one record routine, additive journal fields without a global bump |
| Shape | PR13, PR14 | The library compiles without ratatui, crossterm or keyring; `generate-schema` and an ACP adapter get the vocabulary without Guard code |

**Three-coder schedule.**

| Weeks | Coder A (Tools) | Coder B (Engine) | Coder C (Edges) |
|---|---|---|---|
| 1-2 | PR1, PR6 | PR2, then PR8 design | PR3, PR4, PR5 |
| 3-4 | PR7 (needs PR5, PR6) | PR8 (needs PR2, PR3) | Caching, no dependency |
| 5-6 | PR10 with B (needs PR7, PR8) | PR9 | PR13 scaffolding (workspace, lints, CI paths) |
| 7-8 | PR11 | PR12 | PR13 documentation migration |
| 9 | PR13 move (one coder) | Hooks (roadmap 6) on `execute_step` | PR14 when item 8 starts |

**Shared hot spots.** `src/engine/run_loop.rs` (PR7 deadlines, PR8 budget calls, PR10 deletion), `src/session/events.rs` (PR5, PR7, PR9), `src/lib.rs` (PR3, PR12, PR13). Everything else is disjoint by lane.

### The 14 PRs

Each entry gives the PR, lane, size, risk, dependencies and verification. Behavior-preserving unless stated.

**PR1. Cut the `tools` to `engine` cycle and make reservation overhead a runtime method.** Lane Tools. Size S. Risk none. Depends on nothing.
- Move `RunCancellation` to `src/tools/cancel.rs` as `tools::Cancellation`, re-exported under the old public name. Replace `PREPARATION_OVERHEAD` with `ToolRuntime::reservation_overhead(&self, call) -> Duration`.
- Verification: corpus `tests/engine_run.rs`, `goals.rs` (paused-time deadline arithmetic), `tests/session_run/tools`.

**PR2. Reducer by aggregate.** Lane Engine. Size S. Risk very low. Depends on nothing.
- Split `SessionView::apply_committed` (`reducer.rs:668-1547`, 879 lines) into per-aggregate `apply` functions (session, goal, run, agent, tool), keeping the single entry point and cross-aggregate guards; `child.rs` merges into `agent.rs`. Pure code motion.
- Verification: `src/session/reducer` unit tests, `tests/engine_run/replay.rs`, `tests/session_run/tools/replay.rs`, cold-replay assertions in every journey.

**PR3. Provider identity as one value; semantic inverse beside the renderer.** Lane Edges. Size S to M. Risk low. Depends on nothing.
- Replace seven identity getters with `identity() -> &ProviderIdentity`. Delete `ProviderSelection` and the `RunControls` provenance fields. Move `tests/common/semantic.rs` into `src/provider/render/semantic.rs` with a round-trip test, `semantic(render(input)) == input`.
- Verification: wire corpora per adapter, `tests/engine_run.rs`, `tests/session_run/{custom,live,loopback}.rs`, billing-route display tests, and the new round-trip test.

**PR4. `StateRoot` records, Store evidence and one fault seam.** Lane Edges. Size S to M. Risk none behavioral. Depends on nothing.
- Replace 25 record methods with `RecordKind` and `Record`. Replace six Store evidence requests with `Request::Evidence(EvidenceOp)`, and move the record types to `provider::evidence`. Replace four fault hooks with one `FaultPlan` behind the `test-support` feature.
- Verification: `store/state` tests, `tests/session_run/setup.rs`, `engine.rs` goal-lease tests, `tests/session_run/disk_fault.rs`.

**PR5. Per-kind payload versions.** Lane Edges. Size S. Risk low. Depends on nothing; PR7 and PR9 depend on it.
- Replace `EVENT_PAYLOAD_VERSION` with a `payload_version(kind)` table. `decode` compares against the kind's current version. Stale versions are rejected, data untouched.
- Verification: `store/journal` crash tests, `tests/engine_run/replay.rs`. New test: a bumped kind rejects the previous version and an unbumped kind still decodes.

**PR6. Tool operation facts table and per-family dispatch.** Lane Tools. Size M. Risk medium. Depends on PR1.
- Add `tools/catalog.rs` (`OperationFacts`, the `ALL` table, schemas, prose) and `tools/dispatch.rs`. Family modules gain `valid`, `schema` and `run`. `helper_main` shrinks to protocol, `dispatch::run` and receipt (about 180 lines). Six predicates, two string arrays and `child_research_operation` become reads of `facts()`. Every site is still the Guard in this PR.
- Verification: snapshot the rendered catalog and `tool_specs` output before the PR and assert byte equality. Corpus `tool_specs_cover_every_catalog_operation`, `the_catalog_filters_tools_and_pins_names_without_changing_the_order`, wire corpora, reducer prelude tests, `src/tools/tests.rs`, `tests/session_run/tools*.rs`. New: an `ALL` to enum permutation test.

**PR7. In-process reads with attestation.** Lane Tools. Size M. Risk medium; security pass required. Depends on PR1, PR5, PR6. Behavior change (stated).
- `List`, `Read` and `Search` run in the Engine process under the admitted `cap-std` `Dir`. `ToolObservation.guard` becomes `attestation: Option<Attestation>`, with `Attestation::{InProcess {..}, Guard(GuardReceipt)}`. Dispatch happens in `ToolRuntime::execute` by `facts().site`. Read bounds rise to 64 KiB. Typed Git reads stay in the Guard. The design is in [PR7 design](#pr7-design-in-process-reads) below.
- Verification: `src/tools/tests.rs` and `tests/session_run/tools*.rs` for both sites; native Guard lane for mutations on both platforms; `tests/engine_run/replay.rs`; reducer tests. New: replay with `InProcess` attestations survives process exit; an `InProcess` attestation on `Write` is rejected by the reducer; Read p95 at or below 50 ms (roadmap item 2).

**PR8. `GoalLedger` over the reducer's view; `Action::max_events`.** Lane Engine. Size M. Risk medium. Depends on PR2, PR3. Byte-identical journals.
- Add `engine/ledger.rs` with `admits`, `reserve` and `settle` over `Action` (`ProviderCall`, `Tool`, `Review`, `Compaction`, `Pause`, `Delegation`), with `max_events` as the one table. Delete `GoalRunBudget` (about 460 lines). Split `run_turn` into `admit_turn`, `prepare_turn` and `settle_turn`, and move `goal_report` to `engine/report.rs`. `ProgressPublisher` shares the ledger's view.
- Verification: `tests/engine_run/goals.rs` (61 tests), `engine_run.rs`, `replay.rs`, `tests/session_run/tools/replay.rs`, `auto_compaction.rs`. New: `max_events` is at least the observed Events for every action kind, in one table test. `goal_budget_tests` move to `ledger/tests.rs`.

**PR9. Ledger diet.** Lane Engine. Size M. Risk medium. Depends on PR5, PR8. Journal change (stated).
- Run-level active deadline computed once in `reserve`; `RunStarted` gains `active_deadline_ms`; per-action reservations become in-memory; `RunFinished` carries `active_ms`; `GoalActiveTimeReserved` and `GoalActiveTimeSettled` are deleted. Bumps `RunStarted`, `RunFinished` and `GoalFinished`. Updates the Goal spec's lifecycle, budgets and recovery section.
- Verification: `goals.rs` with updated expected sequences (the diff is the spec), `replay.rs`, the performance history fixture. New: at most 4 Events per Tool step; a crash between `ToolStarted` and `ToolFinished` replays as Interrupted with the reserved time charged.

**PR10. One `execute_step` for primary, researcher and (later) worker.** Lane Engine. Size M. Risk medium. Depends on PR7, PR8.
- `engine/step.rs`: `execute_step(AgentStep, request, turn)` replaces `execute_tool`, `execute_intent` and `execute_child_tool`. The step sizes by `ToolContext::projected_len`, asks the ledger, journals `ToolStarted`, calls `before_dispatch(grant)` (the single hook point for approval, AI review and timeout denial), runs `runtime.execute` under cancellation, journals `ToolFinished`, integrates changesets for the primary, and pushes `ToolStep`. `Grant` is `Primary`, `Researcher` (and `Worker` in C2). `run_children` splits into `spawn_children`, `drive_children` and `synthesize`. `compact_tool_context_if_needed` moves to `engine/tool_compaction.rs` and splits into `select_compaction_window`, `summarize` and `record_compaction`. The fake receipt is deleted.
- Verification: `engine_run.rs` (38 tests), `goals.rs`, `replay.rs` (child research), `tests/session_run/tools*.rs`, `agent_inspector.rs`. New: one table test per `Grant` variant, with the Event sequence of one step equal to the pre-PR snapshot.

**PR11. Journal tap: progress, telemetry and diagnostics as subscribers.** Lane Engine. Size M. Risk low to medium. Depends on PR8, PR10. Span timing change (stated).
- `Journal { store, tap: broadcast::Sender<Arc<EventEnvelope>> }` with `append`, `subscribe` and `admit`. `ProgressPublisher`, `telemetry::RunTrace` and `diagnostics::event_failure` each become a subscriber. `TraceEventKind` becomes `From<&Event>`. Trace parameters leave the loop. The two in-loop timings not derivable from Events stay as attributes on the next Event.
- Verification: `src/telemetry/tests.rs` (1,007 lines), `tests/session_run/telemetry.rs` (643 lines), and every journey that watches `RunProgress`. New: a lagging subscriber receives `Resync` and recovers; `exec --output jsonl` equals the stream.

**PR12. `Engine` as a long-lived service with `RunHandle`.** Lane Engine. Size M. Risk low to medium. Depends on PR11.
- `Engine::open(state, EngineConfig)` once per process; `submit(providers, request, controls) -> RunHandle`; `RunResult` replaces the `StoppedAfterRun` error. Delete the four setters and the `run_*` variants. The CLI opens one Engine in `attached::run` and `exec::run`. `compact_with_provider` uses the same Engine. `is_some_and(|(config, _)| config.write)` becomes a `match` on `ToolGrant`.
- Verification: `engine_run.rs`, `goals.rs`, `tests/session_run/{active_terminal,exec_interrupt,session_picker,startup_trust}.rs`. New: `submit` while busy is refused; the Provider changes between two submissions on one Engine.

**PR13. Workspace split: `arany` library and `arany-cli` binary; documentation ownership migration.** Lane Shape. Size M. Risk low. Depends on PR12. Scaffolding (workspace, lints) may land earlier.
- `crates/arany` (library) and `crates/arany-cli` (`[[bin]] name = "arany"`, with `cli/`, `terminal/`, `presentation/`, `accounts/`). Test corpora move as in [Test corpora per crate](#test-corpora-per-crate-approved). `tests/common` moves behind `test-support`. Guard helper entry points stay reachable from the binary's `main.rs`. Workspace-level `[workspace.lints]` carries the Clippy settings from the readability bounds. `ci.yml` test paths update in the same PR. Documentation: correct [system overview](../../docs/architecture/system-overview.md) §2 and the AGENTS.md routing table (F12). Nothing in the Engine changes.
- Verification: `cargo test -p arany` and `cargo test -p arany-cli`. New: `cargo tree -p arany -e normal` has no `ratatui`, `crossterm` or `keyring`, asserted in CI. Manual native lanes run once.

**PR14. `arany-journal` vocabulary crate (conditional).** Lane Shape. Size S to M. Risk low (code motion). Depends on PR13. Starts only when roadmap item 8 starts.
- Move `events`, `reducer`, `views`, `tools::types` and `tools::catalog` vocabulary, `provider::{ProviderIdentity, Usage, ImageAttachment, AgentPhase, OutputTokenBound}`, `tools::cancel` and `replay` into `crates/arany-journal`. The SQLite thread stays in `arany`. `arany` re-exports them, so callers do not change.
- Verification: full corpus. New: `cargo tree -p arany-journal` has no `tokio`, `reqwest` or `rusqlite`; the reducer and cold-replay tests live in its `tests/`.

### PR7 design: in-process reads

Design A, a `trait ToolExecutor` with `InProcess` and `Guard` adapters, was rejected. The two are not interchangeable: the site is a property of the call, and a fake Guard would let Engine tests pass against enforcement that never ran. Design B, dispatch inside `ToolRuntime::execute` by `facts().site`, is chosen:

```rust
pub enum Attestation {
    InProcess { policy_digest: [u8; 32], workspace_device: u64, workspace_inode: u64 },
    Guard(GuardReceipt),
}
impl ToolRuntime {
    pub(crate) async fn execute(&self, intent: EffectIntent, cancel: Cancellation) -> ToolObservation {
        if !self.config.allows(&intent.call) { return denied(intent) }
        match intent.call.facts().site {
            Site::InProcess => self.run_in_process(intent),   // dispatch::run on the Dir opened at admit
            Site::Guard => guard::execute(self, &intent, cancel).await.unwrap_or_else(|e| uncertain(intent, e)),
        }
    }
}
```

The runtime opens the workspace `Dir` once at `admit`, and `reservation_overhead` returns zero for in-process calls. `src/provider/test_support.rs:131-148` changes to build `Attestation::InProcess`.

### Roadmap fit

Each roadmap item lands on a named PR, and the refactor removes the duplication from its path.

| Roadmap item (NEXT_STEPS) | Lands on | What the refactor removes from its path |
|---|---|---|
| Prompt caching (separate work) | `provider/render`, wire adapters | Nothing to wait for; runs in parallel. PR3 shrinks the ScriptedProvider the cache tests use |
| 2 In-process reads | PR7, on PR6's `facts().site` and PR1's cycle cut | Without PR6, the in-process path would be a tenth hand-written match |
| 3 Engine hygiene, event stream | PR11 (`Journal` tap) and PR12 (`RunHandle`) | `exec --output jsonl` and telemetry become receivers of one stream |
| 4 Spec diet | Documentation changes in PR13 | `agents/<module>.md` behavior moves to specs and invariants to tests; `agents/` keeps general practice only |
| 5 Ledger diet | PR8, then PR9 | Tail literals and `GoalRunBudget` are gone before the Event shape changes |
| 6 Hooks and permission modes | PR10's `before_dispatch` | Hook adapters replace two inline branches instead of being threaded through two Tool paths |
| 8 `serve --stdio` | PR12 (`submit`, `RunHandle.events`), PR13 (binary hosts `serve`), PR14 (vocabulary for `generate-schema`) | No per-submission Engine and no coalescing `watch` |
| 9 Routing, 10 Multi-call | PR3 (`ProviderIdentity` per role), PR10 (`execute_step` over a request slice) | `RunControls` no longer carries ChatGPT fields |
| 11 C2 workers | PR10's `Grant::Worker` and PR7's attestation | A third Tool path is never written |
| 13 ACP, crate split | PR13, PR14 | The vocabulary crate exists |

## Open questions

- **When PR14 starts.** It is gated on roadmap item 8 (`serve --stdio`). Until item 8 starts, the journal crate is a seam with one consumer. Owner: `fpmirabile`.

## Exit criteria

- **Per PR:** the verification listed above passes on the PR's own corpus, and the PR states whether it is behavior-preserving. Behavior changes in PR7, PR9 and PR11 land with their spec and telemetry updates in the same PR.
- **Model-facing bytes:** the catalog and `tool_specs` snapshot is byte-equal before and after PR6, and changes in PR7 are reviewed as the one-time prefix invalidation they are.
- **Performance:** Read p95 at or below 50 ms in the Tool journeys after PR7 (roadmap item 2).
- **Crate boundary:** `cargo tree -p arany -e normal` has no `ratatui`, `crossterm` or `keyring` after PR13; `cargo tree -p arany-journal` has no `tokio`, `reqwest` or `rusqlite` after PR14.
- **Native evidence:** the native lanes for Linux and macOS run once after PR1, PR7 and PR13. Anything the synthetic corpora cannot prove stays in [NEXT_STEPS.md](../../NEXT_STEPS.md); no native support is claimed from this plan.
- **Documentation:** the Tools spec (PR7), the Goal spec (PR9), telemetry timestamps (PR11), and the system overview §2 with the AGENTS.md routing table (PR13) are updated. F12 drift is fixed. No `//!` docs are added.
- **CI:** `ci.yml` test paths are updated in PR13, and CI is green on each PR.
- **Closeout:** unresolved defects and deferred checks move to NEXT_STEPS, inbound links are updated, and this folder is deleted under the [planning lifecycle](../README.md#lifecycle).
