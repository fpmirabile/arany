# Minimum-Arany next-step decision register

**Status:** research closure for the first executable slice  
**Date:** 2026-09-29  
**Canonical physical design:** [minimum harness architecture](../architecture/system-overview.md)

This register closes the implementation decisions that remained after the broader architecture research. It does not create more modules. The architecture document owns the physical shape; the subject reports linked here own the evidence and detailed failure analysis.

Every item has one of three statuses:

- **V1 decision:** implement this in the minimum demonstrator.
- **Implementation gate:** code or runtime evidence must pass before the associated guarantee is claimed.
- **Triggered scope:** do not build it until the named product or measurement trigger occurs.

## Executive result

There is no missing research choice blocking an implementation plan for the minimum Rust CLI harness or its optional standard observability edge. Implementation itself starts only after that plan imports the V1 security claim, non-claims, P0 controls, and verification gates in [Harness security lessons and controls](./harness-security-lessons-and-controls.md).

The selected slice is one Cargo package and one process. A synchronously owned current-thread Tokio coordinator drives one root and exactly two concurrent child AgentRuns through four Provider calls. A dedicated standard-library thread owns one bundled SQLite connection. The CLI snapshots bounded Workspace files through capability-rooted no-follow handles, persists semantic transitions before rendering them, prints append-only progress, and calls OpenAI through one bounded non-streaming HTTPS adapter. After that proof passes, one private opt-in module may project content-free traces over OTLP/HTTP protobuf to a numeric-loopback Collector.

This is intentionally not the final platform. It is the smallest slice that can honestly prove delegation, concurrency, join behavior, cancellation, durable replay, safe inputs, a replaceable Provider, and a narrow non-effectful security boundary. Rust, `cap-std`, and SQLite are not security claims by themselves.

## 1. Why SQLite wins for agent state

SQLite is selected because the product needs a durable acknowledged event prefix, not because the schema needs sophisticated SQL.

| Option | Why it is not V1 canonical state | Proper role or trigger |
|---|---|---|
| In-memory `Vec<Event>` | Disappears on exit and cannot satisfy `arany show` | First reducer/loop test only |
| JSONL append file | Atomic multi-Event transitions, torn-tail recovery, checksums, locking, sync, directory durability, indexing, and migration would become Arany code | Deterministic output/export and test fixtures |
| Directory per Run | Atomicity does not span several files; derived summaries can disagree with the event log | Large immutable Artifacts later |
| SQLite rollback journal | Already supplies transactions, recovery, constraints, inspection, migration, and reopening without a service | **Selected** |
| SQLite WAL | Adds persistent sidecars, checkpoint policy, and backup rules while current V1 has no concurrent reader | Benchmark `WAL + FULL` on a SQLite release containing the WAL-reset fix after a live reader or durable-commit breach |
| redb | Strong pure-Rust ACID option, but indexes, value migrations, queries, inspection, and export become Arany responsibilities | Spike only when C/FFI is forbidden |
| Fjall or RocksDB | LSM compaction and throughput machinery solve no measured V1 workload; RocksDB also adds C++ and a large build surface | Benchmark only after sustained SQLite write-pressure evidence |
| sled | Pre-1.0 format and reliability posture are inappropriate for canonical history | Not selected |
| PostgreSQL | Adds a server, authentication, lifecycle, upgrades, and backup operations to a local CLI | Multiple authoritative hosts/writers, tenancy, replication, or PITR |

The exact storage decision is:

- bundled `rusqlite`/SQLite on a local filesystem;
- one connection on one named standard-library thread;
- private outside-Workspace state root with verified owner/permissions or ACL;
- read-write/create open with `SQLITE_OPEN_NOFOLLOW` and defensive mode;
- `journal_mode=DELETE`;
- `synchronous=EXTRA`;
- `trusted_schema=OFF`;
- 250 ms busy timeout;
- short `BEGIN IMMEDIATE` transactions;
- commit before reduction, `RunUpdate`, terminal progress, or JSONL;
- one `STRICT` Events table and `(run_id, sequence)` index;
- plain `INTEGER PRIMARY KEY`, not `AUTOINCREMENT`;
- `PRAGMA user_version=1` plus per-kind `event_version=1`;
- 64 KiB maximum serialized Event payload; and
- 4 KiB pages, a 65,536-page/256 MiB cap, and 4 MiB new-Run headroom; and
- incomplete committed histories display as `Interrupted`; V1 never resumes them automatically.

