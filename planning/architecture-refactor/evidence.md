# Architecture refactor evidence

Measured facts behind the [architecture refactor plan](./README.md). Every `file:line` refers to `origin/main` at `8857765`. Diagrams are in [diagrams.md](./diagrams.md). Measurements came from a script over a `git archive` extract, so they are reproducible but not CI-enforced. Line counts exclude `#[cfg(test)]` blocks and `*tests.rs` files unless marked "total".

## Measured shape

- 103,860 lines of Rust under `src/` (64,837 non-test), 50,783 under `tests/`, 741 test functions.
- 124 source files in one package with one `lib` and one `bin` target.
- `unsafe` is forbidden in the binary and denied in the library, except `tools/guard/macos/ffi.rs` (38 mentions) and `private_fs/acl.rs` (6 mentions).
- Platform `cfg` sites (non-test): tools 129, terminal 77, store 72, cli 23, private_fs 14, session 7, engine 0.

## Dependency facts

| Fact | Evidence |
|---|---|
| Fan-in (dependent modules / references): `tools` 6 / 274, `session` 8 / 160, `provider` 8 / 99, `store` 5 / 39, `private_fs` 2 / 32, `engine` 3 / 11, `telemetry` 1 / 14, `presentation` 1 / 37, `terminal` 0 / 0 | Measured on the extract |
| Largest outgoing edges: engine to tools 153, engine to session 80, engine to provider 31, session to tools 60, session to provider 43, provider to tools 25 | Measured on the extract |
| Single reference cycle: `tools` to `engine` for `RunCancellation` | `src/tools/guard.rs:5`, `src/tools/guard/linux.rs:5`, `src/tools/guard/macos.rs:5`, `src/tools/guard/macos/unit.rs:9` |
| `engine` imports the platform overhead constant | `src/engine/run_loop.rs:19` imports `crate::tools::PREPARATION_OVERHEAD` (5 s on Linux at `src/tools/guard/linux.rs:19`; derived from a 30 s deadline on macOS at `src/tools/guard/macos.rs:43-46`) |
| `terminal` and `presentation` have no library consumer beyond the binary | `grep crate::terminal` outside `src/terminal` and `src/cli` is empty; `presentation` has one consumer |
| `keyring` is used only under `src/cli/credentials`; `ratatui` and `crossterm` only under `src/terminal` | Measured on the extract |
| `Event` embeds journal vocabulary from `tools` and `provider` | `src/session/events.rs:2361-2494` (`EffectIntent`, `ToolObservation`, `ToolResourceSettings`, `ImageAttachment`, `ChatGptProvenance`); 94 references to `crate::tools` types from `events.rs` |

## Depth audit

Interface = `pub` / `pub(crate)` declarations. Behaviour = non-test lines.

| Module | Interface (pub / pub(crate)) | Behaviour | Verdict |
|---|---|---|---|
| `session` (events, reducer, context) | 69 / 90 | 5,193 | Deep; keep |
| `store` | 41 / 70 | 5,534 | Deep core; `StateRoot` shallow (25 pass-through methods) |
| `provider` (post-A2) | 104 / 124 | 7,034 | Deep at the `Provider` seam; wide at the crate edge |
| `engine` | 50 / 32 | 6,544 | Shallow interface over deep behaviour |
| `engine/run_loop` + `children` | 0 / 14 | 3,484 | Deep but duplicated |
| `tools` (router, types, config, fs, git) | 68 / 256 | 13,207 incl. guard | Router shallow; `fs` and `git` deep |
| `tools/guard` | 2 pub / about 60 `pub(super)` | 7,381 | Deep hard boundary; two `helper_main`s duplicate plumbing |
| `private_fs` | 0 / 7 | 363 | Deep, two consumers |
| `presentation` | 7 / 29 | 1,355 | Deep pure projection |
| `terminal` | 116 / 116 | 8,677 | Shallow toolbox (about 70 `AttachedTerminal` methods) |
| `cli` (bin) | 0 / 201 | 14,951 | Shallow by construction; flows live here |
| `cli/chatgpt` + `cli/credentials` | 0 / about 80 | 5,096 | Deep internally, misplaced in the engine package |
| `telemetry` | 5 / 38 | 1,099 | Deep but inline; `TraceEventKind` mirrors `Event` |
| `diagnostics` | 3 / 9 | 592 | Deep leaf |

