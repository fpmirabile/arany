# Arany beta decision register

**Status:** research closure for the first implementation
**Date:** 2026-09-29  
**Canonical design:** [Arany beta architecture](../architecture/system-overview.md)

Every item has one of three statuses:

- **Beta decision:** implement it.
- **Implementation gate:** prove it with code or runtime evidence before claiming it.
- **Triggered scope:** do not build it until the named condition occurs.

There is no missing research choice blocking the implementation plan. The repository now includes the exact Apache-2.0 `LICENSE` and a project `NOTICE` attributed to `fpmirabile`.

## 1. Why SQLite remains canonical state

Arany now needs durable Sessions as well as Runs, but its storage requirement is unchanged: one local authoritative writer, transactional semantic transitions, crash recovery, indexed replay, and no service.

| Option | Why it is not beta canonical state | Triggered role |
|---|---|---|
| Memory only | Cannot resume after process exit | Reducer tests only |
| JSONL files | Arany would own torn tails, multi-Event atomicity, locking, sync, indexes, and migration | Deterministic export |
| Directory per Session | Cross-file atomicity and derived-summary consistency become application code | Large Artifacts later |
| SQLite rollback journal | Supplies transactions, recovery, constraints, migrations, and inspection locally | **Selected** |
| SQLite WAL | Adds sidecars and checkpoint policy without a live second reader | Benchmark after measured reader/commit pressure |
| redb/Fjall/RocksDB | Indexes, queries, migrations, inspection, or compaction become Arany work | Revisit only after a measured SQLite breach or C prohibition |
| PostgreSQL | Adds a server and operator lifecycle | Multi-host writers, tenancy, replication, or PITR |

Use bundled `rusqlite`/SQLite on a verified local filesystem, one connection on one named thread, a private state root outside the Workspace, no-follow open, defensive mode, `DELETE + EXTRA`, `trusted_schema=OFF`, a 250 ms busy timeout, short `BEGIN IMMEDIATE` transactions, bounded payloads/pages, and commit-before-feedback. Incomplete committed histories replay as `Interrupted`; they are never auto-resumed as work.

## 2. Settled beta decisions

### D-01 — Runtime and physical ownership

**Beta decision:** one package and process. A current-thread Tokio runtime owns network, timers, signals, AgentRuns, and coordination. One standard-library thread owns SQLite; one bounded worker owns OTLP export. Do not use the multithreaded Tokio scheduler, `spawn_blocking`, SQLx, or a pool without profile evidence.

The physical files are `main.rs`, deep `lib.rs`, `session.rs`, `provider.rs`, `store.rs`, `presentation.rs`, `terminal.rs`, and last-slice `telemetry.rs`. A daemon, process protocol, or crate split requires a measured client/release/privilege/dependency boundary.

### D-02 — Durable Session model

**Beta decision:** `Session -> Run -> AgentRun` is canonical vocabulary.

- Session: durable ordered Messages, Runs, title, defaults, fork lineage, and derived compaction snapshots.
- Run: one accepted user submission to one terminal outcome.
- AgentRun: one primary or child execution inside a Run.

Resume preserves Session identity. Fork creates a new Session from an exact committed Run/sequence/prefix digest. Historical children remain inspectable but never become live again. Retrying interrupted work creates a new Run. Provider/model/profile defaults may change only between Runs; `RunStarted` persists resolved values.

Compaction creates digest-bound derived context and never deletes or replaces canonical history. Manual compaction is idle-only and visible; automatic compaction runs only at a committed boundary after warning. Provider state is an optimization, never the sole resume path.

### D-03 — Generic bounded agent protocol

**Beta decision:** every Run pins one collaboration policy:

- `single`: no children;
- `auto(N)`: primary may finish directly or delegate; and
- `team(N)`: primary must propose a non-empty bounded decomposition.

The beta topology is one primary plus an ordered `0..N` collection of direct read-only children. Children cannot create nested teams. The default is `auto` with three active children maximum; the beta process hard ceiling is eight active children. `--max-agents` counts active children and excludes the primary.