The complete comparison, crash model, benchmarks, and backend triggers are in [agent-state-persistence.md](./agent-state-persistence.md).

## 2. Settled V1 decisions

### D-01 — Runtime ownership

**V1 decision:** use Tokio's current-thread runtime for network, timers, signals, and task coordination. Synchronous `main` constructs the runtime, calls `runtime.block_on(...)`, completes canonical Engine and store cleanup, then performs bounded telemetry shutdown after the runtime returns. Put synchronous SQLite on one owned `std::thread` behind a bounded request channel and one-shot replies. Do not use Tokio's multi-thread scheduler, `spawn_blocking`, SQLx, or a connection pool.

**Reason:** only two remote calls run concurrently; database flush latency must not block the coordinator; one owner makes ordering and shutdown explicit.

**Gate:** stress the fixed channel capacities, prove no task or store thread survives shutdown, and enable a multi-thread runtime only after profiling coordinator starvation.

### D-02 — Fixed agent protocol

**V1 decision:** the success path is exactly:

1. `RootPlan -> Delegate([child_a, child_b])`;
2. two concurrent `ChildWork -> Finish` calls;
3. join both successful children; and
4. `RootSynthesis -> Finish`.

This is exactly four Provider invocations. Children cannot delegate, root cannot synthesize early, either child failure prevents synthesis, there is no retry, and only root owns the final answer.

**Gate:** the strict scripted Provider proves overlapping child calls, reverse completion order, join-all, invalid phase/outcome rejection, and cancellation of pending calls.

### D-03 — Provider seam

**V1 decision:** `Provider` is the only trait. Use a statically dispatched generic with two implementations: the strict scripted fake and OpenAI. The semantic request contains phase, objective, instruction snapshot, documents, child results, and bounds; provider wire types never cross inward.

The OpenAI adapter uses direct `reqwest` HTTPS to the fixed Responses endpoint with Rustls, a reused client, redirects off, inherited proxies off, retries off, cookies off, unsafe TLS options absent, `store:false`, truncation disabled, explicit model, strict Structured Outputs, and `max_output_tokens=4096`.

**V1 deliberately does not stream model tokens.** It reads the non-streaming body incrementally through `Response::chunk()` and rejects byte 1 MiB + 1. Streaming is triggered only when a real Client consumes partial output or measured time-to-first-useful-output becomes a product requirement.

### D-04 — Event contract and replay

**V1 decision:** persist only five explicit Event kinds:

- `RunStarted`
- `AgentSpawned`
- `AgentUpdated`
- `AgentFinished`
- `RunFinished`

The envelope contains global `sequence`, typed Run/Agent IDs, `event_version`, Unix-millisecond display time, explicit `kind`, and typed JSON payload. Sequence—not timestamp—defines order. Unknown kind/version, a sequence gap, malformed payload, or illegal reducer transition fails closed; replay never skips evidence.

`Run` and `AgentRun` remain reduced state. There are no canonical Run, AgentRun, Message, Session, snapshot, projection, outbox, Memory, or Artifact tables.

### D-05 — Identity and time

**V1 decision:** use typed UUIDv7 newtypes for `RunId` and `AgentRunId`; use the SQLite integer sequence for event order. Use `SystemTime` only for persisted Unix milliseconds and `Instant` for elapsed time/deadlines. Do not add Clock, ID-generator, or calendar-time abstractions.

### D-06 — Workspace and instruction inputs

**V1 decision:** open the user-selected Workspace once as a `cap_std::fs::Dir`. Resolve exact root `AGENTS.md`, using exact root `CLAUDE.md` only on actual absence. Resolve every `--include` by walking one component at a time with no-follow handles. Reject absolute paths, `.`, `..`, empty components, every symlink/junction/reparse component, and non-regular files.

