# Functional effectful beta base

## Metadata

- Status: `Done` (local implementation/correction handoff; live/user checks deferred)
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

Extend the existing schema-constrained semantic outcome with one sequential Tool proposal rather than introducing native function-call continuations or a second agent framework. Read-only requests retain Finish/Delegate semantics, with strict branches narrowed by typed phase and collaboration policy. Exact custom profiles reject Tool context before transport; changed read-only contracts invalidate their old exact evidence independently of configuration version.

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

Local exit criteria passed on Linux x86-64 with Rust 1.98.1. The actual configured subset is implemented, not a research-only scaffold. The reviewer correction slice below replaces the initial admission and evidence claims. Formatting, strict all-target lint, locked offline debug/release suites and activated native lanes have distinct scopes; no zero-test, ignored-by-default or synthetic Provider result counts as live/native evidence outside its owner.

| Owned claim | Evidence and scope |
|---|---|
| Iterative coding and persistence | Actual inspect/read/edit/create/build/execute/Skill/resource/MCP/final-answer journey with exact Provider requests and observations; closed SQLite replay, interrupted prefix and compiler-3 compaction without replaying effects |
| Authority and failure handling | Primary-only shared budget, read-only team children, exhaustion without another effect, link/FIFO/parent escapes, create/stale conflicts, invalid configuration admission, runtime-root overlap rejection before any Provider call/Event |
| Effective native lifecycle | Real namespaces/cgroup/filter receipt, outside TCP/UDP canaries, denied socket/destination and Guard-memory access, permitted anonymous IPC, double-fork/setsid cleanup, gated cancellation, scratch/process/memory quotas and exact-invocation kernel OOM accounting plus uncertainty without retry |
| MCP and Skills | Exact legacy stdio negotiation/list/call, 17 hostile protocol/schema/content modes, pinned complete definitions and resources, inert portable metadata and strict bounded parsers |
| Codec and startup | Shared ten-operation strict-wire corpus, phase/policy branches, required nullable fields, hostile calls and encoded credential reflection; shipped `exec --tools` configuration rejection and successful native OpenAI/Anthropic command continuation over synthetic TLS with closed replay |
| Local executable and dependency policy | Ordinary and guarded locked offline release builds, both CLI help paths, fresh strict advisory scan and cached-feed deny advisories/bans/licenses/sources without policy expansion |

Reproduce the executed lanes:

```sh
cargo fmt --all --check
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo test --locked --offline --all-targets -q
cargo test --locked --offline --release --all-targets -q
cargo test --locked --offline --test engine_run database_capacity_is_admitted_before_provider_usage -- --ignored --exact
cargo test --locked --offline --release --test engine_run database_capacity_is_admitted_before_provider_usage -- --ignored --exact
cargo test --locked --offline --test session_run tools:: -- --ignored --nocapture
cargo test --locked --offline --release --test session_run tools:: -- --ignored --nocapture
cargo build --locked --offline --release
bash scripts/release-build.sh
cargo audit -D warnings --db /tmp/arany-effectful-advisories-20261004
cargo deny --locked --offline check
```

The audit path above was a newly fetched private database for this run, not a required product directory. It contained 1,290 advisories and scanned 458 locked packages without warnings. Deny's duplicate-version warnings are nonfatal; its advisory source is the cached feed. The existing Crossterm dependency emits an unchanged `unused_parens` warning; application lint still passes with `-D warnings`. See the [current inventory](../../docs/security/beta-dependency-inventory.md) and [Tool dependency review](../../docs/security/local-tool-dependency-review.md) for the source/unsafe/license limits.

The current ordinary executable `target/release/arany` has SHA-256 `1bdf421de4f6eab4ca611f8b5da3d0716558dc194001ba726e8673111f6d7cf9`. The fresh protected output `target/release-guarded.D0CQUPnn/release/arany` has SHA-256 `b77f87653baec71481deb509a84c30b870e5385ff307a2fcaaeb5fcdb9c83699`. Both builds and help paths pass; these artifacts are not byte-identical, and no reproducibility claim is made. Only the protected build carries its explicit bundled-source/link checks. Neither result establishes compiler provenance, Rust 1.88 or redistribution approval. The normal executable is ready for the user's local trial.