Effective capacity is the minimum of the selected maximum, hard ceiling, aggregate call/token/byte/time budgets, and Provider concurrency. A direct answer is one Provider call. With `k` children, a successful team Run is `k + 2` calls: primary plan, `k` child calls, primary synthesis. All required children must finish successfully before synthesis. There are no detached children or automatic retries.

**Gate:** one scripted scenario proves direct single completion, generic ordered children, bounded concurrency, reverse completion, join-all, overflow rejection, cancellation of all children, and invalid phase/outcome rejection. Boundary tables cover `N = 0, 1, 3, 8, 9`.

### D-04 — Provider seam and profiles

**Beta decision:** `Provider` is the only Engine trait. Engine types carry Arany semantics, never vendor wire objects or generic parameter bags. Use the strict scripted fake for Engine proof and a closed adapter enum for product Providers.

Native OpenAI Responses and Anthropic Messages are supported first. One ProviderProfile/model/outcome encoding/privacy profile is pinned before Workspace input and for every call in a Run. Strict provider-enforced JSON Schema is preferred; a synthetic outcome tool is allowed only with documented strict arguments and forced selection. JSON mode, prompt-only repair, and automatic tool selection fail admission.

Custom endpoint support is part of beta, beginning with a closed `openai-responses` protocol family. Profiles live only in trusted user configuration and name exact normalized origin/base path, model, credential environment reference, outcome encoding, privacy assertion, and evidence version. `arany provider check PROFILE` must pass a data-free bounded conformance suite before the profile receives Workspace content. Evidence is keyed by Arany/adapter/test versions, exact endpoint, model, encoding, output cap, timestamp, and expiry. A model catalog alone proves nothing.

Receipts classify `native supported`, `custom verified`, `custom unverified`, or future `broker constrained`. A built-in OpenRouter profile is triggered scope behind route/privacy/provenance/no-fallback gates. Z.AI remains unadmitted while the exact route lacks strict outcome enforcement.

### D-05 — Event contract and replay

**Beta decision:** one STRICT Events table contains global sequence, required Session ID, optional Run/AgentRun IDs, kind, event version, bounded JSON payload, and display timestamp. Sequence defines order.

Initial semantic kinds are:

- `SessionStarted`, `SessionRenamed`, `SessionDefaultChanged`, `SessionForked`, `ContextCompacted`;
- `MessageAccepted`, `MessageCommitted`;
- `RunStarted`, `RunFinished`; and
- `AgentSpawned`, `AgentUpdated`, `AgentFinished`.

SessionView and RunView are pure reductions. Unknown kind/version, gaps, malformed payload, illegal transition, wrong scope, invalid fork boundary, or compaction digest mismatch fails closed. Rebuildable Session-list indexes are allowed; duplicate canonical Session/Run/Message tables are not.

### D-06 — Identity and time

**Beta decision:** typed UUIDv7 newtypes identify Sessions, Runs, and AgentRuns; SQLite integer sequence is authoritative Event order. Persist Unix milliseconds for display and use monotonic `Instant` for deadlines. Do not add Clock or ID-generator traits.

### D-07 — Workspace and instruction inputs

**Beta decision:** pin the user-selected Workspace as a capability. Resolve exact root `AGENTS.md`, falling back to exact root `CLAUDE.md` only on absence. Resolve every explicit include component-by-component with no-follow handles. Reject absolute paths, `.`, `..`, empty components, links/reparse points, and non-regular files.

Read each opened handle once, bound it, validate UTF-8, hash exact bytes, and send snapshots rather than paths. Repository/Git configuration, `.env`, hooks, plugins, packages, tests, and startup commands are never consulted.

### D-08 — Resource bounds

**Beta decision:** keep limits fixed until measurements justify configuration.

| Resource | Beta limit |
|---|---:|
| User Message/objective | 8 KiB |
| Explicit includes | 16 files |
| One include / all includes | 128 KiB / 256 KiB |
| Instruction file | 64 KiB |
| Compiled Provider input | 384 KiB |
| Child objective / summary / result | 2 KiB / 2 KiB / 16 KiB |
| Primary final result | 32 KiB |
| Provider response body | 1 MiB |
| Event payload | 64 KiB |
| Default / hard active children | 3 / 8 |
| Provider calls | `1` direct or at most `N + 2` team calls |
| Output tokens per call | 4,096 |
| Provider-call / Run deadline | 120 s / 300 s |
| Cancellation drain | 2 s |
| Automatic retries | 0 |
| SQLite busy wait / database / admission headroom | 250 ms / 256 MiB / 4 MiB |
| Update / store channels | 32 / 64 items |