Read each opened handle once into a bounded buffer, validate UTF-8, calculate SHA-256 over those exact bytes, and give the Provider the immutable snapshot. Persist only relative manifests/digests, not included file bodies. The contract prevents pathname escape and check-then-reopen races; it does not pretend several files form an atomic repository snapshot.

### D-07 — Fixed resource bounds

**V1 decision:** limits are constants, not configuration.

| Resource | Limit |
|---|---:|
| Objective | 8 KiB |
| Includes | 16 files |
| One include | 128 KiB |
| All includes | 256 KiB |
| Instruction file | 64 KiB |
| Compiled provider input | 384 KiB |
| Child objective | 2 KiB |
| Agent summary | 2 KiB |
| Worker result | 16 KiB |
| Root final result | 32 KiB |
| Provider response body | 1 MiB |
| Event payload | 64 KiB |
| Provider calls | 4 |
| Output tokens per call | 4,096 |
| Provider-call deadline | 120 seconds |
| Whole-Run deadline | 300 seconds |
| Cancellation drain | 2 seconds |
| Automatic retries | 0 |
| SQLite busy wait | 250 ms |
| SQLite database | 4 KiB pages × 65,536 = 256 MiB |
| Headroom to admit a Run | 4 MiB |
| RunUpdate channel | 32 items |
| Store request channel | 64 items |

Every overflow is typed and visible; canonical content is never silently truncated.

### D-08 — CLI and output

**V1 decision:** expose only:

```text
arany [--state-dir DIR] run [--workspace DIR] [--include PATH]... [--model ID] [--otlp-endpoint URL] [--jsonl] OBJECTIVE
arany [--state-dir DIR] show [--jsonl] RUN_ID
```

Human `run` writes committed append-only progress to stderr and the final root result to stdout. Human `show` writes deterministic reconstructed state to stdout. JSONL writes one compact persisted Event per stdout line and typed diagnostics to stderr.

Reuse the mature terminal contracts compared in [CLI user-feedback patterns from agent harnesses](./cli-user-feedback-patterns-from-agent-harnesses.md):

- every AgentRun has a trusted stable display label plus typed ID; untrusted objectives and summaries never supply the prefix;
- root wait updates identify the exact outstanding worker IDs and count;
- the terminal Run Event separates outcome, usage, and recovery, reporting calls used/limit, provider-reported tokens when present, elapsed time, terminal reason, Run ID, last committed sequence, and the exact `arany show` replay command;
- provider-reported, locally calculated, estimated, and unavailable usage are labelled distinctly; V1 never invents currency cost;
- every worker receives one persisted terminal success, failure, or cancellation status before the Run terminates, even when it produced no result; and
- errors identify the phase, stable symbolic code, affected Run/AgentRun when known, last durable sequence, and one safe next action without exposing raw provider or secret data.

There is no TTY branch, color, cursor movement, redraw, width calculation, resize handling, raw mode, spinner, alternate screen, recursive/glob input, prompt-from-stdin, verbose logging, or general config file.

Exit codes are `0` success/help, `1` operational failure, `2` invalid invocation or pre-Run input/configuration, and `130` user cancellation.

### D-09 — Configuration, state, and credentials

**V1 decision:** use named precedence only:

- state directory: `--state-dir` > non-empty `ARANY_STATE_DIR` > platform `ProjectDirs` location;
- model: `--model` > non-empty `ARANY_OPENAI_MODEL` > error; and
- OTLP traces endpoint: `--otlp-endpoint` > `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` > `OTEL_EXPORTER_OTLP_ENDPOINT` > disabled; and
- credential: non-empty `OPENAI_API_KEY` > error.

There is no compiled model default, `.env` loading, API-key flag, login flow, keychain, arbitrary base URL, or ambient enterprise proxy. The API key is held in `secrecy::SecretString`, exposed only to build the sensitive Authorization header, and never persisted or formatted. This reduces accidental application disclosure; it does not claim to erase environment or TLS copies.