## Function sizes

| Function | Lines | Location |
|---|---|---|
| `SessionView::apply_committed` | 879 | `src/session/reducer.rs:668` (state machine, about 115 `InvalidTransition` sites) |
| `cli/attached.rs run` | 821 | `src/cli/attached.rs:306` |
| `streaming.rs feed` | 540 | `src/provider/streaming.rs:109` |
| `active.rs submit_objective` | 523 | `src/cli/attached/active.rs:282` |
| `Engine::run_turn` | 467 | `src/engine.rs:2110` |
| `run_children` | 423 | `src/engine/run_loop/children.rs:127` |
| `compact_tool_context_if_needed` | 357 | `src/engine/run_loop.rs:1666` |
| `helper_main` (Linux Guard) | 347 | `src/tools/guard.rs:837` |
| `helper_main` (macOS unit) | 329 | `src/tools/guard/macos/unit.rs:941` |
| `execute_intent` | 290 | `src/engine/run_loop.rs:2024` |
| `compact_session_inner` | 286 | `src/engine/compaction.rs:43` |
| `linux::execute_inner` | 283 | `src/tools/guard/linux.rs:556` |
| `goal_report` | 269 | `src/engine.rs:436` |
| `execute_tool` | 231 | `src/engine/run_loop.rs:1434` |
| `execute_child_tool` | 115 | `src/engine/run_loop/children.rs:675` |
| `RunLoop::execute` | 152 | `src/engine/run_loop.rs:994` |

## Goal bookkeeping

- `GoalRunBudget`: 17 fields, six mutexes, about 30 methods, about 460 lines; `src/engine/run_loop.rs:159-181` (methods through `:636`).
- `GoalView`, maintained by the reducer for the same Run: `src/session/reducer.rs:41-76`.
- Ledger Events (`GoalActiveTimeReserved/Settled`) at seven sites.
- Event-tail literals in `admit_operation`: `src/engine/run_loop.rs:966, 1850, 1983, 2065`; `src/engine/run_loop/children.rs:733`; `src/engine/compaction.rs:128`; `src/engine.rs:2169-2175`.
- Test coupling to Event indices: `tests/session_run/tools/replay.rs:17-21` and `:376-385` (`AGENT_SPAWNED = 5`, `TOOL_STARTED = 11`, `NEXT_CALL_RESERVED = 14`).

## Tool vocabulary owners

The 13 `ToolCall` operations (file List/Read/Search/Mkdir/Write/Edit; process Command/ApplyChangeset; Skill; MCP McpList/McpCall; Git GitStatus/GitDiff/GitLog) are described in nine places:

| Fact | Location |
|---|---|
| Payload shape, `deny_unknown_fields` | `src/tools/types.rs:138-208` |
| `valid`, `mutates`, `is_process`, `is_git_read`, `is_child_research`, `label` | `src/tools/types.rs:224-345` |
| Goal plan prelude set | `src/tools/types.rs` `GOAL_PLAN_PRELUDE_OPERATIONS` |
| Model-facing order | `src/tools/types.rs:1148` `CALL_ORDER` |
| JSON Schema | `src/tools/types.rs:1030-1145` `call_schemas()` |
| Researcher subset | `src/provider/tool_specs.rs` `child_research_operation` |
| Grant check | `src/tools/config.rs:225-269` |
| Approval policy | `src/tools/approval.rs:30-34` |
| Resource-settings policy | `src/session/events.rs:1055` |
| Reducer prelude rule | `src/session/reducer.rs:690-697` |
| Guard dispatch and failure classification | `src/tools/guard.rs:1017-1070` |
| In-process file implementation | `src/tools/fs.rs:812` |
| Model-facing prose | `src/tools.rs:368-420` |