Before `RunStarted`, reserve the selected policy's maximum call/output/resource budget. Every overflow is typed and visible; canonical content is never silently truncated. The eight-child ceiling may increase only after named-host concurrency, memory, cost-feedback, and terminal-density gates pass.

### D-09 — CLI and Session commands

**Beta decision:** expose:

```text
arany [GLOBAL_OPTIONS] [PROMPT]
arany --continue
arany --resume [SESSION_ID]
arany --fork SESSION_ID
arany exec [GLOBAL_OPTIONS] --output text|jsonl PROMPT
arany show [--state-dir DIR] --output text|jsonl SESSION_OR_RUN_ID
arany provider check PROFILE
```

Bare `arany` creates a persistent Session; with a prompt it starts the first Run and remains interactive. `--continue`, `--resume`, and `--fork` are explicit; directory matching never resumes implicitly. `exec` creates a single-Run Session by default and requires an explicit Session ID to append. There is no `run` alias.

The closed slash set is `/help`, `/status`, `/sessions`, `/new`, `/clear`, `/resume`, `/fork`, `/rename`, `/compact`, `/agents`, `/provider`, `/model`, `/permissions`, `/quit`, and `/exit`. `/clear` aliases `/new`; it does not delete history. `/agents` inspects AgentRuns and configures next-Run `single|auto|team` plus `N`. Provider/model/collaboration changes are locked during a Run. Commands are local and never Provider input; `//` escapes a leading slash. Machine modes interpret prefixes literally.

Approval/sandbox controls remain absent until effectful Tools and enforcement exist.

### D-10 — Terminal contract

**Beta decision:** use a bounded inline Ratatui viewport in normal terminal flow:

1. committed transcript in native scrollback;
2. composer;
3. compact status row below input; and
4. conditional activity shelf.

The single-agent case gets one active row, not empty team chrome. Multiple/attention agents use at most three rows plus `+N more`; `/agents` opens full details. The footer prioritizes Session, pinned/next Provider and model, permission profile, collaboration mode, and context remaining.

Keyboard behavior is complete. Normal mode does not enable mouse reporting. Open command/Session/agent pickers may enable it transiently; every action has a keyboard equivalent and the RAII owner disables mouse on close, signal, suspension, panic, cancellation, render failure, or lost ownership. Never enter alternate screen or enable focus, title/clipboard OSC, or global mouse capture.

Screen-reader mode is append-only and control-free. `exec` and `show` never initialize terminal state. The command, not TTY detection, selects behavior.

### D-11 — Configuration, credentials, and custom endpoints

**Beta decision:** resolve CLI selections over trusted user config/Session defaults. State/config roots are platform user locations outside the Workspace and admitted before project input. Custom Provider profiles are configuration, not repository data.

The current CLI retains explicit API-key environment references for `exec`, custom profiles, and flag-selected attached use. Bare new attached mode can select one native API account from the OS credential store; first-run setup writes one versioned record containing its key and selected Provider/model/effort, and Session defaults pin only the account UUID. Replacing the record makes older saved-account Sessions fail closed. Linux Secret Service and macOS Keychain use keyring 4.2.0 with only its `v1` feature; native-platform and dependency review remain release gates. No plaintext fallback or at-rest encryption guarantee is claimed for every Linux backend. Never accept keys in arguments, profile files, Events, terminal output, or telemetry. Do not load `.env` or run credential commands.

Native origins are compiled. Custom non-loopback origins require HTTPS; numeric loopback may use explicit HTTP. Normalize and pin origin/base path, disable redirects/cookies/ambient proxies, reject metadata/link-local/multicast and route drift, and bind one credential to one origin.

### D-12 — Cancellation, errors, and retries

**Beta decision:** the first Ctrl-C cancels the active Run and every child but leaves the Session durable. A second press or two-second drain deadline aborts/reaps remaining tasks; the supervisor persists the strongest honest terminal prefix. Store failure returns operational exit 1 rather than false durable cancellation. Process death replays as `Interrupted`.