Resolve the state root before reading repository input. Whether defaulted or explicit, it must be outside the Workspace, local, no-follow, owned by the current OS user, and private: directory `0700` and database/sidecars `0600` on Unix, or an equivalently verified effective ACL on Windows. An insecure or unverifiable root fails before `RunStarted`; the program never repairs broad permissions silently.

### D-10 — Cancellation and terminal truth

**V1 decision:** first Ctrl-C cancels the Run token and every child. The supervisor—not child tasks—owns terminal Events. A second Ctrl-C or the two-second deadline aborts and reaps the `JoinSet`, after which the supervisor best-effort persists stable cancelled Agent/Run terminals. Do not call `process::exit`.

If terminal persistence succeeds, return 130. If the store fails, return 1 rather than claiming durable cancellation. Whole-Run deadline produces `TimedOut` and exit 1. A forced process death leaves a valid prefix that offline replay labels `Interrupted` without appending a fictional Event.

### D-11 — Error and retry boundary

**V1 decision:** typed Engine errors cover input, storage, update-sink loss, and invariant failure. Provider failures after `RunStarted` become bounded safe terminal Event data when the store remains writable. Diagnostics allowlist codes, IDs, counts, status, request ID, and safe timing; they never include credentials, headers, prompts, included content, response bodies, or raw database payloads.

There are zero automatic retries, including reqwest protocol retries. A post-send transport failure is potentially billable/ambiguous and is not replayed. Retry becomes a separate Engine policy only after measured transient failure, explicit attempt Events, one total deadline, and provider idempotency/resume evidence where required.

### D-12 — Exact dependency boundary

