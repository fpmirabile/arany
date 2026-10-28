# Per-command Build class and timeout

## Metadata

- Status: `Ready` (Research and Plan gates passed; one OWNER DECISION with a recommendation under [Contract and scenarios](#contract-and-scenarios), resolved by merging)
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

**How the class is chosen today.** [`Ledger::requested_command_resources`](../../crates/arany/src/engine/ledger.rs) returns the recorded plan contract's `resource_class` and `command_timeout_ms`, else `default` and the pinned default timeout; it takes no call. [`intent.rs::workspace_intent`](../../crates/arany/src/engine/run_loop/step/intent.rs) applies that pair to every primary and editor `Command` intent through `command_limits`, clamps the runtime to the remaining Goal active time (a result under one second is the existing active-time sizing refusal), and `bind_build_jobs` binds the Goal-pinned jobs. `Ledger::command_timeout_limit(&call)` is called from intent construction, `step.rs` (denied and pre-dispatch paths) and `permission.rs::review_will_run`; `run_loop.rs::check_plan` reads the pinned settings through the same accessor. The timeout-limit pair is re-derived from the plan contract in two more places outside the Engine: the reducer ([`lifecycle.rs::validate_timeout_limit`](../../crates/arany/src/session/reducer/tool/lifecycle.rs)) and the Goal report ([`report.rs`](../../crates/arany/src/engine/report.rs), `Limit` branch). The session module cannot call the Engine, so the shared resolution cannot live on the Ledger. `ToolCall::Command` carries only `program`, `args` and `cwd` ([`types.rs`](../../crates/arany/src/tools/types.rs)), and its model schema in [`process.rs::schema`](../../crates/arany/src/tools/process.rs) has the same three fields. `resource_class` appears to the model only inside the `arany_plan` contract schema, as an undescribed enum, and `command_timeout_ms` there hardcodes "1000 through 600000" instead of deriving the range from the constants ([`catalog.rs::plan_contract_schema`](../../crates/arany/src/tools/catalog.rs)); the one-second floor is a bare `1_000` literal in `ToolResourceSettings::validate` and in `workspace_intent`. [`COMMAND_CONTRACT`](../../crates/arany/src/tools/catalog.rs) never mentions the class; editors receive the same `command` schema with [`WORKER_COMMAND_CONTRACT`](../../crates/arany/src/tools/catalog.rs). Planning is optional, one contract per Goal, and requires a success criterion ([Goals](../../docs/specs/goals.md#initial-plan-contract-and-success-receipt)), so an ordinary "run the tests" turn never reaches Build.

**Existing validation to reuse.**

- `run_loop.rs` refuses a Build plan with `PlanInvalid` when the Goal has no resolved jobs or CPU cores ([`run_loop.rs`](../../crates/arany/src/engine/run_loop.rs), plan admission). [`resources.rs`](../../crates/arany/src/engine/resources.rs) resolves those once at Goal admission.
- The reducer accepts a Build intent only for `Command` with the Goal's pinned `build_jobs` and `build_cpu_cores` ([`lifecycle.rs::proposed`](../../crates/arany/src/session/reducer/tool/lifecycle.rs)).
- A requested timeout above the pinned maximum becomes a non-dispatched `Limit(RequestedTime)` receipt whose output is `command_timeout_limit_message` ([`types.rs`](../../crates/arany/src/tools/types.rs)). The engine computes it from `Ledger::command_timeout_limit(&call)` and the reducer re-derives it from the plan contract in `lifecycle.rs`.
- Approval lines render each call variant through one exhaustive table ([`tool_rows.rs::approval_call_lines`](../../crates/arany-cli/src/presentation/tool_rows.rs)).

**Journal format.** `call` is stored in `ToolProposed`, `ToolStarted`, `ToolFinished`, hook Events and plan or delegation success criteria. [Execution](../../docs/specs/execution.md#admission-persistence-and-recovery) allows an additive optional field that is omitted at its default without a payload version bump or migration: existing records keep their bytes and digests, and an older binary rejects a non-default record through the incompatible-history archive path. Search's optional fields and `ToolLimits.build_jobs` already use this rule.

**Peers.**

- Claude Code: "Each command runs under a timeout, and Claude manages it: when it wants longer than the default for a command, it passes the `timeout` parameter with that call"; `BASH_DEFAULT_TIMEOUT_MS` is two minutes and `BASH_MAX_TIMEOUT_MS` caps whatever Claude requests, ten minutes by default ([tools reference](https://code.claude.com/docs/en/tools-reference#timeout-and-output-limits)). A foreground command that reaches its timeout moves to the background.
- Codex: the one-shot `exec_command` adds a per-call `timeout_ms` ("Maximum command runtime. Defaults to 10000 ms.", [`exec_command.rs`](https://github.com/openai/codex/blob/f021168bde5357009fceb0477835ad90bd54c17c/codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs#L279-L296)), and `sandbox_permissions` is a "Per-command sandbox override. Defaults to `use_default`" ([`shell_spec.rs`](https://github.com/openai/codex/blob/f021168bde5357009fceb0477835ad90bd54c17c/codex-rs/core/src/tools/handlers/shell_spec.rs#L232-L245)).
- Neither peer has resource classes: their sandboxes bound filesystem and network, not CPU, memory or storage ([Tools](../../docs/specs/tools.md#limits-and-failure-behavior), Build process profile row). The shared design is a per-call, model-chosen request capped by a user-owned maximum. Arany diverges on expiry: Claude Code moves a timed-out foreground command to the background, while Arany keeps the existing `Limit(timeout)` receipt, because a detached process has no journaled changeset or receipt and would need a daemon ([Tools](../../docs/specs/tools.md#limits-and-failure-behavior), Time row). R1 records this divergence in that row.

## Contract and scenarios

- **Owning specs.** [Tools](../../docs/specs/tools.md#persistent-workspace-build-cache) for the `command` arguments, class resolution and model-facing contract; [Goals](../../docs/specs/goals.md#initial-plan-contract-and-success-receipt) for how a per-call request narrows a recorded plan; [Execution](../../docs/specs/execution.md#admission-persistence-and-recovery) for the additive-field note. No limit, setting or Event kind changes.
- **Proposed contract.** `command` gains optional `resource_class` (`"default"` or `"build"`) and `timeout_ms` (integer from the existing one-second floor, named once as `MIN_COMMAND_TIMEOUT_MS` beside [`DEFAULT_COMMAND_TIMEOUT_MS`](../../docs/specs/limits.md#default_command_timeout_ms) and replacing the two `1_000` literals, to [`MAX_COMMAND_TIMEOUT_MS`](../../docs/specs/limits.md#max_command_timeout_ms)). The effective class is the call's value, else the recorded plan's, else `default`. The effective timeout is the call's value, else the plan's, else the pinned default. A recorded plan is a ceiling: the call may equal or narrow its class and timeout, never widen them. The effective runtime is then clamped to the remaining Goal active time exactly as today. Each field is stored in the call only when present. Schema descriptions for `command` and `arany_plan` derive the range from the constants; no new limit or setting is added.
- **OWNER DECISION (recommendation: strict ceiling).** A recorded plan whose `command_timeout_ms` is null resolves today to the pinned default. Under the strict-ceiling reading, a call may then only shorten the default; under the loose reading, a null plan timeout leaves the call free up to the pinned maximum. Recommendation: strict ceiling, because the plan contract already states that model text can only narrow authority, it keeps one rule for both fields, and a plan author who needs longer commands writes the timeout into the plan. The scenario table below follows the recommendation.

| Scenario | Result |
|---|---|
| No plan; `sh -c 'cargo test --workspace'` with `resource_class: "build"` and `timeout_ms: 600000`, within pinned settings | The intent carries the Build profile, the Goal-pinned `build_jobs` and `build_cpu_cores`, the Build cache and project sizes and `runtime_ms` 600000; Cargo receives `CARGO_BUILD_JOBS` and `CARGO_NET_OFFLINE`; the test count is reported like any Build command |
| Same call in Ask mode | The approval shows the argv plus `Build` and the timeout; approval latches exactly those limits, as plan approval does today |
| No plan; `timeout_ms` above the saved maximum | The existing non-dispatched `Limit(RequestedTime)` receipt with `command_timeout_limit_message`; no Guard receipt; the Goal keeps its pinned settings |
| `timeout_ms` below 1,000 or not an integer, or an unknown `resource_class` | Proposal refusal `InvalidArguments` under the [refusal continuation](../../docs/specs/execution.md#accepted-outcomes); nothing dispatched |
| `resource_class: "build"` while the Goal has no resolved jobs or CPU cores | Proposal refusal `InvalidArguments` naming Build as unavailable for this Goal; nothing dispatched |
| Recorded plan with `default`, or with a timeout (null meaning the pinned default), and a call asking for `build` or a longer timeout | Proposal refusal `PlanScopeExcludesCall`; a call may only narrow a recorded plan (Build to default, a shorter timeout) |
| `timeout_ms` within the maximum but above the remaining Goal active time | Today's behavior, unchanged: the runtime is clamped to the remaining reservation, or the existing active-time sizing refusal applies when less than the floor remains; no new outcome |
| A Build command cancelled, timed out, or interrupted by a crash | The existing Command receipts: confirmed stops record `Limit` or `Cancelled` and publish no changeset, an unknown outcome is `Uncertain` and stops continuation; there is no retry, backgrounding or fallback to `default` |
| Fields omitted | Byte-identical intents, Events and digests to today |
| Strict replay | The reducer re-derives the effective class and timeout from the call, plan and pinned settings: `limits.resource_class` must equal the effective class, `limits.runtime_ms` may not exceed the effective timeout, and a `RequestedTime` receipt must match the call's request; any mismatch is `InvalidTransition` and nothing executes |
| An older binary opens a journal with the new fields | It rejects the record through the existing incompatible-history archive path |
| An editor `command` with the fields | Same resolution against the same Goal-pinned settings through the shared `workspace_intent` path, then the existing editor deadline narrowing; it gains no authority. Researchers cannot run commands today and this does not change |
| Hooks and permission rules | `tool_input` carries the fields when present; hooks and rules may only deny or narrow, never select or widen the class, and a remembered prefix rule for the program never implies Build |

- **Evidence owners.** The guarded command journeys in [`native_exec.rs`](../../crates/arany-cli/tests/session_run/tools/native_exec.rs) for Build dispatch and receipts; reducer tests beside [`lifecycle.rs`](../../crates/arany/src/session/reducer/tool/lifecycle.rs) for tamper and replay; [`plan_contract.rs`](../../crates/arany/tests/engine_run/plan_contract.rs) for plan narrowing refusals; the report tests for the `RequestedTime` fact; the catalog runtime tests and Provider schema snapshots for the schema; the Event round-trip corpus for omitted-default bytes. The live repository `cargo test` on a native Mac and on Linux stays a deferred owner check in [NEXT_STEPS](../../NEXT_STEPS.md#daily-use-first).

## Chosen direction

- **Simplest sufficient solution.** Reuse the existing class, limits, Goal pinning, `RequestedTime` refusal and Build Guard path. Only the source of the requested pair changes: from "plan only" to "call, then plan". This means two optional fields, one resolution function shared by the Engine and the reducer, a sentence in `COMMAND_CONTRACT`, and approval lines. Rejected alternatives:
  - Make `build` the default for primary commands. That changes every command's profile, contradicts "`default` keeps its profile" and spends Build budgets on `ls`.
  - Infer Build from argv such as `cargo`. Wrappers and scripts defeat it, and model text should state intent explicitly, as Codex `sandbox_permissions` does.
  - Only describe `resource_class` in the plan schema. That still forces a plan, a success criterion and a scope for every test run.
- **OS impact.** The behavior is the same on Linux and macOS, because the change is in the Engine, reducer and presentation. Enforcement differences stay where the Build profile already documents them: a Linux `cpu.max` quota versus a macOS per-process threshold. Native support is claimed only after the deferred checks.
- **Ownership.** One pure function beside `GoalPlanContract` in [`goal.rs`](../../crates/arany/src/session/events/goal.rs), taking the call, the optional recorded contract and the pinned `ToolResourceSettings` and returning the effective class and timeout or the narrowing refusal, becomes the single owner of resolution. The Ledger's `requested_command_resources(&call)` and `command_timeout_limit(&call)` wrap it for intent construction, `check_plan` and permission review; the reducer's `validate_timeout_limit` and the report's `Limit` branch call it directly, so Engine, reducer and report cannot drift. `ToolCall::Command` keeps the payload shape; the two fields are additive and omitted by default.

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
- Reducer, report and Engine resolution drift apart. → One shared resolution function in the session layer that all three call, plus reducer tamper rows for class, timeout and `RequestedTime` output.
- The model keeps asking for Build after the Goal has no resolved jobs or cores (for example, no CPU count on this host). → The `InvalidArguments` refusal text says Build is unavailable for this Goal and to run the command under `default`; the identical-pair refusal cap in [Execution](../../docs/specs/execution.md#accepted-outcomes) bounds repeats.
- A slice conflicts with the Guard user environment U1, which also edits `COMMAND_CONTRACT`. → R1 adds one sentence; whichever lands second merges main and keeps both.

## Vertical slices

Each slice ID is one PR within the [slice ceiling](../PLANNING.md#slice-delivery). Estimates are planning budgets (P production, E evidence, D documentation changed lines).

1. **R1 — Per-command `resource_class` and `timeout_ms`, end to end (P 480, E 560, D 80; total about 1,100).**
   - Schema and contract: add both optional fields to `ToolCall::Command` and to the [`process.rs::schema`](../../crates/arany/src/tools/process.rs) entry for `command`, with descriptions that derive the floor and the pinned maximum from `MIN_COMMAND_TIMEOUT_MS` and `MAX_COMMAND_TIMEOUT_MS`; the `arany_plan` `command_timeout_ms` description uses the same derivation. Add one sentence to both platforms' [`COMMAND_CONTRACT`](../../crates/arany/src/tools/catalog.rs): pass `resource_class: "build"` and a `timeout_ms` for compile and test commands; the default class has a small cache, no Cargo jobs on macOS and a short timeout. `WORKER_COMMAND_CONTRACT` is unchanged; editors see the fields through the shared schema.
   - Engine: resolution moves into the shared session-layer function above; `Ledger::requested_command_resources(&call)` and `command_timeout_limit(&call)` wrap it for [`intent.rs`](../../crates/arany/src/engine/run_loop/step/intent.rs), `step.rs`, `permission.rs` and `check_plan`. Refuse a value outside a recorded plan (`PlanScopeExcludesCall`), Build without resolved jobs or cores and out-of-range or non-integer values (`InvalidArguments`) before dispatch.
   - Reducer and report: [`lifecycle.rs`](../../crates/arany/src/session/reducer/tool/lifecycle.rs) and the [`report.rs`](../../crates/arany/src/engine/report.rs) `Limit` branch call the same function.
   - Presentation: show `Build` and the timeout in approval lines when present ([`tool_rows.rs::approval_call_lines`](../../crates/arany-cli/src/presentation/tool_rows.rs)).
   - Specs: Tools (Commands section for the arguments and resolution; Time row for the ceiling, the plan-null rule and the Claude Code backgrounding divergence), Goals (a call narrows a recorded plan; plan approval no longer the only latch) and the Execution additive-field note.
   - Evidence: a guarded Build journey without a plan; the over-maximum `RequestedTime` row, including its report fact; each refusal row from the scenario table, including plan narrowing in `plan_contract.rs`; reducer tamper rows for class, runtime and receipt; omitted-default byte equality in the Event corpus; the catalog schema snapshot and Provider tool-schema snapshots.
   - Closing: this PR closes the plan. It carries the [slice delivery](../PLANNING.md#slice-delivery) checklist and moves the deferred native checks below into NEXT_STEPS before deleting this folder.

## Gates

Merging this plan records the owner's decision that R1 implements the daily-use item "`cargo test` here inside the Guard" and adds no feature under the [frozen areas](../../NEXT_STEPS.md#frozen-areas) (macOS Guard features beyond parity probes, build cache): it changes no enforcement, cache mechanism, limit or setting. The OWNER DECISION on a null plan timeout is resolved by merging this plan as recommended or by editing the contract paragraph first. R1 has no dependency on open plans and may land before or after Guard user environment U1.

## Exit criteria

- R1's journeys prove on CI (Linux and macOS lanes):
  - a planless Build command dispatches with the pinned Build limits and the jobs environment;
  - each refusal and the `RequestedTime` receipt dispatch nothing, and the Goal report names the `RequestedTime` fact from the call's request;
  - a call that widens a recorded plan's class or timeout is refused with `PlanScopeExcludesCall`;
  - omitted fields keep existing Event bytes and digests;
  - strict replay rejects every tampered class, timeout and receipt and executes nothing.
- The Tools, Goals and Execution specs describe the current behavior, and `python3 scripts/spec-bounds.py` passes.
- Deferred owner checks are listed in [NEXT_STEPS](../../NEXT_STEPS.md#daily-use-first) before the folder is deleted: an attached session in this repository runs `cargo test --workspace` through the Guard with `resource_class: "build"` to an executed test count on a native Mac and on Linux, retaining the intent limits, the receipt and the ignored counts.