Applied patches and affected files were reviewed directly without Git operations or commits. Architecture, module rules, private setup, dependency inventory and [next steps](../../NEXT_STEPS.md) now describe the actual implementation. Durable findings include explicit user-namespace setup, retained cgroup ownership through unit garbage collection, group OOM attestation, runtime-root exclusions, socket/IPC separation, bounded mutation receipts, nested argument reflection and validation of public typed replay.

## Handoff and remaining scope

Use the [tool guide](../../docs/tools.md) to create private grants, then run `cargo run --release -- --tools`. This is an offline selected-project foundation: generic subprocess changes are discarded, and typed file operations integrate edits. It neither downloads dependencies nor grants network/credential access, interactive/background jobs, remote MCP or child effects. Unsupported native enforcement fails closed.

[NEXT_STEPS.md](../../NEXT_STEPS.md) owns the final user checklist and beta-2 expansion. Real tool-capable Provider/ChatGPT turns, account/credential lifecycle, clipboard/vision and terminal/accessibility checks remain unverified and were not used as coding blockers. Native macOS/ARM64, alternative Linux enforcers, Rust 1.88, larger build/runtime profiles and distribution evidence remain separate work. Anthropic consumer-plan permission/route admission remains deferred functionality, not a passed check or API-key substitute. No real account, wallet, clipboard or paid Provider call was used.

## Change log

- `2026-10-04`: User expanded beta 1 to tools, commands, Skills and MCP; researched current primary sources and selected a bounded local foundation. The older goal-manager record cannot be replaced while unfinished; this plan tracks the authorized new work without falsely completing it.
- `2026-10-04`: Implemented and verified the local coding foundation, closed the final isolation/receipt/reflection/replay corrections, built the executable and recorded the final user-owned checks. No commits or shared CI/release-script policy changes.
- `2026-10-04`: Reopened after independently reproducing Event admission, compaction-count admission, escaped Finish persistence and missing/null text defects. The initial passing MCP/OOM assertions did not prove their full intended claims. Corrected those defects/evidence owners, fixed synthetic subscription SSE compatibility and bounded failure diagnostics, and consolidated local checks without paid requests or commits. The screenshot's real account cause remains unverified.
- `2026-10-04`: Reproduced and corrected the database-byte admission floor: a Provider call previously preceded predictable failure to close the Run. The activated Engine owner now proves no-call direct/team/Tool/compaction rejection, unchanged replay and recovery in both profiles. Rebuilt ordinary/protected executables and rechecked offline suites, strict lint, eight native Tool owners, TLS, synthetic ChatGPT and cached advisory/license policy. Byte preflight remains distinct from a global lease or physical disk reservation.
- `2026-10-04`: Verified the separate input-size relationship without a production change: maximal JSON-escaped objectives, alone and with the maximum image, survive the existing exact-request/reopen/compaction/export journey; oversized objectives reject before hostile Workspace input with no calls or Events. Extended two existing owners rather than adding tests or a redundant runtime guard. Debug/release offline suites, formatting, strict lint and direct diff review pass; executable hashes remain unchanged.

## Reviewer correction slice

Input-bound follow-up: the suspected input overflow is not a defect under the current caps. The Engine's 8 KiB objective cap, unlike its 32 KiB answer cap, fits the ordinary Event even at six-byte JSON escaping. The extended snapshot/reopen/fork/compaction/export journey proves maximal escaping with the maximum image and then without images, complete Provider semantics, exact closed replay and shipped human/JSONL export. Its text-only payload is 49,163 bytes; the image payload remains below 320 KiB. The existing pre-Workspace table also proves rejection above the raw objective cap, with or without an image, and no Provider calls or Events. Both owners pass in debug and release, including the full offline suites; formatting, strict lint and the direct diff review pass. Engine rules retain the cap/encoding relationship. No redundant serializer or runtime guard, raised limit, credential/transport/schema or production-source change was required, and the previously built executables retain their recorded hashes. Native/manual/live claims remain unchanged and unverified.