**V1 decision:** the direct dependency set is the one in [rust-foundation-and-engine-contract.md](./rust-foundation-and-engine-contract.md): `cap-std`, `cap-fs-ext`, `clap`, `directories`, `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `reqwest`, `rusqlite`, `secrecy`, `serde`, `serde_json`, `sha2`, `thiserror`, `tokio`, `tokio-util`, `uuid`, target-specific `windows-sys`, and dev-only `tempfile`, the OpenTelemetry SDK `testing` feature, and Tokio `test-util` without macros.

Notably absent as direct architectural choices are SQLx, a pool, `async-trait`, an OpenAI community SDK, SSE libraries, a TUI, a configuration framework, `tracing`, a schema generator, a migration framework, and a web framework. An OpenTelemetry dependency may pull `futures-util` transitively; dependency inspection must distinguish unavoidable upstream internals from direct application APIs. `Cargo.lock` must pin transitive resolution and dependency updates repeat feature/advisory/license review.

### D-13 — Optional OTLP trace export

**V1 decision:** after the core team-run/replay/cancellation proof passes, add one private `telemetry.rs` module. It exports traces only over OTLP/HTTP binary protobuf to an explicit numeric-loopback `http` Collector endpoint. It defines no trait, exposes no OpenTelemetry type to Engine callers, creates no global tracer or `tracing` subscriber, and cannot affect persisted Events, `RunView`, cancellation, or process exit status after a Run starts.

SQLite Events remain canonical. The module accepts a compile-time typed allowlist of identifiers, phases, outcomes, event kinds/sequences, timing, and bounded model/token metadata. It cannot accept objectives, prompts, instructions, summaries, results, paths, file metadata or contents, Event payloads, provider bodies, headers, credentials, arbitrary attributes, or formatted errors.

Initial operational bounds are fixed:

| Resource | Limit |
|---|---:|
| Successful fixed workflow | 9 spans |
| Ended-span queue | 256; drop new spans when full |
| Export batch | 64 spans |
| Batch schedule | 250 ms |
| Attributes / span | 32 |
| Events / span | 32 |
| Attribute string | 256 UTF-8 bytes |
| Encoded request | 256 KiB |
| One export attempt | 500 ms |
| Retries | 1; two attempts total |
| Normal telemetry shutdown | 750 ms |
| Queued telemetry memory target | 4 MiB |

Redirects and ambient proxies are disabled. The local Collector owns remote TLS, authentication, secrets, routing, and backend retry. Invalid explicit telemetry configuration fails before `RunStarted` with exit 2; a valid but unavailable Collector produces a safe diagnostic and is non-fatal. The evidence contract is in [OTLP observability](./otlp-observability.md).

### D-14 — V1 security boundary

**V1 decision:** establish every authority source before consuming project-controlled input. Parse only CLI syntax and named trusted process configuration, resolve and pin the private state root and caller-selected Workspace, and then read exact instruction/include snapshots. V1 never reads or executes repository/Git configuration, `.env`, hooks, filters, `fsmonitor`, plugins, package-manager metadata, test discovery, startup commands, or a trust cache. Repository text, provider output, child messages, and replayed Events are untrusted data; none can select or widen a root, endpoint, model, credential, budget, topology, policy, instruction role, or telemetry destination.

The initial state and output rules are:

- open SQLite read-write/create with `SQLITE_OPEN_NOFOLLOW`, defensive mode, `trusted_schema=OFF`, and the selected rollback configuration on one dedicated thread;
- refuse non-local storage, links/reparse points, wrong type, unexpected owner, or broad permissions/ACL;
- initialize 4 KiB pages with a 65,536-page limit (256 MiB), require at least 4 MiB headroom before a new Run, and never silently prune canonical Events;
- validate schema/version/identity/sequence/legal transitions/counts/sizes during replay and never feed persisted text into instructions, policy, configuration, or a later Provider request;
- reserve one aggregate four-call and maximum-output-token budget before `RunStarted`; children draw from the same non-widening Run budget; and
- escape or reject terminal ESC/CSI/OSC/DCS, C0/C1 controls, carriage return, backspace, bidi controls, and forged status prefixes; emit JSONL only through a serializer as one object per line.

Provider egress remains the only intentional network authority: one compiled origin/path, no redirects/proxies/cookies/ambient credentials/retries, bounded compressed and decompressed bodies, and an explicit request assembler that accepts only phase-required data. Endpoint authorization never authorizes arbitrary upload. Canary tests prove omitted Workspace content, secrets, state, and telemetry-forbidden data do not leave.

The application crate uses `#![forbid(unsafe_code)]`, commits `Cargo.lock`, pins the toolchain, minimizes features, and inventories dependency sources, build scripts, procedural macros, native code, FFI, unsafe code, licenses, advisories, and development-Skill provenance. Bundled SQLite is an explicit native-code exception with runtime option checks. V1 has no listener, runtime plugin, self-update, temp response file, shell, subprocess, arbitrary HTTP, callback URL, or effectful Tool.

**Implementation gates:** pass the incident-derived startup, path-race, state permission/ACL, corrupt replay, disk-full, hostile provider-network, secret/egress canary, terminal-control, aggregate-budget, cancellation/backpressure, supply-chain, and platform-capability suites in the [security report](./harness-security-lessons-and-controls.md). Publish the supported-platform matrix and explicit non-claims with the result. No sandbox or arbitrary-code containment claim is permitted.

### D-15 — Evidence-dense V1 testing

**V1 decision:** optimize for evidence per test rather than test count or coverage percentage. Every acceptance scenario produces one `ObservationBundle`: exit class, exact stdout bytes, exact stderr bytes, Events reopened from a closed SQLite store, and the RunView reduced from those Events. The central deterministic journey uses the real CLI adapter, Engine, scheduler, file-backed SQLite, reducer, and renderers with only the semantic Provider scripted. A product-process table separately owns arguments, channel separation, exit codes, signal wiring, and `show`; one ignored paid OpenAI smoke owns the complete live path.

Add a test only for a user-visible contract, durable/security invariant, protocol boundary, concurrency/failure mode, or demonstrated regression that no existing owner catches with equally useful diagnostics. Large verification matrices become compact scenarios, data tables, hostile corpora, or failpoint loops; exact boundary rows do not become individual test functions. Refactors normally add no tests, and superseded tests/fixtures are deleted when a stronger owner covers the same defect.