Adding typed Git reads (A5) touched 28 files for this reason.

## Other measured points

- `EVENT_PAYLOAD_VERSION` is a single constant, `src/session/events.rs:101` = 6; v4 to v5 to v6 within ten days; v6 touched 42 files for two fields.
- `RunProgress` is a coalescing `watch`, `src/engine/progress.rs:14-23`, so a client cannot see every Event.
- `append_observed` fans one committed Event to diagnostics, telemetry and progress inline, `src/engine/progress.rs:242-288`.
- `TraceEventKind` re-enumerates 20 of 26 Event kinds, `src/telemetry.rs:122`.
- Trace tokens threaded through control flow: 63 in `run_loop.rs`, 31 in `children.rs`, 23 in `engine.rs`, 18 in `compaction.rs`.
- Store `Request` enum has 19 variants; six are Provider and native evidence caches, `src/store.rs:201-224`, with 11 `provider to store` references.
- `Provider` trait has 13 methods, seven of them identity getters, `src/provider.rs:698-760`; reassembled into `ProviderSelection` at `src/engine.rs:390-400`.
- `ProviderIdentity::valid()` is at `src/provider.rs:259-290`.
- `RunControls` carries `chatgpt_provenance` and `output_token_bound`, `src/engine/run_loop.rs:143-144`.
- Fault hooks: `Store::fail_next_goal_append` and `gate_goal_append_acknowledgment` (`src/store.rs:485-517`, `cfg(test)` on production types); `fail_next_candidate_sync` (`src/tools/guard.rs:654`); `fail_next_commit_marker` (`src/store/state/build_cache.rs:123`).
- Test helper that inverts the renderer: `tests/common/semantic.rs` (127 lines).
- `src/provider/test_support.rs:131-148` and `src/engine/run_loop/children.rs:701-716` both hand-build `GuardReceipt` values.
- `ScriptedProvider` builders: `tests/engine_run.rs:110-135` (fields `saved_api_account_id`, `chatgpt_provenance`, `output_token_bound`).
- `StoppedAfterRun` is an error that carries a successful Run, `src/engine.rs:2093-2106`.
- Engine setters: `src/engine.rs:1801-1810` (four `Option` fields), `:1977-2005` (`enable_tools` resets `workspace_permissions`).
- CLI setup repeated per submission: `src/cli/attached/run.rs:328-368`, `src/cli/exec.rs:247-290`.
- `AttachedTerminal`: `src/terminal.rs:361-2125`; library re-exports of terminal items, `src/lib.rs:58-67`.
- `approval.rs` is 93 lines: one `mpsc(1)` channel, one `watch`, `asks(call)`.

## Corpora

- `tests/engine_run.rs` 6,112 lines, 38 tests; `tests/engine_run/goals.rs` 4,252 lines, 61 tests; `replay.rs`, `native_exec.rs`, `workspace_security.rs`.
- `tests/session_run/*` journeys, including `setup.rs` (3,738 lines of record lifecycles).
- `src/tools/tests.rs` 1,983 lines (process fixtures); `src/telemetry/tests.rs` 1,007 lines; `tests/session_run/telemetry.rs` 643 lines.
- Wire corpora under `src/provider/*/wire/tests.rs`.
- CI jobs in `.github/workflows/ci.yml`: format-and-clippy, Linux debug and release, ARM64, `linux-native-advanced`, macOS.

## Findings F1-F13