Database-headroom follow-up: the same 4 MiB floor was checked at operation admission and every append, without funding the operation's later writes. The activated file-backed Engine regression reproduced a Provider call followed by predictable `StorageFull`. Shared Store preflight now adds all admitted slots' maximum ordinary payloads and a conservative page/index allowance, plus one larger image Message, above the unchanged append floor. The exact owner passes in debug and release: direct, team, Tool-enabled Run and compaction rejection make zero calls and preserve closed history; after returning capacity, a fully asserted direct request and compaction succeed in the same Session and replay closed. The bounded 256 MiB fixture is explicitly ignored by default, not a default passing assertion. Quotas, Event format, grants and independent-lineage concurrency are unchanged. This is conservative single-writer byte admission, not a physical disk reservation or a lease against concurrent independent writers. Local consolidation, executable build and direct diff review close the slice; Store/Engine rules, architecture and next steps retain that scope.

Engine owns whole-operation Event headroom under its existing per-Session lock. Store checks the resolved lineage count and database admission on its owner thread before admission; compaction also checks its lifetime count before inference. Reserve the worst bounded Run topology rather than checking only the next intent/result pair: direct lifecycle, configured children and all shared Tool continuations must fit. This is logical quota admission, not protection against disk faults, noncooperating writers or process death. Existing interrupted/uncertain recovery remains required.

An incremental per-Tool check alone was rejected: it could still admit Provider usage or children without terminal capacity. A new executor seam or persisted reservation subsystem was rejected: cooperating Session mutations already share one lock and the current defect is the known per-Session logical quota. Keep the existing caps, canonical format, private Store owner and separate Guard.

1. Extend the existing Engine/wire corpora with exact no-call admission, closed-replay escaped output and missing/null-then-valid cases; require a failing regression before correction. Add bounded logical quota admission and exact canonical payload checks.
2. Communicate a typed allowed-outcome/delegation contract through the existing Provider interface; keep adapters and Engine strict. Reject unsolicited custom Tool contexts before HTTP. Validate canonical rejection reasons and exact semantic fake calls outside panic handling.
3. Correct MCP refusal evidence, independently prove actual memory payload/OOM accounting and distinguish observation failure from absence. Convert remaining process captures to concurrent bounded drains with EOF deadlines; native credential deletion uses the production supervisor. Preserve the selected Anthropic workspace in its explicit live owner, without running that paid lane.
4. Close successful shipped `exec --tools` adapter/Guard/replay coverage with a networkless synthetic fixture. Recheck formatting, strict lint, offline debug/release tests and affected activated native owners; review source diffs and update module rules, architecture and next steps.
5. Only after the reviewer slice, investigate the user's subscription failure through synthetic response/request fixtures and official OpenAI documentation. Never read real tokens, log response bodies, make unapproved usage-consuming requests, relax strict outcomes or claim a live fix without live evidence. Missing real-account evidence remains a final user-owned check, not a coding pause.

The security pass covers predictable persistence exhaustion before effects/Provider spend, bounded serialization of hostile output, pre-disclosure custom-profile admission, truthful native failure evidence and unchanged credential/endpoint isolation. The same existing event/count/byte/time/descendant limits remain authoritative; no retries, fallback, grants or new dependency are added.

### Subscription compatibility and diagnostic slice

The latest screenshot identifies only the old generic invalid-response category. No raw live response is available or retained, so its specific cause remains unknown. A new synthetic regression failed because the stream parser required an `event:` line even when valid SSE data contained its semantic `type`. The corrected parser admits named or data-only typed frames across the existing line endings/chunk boundaries, still requires matching labels when present, and rejects malformed, incomplete, unbounded or token-reflecting data. Only a complete strict terminal response yields an outcome.

