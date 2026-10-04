# Functional effectful beta base

## Metadata

- Status: `Done` (local functional foundation; final user-owned integration checks remain unverified)
- Owner: Codex and the user
- Last updated: `2026-10-04`

## Goal

Extend beta 1 into a usable coding harness: an AgentRun can inspect selected project files, make guarded edits, run bounded commands, load portable Skills and invoke explicitly configured local MCP tools, then use their observations to finish the same Run. UI polish and real-account/manual testing are not prerequisites for this implementation handoff. No commits are authorized.

## Scope

- Built-in file listing, text reading/search, digest-preconditioned write/edit, command execution, Skill loading/resources and local stdio MCP discovery/calls.
- A real iterative primary loop, strict Provider outcome encoding, durable correlated intent/result facts and replay that never executes history.
- Primary-only effects initially. Flat children retain their existing read-only reasoning contract; they receive no command, write or MCP authority.
- Trusted private `tools.json`, enabled explicitly with `--tools` in attached and `exec` modes. Missing/invalid configuration and changed pinned executable/resource inputs fail closed. A Run's pinned grants are immutable; later configuration edits do not cancel it.
- Linux containment through existing kernel namespaces/cgroup limits and installed native facilities. Unsupported enforcement fails closed; no desktop-specific contract. Native macOS implementation/evidence, remote MCP HTTP/OAuth, interactive/background commands, package installation and arbitrary plugins are outside this first local base.
- Anthropic subscription authority and user-owned live/provider/terminal tests remain separate deferred work; no credentials, paid requests, wallet or clipboard inspection.

## Current state

The Engine now invokes strict `Finish | Delegate | Tool` outcomes under explicit Tool activation, with shared primary planning/synthesis limits and unchanged one-call read-only children. Private `tools` files own immutable configuration, native file operations, the separately enforcing Guard, progressive Skills and local stdio MCP. Canonical Tool attempt/result Events and strict reduction preserve uncertainty; context and compaction carry only past facts. Research compares [current harness loops](../../docs/research/effectful-harness-tool-loops-2026-10.md), [MCP and Skills](../../docs/research/mcp-and-runtime-skills-2026-10.md), and the [platform Guard](../../docs/research/effect-guard-platform-base-2026-10.md). [Local tools](../../docs/tools.md) documents the actual subset and setup. Existing security/Provider/Engine/Session/Store/CLI/testing rules and [Tool rules](../../agents/tools.md) remain authoritative.

## Ownership and seams

| Responsibility | Owner | Interface / seam |
|---|---|---|
| Model decisions and continuation | Provider / Engine | Closed semantic Tool outcome and bounded observation transcript; existing real Provider adapters |
| Immutable grants, intent normalization and aggregate Tool reservations | `tools` | Private deterministic Policy; host configuration precedes Workspace input |
| File/command isolation and lifecycle | private `tools::guard` | Supervised same-binary process, digest-bound one-use intent and effective-limit receipt; privilege seam |
| MCP JSON-RPC/version/schema/result translation | private `tools::mcp` | Bounded local stdio profile; external protocol seam, no Engine transport types |
| Portable Skill metadata and snapshots | private `tools::skills` | Explicit pinned resources; instructions do not grant execution |
| Attempts, results and crash recovery | Session / Store | Strict typed Events and reducers; no schema migration or replay execution |
| Startup and safe receipts | CLI / presentation | Explicit activation, plain errors and canonical inspection; no new UX surface required |

## Chosen direction

Extend the existing schema-constrained semantic outcome with one sequential Tool proposal rather than introducing native function-call continuations or a second agent framework. Read-only requests retain their current schema. Exact custom profiles cannot use the changed tool-capable encoding under old conformance evidence.

Every effect goes through the same router: normalize and validate, reserve, authorize with the pinned restrict-only grant, commit the intended attempt, execute in the separately enforcing Guard, commit the bounded observation, then infer again. The journal contains no reusable approval. A missing terminal result means interrupted/uncertain, never an automatic retry.

Built-in mutations use no-follow handles, expected-content digests and fresh atomic replacement, never truncation of a potentially aliased inode. Commands and MCP run with an isolated bounded snapshot of explicitly selected project paths, read-only runtime/Skill inputs, no network, no account/StateRoot mounts, a minimal environment and aggregate descendant limits. Changes made by arbitrary subprocesses do not automatically overwrite host files; native typed write/edit remains the integration path. This trades direct shell mutation and external network access for a useful, contained first base. No process-group-only or unbounded scratch fallback is permitted.

