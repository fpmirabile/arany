# Linux end-to-end Tool latency

## Metadata

- Status: `Ready`
- Owner: repository owner (target and named-host decisions); implementing lead (corpus and verification)
- Last updated: `2026-10-22`
- Research baseline: `14384e1b080946346fe5391d2d0dfe462f6cf72d`
- Next action: resolve the owner questions, then implement the two slices in order. This change writes the plan and index only; the lead owns commits.

## Outcome and scope

Deliver one capability: a reproducible, offline Linux report of p95 latency from a complete Provider Tool proposal reaching the Engine through acknowledgement of its durable result, for Read, Edit and one fixed guarded command. This supplies the measurable part of [Daily use first item 11](../../NEXT_STEPS.md#daily-use-first), without changing effect behavior or replacing [Pursue item 2](../../NEXT_STEPS.md#pursue-in-order)'s narrower Read-dispatch evidence.

The deliverable is an explicitly ignored release corpus, deterministic checks of its measurement boundaries and results, and an owner-run named-host evidence procedure. Performance optimization, a new CLI/OTLP reporting surface, cold-disk measurements, live Provider timing, macOS latency evidence, CI repair and flake closure are outside this feature.

## Research

### Current timing and evidence

| Finding | Source and consequence |
|---|---|
| Every Event envelope carries `created_at_ms`, alongside sequence, Session, Run and agent identity. | [`session/events.rs:457`](../../crates/arany/src/session/events.rs#L457). Pair by scope and intent identity, never just adjacent Events. |
| The timestamp is Unix wall time at millisecond resolution, taken **inside the transaction before reducer validation, INSERT and commit**. | [`store/journal.rs:295`](../../crates/arany/src/store/journal.rs#L295), validation/INSERT at [343](../../crates/arany/src/store/journal.rs#L343), commit at [243](../../crates/arany/src/store/journal.rs#L243). A timestamp difference is reproducible from history, but is neither monotonic nor the exact interval through the final commit acknowledgement. |
| Journal publication follows the Store append acknowledgement and ledger update. The subscriber ring is lossy. | [`journal.rs:123`](../../crates/arany/src/journal.rs#L123), [tap completeness caveat at 22](../../crates/arany/src/journal.rs#L22); [`execution.md:9`](../../docs/specs/execution.md#committed-event-stream). Subscriber receive time includes observer scheduling and cannot supply an exact endpoint. |
| The Engine receives the Provider outcome, validates it and commits `ProviderCallRecorded` before executing the proposed step. | [`engine/run_loop.rs:447`](../../crates/arany/src/engine/run_loop.rs#L447), [466](../../crates/arany/src/engine/run_loop.rs#L466); Provider invocation at [1163](../../crates/arany/src/engine/run_loop.rs#L1163). Starting at `ToolStarted` misses this work and intent admission. |
| `ToolStarted` is appended before the permission verdict, hooks and dispatch; `ToolFinished` is appended after the observation returns. Neither records a dedicated elapsed duration. | [`run_loop/step.rs:731`](../../crates/arany/src/engine/run_loop/step.rs#L731), [746](../../crates/arany/src/engine/run_loop/step.rs#L746), [818](../../crates/arany/src/engine/run_loop/step.rs#L818). Guard execution and checkpoint capture precede the finish acknowledgement: [1538](../../crates/arany/src/engine/run_loop/step.rs#L1538). |
| A command's captured changeset can cause a separate integration after its own `ToolFinished`. | [`run_loop/step.rs:307`](../../crates/arany/src/engine/run_loop/step.rs#L307), [326](../../crates/arany/src/engine/run_loop/step.rs#L326). The proposed no-op command has an empty changeset; do not claim its timing covers publication of a nonempty command changeset. Edit publication is inside its measured operation. |
| OTLP exports Run, Goal, Agent and Provider spans and Tool Event markers, not a lossless per-Tool latency ledger. | [`telemetry.md:14`](../../docs/specs/telemetry.md#spans), [bounds at 29](../../docs/specs/telemetry.md#export-bounds); [`telemetry.rs:339`](../../crates/arany/src/telemetry.rs#L339) uses `created_at_ms`, and [1067](../../crates/arany/src/telemetry.rs#L1067) classifies Tool Events as Run markers. The spec calls these “commit times”; the implementation's pre-commit timestamp placement above limits that terminology. Do not infer post-commit precision from it. |
| Development diagnostics are failure-oriented, best effort, bounded and absent in optimized builds. | [`diagnostics.md:5`](../../docs/specs/diagnostics.md#L5), [7](../../docs/specs/diagnostics.md#L7), [18](../../docs/specs/diagnostics.md#L18). Their timestamps do not form a successful-Tool timing series. |
| `arany show --output jsonl` exposes persisted envelopes after read-only load/replay; text lists sequence, kind and payload. Neither computes p95. | [`main.rs:198`](../../crates/arany-cli/src/main.rs#L198), [`presentation.rs:1402`](../../crates/arany-cli/src/presentation.rs#L1402), [1742](../../crates/arany-cli/src/presentation.rs#L1742). Reuse this inspection surface for audit, not as a stopwatch. |
| The existing repeated-Read test measures 40 sequential runtime dispatches on a freshly written 53,248-byte file and prints p50, p95 and max. It is **not ignored or release-only**, and asserts correctness, not elapsed time. | [`tools/tests.rs:3083`](../../crates/arany/src/tools/tests.rs#L3083). It requests the default 2,000-line page, sorts durations, and uses index 37 for nearest-rank p95. No Engine, journal commit or Guard launch is inside that timer. Release with `--nocapture` is the named-host measurement procedure, not a property enforced by this existing test. |
| Scripted Providers and real Engine/Store/Guard journeys already exist. | [`test_support/scenario.rs:33`](../../crates/arany/src/test_support/scenario.rs#L33), [181](../../crates/arany/src/test_support/scenario.rs#L181); [`guard_runs.rs:20`](../../crates/arany-cli/tests/guard_runs.rs#L20), [`guard_runs/permissions.rs:66`](../../crates/arany-cli/tests/guard_runs/permissions.rs#L66), [`session_run/tools.rs:82`](../../crates/arany-cli/tests/session_run/tools.rs#L82). Reuse these owners; Provider remains the only behavior double. |

### Targets and peer practice

Neither the [Tools latency evidence row (`tools.md:814`)](../../docs/specs/tools.md#L814) nor the [Execution limits (`execution.md:251`)](../../docs/specs/execution.md#persistence-bounds) specifies an end-to-end p95. Read p95 <= 50 ms belongs to [`NEXT_STEPS.md:38`](../../NEXT_STEPS.md#pursue-in-order)'s named-host dispatch check. It is not an existing 50 ms durable-result SLO. Commands default to 120 seconds, with other preparation/cleanup limits and Goal accounting; these are safety deadlines, not performance targets ([`tools.md:673`](../../docs/specs/tools.md#L673)). SQLite uses DELETE journaling and `synchronous=EXTRA` ([`store/journal.rs:192`](../../crates/arany/src/store/journal.rs#L192)); the benchmark must retain both.

Peer sources checked on 2026-10-22:

- **Codex CLI:** [official telemetry documentation](https://learn.chatgpt.com/docs/config-file/config-advanced#otel-metrics-emitted) distinguishes tool-duration metrics from turn end-to-end duration. The former official `developers.openai.com/codex/config-advanced` URL redirects there. At pinned revision `b4ad33d1a3c10c9652dd57f7afd7a949f8f26895`, [`session_telemetry.rs:1255–1335`](https://github.com/openai/codex/blob/b4ad33d1a3c10c9652dd57f7afd7a949f8f26895/codex-rs/otel/src/events/session_telemetry.rs#L1255-L1335) measures an awaited handler using `Instant`; [`registry.rs:701–843`](https://github.com/openai/codex/blob/b4ad33d1a3c10c9652dd57f7afd7a949f8f26895/codex-rs/core/src/tools/registry.rs#L701-L843) places pre/post hooks outside that timed handler. This is not Provider receipt through durable result commit.
- **Claude Code:** [tool spans](https://code.claude.com/docs/en/monitoring-usage#span-attributes) separate the parent Tool duration (permission wait plus execution) from the execution child span; the [tool-result event](https://code.claude.com/docs/en/monitoring-usage#tool-result-event) reports execution `duration_ms`. [Bash settings](https://code.claude.com/docs/en/env-vars) default to a 120,000 ms timeout and a 600,000 ms maximum configurable by the model. No durable-result commit endpoint is promised by those measurements.
- No published Read/Edit/command p95 SLO was found in those reviewed official sources and official-domain searches. This is a bounded research finding, not a claim about peers' internal objectives. Copy per-operation reporting and explicit timing boundaries; Arany's durability and Guard guarantees require a broader interval, so numeric peer parity cannot be inferred.

Recommended **provisional named-host objectives**, subject to owner acceptance before implementation:

| Corpus row | Proposed p95 | Reason and limitation |
|---|---:|---|
| Existing warm Read dispatch | <= 50 ms | Preserve Pursue item 2's existing objective and measurement owner. |
| Warm Read, receipt to committed result | <= 100 ms | Adds a proposed 50 ms allowance for validation, permission and durable journal work to the dispatch objective; not a measured cost decomposition. |
| Small guarded Edit, receipt to committed result | <= 250 ms | A proposed quarter-second responsiveness budget for a small edit, including process isolation, checkpoint work and publication. No baseline currently establishes feasibility. |
| Fixed guarded no-op command, receipt to committed result | <= 500 ms | A proposed half-second budget including native unit startup, snapshot/capture and verified teardown. It excludes arbitrary workload runtime claims. |

These are report comparisons, never CI wall-time assertions or new runtime deadlines. A miss retains its samples and opens diagnosis; do not increase a target or relax enforcement to obtain a pass.

### Separately tracked reliability state

At research time, the latest completed `main` [CI run 39726180522](https://github.com/fpmirabile/arany/actions/runs/39726180522) was successful but **both macOS test jobs were skipped** for the documentation change. That run does not prove green executed macOS lanes. The backlog still records busy-detach instability, an intermittent Keychain `Locked` refusal and an unclassified permissions-journey early stop ([`NEXT_STEPS.md:209`](../../NEXT_STEPS.md#L209), [215](../../NEXT_STEPS.md#L215), [216](../../NEXT_STEPS.md#L216)). Their closure remains separate; this plan does not claim a closed flake list or repair CI.

## Contract and scenarios

The existing [Tools contract](../../docs/specs/tools.md#in-process-reads) and [Execution contract](../../docs/specs/execution.md#committed-event-stream) remain authoritative. This is measurement of those behaviors, not a new Tool, permission mode or stored Event format. Accepted benchmark methodology belongs with its corpus and owner procedure; targets remain named-host evidence objectives in `NEXT_STEPS.md`, not universal product guarantees. No spec edit is required for unchanged behavior. Before implementation, resolve the timestamp terminology discrepancy by explicitly recording the pre-commit interpretation in the measurement procedure; do not silently change journal timestamps or telemetry semantics.

Define `tool_receipt_to_commit_us = t_finish_ack - t_receipt`, using a monotonic clock:

- **Start:** the Engine receives the complete typed response from the scripted Provider containing exactly one primary Tool proposal, before Engine semantic acceptance, `ProviderCallRecorded`, intent sizing/admission and `ToolStarted`. This is not the first streamed argument byte. Network transport, model inference and Provider wire decoding precede this boundary and are excluded.
- **End:** the matching `ToolFinished` append successfully acknowledges durable commit and reducer acceptance. Include all work between these endpoints: permission verdict journaling, any in-interval waiting, Guard capacity acquisition, native startup/handshake, execution, output fitting, receipts, verified cleanup and Edit checkpoint/publication work. Read correctly uses its admitted in-process path and has no per-read Guard spawn.
- Exclude fixture creation, Engine startup, initial Goal-plan establishment, final model answer, terminal rendering, `show`, post-result hooks and subsequent Provider calls. The baseline configures no hooks, remote telemetry, human/AI approval waits, children or competing Tools. This narrows the workload, not its enforcement. A populated command changeset's subsequent integration would need a separate endpoint and workload; the baseline command must prove an empty changeset.

**Replayability:** closed SQLite history must identify and validate every accepted sample's Provider receipt, intent, permission decision, observation and outcome without executing anything. Also report `ProviderCallRecorded → ToolFinished` and `ToolStarted → ToolFinished` wall-time deltas from persisted envelopes, labeled `journal_interval_ms`, never as exact commit latency. Reject negative wall-time intervals as clock-invalid, and report that classification without altering the monotonic duration. Exact monotonic durations cannot be reconstructed from today's journal alone: retain a bounded local sample report keyed by Run, call index, intent and Event sequences, validate its joins against reopened history, and recompute percentiles from that report. No new canonical timing Event or migration is justified for this benchmark. A crash before the terminal acknowledgement produces an incomplete measurement, never a fabricated latency or retry.

| Scenario | Required observation and evidence owner |
|---|---|
| Read succeeds | Exact expected page/digest and `in_process` attestation; one receipt/intent/finish pair; reopened view agrees. Extend the existing Read corpus and [`session_run/tools.rs:815`](../../crates/arany-cli/tests/session_run/tools.rs#L815)'s replay owner only where the new timing boundary needs proof. |
| Edit succeeds | Exact before/after digest and bytes, Guard attestation, checkpoint outcome, no second publication on replay; end marker follows finish acknowledgement. Existing guarded coding journey at [`session_run/tools.rs:204`](../../crates/arany-cli/tests/session_run/tools.rs#L204) owns effect semantics. |
| Command succeeds | Pinned executable exits zero, expected bounded output, empty validated changeset, Guard attestation and confirmed cleanup; closed replay launches nothing. Reuse `guard_runs` and the guarded coding journey. |
| Invalid, denied or conflicting proposal | No success sample; report the closed class and which receipt/finish is absent or present. No dispatch for denied authority; Edit conflict preserves original bytes. Existing permissions corpus owns behavior, new measurement assertions own classification. |
| Missing finish, append failure, cancellation or Uncertain | No completed-success percentile for the failed invocation. Preserve partial counts and existing unknown/interrupted state; never retry automatically. Reuse Store crash-prefix and Guard uncertainty owners ([`store/journal/crash_tests.rs:18`](../../crates/arany/src/store/journal/crash_tests.rs#L18), [`permissions.rs:3026`](../../crates/arany-cli/tests/guard_runs/permissions.rs#L3026)). |
| Unsupported host, capture overflow or mismatched replay | Explicit invalid/failed corpus with reason and attempted count; never zero-test, skip-as-pass or silently drop a slow/failed sample. |

## Chosen direction

After Ponytail: extend the existing test-support and corpus paths with `std::time::Instant`, fixed-size/bounded sample storage and sorting. No benchmark framework, new dependency, production instrumentation API, generic observer trait, exporter, CLI option or crate.

Add a narrow opt-in recorder behind the existing `test-support` boundary at complete Provider receipt and immediately after the successful Tool-result append. It records only scoped IDs, closed operation/outcome and instants; it performs no I/O or asynchronous send in the timed path. The primary caller owns the start and maps it to the admitted intent; the finish owner supplies the acknowledged sequence. Keep one pending sample, scoped to one Engine/Run, with explicit disposal on error/cancellation. Do not time a subscription callback or start at `ProviderCallRecorded`, both of which lose required work. Production builds without `test-support` contain no recorder. The default test-support path keeps it disabled.

The [Engine owns transitions and Journal owns acknowledgement](../../docs/architecture/system-overview.md#2-runtime-architecture); Tools retains dispatch-site selection and Guard retains effects. This adds test observation at existing boundaries, not interchangeable effect adapters. Test the recorder through a scripted Engine journey and closed Store; the meaningful ordering test withholds the final append acknowledgement and proves no completed sample exists until release, using an explicit gate rather than a sleep or elapsed-time assertion.

Linux is the initial measurement platform. Use the real release `arany` Guard executable from the same worktree and native enforcement; no fake Guard, debug fallback or outer-sandbox bypass. macOS keeps common code/correctness compilation and normal CI coverage, but gets a separately deferred named-host measurement row, with its native preparation/teardown and no inferred Linux threshold parity.

### Fixed workload and report

- One serial invocation performs 10 explicitly labeled warm-ups then 100 measured samples **per operation** (330 total after slice 2). Use `--test-threads=1`; no discarded outliers, replacement samples or automatic repeats. Owner comparison uses three separately identified invocations on the same quiet host, reporting each p95 rather than pooling them or keeping the fastest.
- Each sample has fresh private State/Workspace and a short, fixed-history Run on the production current-thread runtime. Setup/admission and any required Goal plan precede the timer. Use identical file bytes and history shape for each sample; close/reopen/verify before proceeding. Page-cache warming is explicit and performed outside the timer. Warm-up never means a persistent Guard: every effect pays its normal startup/teardown cost. This measures warm filesystem data with fresh per-effect isolation, not cold disk, long history or concurrent load.
- **Read:** copy the existing 53,248-byte/4,096-line fixture and default first 2,000-line page. Report the existing 40-sample dispatch corpus separately under its unchanged name and target; never merge dispatch samples into the new distribution.
- **Edit:** a fixed 4 KiB UTF-8 file with one unique equal-length replacement and a pinned before digest. Each sample starts from identical original bytes. Include normal checkpoint capture and verify the saved outcome; no checkpoint-disabled fast path.
- **Command:** explicit argv for an admitted, digest-pinned native `true` executable, no interpreter, network, cache or runtime grants, on a fixed small project snapshot. Resolve its executable through normal fixture admission and record identity/version; absence is an unavailable prerequisite, not a shell substitution. Prove exit zero, empty stdout/stderr and empty changeset. This is a Guard-overhead baseline, not `cargo test` or a large-repository snapshot benchmark.
- Use explicit private grants/settings that permit only the fixture path and command, with ordinary committed permission decisions. Record default process/resource profile and all effective Tool/Goal bounds. Set sufficient finite Goal headroom for one measured operation and the scripted continuation; fail unexpected review, compaction, fallback or extra Provider calls rather than silently including a different path.
- Buffer results and print after timing with `--nocapture`: corpus version, revision/build profile, host metadata, warm-up/measured/failed counts, each raw sample, p50/p95/max and proposed-target comparison. Use integer microseconds and nearest rank `ceil(p*n)-1` on sorted successful durations (p95 index 94 for n=100). An incomplete invocation prints diagnostics but no valid target verdict. Include separately labeled journal intervals and endpoint sequences, not payloads.

## Security and resource model

The [security pass](../../agents/security.md#security-pass-triggers) covers the added recorder and disposable filesystem/process fixtures, not expanded product authority. Assets are the host, private State, fixture files and sample artifacts. Provider proposals and paths remain untrusted data; authority comes only from the existing pinned grant and deterministic permission path. No secrets, live accounts, network calls, environment dumps or user repository data enter the workload. Export only compiled labels, numbers, synthetic IDs and reviewed host metadata. Guard descendants keep existing confinement; no automatic unguarded rerun is permitted.

| Resource | Corpus bound and overflow behavior |
|---|---|
| Concurrency and pending measurements | One Run/Tool and one pending timer; use the existing native-test serialization discipline. Unexpected overlap or unmatched finish invalidates the corpus. |
| Samples and scripted calls | At most 330 operation attempts, plus at most 40 dispatch-comparison reads; at most 4 scripted Provider calls per operation Run, zero retries. All queues and retained requests bounded by these counts. |
| Inputs and outputs | At most 8 regular Workspace files, 64 KiB total; fixed arguments under 1 KiB; keep the normal 64 KiB Tool result bound. Captured process diagnostics at most 64 KiB per child; corpus text/report at most 1 MiB. Overflow is failure, not truncation of statistical evidence. |
| Timing retention | At most 330 records and 1 MiB of timing/metadata storage. Flush only outside timed sections; no change to the 256-Event Journal tap or OTLP queues. |
| Fixture storage | One live fixture; at most 8 MiB journal/checkpoint artifacts per sample, 256 MiB retained evidence per invocation. Warm-up artifacts are removed after verified replay; measured artifacts remain private for audit within the aggregate bound. Stop before exceeding it. Existing Store headroom and Guard quotas still apply. |
| Runtime | Existing finite Tool/preparation/cleanup bounds remain authoritative. Parent supervises each sample for at most 120 seconds and the invocation for 30 minutes; cancellation, positive kill/reap and teardown use existing process owners. A deadline aborts the invocation and retains incomplete evidence; no p95 timeout assertion. |
| Builds and cleanup | Build only in this worktree's default `./target` or through `/home/fpmirabile/projects/harness-team/bin/cargo`. Never set `CARGO_TARGET_DIR` or place build products under `/tmp`. Runtime fixture storage is separate from build output; select and record a real local filesystem, not an accidental tmpfs performance result. Remove only test-owned artifacts after audit; keep bounded failure artifacts until process cleanup is proved. |

All five guarantees remain unchanged: ordinary committed Provider/intent/verdict/result order; unknown outcomes stop without retries; admitted reads and real Guard mutations/processes; finite durable Goal accounting and cancellation; repository/model text never selects trust. Instrumentation cannot authorize, skip, retry or change an outcome. A blocked security prerequisite is reported with its native owner, never worked around to produce a number.

## Risks and mitigations

- Wall-clock shifts, millisecond rounding and pre-commit stamps → monotonic primary metric, replayable journal diagnostics separately, explicit clock-invalid classification.
- CI contention and filesystem cache variation → ignored reporting corpus, named host/load/storage metadata, fixed warm workload and no elapsed-time pass/fail in hosted CI.
- Test recorder perturbation → bounded synchronous observations without allocation-heavy work or I/O; disclose that the optimized benchmark has test-support instrumentation, keep it absent from production and avoid subtracting guessed overhead.
- Short history/no-op command mistaken for daily workload coverage → label workload and exclusions in every report; keep large snapshot/build evidence with Daily use first items 6–7.
- Failure or slow-sample censoring → retain every attempted ordinal, fail an incomplete corpus and never substitute retries; correctness failures remain failures even if earlier samples are fast.
- Owner host unavailable → land validated measurement capability and leave evidence explicitly pending in `NEXT_STEPS.md`; no native latency claim from synthetic or compile-only results.

## Vertical slices

### 1. PR: report replay-validated Read latency through durable result acknowledgement

Ship an independently usable ignored Linux release corpus in the existing `guard_runs` integration target, initially containing the Read row. Cross the full path: scripted Tool response → Engine acceptance/permission → in-process Read → committed result → buffered report → closed Store/replay and fresh `arany show` audit. Add the narrow recorder at the two actual timing boundaries and deterministic evidence for receipt/finish correlation, withheld acknowledgement, failed/incomplete classification and percentile arithmetic in the existing relevant corpus. Keep the current dispatch test and its meaning unchanged.

No product spec behavior changes. Put the exact accepted endpoint/workload procedure with the corpus, link the existing Tools/Execution evidence owners, and add an explicitly unverified Linux named-host measurement entry under item 11 in `NEXT_STEPS.md`. Do not duplicate or close Pursue item 2.

Verification: documentation links; `cargo fmt --all --check`; workspace Clippy and production-without-test-support checks from the [development guide](../../docs/development.md#verification); targeted recorder/Store ordering and Read replay owners. The deterministic tests assert order/counts/bytes, never milliseconds. Build/list the ignored release corpus and prove its exact nonzero selection. Run it only where native prerequisites are available; report refusal otherwise. Code-changing PR requires complete final-head CI, with timing still ignored.

### 2. PR: complete the same report with guarded Edit and command rows

Extend the same corpus, not a second benchmark system: scripted Edit/command proposal → committed intent/verdict → real native Guard → publication or empty command capture and cleanup → finish acknowledgement → report and closed replay. Add the fixed guarded rows, fresh-fixture resets, classified failures and aggregate bounds above. Reuse existing permissions, conflict, cancellation, uncertainty and crash-prefix owners; add assertions only for the new measurement endpoint/classification. No product permission, persistence or effect-contract change.

Verification: repeat formatting/Clippy for touched code, targeted Read/guarded coding and permissions/uncertainty owners, and recorder checks showing no success endpoint on failed acknowledgement. Run the release corpus explicitly on the owner-selected Linux host, or leave its named-host row pending. All three rows must have exactly 100 measured successes for a valid invocation; failures cannot become performance passes. Complete final-head CI verifies correctness across Linux/macOS; the latency test stays explicitly ignored even under `arany_ci` and adds no CI configuration or wall-time gate.

Update item 11's single owner-run evidence row with the actual invocation procedure, accepted targets and remaining evidence. Recommended proposed test name/command, to be made exact by this slice:

```sh
cargo test -p arany-cli --test guard_runs --release --locked --offline linux_tool_latency_corpus -- --exact --ignored --nocapture --test-threads=1
```

Reject debug execution with a clear release-required diagnostic. Preserve the existing comparison invocation:

```sh
cargo test -p arany --lib --release --locked --offline tools::tests::in_process_repeated_reads_all_succeed_on_a_large_file -- --exact --nocapture --test-threads=1
```

The owner records: date, exact revision, binary/toolchain/profile, host label, CPU/model/core count, RAM, kernel/architecture, filesystem/device and State/Workspace location, power mode and observed load, native Guard prerequisites/backend, pinned command identity, effective grant/resource/checkpoint settings, sample counts, all three invocation summaries, raw-report location/digest, replay audit and target misses. Add a separate **macOS named-host latency: unverified** row with native owner/procedure and no Linux-derived pass. Completed evidence belongs in PR/run artifacts; `NEXT_STEPS.md` keeps only remaining claims and links.

## RPI gates

- **Research → Plan:** satisfied for current code/contract/peer facts above, including the lack of exact durable elapsed time in existing Events. Recheck endpoint call sites at implementation head. Proposed targets and host selection are recommendations, not researched measurements.
- **Plan → Implement:** passed 2026-10-22; the owner decisions below are recorded. Implementation follows the slices above.
- **Per-slice implementation gate:** exact deterministic success/failure observations, relevant checks and complete final-head CI; stop and revise the plan if the real flow contradicts its timing boundary. Native performance evidence is a separate owner-run gate and never licenses weakened guarantees.

## Decisions (2026-10-22, owner)

- Accepted the monotonic receipt-to-finish-acknowledgement metric, audited against closed journals, with replayable wall-time intervals reported separately. Retain local sample reports without changing the stored Event format.
- Accepted provisional named-host p95 objectives of Read 100 ms, Edit 250 ms and no-op command 500 ms as a starting point, with Read dispatch <= 50 ms reported separately. Record and investigate baseline misses; no CI wall-time gate or automatic threshold adjustment.
- Selected the owner's ordinary native Linux development machine, `asustuf` (kernel `7.2.10-zen1-1-zen`), idle on stable power and local disk, for three separately reported invocations. `/home` uses `/dev/nvme1n1p1`, ext4, on `nvme1n1` (`WD_BLACK SN770 2TB`, `ROTA=0`, `TRAN=nvme`). Accepted the fixed warm/no-op workload for item 11's first row; cold disk, long history, large snapshots and macOS remain explicitly unverified.

## Exit criteria

- Both vertical slices provide one usable reporting capability; default CI has deterministic correctness checks and no hardware-sensitive latency assertion. Specs, Guard, SQLite durability and Goal limits retain their existing behavior.
- A valid report has complete endpoint joins, exact counts and results, native attestations and closed-journal replay without re-execution; ordered raw samples reproduce its p50/p95/max. Rejected, failed, cancelled, uncertain, missing and overflow cases cannot enter a successful distribution.
- The owner-run Linux evidence row remains open until the named host, fixed workload, three reports and replay audit are recorded. Target misses stay visible with diagnosis ownership; capability delivery alone does not close item 11's latency claim. Dispatch evidence closes Pursue item 2 only through its own measurement owner.
- Green executed macOS CI, flake closure and future macOS latency remain separately tracked. No live Provider was needed or measured.
- Once implementation is complete, move the accepted procedure to the corpus/evidence owner, leave unresolved native evidence in `NEXT_STEPS.md`, and remove this plan and index row per the [planning lifecycle](../README.md#lifecycle), including when implementation is still uncommitted locally.