Default `cargo test` is offline, deterministic, retry-free, free of wall sleeps and hardware-sensitive timing assertions, and uses real temporary filesystem objects plus file-backed SQLite. A few manual byte goldens own only public human/JSONL output and require line-by-line review. Property, fuzz, model-checking, snapshot, process-test, or concurrency frameworks activate only when a named state space or repeated mechanism earns them. Stochastic model quality remains an evaluation; performance remains a named-host measurement.

**Implementation gates:** pass the V1 verification register, platform behavior matrix, exact-output review, flake rules, and test-deletion review in [Testing strategy for the Rust CLI harness](./testing-strategy-for-rust-cli-harness.md). The ignored live test must run explicitly before claiming OpenAI compatibility, but it never enters the default deterministic suite.

### D-16 — Product identity, beta platforms, and distribution intent

**V1 decision:** the product is **Arany** and the executable and Cargo package are `arany`. The checkout directory is not part of the public contract. Environment variables owned by the product use the `ARANY_` prefix.

The beta supports Linux and macOS only after the same native startup, path, persistence, signal, cancellation, terminal, and packaging suites pass on each platform. Windows is intentionally deferred until its reparse-point, ACL, console-control, path-namespace, packaging, and future containment gates pass; this is a release boundary, not an architectural fork.

Arany will be available without a purchase requirement and will preserve author attribution. The exact open-source license is a pre-public-release product decision because permissive attribution can be expressed by more than one legal instrument; repository prose must not imply MIT, Apache-2.0, dual licensing, or another license until that choice is recorded and a license file exists.

## 3. Every previously listed next step

| Previous next step | Status | Closed by |
|---|---|---|
| Choose Rust dependencies and runtime | V1 decision | [Rust foundation](./rust-foundation-and-engine-contract.md) D-01/D-12 |
| Define `Delegate`, `Finish`, Event, and RunView | V1 decision | [Rust foundation](./rust-foundation-and-engine-contract.md) D-02–D-04 |
| Decide all fixed limits | V1 decision | [CLI and operability](./cli-inputs-configuration-and-operability.md) D-07 |
| Decide IDs, time, versions, and redaction | V1 decision | Rust foundation, [persistence](./agent-state-persistence.md), and CLI report |
| Define terminal behavior | V1 decision | CLI report D-08 |
| Define OpenAI model/config/credential behavior | V1 decision | CLI report and Rust foundation D-03/D-09 |
| Define deterministic fake and live fixture | V1 decision plus implementation gate | CLI report sections 11–13 |
| Compare SQLite with other persistence options | V1 decision plus backend triggers | Persistence report |
| Prove crash durability and replay | Implementation gate | Persistence report sections 12–13 |
| Prove local overhead is negligible | Implementation gate | Persistence and CLI performance gates |
| Prove no-follow input handling on supported beta platforms | Implementation gate | Native Linux and macOS CLI/Rust resolver suites; Windows is deferred to its support gate |
| Prove provider compatibility | Implementation gate | Strict fake plus ignored live OpenAI smoke |
| Define standard observability export | V1 decision plus implementation gates | [OTLP observability](./otlp-observability.md) D-01/D-13 |
| Learn from existing harness security failures and fix the V1 boundary | V1 decision plus release gates | [Harness security lessons](./harness-security-lessons-and-controls.md) D-14 |
| Keep testing small while proving real user-visible behavior | V1 decision plus release gates | [Testing strategy](./testing-strategy-for-rust-cli-harness.md) D-15 |
| Name the product and bound beta support | V1 decision plus native release gates | D-16 |

The implementation gates require executable evidence. More general architecture research cannot make them pass.

## 4. Triggered modules already researched

These items are not unfinished V1 work. Their architecture is researched and their activation condition is explicit.