Provider/Engine errors are typed safe classes. No credentials, headers, prompts, Messages, includes, response bodies, or raw stored payloads enter diagnostics. Automatic retries, provider/model fallback, and broker fallback are zero; a sent request may already be billable.

### D-13 — Dependency boundary

**Beta decision:** direct dependencies remain the reviewed Rust foundation set: capability filesystem crates, clap, Tokio/cancellation, serde/JSON, reqwest+Rustls, rusqlite/bundled SQLite, directories, secrecy, SHA-256, UUID, error handling, Ratatui/Crossterm, target-specific platform APIs, and trace-only OpenTelemetry crates for the final slice. Dev dependencies include tempfile, Tokio test utilities, OpenTelemetry test support, and a reviewed PTY helper such as expectrl.

Do not add SQLx, a pool, async-trait, vendor community SDKs, a generic configuration framework, tracing subscriber, a second terminal framework, or dynamic plugin loading. The approved account-setup and ChatGPT-plan slices may need reviewed OS credential, OAuth, browser-open, and streaming support; add only the minimal dependencies justified by those concrete boundaries. `Cargo.lock`, feature inspection, advisories, licenses, build scripts, proc macros, native code, and application `unsafe` are release gates.

### D-14 — OTLP ships last in beta

**Beta decision:** after Session/team/native/custom-provider gates pass, ship runtime-opt-in trace-only OTLP/HTTP protobuf to an explicit numeric-loopback Collector. It is part of beta, not deferred to an external request.

Each Run is a trace correlated by safe Session/Run/AgentRun IDs. Spans are dynamic and bounded by admitted agents and calls; the topology is not fixed at nine. Typed fields exclude user Messages, objectives, prompts, instructions, summaries, results, paths, contents, Event payloads, bodies, headers, credentials, and arbitrary attributes. Export failure/drop/shutdown never changes canonical state or Run exit.

### D-15 — Security boundary

**Beta decision:** establish all authority before project input. Session history, repository text, Provider/custom-profile output, compaction, child data, and replay are untrusted. None may widen roots, endpoints, credentials, protocol, models, budgets, topology, policy, instruction role, telemetry, or capabilities.

The current implementation has no shell, subprocess, Workspace write, arbitrary fetch, MCP, runtime plugin, callback listener, or sandbox claim. The approved official ChatGPT OAuth slice may add a bounded loopback callback listener only after its separate authentication and local-transport review. State is private/no-follow/defensive; output is inert; custom egress requires exact current evidence; budgets are aggregate; mouse capture is transient and restored; supply-chain inputs are pinned and reviewed. The first effectful Tool activates typed EffectIntent, Policy, approval proof, separate Guard, and attestation.

### D-16 — Evidence-dense testing

**Beta decision:** maximize evidence per test, not test count or line coverage. One deterministic Session journey owns new/single Run, exit/resume, Provider switch, `N`-child team, controlled completion, fork, compaction failure, cancellation, replay, and exact output with only Provider scripted.

Tables/corpora own other `N` values, parser/command states, illegal histories, path/security inputs, custom-profile conformance, and terminal controls. TestBackend owns semantic composer/footer/shelf frames. Native Linux/macOS PTYs own scrollback, Session/agent pickers, transient mouse, signals, suspension, cancellation, and restoration. Native adapters own explicit paid live runs; custom profiles own data-free conformance before Workspace-bearing smoke.

### D-17 — Product identity, platforms, and license

**Beta decision:** the product and executable are Arany/`arany`. Linux and macOS require the same native filesystem, persistence, signal, terminal, packaging, and accessibility evidence. Windows remains triggered scope.

Use **Apache-2.0 plus a project NOTICE** attributed to `fpmirabile`. This is the closest standard license to free use/modification with redistributed attribution, changed-file marking, and an explicit patent grant. Do not dual-license with MIT and do not append a custom attribution clause. It cannot force private users or hosted services to display credit. Preserve `LICENSE` and `NOTICE` in every release artifact.

### D-18 — Explicitly accepted ChatGPT-plan route is beta scope