The request still follows [OpenAI's preview contract](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations): `store: false`, streaming, local input history, no unsupported remote cap. [Semantic Responses events](https://developers.openai.com/api/docs/guides/streaming-responses) do not grant permission to accept partial text, loosen structured outcomes or switch billing. New closed failures separate stream protocol, selected model/message/usage contract, structured outcome and local output acceptance; Engine preserves the reason in existing version-2 call Events and shared feedback. Native nonstreaming adapter error behavior stays unchanged. Optional checker/admission contract versions are invalidated without requiring a paid check or discarding current consent.

The optimized shipped ChatGPT journey uses a signed synthetic identity, isolated private-file account and ordinary TLS to the compiled endpoints. Its first direct Run has no model-check record and receives data-only SSE; consecutive direct history and a single-child team continue through strict replay. This proves that compatibility case, not a repaired real subscription. Real account, native keyring deletion/renewal, clipboard, macOS and Rust 1.88 checks remain final handoff items, not coding blockers.

### Corrected evidence ownership

- Persistence: the Engine corpus admits exact-fit direct/team/fork Runs and rejects insufficient full envelopes with zero calls; the 65th direct compaction attempt rejects before egress while inherited summaries do not charge a fork's direct quota. Escaped Finish outputs are accepted only when their actual canonical payload fits, otherwise recorded as invalid response and reopened without an answer.
- Wire authority: existing inbound native corpora reject all missing/null-then-valid text cases; outbound corpora assert typed policy/phase branches and child isolation. Public custom calls reject Tool/invalid child scope before any listener connection. Existing negative Engine rows assert exact calls and invalid-response disposition outside fake panic handling.
- MCP: forbidden calls now receive valid peer success rather than crashing the peer. In a separate temporary source copy, disabling each schema-drift and argument-validation guard made its exact row fail with unexpected success. Both deliberate mutations are restored; production checks were never disabled.
- OOM: the payload stops at a public kernel gate before allocating. A retained `memory.events` descriptor initially raced cgroup retirement and failed with `ENODEV` under load; it was not accepted as evidence or retried away. The corrected owner pins the actual unit invocation, requires known `OOMKills` to advance from zero and `Result=oom-kill` before Engine cleanup, then proves quiescence and closed failed replay. The [systemd D-Bus contract](https://github.com/systemd/systemd/blob/main/man/org.freedesktop.systemd1.xml) distinguishes this kernel counter from userspace OOM accounting and its unknown sentinel. This native gate requires that accounting property; it changes no product grant or OS limit. Observation errors and unknown counters fail, never imply disappearance or OOM.
- Process/credential owners: product captures drain both bounded pipes while awaiting exit/EOF, with deadline and kill/reap ownership. Existing descendant/PTY guards retain their scope. Native ChatGPT deletion now calls the production supervisor and checks absence, but that actual-store owner was not activated. The explicit Anthropic live owner forwards only its validated selected API workspace; no live call ran.

### Local consolidation

The locked offline default suites pass 296 debug and 283 optimized tests; 53 and 60 respectively remain explicitly ignored. Counts differ because debug-only account-root fixtures are not optimized proof. The database-capacity owner is explicitly activated and passes in both profiles, not counted as a default pass. All eight activated Linux Tool owners pass in debug and release; the TLS parent passes with exactly one activated assertion-bearing child. The optimized synthetic ChatGPT direct/team journey passes as one exact owner, including its data-only SSE turn. Formatting, strict all-target Clippy, ordinary/guarded release builds and both help paths, strict advisory review and deny advisories/bans/licenses/sources pass. The advisory scan uses the explicitly fetched database above without refetching; deny uses its existing cached feed. Unchanged dependency duplicate-version and Crossterm warnings are not application lint failures.

Source review compares the saved read-only reviewer snapshot with the current files, without Git operations. No lockfile, dependency, CI, release script, real account, wallet or clipboard change was required. Module rules now record whole-operation/ancestor admission, escaped payload bounds and truthful negative evidence; current setup, architecture and next steps describe the corrected behavior and remaining growth limits. Manual/live verification remains unperformed, and Anthropic consumer-plan access remains deferred missing functionality.