| Trigger | Introduce | Existing evidence |
|---|---|---|
| First effectful Tool | One immutable `EffectIntent`, deterministic restrict-only Policy, digest-bound `ApprovalProof`, separate Guard process, effective-capability attestation, and adversary corpus | [Harness security lessons](./harness-security-lessons-and-controls.md), [deterministic protection](./deterministic-harness-protection.md), [instruction policy](./instruction-markdown-and-policy-enforcement.md) |
| Agents may write concurrently | Isolated Workspace views, deterministic changesets, one integration owner | [Multi-agent loop](./multi-agent-loop-and-user-feedback.md) |
| Dependencies, optional work, ownership transfer, or custom joins | Assignment DAG, attempts, leases, fencing, join policy | Multi-agent loop report |
| Follow-up objectives need a durable conversation | Session | [Context and Memory](./context-memory-and-compaction.md) |
| Knowledge must improve a later Run | Scoped Memory, retrieval evaluation, review/deletion lifecycle | Context and Memory report |
| Event payloads exceed 64 KiB | Content-addressed Artifacts with retention and atomic publication | Context/Memory and [provider/tool runtime](./provider-and-tool-runtime.md) |
| Replay breaches its measured budget | Rebuildable snapshots | Persistence report |
| Lexical retrieval is required | FTS5 derived index | Context and Memory report |
| Labelled retrieval evaluation proves lexical quality insufficient | Vector/hybrid derived index | Context and Memory report |
| Second Client or detached execution | Per-user daemon and versioned local process protocol | [Modular architecture](./modular-harness-architecture.md) |
| OpenAI blocks a required behavior | Second hosted Provider adapter | Provider/tool runtime report |
| A Client needs incremental output | Bounded provider streaming and partial-output contract | Rust foundation |
| Repeated trials need statistical release evidence | Evaluation runner, datasets, graders, baselines | [Evaluation strategy](./evaluation-and-quality-strategy.md) |
| Browser, remote, or multi-tenant product exists | HTTP/SSE, identity, authorization, encryption, quota, retention, new threat model | Modular architecture and protection reports |
| Measured terminal usability requires a richer renderer | TTY renderer over the same RunView, then color/width/resize policy | CLI report |
| Direct remote telemetry is required | Explicit TLS, authentication, secret, proxy, and trust policy; preserve the Collector option | [OTLP observability](./otlp-observability.md) |
| A daemon or aggregate SLO requires process-wide telemetry | Evaluate bounded OTLP metrics without making them canonical state | OTLP report |
| Safe traces and canonical Events cannot diagnose a concrete field failure | Design allowlisted structured logs; never mirror Event payloads by default | OTLP report |

## 5. Implementation handoff

Research is complete when every design choice is settled or triggered; product guarantees still require code. The next execution sequence is:

1. Write one implementation plan from the repository planning template, referencing this register and the canonical architecture. Import D-14's security claim, non-claims, P0 gates, and supported-platform evidence plus D-15's evidence owners, test-admission rule, and execution lanes explicitly.
2. Create the one Cargo package with `#![forbid(unsafe_code)]`, the exact dependency/features boundary, pinned toolchain, committed lockfile, and reviewed build/proc-macro/native/Skill inventory.
3. Implement and adversarially test startup ordering, trusted configuration, private state-root admission, Workspace handle pinning/snapshots, and inert human/JSONL output before any live Provider call.
4. Add the dedicated SQLite thread, hardened open/schema, aggregate page admission, transition transactions, strict data-only replay, crash/disk/corruption tests, and forced-death recovery.
5. Make the fixed four-call scripted `team_run` pass under one aggregate Run budget, then add graceful/forced cancellation and prove task/thread cleanup and backpressure behavior.
6. Add the fixed bounded OpenAI adapter, hostile-network and secret/egress-canary tests, then run the ignored live structural smoke.
7. Run every deterministic, fault, security, supply-chain, packaging, and performance gate natively on Linux and macOS before claiming the beta core demonstrator complete; publish its exact security claims and non-claims. Windows remains unsupported until its deferred native gate passes.
8. In a separate patch, add private opt-in OTLP trace export and pass its topology, privacy-canary, loopback receiver, failure, cancellation, saturation, and performance gates.

No daemon, protocol, Guard, effect runtime, Memory, Artifact store, Assignment DAG, snapshot, search index, second Provider, evaluation service, TUI, web API, direct remote telemetry, OTLP logs, or OTLP metrics precedes that vertical proof.