Local MCP explicitly supports the complete documented 2025-11-25 tools profile first. It initializes, validates its negotiated version, lists bounded tools, validates arguments/results, calls exactly once and disconnects. Modern 2026-07-28 and remote transports require different lifecycle/disclosure adapters; this implementation does not claim them. Skills use the portable name/description/body and lazy resources; scripts require ordinary configured command/interpreter authority, not frontmatter grants.

## Security and resource model

- Assets: account credentials, canonical state, omitted host/project files, authorized file contents, Provider budget and host process/memory/disk availability.
- Inputs: all project text, model proposals, MCP metadata/results, Skill instructions, command output and replayed facts are untrusted. Only owner-verified private configuration supplies grants.
- Intent: actor, Run, Tool operation, normalized resources/argv, destinations (none for subprocesses), finite expiry/use count and limits share one digest across Policy, journal and Guard. Denial produces a bounded observation without execution.
- Startup: configure and pin executable/resource digests before project input; reserve the aggregate call/Tool/context budget before Run start. Provider credentials remain in the Engine process and never enter subprocess arguments, environment or mounts.
- Limits: finite model/Tool iterations, selected files/bytes, request/result sizes, mutation sizes, process time, scratch bytes, memory, descendants, catalog pages/tools, JSON/schema depth and notification counts. Whole-record rejection or explicit truncation; progress never renews deadlines.
- Cleanup: own the exact transient process unit/cgroup and its lifetime, not just a launcher PID. Cancellation/deadline/overflow closes channels, terminates the owned descendants, drains under a parent deadline and verifies quiescence. No automatic effect or Provider retries.
- Non-claims: no host-root or hostile same-UID protection, no universal secret detector for explicitly disclosed project files, no rollback or exactly-once external effects, no remote MCP support, no native macOS containment proof or live Provider compatibility.

## Execution and verification map

1. Add typed Tool/config/Policy values and Guard enforcement; a real native corpus owns permitted execution plus denied roots/links, altered inputs, environment isolation, output/time/disk/memory/process bounds and descendant cancellation.
2. Extend Provider schemas and the primary Run loop with bounded correlated observations. The existing strict semantic fake owns read/edit/error/continuation/final-answer expectations and aggregate exhaustion; adapters own offline strict-wire fixtures.
3. Add canonical Tool attempt/result Events and strict reduction, including incomplete-dispatch recovery, old-journal compatibility and complete-turn compaction context. Reopened file-backed Store owns durable proof; replay cannot call the executor.
4. Implement portable Skills and local MCP through the same Guard. Real stdio peers own version, catalog/schema, IDs, hostile framing/results, cancellation and no-replay evidence; Skill corpus owns scalar metadata/provenance/resources and inert behavioral fields.
5. Wire explicit CLI activation in both presentations, document setup/limitations and update current architecture/next steps. Product-process evidence owns argument/channel behavior and canonical `exec`/`show` receipts, not a test-only unsandboxed shortcut.
6. Consolidate formatting, strict lint, local tests, executable build, dependency-policy and diff/security/auto-learning review. Keep real-account, manual/accessibility and native macOS checks in the final user checklist; they are unverified, not implementation blockers.

## Exit criteria

A synthetic model completes an actual guarded inspect/edit/command/Skill/MCP/answer journey, with exact continuation expectations, finite budgets, safe errors, observable cancellation, closed-journal replay and a usable executable. Unsupported enforcement and capabilities fail explicitly. Documentation matches the actual local subset and names final user-owned tests without converting them into passing evidence.

## Verification outcome

Local exit criteria passed on Linux x86-64 with Rust 1.98.1. The actual configured subset is implemented, not a research-only scaffold. Formatting, strict all-target lint, locked offline default tests and optimized default tests pass. The activated native Tool lane passes all seven intended owners in both debug and release; no zero-test, ignored-by-default or synthetic Provider result counts as live/native evidence outside its stated owner.