Each finding is the basis for one or more PRs in the [plan](./README.md#execution).

| # | Failure | Evidence (file:line) | Kind | PR |
|---|---|---|---|---|
| F1 | Two Goal state machines: `GoalRunBudget` duplicates `GoalView` | `run_loop.rs:159-181` vs `reducer.rs:41-76`; ledger Events at seven sites; tail literals (see Goal bookkeeping) | Duplicated path, god-struct | PR8, PR9 |
| F2 | Engine interface is a mutation sequence with hidden ordering and four de facto seams | `engine.rs:1801-1810, 1977-2005, 2023-2046`; setup repeated in `cli/attached/run.rs:328-368` and `cli/exec.rs:247-290` | Shallow, leaky interface | PR12 |
| F3 | Tool step implemented twice; fake receipt used for sizing | `run_loop.rs:1434-2315` vs `children.rs:675-789`; `children.rs:701-716` | Duplicated path | PR10 |
| F4 | Every read-only Tool launches a Guard unit; `engine` sizes reservations with a platform constant | `guard.rs:482-495`, `linux.rs:19`, `macos.rs:43-46`, `run_loop.rs:19` | Misplaced seam, platform leak | PR1, PR7 |
| F5 | Telemetry span lifecycle threaded through the loop; three inline observers of one Event | `run_loop.rs` trace tokens, `RunLoop.trace`, `progress.rs:242-288`, `telemetry.rs:122` | Cross-module knowledge leak | PR11 |
| F6 | Provider provenance reassembled in `engine` from seven getters; ChatGPT fields in `RunControls` | `provider.rs:698-736`, `engine.rs:390-400, 2615-2650`, `run_loop.rs:143` | Knowledge leak | PR3 |
| F7 | `AttachedTerminal` toolbox; flows live in the binary; terminal in the library with zero library consumers | `terminal.rs:409-2125`, `cli/attached.rs:306`, fan-in 0 | Shallow interface, misplaced module | PR13 |
| F8 | `StateRoot` record bag; Provider evidence caches inside Store | `store/state.rs:249-375`, `store.rs:56-71, 201-224` | Shallow interface, knowledge leak | PR4 |
| F9 | God functions | See Function sizes | Locality | PR2, PR6, PR8, PR10 |
| F10 | Tool vocabulary split across nine owners; hand-written JSON Schema with a parallel `CALL_ORDER` | `types.rs:138-208, 1030-1162`, `config.rs:225`, `guard.rs:837`, `reducer.rs:690-697` | Locality | PR6 |
| F11 | Test helpers re-implement production knowledge; `cfg(test)` fault hooks on production types | `tests/common/semantic.rs`, `tests/session_run/tools/replay.rs:376-385`, fault hooks (see above) | Testing past the interface | PR3, PR4 |
| F12 | Spec and code drift | `agents/provider.md:18` says `Provider` is the only Engine behaviour seam, but four setter seams exist; `docs/architecture/system-overview.md` §2 says "one long-lived process" although the Engine opens per submission, labels children "one-call read-only" while `events.rs:102` admits 16 Tool calls, and duplicates its compaction paragraph at `:223-224`; `planning/agentic-teams/README.md` current facts are stale | Drift | PR13 |
| F13 | Dependency cycle `tools` to `engine` for `RunCancellation` | `guard.rs:5`, `linux.rs:5`, `macos.rs:5`, `macos/unit.rs:9` | Dependency direction | PR1 |

## What works and should be kept

- Journal, reducer and strict replay: `session/events.rs`, `session/reducer.rs`, `store/replay.rs`, `store/journal.rs`.
- A2 renderer and decoder: `provider/render.rs`, `provider/conversation.rs` (`SystemBlock` closed enum at `:77-80`; opaque `ProviderTurn` at `:161-165`), `provider/decode.rs`.
- Guard privilege seam with receipts: `tools/guard.rs:482-495`, `tools/types.rs:697-712`.
- `private_fs` as the model of a small shared deep module.
- `presentation` as pure projection.
- `tools/fs.rs` and `tools/git.rs` as deep leaves with `cap-std` opens, no-follow reads and device/inode pins.
- The scripted-Provider Engine corpus: real SQLite, paused time, acknowledgment gates.