**Context:** OpenAI's official open-source Sign in with ChatGPT flow now permits eligible Plus/Pro users to authorize Responses inference with their plan. Its preview route requires `stream: true` and rejects `max_output_tokens`, so it cannot satisfy the existing remote output-token guarantee. See the [official quickstart](https://developers.openai.com/siwc/quickstart), [route limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations), and [error guidance](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery).

**Beta decision (approved, not implemented):** offer API key and ChatGPT-plan access as distinct first-run choices. The plan route requires a clear warning and affirmative account-level acceptance that local time/read limits cannot guarantee a remote output-token or plan-usage cap. Pin access method and effective guarantee before each Run; never silently fall back to API-key billing. Keep the strict remote cap on existing API-key and custom routes. This accepts a weaker cost guarantee for the voluntary subscription route in exchange for using an eligible existing plan; it does not authorize an unbounded local stream or a claim of exact cost control. Browser/OIDC, protected token storage, streaming, model/effort admission, and offline/live conformance remain release gates. Anthropic subscription auth remains unadmitted. Never import other harness credentials or call private ChatGPT routes.

**Implementation boundary:** the user's approval sets product policy, not consent for any particular ChatGPT account. Bind affirmative acceptance to the verified registration and warning version before activating it; an account switch or changed warning needs its own acceptance. The official route has a distinct account-specific model catalog and treats only a terminal `response.completed` event as success. Sources: [accounts and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions), [models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference).

## 3. Triggered modules

| Trigger | Introduce | Evidence already available |
|---|---|---|
| First effectful Tool | Typed effects, Policy, approval proof, separate Guard | Security/protection reports |
| Children need nested delegation | Recursive supervision and depth budgets | Multi-agent report |
| Concurrent writes | Isolated Workspace views and integration owner | Multi-agent report |
| Dependencies/ownership transfer | Assignment DAG, attempts, leases, fencing | Multi-agent report |
| Knowledge helps another Session | Scoped Memory and retrieval/deletion evaluation | Context/Memory report |
| Event payload bound is exceeded | Content-addressed Artifacts | Context/Provider reports |
| Replay misses measured budget | Rebuildable snapshots | Persistence report |
| Retrieval quality requires it | FTS5, then vector/hybrid only after labelled gap | Context report |
| Second Client or detached work | Per-user daemon and versioned local protocol | Modular architecture |
| Built-in OpenRouter support is claimed | Exact broker route/privacy/provenance/no-fallback adapter | Provider report |
| Browser/remote/multi-tenant product exists | New authn/authz/quota/encryption/retention/threat model | Modular/security reports |
| Direct remote telemetry is required | TLS/auth/secrets/proxy/SSRF design | OTLP report |

## 4. Implementation handoff

1. Write one implementation plan from `planning/TEMPLATE.md`, importing D-15 security and D-16 evidence owners.
2. Create the one package with pinned toolchain, lockfile, minimal features, and supply-chain inventory.
3. Implement trusted startup, private state, Workspace snapshots, Session/Event schema, reducers, deterministic output, and hostile-content handling.
4. Prove create/exit/resume/multi-Run/fork and deterministic context compilation with the scripted Provider.
5. Prove `single`, `auto`, `team`, ordered `0..N` children, aggregate budgets, join, and cancellation.
6. Add the native-scrollback composer/footer/activity shelf, Session/agent pickers, accessibility, and transient mouse after keyboard/restoration proof.
7. Add native OpenAI and Anthropic API-key adapters and their offline/live conformance.
8. Add trusted custom `openai-responses` profiles and data-free `provider check`.
9. Add slash-command Tab completion and persistent argument placeholders, then first-run account setup with protected keys and the separately admitted ChatGPT-plan route.
10. Pass native Linux/macOS security, fault, PTY/accessibility, packaging, license, and performance gates.
11. Recheck the existing opt-in OTLP slice for topology, privacy, bounds, failure isolation, and shutdown after the new routes.

No daemon, process protocol, Guard, Tool runtime, cross-Session Memory, Artifact store, nested team, Assignment DAG, search index, automatic Provider router, built-in OpenRouter, evaluation service, web API, direct remote telemetry, OTLP logs, or OTLP metrics precedes that proof.