| Owned claim | Evidence and scope |
|---|---|
| Iterative coding and persistence | Actual inspect/read/edit/create/build/execute/Skill/resource/MCP/final-answer journey with exact Provider requests and observations; closed SQLite replay, interrupted prefix and compiler-3 compaction without replaying effects |
| Authority and failure handling | Primary-only shared budget, read-only team children, exhaustion without another effect, link/FIFO/parent escapes, create/stale conflicts, invalid configuration admission, runtime-root overlap rejection before any Provider call/Event |
| Effective native lifecycle | Real namespaces/cgroup/filter receipt, outside TCP/UDP canaries, denied socket/destination and Guard-memory access, permitted anonymous IPC, double-fork/setsid cleanup, gated cancellation, scratch/process/memory quotas and group OOM uncertainty without retry |
| MCP and Skills | Exact legacy stdio negotiation/list/call, 17 hostile protocol/schema/content modes, pinned complete definitions and resources, inert portable metadata and strict bounded parsers |
| Codec and startup | Shared ten-operation native strict-wire corpus, required nullable fields, hostile calls and encoded selected-credential reflection; shipped `exec --tools` configuration rejection with exact channels, unchanged canary and empty closed journal |
| Local executable and dependency policy | Ordinary and guarded locked offline release builds, both CLI help paths, fresh strict advisory scan and cached-feed deny advisories/bans/licenses/sources without policy expansion |

Reproduce the executed lanes:

```sh
cargo fmt --all --check
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo test --locked --offline --all-targets -q
cargo test --locked --offline --release --all-targets -q
cargo test --locked --offline --test session_run tools:: -- --ignored --nocapture
cargo test --locked --offline --release --test session_run tools:: -- --ignored --nocapture
cargo build --locked --offline --release
bash scripts/release-build.sh
cargo audit -D warnings --db /tmp/arany-effectful-advisories-20261004
cargo deny --locked --offline check
```

The audit path above was a newly fetched private database for this run, not a required product directory. It contained 1,290 advisories and scanned 458 locked packages without warnings. Deny's duplicate-version warnings are nonfatal; its advisory source is the cached feed. The existing Crossterm dependency emits an unchanged `unused_parens` warning; application lint still passes with `-D warnings`. See the [current inventory](../../docs/security/beta-dependency-inventory.md) and [Tool dependency review](../../docs/security/local-tool-dependency-review.md) for the source/unsafe/license limits.

The final ordinary and guarded executables both have SHA-256 `271f221d515e8067b48cb208e7da4f2e2ef7ce6d7434c2d5678dc422a3d35e4c`; the guarded output is `target/release-guarded.kVleFbve/release/arany`. Matching outputs from this host do not establish portable reproducibility, compiler provenance, Rust 1.88 or redistribution approval. The normal `target/release/arany` is ready for the user's local trial.

Applied patches and affected files were reviewed directly without Git operations or commits. Architecture, module rules, private setup, dependency inventory and [next steps](../../NEXT_STEPS.md) now describe the actual implementation. Durable findings include explicit user-namespace setup, retained cgroup ownership through unit garbage collection, group OOM attestation, runtime-root exclusions, socket/IPC separation, bounded mutation receipts, nested argument reflection and validation of public typed replay.

## Handoff and remaining scope

Use the [tool guide](../../docs/tools.md) to create private grants, then run `cargo run --release -- --tools`. This is an offline selected-project foundation: generic subprocess changes are discarded, and typed file operations integrate edits. It neither downloads dependencies nor grants network/credential access, interactive/background jobs, remote MCP or child effects. Unsupported native enforcement fails closed.

[NEXT_STEPS.md](../../NEXT_STEPS.md) owns the final user checklist and beta-2 expansion. Real tool-capable Provider/ChatGPT turns, account/credential lifecycle, clipboard/vision and terminal/accessibility checks remain unverified and were not used as coding blockers. Native macOS/ARM64, alternative Linux enforcers, Rust 1.88, larger build/runtime profiles and distribution evidence remain separate work. Anthropic consumer-plan permission/route admission remains deferred functionality, not a passed check or API-key substitute. No real account, wallet, clipboard or paid Provider call was used.

## Change log

- `2026-10-04`: User expanded beta 1 to tools, commands, Skills and MCP; researched current primary sources and selected a bounded local foundation. The older goal-manager record cannot be replaced while unfinished; this plan tracks the authorized new work without falsely completing it.
- `2026-10-04`: Implemented and verified the local coding foundation, closed the final isolation/receipt/reflection/replay corrections, built the executable and recorded the final user-owned checks. No commits or shared CI/release-script policy changes.
