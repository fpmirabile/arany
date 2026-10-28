# Per-command Build class and timeout

## Metadata

- Status: `Ready` (Research and Plan gates passed; no material open question)
- Owner: harness maintainers
- Last updated: 2026-10-28
- Next action: implement R1.

## Outcome and scope

One capability: the model sizes a single guarded `command` by asking for the existing `build` resource class and a timeout on that call, the way Claude Code passes `timeout` and Codex passes `timeout_ms` and `sandbox_permissions` per command. A plain request such as "run the tests" then runs `cargo test --workspace` in the Guard on Linux and macOS without the primary first recording an `arany_plan` contract. This serves the [daily-use item](../../NEXT_STEPS.md#daily-use-first) "`cargo test` here inside the Guard".

Included: two optional `command` arguments, their Engine resolution against the Goal-pinned settings and any recorded plan, reducer validation, approval and Tool-row display, model-facing contract text, and the owning spec edits. Excluded: new Guard enforcement, new limits or setting rows, cache mechanics, background or auto-backgrounded commands, `command_unguarded` (host commands keep their own [supervision contract](../../docs/specs/tools.md#primary-host-commands)), and automatic class selection from argv.

## Research

**Observed failure.** Session `01a1820d-0f5b-752c-9de0-665bc173aeb7` (macOS, 2026-10-28) ran four primary `sh -c 'cargo test --workspace'` variants. Every intent carried `resource_class: "default"`, `runtime_ms: 120000`, `cache_bytes: 67108864` and `project_bytes: 268435456`. The results were: `failed to determine the amount of parallelism available: Operation not permitted`; with `CARGO_BUILD_JOBS=2`, `No space left on device` linking `aws-lc-sys` under the cache view's `target/`; `failed to create directory /tmp`; and with a Workspace `CARGO_TARGET_DIR`, `limit: resource` with "Command changeset could not be captured". No test executed.

**Why the default class cannot build.**

- The macOS profile denies every sysctl and host CPU discovery. Only selected-Rust Build commands receive `CARGO_BUILD_JOBS` from the Goal-pinned jobs ([Tools](../../docs/specs/tools.md#persistent-workspace-build-cache), paragraph on Rust commands; [macOS enforcement](../../docs/specs/tools.md#macos-enforcement-and-compatibility)). So Cargo's `available_parallelism` fails under `default`.
- The `default` profile is 64 MiB cache view, 256 MiB primary project volume, [`MEMORY_BYTES`](../../docs/specs/limits.md#memory_bytes) and [`MAX_PROCESSES_OR_THREADS`](../../docs/specs/limits.md#max_processes_or_threads); Linux adds 25% of one CPU and macOS 15 CPU seconds per process ([limits table](../../docs/specs/tools.md#limits-and-failure-behavior), rows "Default process profile", "Private command storage", "Persistent build cache"). The `build` profile raises these up to [`MAX_BUILD_MEMORY_BYTES`](../../docs/specs/limits.md#max_build_memory_bytes), [`MAX_BUILD_PROCESSES_OR_THREADS`](../../docs/specs/limits.md#max_build_processes_or_threads), [`MAX_BUILD_PROJECT_BYTES`](../../docs/specs/limits.md#max_build_project_bytes) and [`MAX_BUILD_CACHE_BYTES`](../../docs/specs/limits.md#max_build_cache_bytes), lowered through `/settings`, and its timeout up to [`MAX_COMMAND_TIMEOUT_MS`](../../docs/specs/limits.md#max_command_timeout_ms).
- These are the same constraints on Linux except the sysctl denial: a cold workspace build still exceeds the default cache view, CPU share and [`DEFAULT_COMMAND_TIMEOUT_MS`](../../docs/specs/limits.md#default_command_timeout_ms).

**How the class is chosen today.** [`Ledger::requested_command_resources`](../../crates/arany/src/engine/ledger.rs) returns the recorded plan contract's `resource_class` and `command_timeout_ms`, else `default` and the pinned default timeout. [`intent.rs`](../../crates/arany/src/engine/run_loop/step/intent.rs) applies that pair to every `Command` intent through `command_limits`, and `bind_build_jobs` binds the Goal-pinned jobs. `ToolCall::Command` carries only `program`, `args` and `cwd` ([`types.rs`](../../crates/arany/src/tools/types.rs)), and its model schema in [`process.rs::schema`](../../crates/arany/src/tools/process.rs) has the same three fields. `resource_class` appears to the model only inside the `arany_plan` contract schema, as an undescribed enum ([`catalog.rs::plan_contract_schema`](../../crates/arany/src/tools/catalog.rs)). [`COMMAND_CONTRACT`](../../crates/arany/src/tools/catalog.rs) never mentions it. Planning is optional, one contract per Goal, and requires a success criterion ([Goals](../../docs/specs/goals.md#initial-plan-contract-and-success-receipt)), so an ordinary "run the tests" turn never reaches Build.

**Existing validation to reuse.**

- `run_loop.rs` refuses a Build plan with `PlanInvalid` when the Goal has no resolved jobs or CPU cores ([`run_loop.rs`](../../crates/arany/src/engine/run_loop.rs), plan admission). [`resources.rs`](../../crates/arany/src/engine/resources.rs) resolves those once at Goal admission.
- The reducer accepts a Build intent only for `Command` with the Goal's pinned `build_jobs` and `build_cpu_cores` ([`lifecycle.rs::proposed`](../../crates/arany/src/session/reducer/tool/lifecycle.rs)).
- A requested timeout above the pinned maximum becomes a non-dispatched `Limit(RequestedTime)` receipt whose output is `command_timeout_limit_message` ([`types.rs`](../../crates/arany/src/tools/types.rs)). The engine computes it from `Ledger::command_timeout_limit(&call)` and the reducer re-derives it from the plan contract in `lifecycle.rs`.
- Approval lines render each call variant through one exhaustive table ([`tool_rows.rs::approval_call_lines`](../../crates/arany-cli/src/presentation/tool_rows.rs)).

**Journal format.** `call` is stored in `ToolProposed`, `ToolStarted`, `ToolFinished`, hook Events and plan or delegation success criteria. [Execution](../../docs/specs/execution.md#admission-persistence-and-recovery) allows an additive optional field that is omitted at its default without a payload version bump or migration: existing records keep their bytes and digests, and an older binary rejects a non-default record through the incompatible-history archive path. Search's optional fields and `ToolLimits.build_jobs` already use this rule.

**Peers.**

- Claude Code: "Each command runs under a timeout, and Claude manages it: when it wants longer than the default for a command, it passes the `timeout` parameter with that call"; `BASH_DEFAULT_TIMEOUT_MS` is two minutes and `BASH_MAX_TIMEOUT_MS` caps whatever Claude requests, ten minutes by default ([tools reference](https://code.claude.com/docs/en/tools-reference#timeout-and-output-limits)). A foreground command that reaches its timeout moves to the background.
- Codex: the one-shot `exec_command` adds a per-call `timeout_ms` ("Maximum command runtime. Defaults to 10000 ms.", [`exec_command.rs`](https://github.com/openai/codex/blob/f021168bde5357009fceb0477835ad90bd54c17c/codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs#L279-L296)), and `sandbox_permissions` is a "Per-command sandbox override. Defaults to `use_default`" ([`shell_spec.rs`](https://github.com/openai/codex/blob/f021168bde5357009fceb0477835ad90bd54c17c/codex-rs/core/src/tools/handlers/shell_spec.rs#L232-L245)).
- Neither peer has resource classes: their sandboxes bound filesystem and network, not CPU, memory or storage ([Tools](../../docs/specs/tools.md#limits-and-failure-behavior), Build process profile row). The shared design is a per-call, model-chosen request capped by a user-owned maximum.

## Contract and scenarios

- **Owning specs.** [Tools](../../docs/specs/tools.md#persistent-workspace-build-cache) for the `command` arguments, class resolution and model-facing contract; [Goals](../../docs/specs/goals.md#initial-plan-contract-and-success-receipt) for how a per-call request narrows a recorded plan; [Execution](../../docs/specs/execution.md#admission-persistence-and-recovery) for the additive-field note. No limit, setting or Event kind changes.
- **Proposed contract.** `command` gains optional `resource_class` (`"default"` or `"build"`) and `timeout_ms` (integer from 1,000 to [`MAX_COMMAND_TIMEOUT_MS`](../../docs/specs/limits.md#max_command_timeout_ms)). The effective class is the call's value, else the recorded plan's, else `default`. The effective timeout is the call's value, else the plan's, else the pinned default. Each field is stored in the call only when present.

| Scenario | Result |
|---|---|
| No plan; `sh -c 'cargo test --workspace'` with `resource_class: "build"` and `timeout_ms: 600000`, within pinned settings | The intent carries the Build profile, the Goal-pinned `build_jobs` and `build_cpu_cores`, the Build cache and project sizes and `runtime_ms` 600000; Cargo receives `CARGO_BUILD_JOBS` and `CARGO_NET_OFFLINE`; the test count is reported like any Build command |
| Same call in Ask mode | The approval shows the argv plus `Build` and the timeout; approval latches exactly those limits, as plan approval does today |
| No plan; `timeout_ms` above the saved maximum | The existing non-dispatched `Limit(RequestedTime)` receipt with `command_timeout_limit_message`; no Guard receipt; the Goal keeps its pinned settings |
| `timeout_ms` below 1,000 or not an integer, or an unknown `resource_class` | Proposal refusal `InvalidArguments` under the [refusal continuation](../../docs/specs/execution.md#accepted-outcomes); nothing dispatched |
| `resource_class: "build"` while the Goal has no resolved jobs or CPU cores | Proposal refusal `InvalidArguments` naming Build as unavailable for this Goal; nothing dispatched |
| Recorded plan with `default`, or a timeout, and a call asking for `build` or a longer timeout | Proposal refusal `PlanScopeExcludesCall`; a call may only narrow a recorded plan (Build to default, a shorter timeout) |
| Fields omitted | Byte-identical intents, Events and digests to today |
| Strict replay | The reducer re-derives the effective class and timeout from the call, plan and pinned settings; a Build intent without a call or plan request, a mismatched timeout, or a `RequestedTime` receipt that does not match is `InvalidTransition`; nothing executes |
| An older binary opens a journal with the new fields | It rejects the record through the existing incompatible-history archive path |
| A child or editor `command` with the fields | Same resolution against the same Goal-pinned settings; it gains no authority |

- **Evidence owners.** The guarded command journeys in [`native_exec.rs`](../../crates/arany-cli/tests/session_run/tools/native_exec.rs) for Build dispatch and receipts; reducer tests beside [`lifecycle.rs`](../../crates/arany/src/session/reducer/tool/lifecycle.rs) for tamper and replay; the catalog runtime tests for the schema; the Event round-trip corpus for omitted-default bytes. The live repository `cargo test` on a native Mac and on Linux stays a deferred owner check in [NEXT_STEPS](../../NEXT_STEPS.md#daily-use-first).

## Chosen direction

- **Simplest sufficient solution.** Reuse the existing class, limits, Goal pinning, `RequestedTime` refusal and Build Guard path. Only the source of the requested pair changes: from "plan only" to "call, then plan". This means two optional fields, one resolution function shared by the Engine and the reducer, a sentence in `COMMAND_CONTRACT`, and approval lines. Rejected alternatives:
  - Make `build` the default for primary commands. That changes every command's profile, contradicts "`default` keeps its profile" and spends Build budgets on `ls`.
  - Infer Build from argv such as `cargo`. Wrappers and scripts defeat it, and model text should state intent explicitly, as Codex `sandbox_permissions` does.
  - Only describe `resource_class` in the plan schema. That still forces a plan, a success criterion and a scope for every test run.
- **OS impact.** The behavior is the same on Linux and macOS, because the change is in the Engine, reducer and presentation. Enforcement differences stay where the Build profile already documents them: a Linux `cpu.max` quota versus a macOS per-process threshold. Native support is claimed only after the deferred checks.
- **Ownership.** `Ledger::requested_command_resources(&call)` becomes the single owner of resolution, used by intent construction, `command_timeout_limit`, plan narrowing and the reducer's re-derivation. `ToolCall::Command` keeps the payload shape; the two fields are additive and omitted by default.

## Security and resource model

The triggers are a changed capability selection, a prompt change and an additive persistent-format field ([security pass](../../agents/security.md#security-pass-triggers)).

- **Assets.** Host CPU, memory and disk; StateRoot aggregate storage; Goal active time.
- **Trust boundary.** Model text chooses a class, as it already can through `arany_plan` in every mode where plan approval is automatic. The class is resource sizing, not authority: it never widens executable, path, Skill, MCP, network or host grants.
- **Authority source.** The user-owned `/settings` Build maxima and the jobs and cores pinned at `GoalStarted`. A call can request at most those values and at most a recorded plan. Approval never bypasses the maximum.
- **Immutable intent.** The effective class, every limit and the timeout are in the normalized intent and the enforcement digest before dispatch. Receipts echo them, and strict replay re-derives and compares them.
- **Descendants.** Children and editors resolve against the same Goal pin. Build was already reachable for every Goal command through a plan.
- **Bounds and failure.** All existing per-command bounds hold, along with the 8 GiB StateRoot aggregate and cache headroom checks. A refused request dispatches nothing. A Build command that exhausts its limits returns the existing `Limit` or failure receipts. There is no automatic retry or escalation from `default` to `build`.
- **Non-claims.** Build limits are policy limits, not measured guarantees ([Build process profile](../../docs/specs/tools.md#limits-and-failure-behavior)). Concurrent Build children can together use more host memory than one command, exactly as they can under a Build plan today.

## Risks and mitigations

- The model asks for Build for trivial commands and spends cache or storage budget. → The contract text reserves Build for compile and test commands. The user maxima still cap usage, and approval in Ask mode shows the class.
- Reducer and Engine resolution drift apart. → One shared resolution function, plus reducer tamper rows for class, timeout and `RequestedTime` output.
- A slice conflicts with the Guard user environment U1, which also edits `COMMAND_CONTRACT`. → R1 adds one sentence; whichever lands second merges main and keeps both.

## Vertical slices

Each slice ID is one PR within the [slice ceiling](../PLANNING.md#slice-delivery). Estimates are planning budgets (P production, E evidence, D documentation changed lines).

1. **R1 — Per-command `resource_class` and `timeout_ms`, end to end (P 420, E 520, D 60; total about 1,000).**
   - Schema and contract: add both optional fields to `ToolCall::Command` and to the [`process.rs::schema`](../../crates/arany/src/tools/process.rs) entry for `command`, with descriptions that name the pinned maximum through its constant. Add one sentence to both platforms' [`COMMAND_CONTRACT`](../../crates/arany/src/tools/catalog.rs): pass `resource_class: "build"` and a `timeout_ms` for compile and test commands; the default class has a small cache, no Cargo jobs on macOS and a short timeout.
   - Engine: move resolution into `Ledger::requested_command_resources(&call)`, used by [`intent.rs`](../../crates/arany/src/engine/run_loop/step/intent.rs) and `command_timeout_limit`. Refuse a value outside a recorded plan, Build without resolved jobs or cores, and out-of-range timeouts before dispatch.
   - Reducer: re-derive the same pair in [`lifecycle.rs`](../../crates/arany/src/session/reducer/tool/lifecycle.rs).
   - Presentation: show `Build` and the timeout in approval lines when present.
   - Specs: Tools, Goals and the Execution additive-field note.
   - Evidence: a guarded Build journey without a plan; the over-maximum `RequestedTime` row; the refusal rows from the scenario table; reducer tamper rows; omitted-default byte equality in the Event corpus; the catalog schema snapshot.
   - Closing: this PR closes the plan. It carries the [slice delivery](../PLANNING.md#slice-delivery) checklist and moves the deferred native checks below into NEXT_STEPS before deleting this folder.

## Gates

Merging this plan records the owner's decision that R1 implements the daily-use item "`cargo test` here inside the Guard" and adds no frozen macOS Guard or build-cache feature: it changes no enforcement, cache mechanism, limit or setting. R1 has no dependency on open plans and may land before or after Guard user environment U1.

## Exit criteria

- R1's journeys prove on CI (Linux and macOS lanes):
  - a planless Build command dispatches with the pinned Build limits and the jobs environment;
  - each refusal and the `RequestedTime` receipt dispatch nothing;
  - omitted fields keep existing Event bytes and digests;
  - strict replay rejects every tampered class, timeout and receipt and executes nothing.
- The Tools, Goals and Execution specs describe the current behavior, and `python3 scripts/spec-bounds.py` passes.
- Deferred owner checks are listed in [NEXT_STEPS](../../NEXT_STEPS.md#daily-use-first) before the folder is deleted: an attached session in this repository runs `cargo test --workspace` through the Guard with `resource_class: "build"` to an executed test count on a native Mac and on Linux, retaining the intent limits, the receipt and the ignored counts.
