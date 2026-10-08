# Testing strategy for the Rust CLI harness

> **Session/team amendment — 2026-09-29:** The central journey is now one durable Session covering a single-agent Run, process exit/resume, provider switch, bounded `N`-child Run, fork, compaction failure, cancellation, and replay. TestBackend owns composer/footer/conditional-shelf frames; native PTY owns Session/agent pickers and transient mouse restoration. Custom profiles require data-free exact conformance before Workspace-bearing smoke. The evidence-per-test and no-duplication rules remain unchanged.

**Status:** recommended beta testing contract
**Research date:** 2026-09-29  
**Scope:** the canonical single-package, single-process, CLI-only, read-only demonstrator in [the system overview](../architecture/system-overview.md)

> **Provider and terminal amendment — 2026-09-29:** [Beta multi-provider routing](./beta-multi-provider-routing-and-adapters.md) replaces the single-OpenAI live lane with one explicitly activated paid test per claimed adapter. [Beta terminal interface](./beta-terminal-interface-and-multi-agent-feedback.md) preserves the exact-byte `ObservationBundle` for deterministic `exec`/`show` and adds separate pure/TestBackend, native PTY, and accessibility lanes for bare interactive `arany`. Raw interactive escape bytes are not public golden output.

> **Interactive/auth amendment — 2026-09-29:** [Interactive CLI conventions](./interactive-cli-conventions-and-command-surface.md) replaces `run` with a durable bare `arany` Session, adds the composer and closed slash-registry corpus, and keeps `exec` as deterministic one-Run automation. [Consumer subscription authentication](./consumer-subscription-authentication-for-provider-adapters.md) keeps the beta API-key only. The suite must prove login commands are absent and imported subscription credentials are rejected; future OpenAI plan support would require separate OAuth/OIDC/secret/SSE/live gates, while Anthropic subscription login remains prohibited without prior approval.

## Executive decision

The harness should optimize for **evidence per test**, not test count or line coverage.

The initial suite should have one deterministic durable-Session journey as its center: one Run finishes directly; a second Run delegates a small admitted team whose children finish in controlled reverse order; the primary synthesizes; Events are committed; visible output is captured; and a fresh process reconstructs the same Session facts from SQLite. Around that journey, add only compact table-driven or corpus tests for boundaries that the happy path cannot prove: rejected startup input, illegal Event histories, terminal-control payloads, cancellation/deadlines, provider wire parsing, and durable failure.

This produces three honest levels of end-to-end evidence:

1. **Deterministic product composition:** real Engine, loop, scheduler, store, reducer, and deterministic renderers with the strict scripted `Provider`. This is the main release gate and compares the exact bytes `exec`/`show` users receive.
2. **Real product process and terminal:** spawn the Cargo-built executable to prove arguments, exits, streams, environment isolation, replay, and native PTY lifecycle. It must not add a hidden fake-provider mode to production.
3. **Real provider smoke:** one explicitly selected, ignored, paid test per claimed adapter runs the actual executable and asserts structural facts, not exact natural language.

The product cannot have a deterministic, subprocess-level bare interactive Run using the fake without exposing a fake route in the production binary or creating a second test binary. Neither is justified for V1. The deterministic journey should therefore exercise the actual CLI adapter and writers in-process, while the product-process/PTy tests own OS plumbing and `show`, and live smokes own complete production paths. Calling these layers separately is more accurate than overstating one test as fully real.

The practical target is a small number of named scenarios, each owning several related assertions. There is no test-per-function policy, no coverage percentage target, no snapshot for every error, and no duplicate assertion merely because another layer could repeat it. Rust's own testing introduction notes that tests demonstrate failures rather than prove their absence; the suite should make the strongest product claims executable without pretending that volume equals assurance. [The Rust Programming Language: Writing Automated Tests](https://doc.rust-lang.org/book/ch11-00-testing.html)

## 1. V1 claims the suite must prove

Testing follows the architecture rather than inventing a parallel product. Beta contains one Cargo package, one CLI process, durable Sessions, one Engine behavior seam, one SQLite Events table, admitted direct or `N + 2` team-call budgets, and no effectful Tool. The suite must prove these externally meaningful claims:

- bare interactive `arany` and `exec` accept the documented bounded inputs, execute direct and bounded-team Runs through the same loop, and do not synthesize until every admitted child finishes;
- bare interactive mode shows the durable causal team state on stderr, restores terminal state on every exit path, and writes only the successful final answer to stdout;
- screen-reader mode is labeled and append-only with no CSI/OSC or rewriting;
- deterministic `exec` text/JSONL and `show` obey exact channel and byte contracts without terminal initialization;
- `show <run-id>` reconstructs the same facts after the originating process and database connection have ended;
- input, state, provider, output, concurrency, time, storage, and cost limits fail in the documented class without widening authority;
- cancellation reaches every active child, drops pending Provider futures, drains or aborts within the fixed bound, and leaves an honest durable prefix;
- malformed or hostile repository content, restored Events, provider data, and terminal content remain data rather than authority;
- every claimed Provider adapter still matches its live protocol when its explicitly paid smoke is run; and
- the same claimed filesystem and process behavior works on every supported platform.

Tests do **not** need to prove deferred modules. There should be no V1 tests for Memory, MCP, plugins, a daemon, a process protocol, a browser, effect Policy/Guard, general assignment DAGs, snapshots, FTS, automatic Provider routing, or remote OTLP. The inline terminal is V1 scope and owns focused tests; this does not justify a general UI framework.

### 1.1 Canonical user-visible observation bundle

Every deterministic noninteractive acceptance scenario produces one diagnostic value, even if its assertions inspect only part of it:

```text
ObservationBundle
  exit                 process/adapter exit class
  stdout_bytes         exact bytes delivered on stdout
  stderr_bytes         exact bytes delivered on stderr
  reopened_events      Events read after the writer/connection has closed
  reopened_run_view    RunView reduced from those reopened Events
```

This is the minimum evidence unit for user-visible behavior. A success assertion is incomplete if it checks only an Engine return value, only a mock call, or only stdout. The bundle proves together what the user received, what automation received, whether the process reported success, and what durable truth survives for `show` after exit. Dynamic IDs and timestamps may be canonically labelled for comparison, but the raw bundle remains available in bounded escaped failure output.

For the deterministic in-process journey, `exit` is the exact exit class returned by the CLI adapter and the byte buffers are the actual writer outputs; the reopened state uses a fresh SQLite connection. For subprocess cases, all five fields come from the shipped executable and its closed state database. Each ignored live smoke produces the same bundle from the complete production path. Bare interactive `arany` adds a PTY observation containing semantic frames, terminal settings before/after, keys/signals, and the final post-restore receipt; it does not replace or redefine this exact-byte bundle.

## 2. Admission rule for a test

A new test is justified only when all four conditions hold:

1. It protects a user-visible contract, a durable/security invariant, a protocol boundary, a concurrency/failure mode, or a demonstrated regression.
2. No existing test already fails for the same defect with equally useful diagnostics.
3. It can be deterministic, isolated, and bounded, or it is explicitly classified as an opt-in live/performance check.
4. The author can state the defect it catches and show that the test would fail when that defect is present.

If any condition is false, prefer strengthening an existing scenario, a type, or a parser instead of adding a test.

Examples that normally do **not** earn a new test:

- one test per private helper or struct field;
- assertions that only repeat Rust type checking or exhaustive enum matching;
- tests of mock call order when order is not part of the protocol;
- snapshots of `Debug` output, SQL strings, internal request structs, or implementation-specific task scheduling;
- a second example from the same equivalence partition with no new boundary;
- exact model wording, elapsed wall-clock timing, or provider latency;
- tests for an abstraction created only to make that test possible; and
- a regression test after the feature or reachable failure class has been removed.

Coverage can reveal an unvisited branch, but it is not an acceptance target. A covered assertion-free path is weak evidence, while one black-box scenario can prove many connected contracts. SQLite's own testing documentation distinguishes statement, branch, fault, crash, boundary, fuzz, and other evidence; its unusually high coverage is one part of a much broader assurance system, not a universal application quota. [SQLite: How SQLite Is Tested](https://www.sqlite.org/testing.html)

## 3. Evidence portfolio, not a test pyramid quota

Use the following portfolio. The “owner” column prevents duplication.

| Evidence owner | What is real | What is controlled | Claims owned | Default run |
|---|---|---|---|---|
| deterministic team journey | Engine, loop, SQLite file, reducer, renderer, filesystem fixtures | Provider semantic outcomes, release gates, monotonic test time | topology, concurrency, commits, replay, visible human/JSONL facts | yes |
| real-binary contract | shipped executable, OS process, clap, stdout, stderr, exit status, `show` | temporary state/workspace and known stored Run | process channels, invocation errors, replay after process exit | yes |
| pure terminal projection | PresentationModel, sanitizer, Ratatui TestBackend | RunView fixtures, widths, colors, focus | semantic parity, hostile text, 40/50/79/80/120 layouts | yes |
| native PTY contract | shipped bare `arany`, OS terminal, Crossterm/Ratatui lifecycle | scripted scenario, composer/commands, keys, resize, signals, terminal emulator/tmux matrix | eligibility, parsing, restoration, scrollback, cancellation, suspend/resume, fallback | CI on claimed OS |
| accessibility contract | linear presentation and shipped screen-reader mode | exact fixtures plus manual assistive technology | no control rewriting, reading order, labelled facts | automated + release manual |
| focused owning-module tests | production parsers/reducer/sanitizer/store code | small tables or hostile corpora | exhaustive transition and boundary invariants | yes |
| crash/fault scenario | child process, real SQLite file, reopen and replay | deterministic kill/fault point | old-or-complete transaction and honest interrupted recovery | release/CI gate |
| cross-platform conformance | actual OS filesystem, ACL/path/signal behavior | same logical cases per platform | supported-platform claims | CI on claimed OS |
| live Provider matrix | shipped executable, DNS/TLS/HTTP, selected adapter, persistence, output | tiny fixed workspace and structural assertions | current wire compatibility and complete production wiring per claimed adapter | ignored/manual |
| performance measurement | release binary and named machine | scripted Provider and fixed fixtures | local overhead and resource budgets | release measurement |
| fuzz/property/model checks | one bounded pure boundary or state machine | generated inputs and retained regressions | broad input/trace classes | triggered, not initial default |

Cargo integration tests are separate crates, and Cargo automatically builds binary targets and provides `CARGO_BIN_EXE_<name>` so an integration test can execute the actual program. Cargo also notes that every integration-test file becomes a separate executable, so the V1 suite should keep process scenarios together rather than multiplying files. [Cargo targets: integration tests](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#integration-tests)

### 3.1 Tests, live smoke, evaluations, and benchmarks are different evidence

Do not mix these categories or make one gate inherit another's uncertainty:

| Category | Question | Assertion style | Default CI |
|---|---|---|---|
| deterministic test | Did a binary invariant hold for this controlled execution? | exact/binary, repeatable | yes |
| live provider smoke | Does the complete adapter still interoperate with the current service? | structural and bounded | no; explicit paid run |
| behavioral evaluation | How reliably and well does a model/scaffold solve a task distribution? | repeated trials, graders, distributions | deferred until its architecture trigger |
| benchmark | What latency/resource/cost distribution does a named build/environment exhibit? | measurement against a release gate | separate named environment |

A live smoke is not an eval: one successful model response does not establish quality or reliability. An eval is not a correctness test: a score cannot override a broken replay invariant, leaked secret, wrong exit code, or invalid output stream. V1 has no evaluation runner; the existing [evaluation research](./evaluation-and-quality-strategy.md) activates only when repeated trials are needed for release evidence.

## 4. The central deterministic journey

### 4.1 One scenario, many observable claims

The main fixture should contain:

- a temporary Workspace with exact root `AGENTS.md`, a decoy `CLAUDE.md`, two explicit include files, and hostile files that were not included;
- a temporary private state directory on a real local filesystem;
- a strict scripted Provider with four expectations matched by phase, role, objective, ordinal, and required input digests;
- separate release gates for the two children so worker B finishes before worker A; and
- bounded fixture results containing representative Unicode plus harmless newline/tab content.

The journey should drive:

```text
root plan -> delegate A and B
worker A  -> wait A -> finish
worker B  -> wait B -> finish first
root      -> synthesize only after A and B -> finish
```

The small-team portion may choose `N=2` to make overlap and reverse completion easy to observe; separate table rows cover `N=0`, configured limits, and the hard ceiling. The scenario then asserts:

- exactly the admitted calls and no unclaimed expectation;
- two child calls overlap, completion order differs from spawn order, and child display order remains stable by spawn sequence;
- root synthesis input includes both committed child results and cannot occur early;
- selected instruction path/digest and include digests match the bytes actually read;
- omitted hostile files and canary secrets never appear in Provider observations, Events, SQLite bytes, stdout, or stderr;
- every visible transition corresponds to a committed Event;
- the live `RunView` equals the view replayed after closing and reopening the store;
- human stdout is exactly the final result plus one newline;
- human stderr is exactly the approved progress grammar after narrow canonicalization of generated identities/time;
- JSONL is valid UTF-8, exactly one object per line, strictly sequence ordered, and semantically equal to stored Events; and
- `show` renders the same stored facts twice, byte for byte.

These are multiple assertions in one user journey, not multiple tests for each implementation layer. A failure message should report the phase plus bounded captured stdout/stderr and the semantic Event diff so an agent can diagnose it without rerunning with ad hoc logging.

### 4.2 Strict Provider double

The scripted Provider is the only Engine behavior double. It must behave like a small protocol verifier, not a queue of “next response” values:

```text
ExpectedCall
  matcher  = phase + role + objective + ordinal + required digests
  behavior = return outcome after gate | remain pending until dropped
```

It atomically claims one unique expectation before awaiting. It fails on no match, multiple matches, duplicate claims, unexpected calls, extra calls, or unclaimed ordinary expectations at teardown. Pending cancellation expectations use a drop guard so the test proves that the in-flight future was actually cancelled.

Do not match the concurrent workers by arrival order. Their arrival is intentionally nondeterministic. Match by semantic identity, then control only the completion ordering with gates. Do not sleep, use randomness, touch the network, inspect ambient environment, or issue production IDs from the fake.

This double belongs at the semantic `Provider` boundary because that is the only production behavior seam. Do not mock the store, reducer, scheduler, filesystem, or renderer in the central journey.

### 4.3 Exact user-visible output

The output assertion should compare bytes, not log records or an internal `RunView` alone:

- capture human stdout and stderr in separate byte buffers through the same writers used by the CLI adapter;
- capture JSONL as bytes, split only on LF, parse each line with `serde_json`, and compare it to the canonical Event projection;
- call the real product binary for `show` against the journey's closed database and compare its stdout to the in-process rendering;
- assert exit status separately from both streams; and
- on failure, show a bounded, escaped diff of both streams.

Generated UUIDv7 values and timestamps must not be erased with broad regular expressions. Parse the structured Events, build a one-to-one mapping such as `RUN`, `ROOT`, `WORKER_A`, `WORKER_B`, and replace only fields declared nondeterministic by the contract. Preserve identity equality, ordering, counts, and every user-controlled byte. A normalizer that removes arbitrary numbers or whole lines can hide a real regression.

## 5. Real subprocess tests

Use `std::process::Command` with the absolute Cargo-provided binary path; never invoke `cargo run`, a shell, or PATH lookup from the test. Rust's process API captures status, stdout, and stderr directly, and `Command::env_clear` prevents ambient variables from entering the child unless explicitly restored. [`std::process`](https://doc.rust-lang.org/std/process/) [`Command`](https://doc.rust-lang.org/std/process/struct.Command.html)

Every subprocess test must:

- use its own `TempDir` Workspace and state directory;
- set an explicit current directory;
- clear the inherited environment and add only the minimum platform variables and harness configuration needed by that case;
- pass arguments as an argument vector, never a command string;
- capture stdout and stderr separately;
- impose a parent-side deadline and kill/reap a hung child;
- assert the documented exit class plus output channels; and
- avoid fixed ports or shared database paths.

The first black-box table should cover related process contracts without one function per row:

| Case | Exit | stdout | stderr |
|---|---:|---|---|
| `--help` | 0 | help contract | empty or documented clap behavior |
| invalid syntax | 2 | empty | safe invocation diagnostic |
| missing required model/key for bare interactive mode | 2 | empty | safe configuration diagnostic |
| unknown Run for `show` | 1 | empty | safe operational diagnostic |
| known completed Run for human `show` | 0 | exact reconstructed view | empty |
| known completed Run for JSONL `show` | 0 | exact stored Events | empty |
| corrupt/incompatible Run | 1 | empty | safe corruption/incompatibility diagnostic |

Do not add `assert_cmd` initially. Standard-library process control and one helper are enough at this size. Add a process-test crate only when at least three scenarios duplicate material timeout/signal/platform machinery; this matches the existing architecture's dependency trigger rather than prepaying for a framework.

### 5.1 Ctrl-C at the process boundary

The Engine-level cancellation scenario is mandatory and deterministic. A true signal test is also needed before claiming CLI Ctrl-C behavior, but it is platform-specific:

- Unix sends `SIGINT` to the spawned product process and waits for exit 130;
- Windows uses the supported console-control mechanism in an isolated process group and waits for the documented result;
- both inspect the reopened database and prove that active agents terminated honestly; and
- both always reap the child, including assertion failures.

Keep the unsafe/platform-specific signal helper in test support, not application code. If a claimed platform cannot deliver the signal reliably in CI, record that as missing platform evidence rather than silently skipping the test. The deterministic supervisor test still owns cancellation ordering; the process signal test owns only OS wiring and exit behavior.

Tokio documents graceful shutdown as detecting shutdown, notifying tasks, and waiting for completion; its `CancellationToken` provides the cancellation tree used here. [Tokio: Graceful Shutdown](https://tokio.rs/tokio/topics/shutdown) [`CancellationToken`](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html)

## 6. Persistence and replay tests

Never use SQLite `:memory:` as evidence for durability, reopen behavior, file permissions, journaling, page limits, or `show`. Every persistence test uses a real temporary database file and closes every connection before replay.

The store suite owns four claim groups:

1. **Schema/configuration:** exact schema version, STRICT table, journal/synchronous/trusted-schema/defensive/no-follow settings, page size/cap, private state permissions or ACL, and one owning connection.
2. **Transition transaction:** a logical transition is either wholly visible or absent; feedback never precedes its commit.
3. **Strict replay:** sequence gaps, unknown kind/version, malformed payload, identity mismatch, illegal transition, excessive count/size, and instruction-looking text fail closed or remain inert data as specified.
4. **Recovery:** normal close, process death between transaction boundaries, disk/full-page refusal, and a committed nonterminal prefix reconstruct honestly as `Interrupted` rather than fabricated cancellation.

Use compact data tables for malformed histories. Do not write a separate test per Event kind × state × error. A reducer transition table can enumerate legal transitions and systematically generate the complementary invalid pairs because V1 has only five Event kinds and a small state space.

For crash evidence, spawn a child that performs the production store operation and terminates at a named test-only fault point before or after commit acknowledgement. The parent then opens the actual file and checks:

- the transaction is wholly absent or wholly present;
- sequence continuity and reducer invariants hold;
- `PRAGMA integrity_check` succeeds; and
- rendered/replayed state never claims a terminal Event that was not committed.

SQLite itself validates atomic commit by repeatedly simulating crashes at varying write points and checking that a transaction occurred completely or not at all. The harness does not need to reproduce SQLite's VFS verification; it must test its own transaction boundary, acknowledgement order, and recovery interpretation. [SQLite: Atomic Commit, crash testing](https://www.sqlite.org/atomiccommit.html#testing_atomic_commit_behavior) [SQLite: How SQLite Is Tested](https://www.sqlite.org/testing.html#crash_testing)

Do not copy a live database file as a fixture. SQLite warns that a database and hot journal must remain paired and recommends the Backup API, `VACUUM INTO`, or copying only while quiescent. V1 can simply close its sole connection before an inspection copy. [SQLite: How To Corrupt Your Database](https://www.sqlite.org/howtocorrupt.html#_backup_or_restore_while_a_transaction_is_active)

## 7. Concurrency, cancellation, and time

Concurrency tests must schedule events; they must not hope for an interleaving.

Required deterministic scenarios for the selected `team(2)` overlap fixture:

- both child Provider calls in the `team(2)` fixture reach claimed expectations before either gate is released;
- worker B completes before A even though A was spawned first;
- primary synthesis remains blocked until both child terminal Events in that fixture commit;
- cancelling while both children are pending drops both Provider futures and prevents late results from committing;
- a response made ready concurrently with cancellation cannot win after the supervisor has observed cancellation;
- a closed update sink or blocked store applies bounded backpressure and cannot exceed the fixture's admitted four calls; and
- the call and whole-Run deadlines produce the documented durable status and exit class.

Use Tokio's paused time on the explicit current-thread runtime. Tokio's `pause`/auto-advance support requires a current-thread runtime and advances timer-backed work without wall sleeps. [`tokio::time::pause`](https://docs.rs/tokio/latest/tokio/time/fn.pause.html)

Never use:

- `sleep` to “let the worker start”;
- assertion timeouts as the only synchronization mechanism;
- repeated runs as a substitute for a controllable gate;
- assumptions about task polling or child completion order; or
- a global mutable clock or public Clock trait added solely for tests.

`loom` is valuable when application-owned low-level synchronization must be checked across possible interleavings, but it requires replacing synchronization primitives and can face combinatorial growth. V1 delegates synchronization to Tokio channels, `JoinSet`, and `CancellationToken`; do not add `loom` unless the harness later owns a custom synchronization primitive whose state space cannot be proven with gates. [`loom` documentation](https://docs.rs/loom/latest/loom/)

## 8. Provider adapter evidence

The Engine fake and the OpenAI adapter answer different questions:

- the strict fake proves Engine semantics independent of a network or model;
- request-construction tests prove exact method, compiled origin/path, headers, sensitive Authorization marking, `store:false`, disabled truncation, strict schema, model, token bound, and absence of Tools;
- byte-fixture parser tests prove valid, refused, incomplete, malformed, oversized, and typed error responses under the 1 MiB cap;
- configuration tests prove redirects, ambient proxy, cookies, unsafe TLS, and automatic retries are disabled; and
- the ignored live smoke proves the current remote service still accepts the complete request.

Do not create a mock HTTP server or configurable base URL merely to intercept production calls. The fixed OpenAI origin is a security property. Keep request assembly and bounded response decoding as private pure/streaming functions that can consume reviewed fixtures, and reserve actual DNS/TLS/HTTP behavior for the live test and hostile-network security gates.

Provider fixtures must be minimal hand-reviewed examples shaped from the official Responses contract, not captured production bodies containing user data. Never snapshot Authorization headers or rich reqwest errors. OpenAI's Structured Outputs contract is the semantic boundary, while `store:false` is part of the data-control request. [OpenAI: Structured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs) [OpenAI: Your data](https://developers.openai.com/api/docs/guides/your-data)

### 8.1 One live smoke

Keep exactly one ignored test, selected by name:

```text
live_openai_team_run
```

It should spawn the actual product executable with a temporary Workspace/state directory, tiny deterministic documents, the explicitly provided model, and the inherited API key passed without formatting. Use one direct Run and one small bounded team Run; the test records and asserts the admitted call count. It asserts only:

- exit 0;
- stdout is a non-empty bounded final result with the documented newline policy;
- stderr contains the expected primary/child lifecycle grammar and no raw terminal controls;
- the admitted provider-call facts and terminal Runs exist in SQLite;
- fresh product-process `show` and `show --jsonl` succeed and expose the same identities/states/results; and
- canary content excluded from the request does not appear in persisted or rendered output.

It must not assert exact prose, worker completion order, latency, token counts beyond bounds, or model quality. It must not retry. Missing key/model should fail only when the user explicitly selects this test, with a setup message that prints neither value.

Rust supports `#[ignore]` so tests run only when explicitly requested. One passing live invocation is compatibility evidence, not a deterministic regression suite or proof of model quality. [The Rust Programming Language: ignored tests](https://doc.rust-lang.org/book/ch11-02-running-tests.html#ignoring-some-tests-unless-specifically-requested)

## 9. Golden and snapshot policy

Use goldens only where the bytes are themselves a product contract:

- one canonical deterministic `exec` text stdout/stderr pair;
- one canonical screen-reader output and post-restore receipt;
- one canonical human `show` output;
- one canonical JSONL Event stream; and
- later, one safe diagnostic format if it cannot be covered legibly inline.

Do not add `insta` in V1. Store small reviewed `.txt`/`.jsonl` fixtures or inline literals and compare directly. This keeps the exact dependency boundary and makes the contract readable without another tool.

Rules for every golden:

- it contains only user-visible output, never `Debug` representations;
- nondeterministic fields are canonicalized narrowly and visibly before comparison;
- it is small enough to review as a whole;
- CI never rewrites or auto-accepts it;
- an agent may update it only alongside an intentional user-visible contract change and must summarize the diff; and
- deleting or changing a line in a golden does not replace semantic assertions such as Event count, identity relationships, exit status, or secret absence.

Snapshot tools can improve review workflows by writing pending files and requiring explicit acceptance, but their own documentation also supports automatic overwrite modes. If the suite later adopts one, CI must force no-update behavior and unreferenced snapshots must fail rather than accumulate. [Insta: reviewing and update modes](https://insta.rs/docs/quickstart/) [Insta: unreferenced snapshots](https://insta.rs/docs/advanced/#handling-unused-snapshots)

The danger is not snapshots themselves; it is accepting a large output change without knowing which contract changed. A golden is appropriate when a human can answer that question from the diff.

## 10. Property, model-based, and fuzz testing

V1 should not begin with `proptest`, `proptest-state-machine`, Kani, or a broad fuzz suite. The state space is currently small enough for explicit tables, and the canonical dependency decision intentionally excludes these tools. Add generative machinery only when one property replaces a material family of hand-written examples.

Good triggered candidates are:

| Boundary | Property | Trigger |
|---|---|---|
| Event reducer/replay | every generated legal trace reduces identically live and after serialization; every invalid mutation fails closed | Event vocabulary/state space stops being cheaply enumerable or a sequence bug escapes |
| terminal sanitizer | output is valid UTF-8, contains no forbidden controls, is idempotent, and cannot forge a trusted prefix | corpus grows beyond readable table coverage or a control-sequence bug escapes |
| capability intersection | child effective authority is a subset of global, parent, and explicit grant | first effectful Tool activates Policy/Guard |
| bounded decoders | arbitrary bytes never panic, exceed allocation bounds, or produce an unchecked Event/provider outcome | parser implementation exists and deterministic corpus is stable |
| storage state machine | implementation and small reference model agree after generated append/reopen/corrupt sequences | schema/event model gains enough operations to make tables unwieldy |

Proptest can generate arbitrary inputs, shrink a failure to a smaller counterexample, and retain regression seeds. Its state-machine support compares a system under test with an abstract reference model, but it requires setup and currently describes sequential state-machine generation. Use it for the reducer/store only after the trigger, not to generate arbitrary agent prose or async schedules. [Proptest README](https://github.com/proptest-rs/proptest/blob/main/proptest/README.md) [Proptest: State Machine testing](https://proptest-rs.github.io/proptest/proptest/state-machine.html)

Fuzz only bounded byte-oriented trust boundaries: Event decoding, provider response decoding, instruction/UTF-8 validation, and terminal escaping. Seed each target with the deterministic hostile corpus and promote every minimized crash into that corpus. `cargo-fuzz` uses libFuzzer, needs nightly and platform support, and can minimize failing inputs; that makes it a scheduled/release security job rather than the default developer loop. [`cargo-fuzz` repository and usage](https://github.com/rust-fuzz/cargo-fuzz)

Kani is not needed for V1. It becomes plausible for small pure functions with a precise bounded property, especially future capability algebra or isolated unsafe code. Kani's own documentation says it proves or disproves assertions within a proof harness but may run out of resources and does not support every Rust feature. [Kani documentation](https://model-checking.github.io/kani/)

## 11. Security and adversarial corpus

Security cases should be corpora and composition scenarios, not hundreds of individually named tests.

Maintain focused tables for:

- startup authority: malicious `.env`, Git configuration, hooks, filters, worktrees, package metadata, Skills, trust-looking instruction text, and unsupported OTLP variables cause no discovery or execution;
- Workspace paths: empty/absolute/dot/parent components, final and intermediate symlinks, reparse points/junctions, non-regular files, invalid UTF-8, concurrent replacement, hard-link policy, exact limit and limit + 1;
- state: state inside Workspace, link substitution, wrong type, broad permissions/ACL, wrong owner, non-local filesystem, malformed header/schema/Event, page/headroom failure;
- egress and secrets: hostile proxy environment, redirects, oversized/compressed responses, omitted-file canary, credential canary, forbidden telemetry fields;
- output: every C0/C1 byte, ESC/CSI/OSC/DCS, CR, backspace, bidi controls, newlines, JSON delimiters, forged prefixes, oversized fields, and broken pipes; and
- topology/budget: direct mode, representative `team(N)` values, limit/hard-ceiling overflow, child delegation, primary early finish, calls above the admitted budget, late completion, closed receiver, and blocked store.

Each corpus row records the invariant and expected error class. A loop executes the table and reports the row name on failure. Add a new row for a genuinely distinct encoding, platform primitive, or escaped defect; do not turn every row into another test function.

Canary scanning is one shared assertion helper over all relevant sinks: provider observations, stdout, stderr, serialized Events, database bytes, panic text produced by controlled failures, and runtime-opt-in OTLP capture. The canary must be a test-only value and output reporting must not echo it when the assertion fails.

The security report remains authoritative for the required threat cases and non-claims: [Harness security lessons and controls](./harness-security-lessons-and-controls.md).

## 12. Cross-platform policy

A platform is supported only when its behavior suite runs on that platform. Compiling for a target is not evidence for ACLs, reparse points, signals, path namespaces, or process behavior. Rust's target tiers describe Rust toolchain guarantees; they do not prove application-specific filesystem and security semantics. [rustc platform support](https://doc.rust-lang.org/rustc/platform-support.html)

The blocking beta matrix runs on Linux and macOS:

- build, Clippy, and deterministic suite on the pinned toolchain;
- real-binary help/error/show channel contract;
- exact instruction/include path suite using the platform's links and special objects;
- state ownership and private permission/ACL admission;
- close/reopen SQLite replay and process-death recovery;
- cancellation supervisor plus actual process signal/control delivery;
- deterministic text/JSONL byte contract, terminal TestBackend frames, native PTY restoration, and manual VoiceOver/Orca evidence; and
- explicit `ProtectionUnavailable` for any required invariant the platform implementation cannot attest.

Use a shared semantic case description with small platform adapters. Do not force identical low-level setup where operating systems differ, and do not hide an unsupported case behind an unconditional skip. Platform exclusions must name the missing claim in the support matrix. Windows is a deferred support target and must pass the corresponding ACL, reparse-point, console-control, path-namespace, persistence, terminal, and packaging cases natively before it becomes supported; compiling for Windows is not a beta release gate and is not a support claim.

The Cargo CI guide recommends balancing platform/toolchain combinations against project risk and cost. For this harness, filesystem and process behavior are high-risk and platform-specific, so running behavior on every claimed host is justified; testing stable/beta/nightly combinations on every OS is not. Pin one release toolchain for blocking CI and move future-toolchain compatibility to a scheduled advisory job. [Cargo Book: Continuous Integration](https://doc.rust-lang.org/cargo/guide/continuous-integration.html)

## 13. Flake prevention

A test that occasionally passes cannot protect a release claim. Apply these rules:

- no real sleeps; use gates, notifications, paused Tokio time, or parent-side deadlines;
- no network in the default suite;
- no dependence on test execution order; Cargo/libtest runs test functions in parallel by default;
- no mutation of process-global environment in parallel tests; pass a configuration map to pure resolution code or set a cleared environment on a subprocess;
- no shared state directory, fixed temporary filename, fixed port, singleton database, or global runtime;
- no assertion on task arrival/completion order unless a gate establishes it;
- no exact wall-clock duration assertions in correctness tests;
- no retries of a failed test in CI and no “rerun until green” policy;
- property/fuzz failures retain the seed or minimized corpus input;
- live-provider and named-machine performance checks are clearly separated from deterministic CI; and
- every spawned process, task, thread, store connection, temporary listener, and file handle is closed or reaped even during failure cleanup.

Cargo documents that libtest executes test functions in parallel and separate integration-test targets serially. Isolation must therefore be structural rather than achieved by forcing the entire suite to one thread. [Cargo targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#tests) [Cargo test](https://doc.rust-lang.org/cargo/commands/cargo-test.html)

If a deterministic test flakes, treat it as a release-blocking bug in either production synchronization or the test. Quarantining is allowed only with a named owner, missing claim, and removal condition; it does not count as passing evidence.

## 14. Failure diagnostics for humans and agents

Tests should show the product contract that failed, not only `left != right`.

Shared assertion helpers should produce:

```text
scenario: deterministic team run / human output
command:  arany <redacted fixture objective> ...
exit:     expected 0, observed 1
stdout:   <bounded escaped bytes>
stderr:   <bounded escaped bytes>
events:   expected semantic sequence vs observed sequence
state:    temporary fixture identifier, not a user home path
```

For security failures, do not print secret canaries, raw credentials, full provider bodies, database payloads, or hostile controls. Report the sink and byte offset/class. For oversized output, show a bounded prefix/suffix plus total byte count. For JSONL, identify line/sequence/kind and parse error without dumping arbitrary content.

Passing tests should stay quiet. The exact user-visible examples live in reviewed golden fixtures, while failures surface those bytes automatically. An agent should not need verbose production logging, `RUST_LOG`, database dumps, or rerunning against OpenAI to understand a deterministic failure.

## 15. Test maintenance and deletion

Every test has one sentence in its name or nearby table that states the owned claim. Delete or merge a test when:

- the feature/contract no longer exists;
- a higher-value test now catches the same defect with equally precise diagnostics;
- its only assertion is an implementation detail no caller can observe and no safety invariant requires;
- its fixture duplicates an existing equivalence partition;
- a regression is made structurally impossible and the remaining test adds no boundary evidence; or
- it has become permanently ignored without a release procedure that runs it.

Do **not** delete a durable/security regression merely because the implementation was fixed. Move it to the narrowest stable corpus or owning scenario. Do not keep obsolete snapshots “for history”; git already stores history.

When changing behavior:

1. identify the existing owner test;
2. demonstrate its failure or temporarily introduce the target defect if the change is already implemented;
3. update the smallest scenario/corpus row that proves the new contract;
4. review exact user-visible output changes manually;
5. remove superseded assertions and fixtures; and
6. record a new test only if the admission rule is satisfied.

This prevents the common LLM pattern of appending tests for every touched function while never pruning overlapping cases.

## 16. Execution tiers

### Developer and pull request

Run on every change:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The default suite contains no network, wall sleeps, stochastic model, or hardware-sensitive latency assertion. Keep it fast enough that agents run it after every relevant change; set an actual duration budget only after measuring the initial implementation.

### Cross-platform blocking CI

Run the deterministic suite and platform conformance subset on every claimed OS. Run lockfile/source/advisory checks as specified by the security policy. Do not multiply the OS matrix by unnecessary Rust release channels.

### Scheduled or release gates

- repeated deterministic journey for flake detection, comparing canonical semantic output;
- process-death/disk-fault scenarios if too expensive for every PR;
- bounded fuzz targets after their trigger;
- sanitizer/Miri or model-checking jobs if the implementation earns them;
- release-mode local latency/RSS measurements on a named environment; and
- one explicitly authorized live smoke per Provider Arany intends to claim.

Performance is a measurement, not a correctness assertion. Keep the existing local thresholds in the CLI research as release evidence, but do not fail ordinary tests on host-sensitive milliseconds. Repeating one scenario 100 times for a release measurement is not 100 tests and should not inflate the developer loop.

## 17. Recommended initial physical shape

Keep test support as small as the production design:

```text
src/
  main.rs       private CLI adapter tests for exact mode/channel behavior
  lib.rs        reducer, bounds, sanitizer, cancellation tables near owners
  provider.rs   strict fake plus request/response fixture tests
  store.rs      real-file append/reopen/replay/fault tests
tests/
  team_run.rs   deterministic journey, real-binary contract, ignored live smokes
  fixtures/
    interactive.stdout.txt
    interactive.stderr.txt
    human-show.stdout.txt
    team-run.jsonl
```

This is a responsibility sketch, not a requirement to create every fixture immediately. Prefer inline strings while output is tiny. Keep one integration-test target until compilation time or organization proves a second target is worthwhile.

Initial dev support remains:

- `tempfile` for isolated real filesystem/state roots;
- Tokio `test-util` for paused current-thread time; and
- the existing OpenTelemetry SDK testing support only when the optional telemetry patch begins.

Do not initially add `assert_cmd`, `insta`, `proptest`, `loom`, `criterion`, `nextest`, a mock HTTP server, or a custom test framework. Each has a named trigger in this report. Cargo's built-in test harness, the standard library, and the strict Provider fake are sufficient to produce strong V1 evidence.

## 18. Concrete V1 verification register

The implementation plan should turn these claims into a small set of scenario/table owners, not one test per row.

| Claim family | Evidence | Release blocking |
|---|---|---|
| direct and bounded-team workflows | deterministic durable-Session journey with controlled reverse child completion | yes |
| exact human stdout/stderr | byte golden from actual CLI adapter | yes |
| JSONL validity/facts/order | parsed lines plus canonical Event comparison | yes |
| real process arguments/channels/exits | product subprocess table | yes |
| replay after exit | close store, product subprocess `show`, exact repeated output | yes |
| strict startup/input/state trust | adversarial table/corpus on claimed platforms | yes |
| transaction acknowledgement/recovery | real SQLite file plus child-process death/fault point | yes |
| reducer legality/data-only replay | exhaustive transition/history table; generative trigger later | yes |
| cancellation/deadlines | gates, drop guards, paused time; real OS signal subset | yes |
| provider wire construction/parsing | reviewed request/response fixtures | yes |
| no secret/omitted-input leakage | shared canary scan over every sink | yes |
| terminal inertness | hostile control corpus plus human/JSONL byte checks | yes |
| platform support | native CI behavior, no silent skip | yes for each claimed OS |
| live adapter compatibility | one ignored actual-binary structural smoke per claimed Provider | required before claiming that adapter works, not default CI |
| local performance | release measurement on named host | release evidence, not ordinary test |
| OTLP privacy/topology/failure | separate runtime-opt-in scenario | only when the final beta telemetry slice ships |

### Native macOS baseline (2026-10-08)

The user resumed native macOS work on an Apple Silicon host running macOS 26.6.2 and Rust 1.98.1. The initial test build failed on the Unix corpus's Linux-only FIFO constructor. The account-path owner then reproduced environment-selected `HOME` changing macOS account scope. The implementation now reuses the pinned POSIX effective-user lookup with macOS's existing account suffix; the owner moved into the existing product test target and uses its bounded subprocess capture. Absolute debug fixtures, relative rejection and optimized override rejection are covered without opening the user's account root or Keychain.

Native baseline corrections preserve actual private FIFO rejection, the platform-specific consent warning and explicit refusal of unsupported guarded reads. The Collector and custom-provider fixtures reset accepted sockets to blocking mode before timeout-bounded reads: macOS inherits listener nonblocking state. The process-global diagnostic privacy owner now runs in an exact filtered subprocess through the existing bounded capture, excluding unrelated tests' legitimate log contention/rotation. The paused callback owner establishes its connection before starting the receiver's virtual deadline. No product bound, account warning, outcome schema or Guard capability was relaxed.

Astra's complete review found that the account-root correction could strand a saved ChatGPT index at the previous environment-selected root. The new preservation row in the existing registration-migration owner failed before the correction. That owner now migrates registration and the validated matching index under both account locks before saved-account access, with destination/host preflight and old-root tombstones only after durable copies. Its corpus passes private-file tokens, token-free keyring metadata, pending/disconnected/permission-disabled state and model evidence, identical partial copies, an already copied registration, conflicts, missing/malformed identity, unsafe pending-file recovery and both busy locks. The existing setup owner proves old-root presence can reach billing choice without parsing either credential body. The subsequent complete Astra review returned `No findings`; it is defect-review evidence, not native Keychain lifecycle proof.

The user's subsequent ChatGPT greeting reproduced unavailable Tool protection after remembered folder trust. The attached permission owner now reports unsupported native enforcement before loading trust, retaining read-only chat; explicit Tool configuration still rejects. Its native PTY/scripted-Provider owner reproduced the exact error before correction and now proves direct and resumed greetings for all three native profiles, complete supplied request semantics, closed SQLite replay, absent Tool context/Events, unchanged trust/draft and explicit Tool rejection before inference. Repeat review corrected supervision of a `script` descendant: the fixture now uses the pinned safe Nix PTY wrapper and directly owns its exact test process through bounded capture and kill/reap cleanup.

The first full debug rerun exposed a P1 fixture-isolation incident: the existing catalog owner isolated only its stable account root. Cleared HOME/XDG fell back to the real legacy root, migration tombstoned live registration/index metadata, and panic cleanup removed the temporary copy after a credential-helper deadline. This supersedes the earlier no-live-account/Keychain-access assertion for this continuation. Migration performs no Keychain deletion and no live inference was confirmed, but the lost metadata requires an external backup or fresh sign-in. [The finding](../security/findings/account-fixture-legacy-root.md) owns impact and recovery limits. All affected launches now isolate both roots, and debug default-path admission refuses an ambient legacy root outside the fixture parent before opening it. The existing account-path owner proves confined roots and ambient rejection. Automatic approval review initially refused another broad suite; targeted isolation evidence and the new admission guard earned approval for the corrected reruns.

Local recovery preserved only the two exact 33-byte markers in a private same-root backup under the existing account lock, after no-follow/type/owner/mode/link/identity checks. Astra reviewed the one-off procedure; synthetic success/refusal fixtures and a real read-only check passed before automatic approval allowed it. Post-operation checks confirmed the original paths absent and both marker bytes preserved. No Keychain item or Session history was changed by recovery. This makes fresh setup available; it does not reconstruct the lost metadata or prove successful reauthorization.

Final native locked offline workspace suites after the setup-redraw correction executed 249 tests in debug and 248 in optimized release, with zero failures and 11 explicitly ignored gates per profile. Formatting, strict all-target Clippy in both profiles and the optimized executable build passed. The activated native callback owner separately completed exactly one test in each profile before the greeting correction: a stalled local connection expired and a subsequent valid synthetic callback succeeded. Fixture commands used physical `TMPDIR=/private/tmp`; ordinary `/tmp` or `/var` aliases still reject through production no-follow admission. Loopback fixtures required execution outside the development environment's network restriction. No user clipboard was accessed.

The user's clarified consent contract is identical on macOS and Linux: every attached entry asks before account or Session work until exact folder trust is remembered. The new native macOS shipped-process linear PTY row failed before the startup correction and now proves setup/explicit-flag ordering, cancellation before saved-account/setup or Session work, read-only re-entry, remembered exact-identity reuse and directory replacement. It also verifies explicit `--tools` is accurately described as retaining private configuration. This startup row is debug-only because optimized account overrides reject; optimized actual-product startup isolation remains unverified. The scripted attached greeting/replay/refusal owner passes in both profiles and opens the same permission prompt in debug. No real account, Keychain or inference was used in this consent correction. Linux PTY readers now isolate passwd home and legacy roots, handle the first decision once, and retain selected release-binary visibility for tmux; their native compilation and execution remain pending. Repeated Astra review fixed fixture type/lint/ordering issues and Provider/mode/Skill eligibility inconsistencies; its final complete review returned `No findings`.

The reported post-ChatGPT-setup renderer failure exposed a shared Unix descriptor bug: setting inherited stdin nonblocking also changes duplicated terminal output. Native macOS inspection reproduced `WouldBlock` after a partial write, and the new terminal-owner process row failed before correction on its inherited-output flag assertion. Both readers now own an independently opened nonblocking handle with no-follow/no-controlling-terminal acquisition and device/inode/rdev validation. The native process owner passes in both profiles: synthetic setup returns to a complete chat redraw, two input submissions straddle release/reacquisition, inherited flags remain unchanged and exact terminal settings restore in inline and linear modes. Its directly owned bounded child opens no account, credential, browser or network. The Linux signal observer now checks the owned reader descriptor; native Linux execution and the actual browser-login return remain unverified. Astra's focused implementation review returned `No findings` by inspection.

The selected-credential startup correction moves OS-store access before chat and credential-bearing Provider preflight, including resume/continue with an initial objective. A fixed service/validated slot and helper-side native UUID/Provider check constrain the status-only authorization; it returns no credential. The interactive supervisor waits up to two minutes, supports future-drop cancellation and kills/reaps the helper. Native discovery without preferences uses the same interactive bound for its existing bounded read, discarding the key before chat; ordinary operations retain five-second deadlines and write/deletion uncertainty. Unconfigured startup no longer probes the OS store. The native debug attached-process corpus uses isolated current/legacy roots and an immediate or held synthetic helper, proving selected item/protocol, authorization before input, missing/denied/invalid response handling, cached/native discovery, private-file/unconfigured bypass, unchanged closed resume/continue Event prefixes, Ctrl+C exit before Session work, exact terminal restoration, helper EOF and positive PID disappearance. The helper protocol/cancellation corpus runs in both profiles. The explicitly activated six-second latency owner failed with the old five-second authorization deadline and passed with the correction in debug and release; the routine write-timeout/uncertainty/reaping owner passed on macOS debug. Final full native suites pass 253 debug and 251 release tests with 14 ignored gates each; format, strict all-target lint in both profiles, optimized executable build and diff checks pass. Astra's final review returned `No findings` after fixing initial preflight ordering, native identity checks and the Linux stalled-bus fixture. Its Linux adaptation explicitly declines synthetic startup access before exercising a later user-submitted admission/cancellation; native Linux execution remains unverified. This correction used no real Keychain item, user credential, sign-in, Provider or clipboard. Per-item native Keychain dialog/ACL behavior remains an isolated synthetic-store/manual gate, and Allow Once can require a subsequent prompt.

The requested continuation adds native descriptor-based ACL admission for private roots, record/pending/database files and account/Session locks. The real ACL corpus keeps private mode bits while adding broad allow grants, proving that mode-only admission misses the defect; deny-only ancestor ACLs remain valid. The dedicated FFI review checked the macOS SDK's enum/permission ABI, absent-ACL/end-of-entry errors, borrowed descriptor lifetime, validated allocation, bounded traversal and single native free. Only this narrow Store adapter allows local unsafe code; the library otherwise denies it and the executable still forbids it. No same-UID or privileged exclusion is claimed.

The macOS helper now uses the already locked Security framework directly for the exact user-domain Keychain/service/slot, inserting only after item-not-found rather than after a denied read. Its debug-only fixed-file fixture checks a private StateRoot and rejects malformed paths; optimized helper/supervisor overrides reject before native access. The explicitly activated isolated owner passes production-supervisor save/read, replacement, selected UUID/Provider checks, locked read/write rejection without a backend/item change, ChatGPT token save/read/deletion and preservation after another executable's denied deletion. It creates a privately named disposable Keychain and observes unchanged user default/search-list preferences before and after; no login-Keychain item or real credential is used. The native framework was promoted from an existing transitive dependency, adding no packages. Human dialogs, optimized lifecycle and cross-user scope remain unverified.

Clipboard transport now uses the checked system JavaScript/AppKit bridge in the existing cleared-environment, killable process owner. A uniquely named synthetic pasteboard gate passes PNG-over-text selection, exact text/PNG bytes, malformed PNG, unsupported TIFF with a text fallback, and oversized PNG/text rejection in debug and release. Type count/name length, pasteboard generation and payload bounds are checked before returning the closed typed frame; ordinary image/text admission remains authoritative. The user's general clipboard was never accessed. Existing draft/modal/cancellation/replay owners retain their scope; general-pasteboard UX and live vision are separate checks.

Handle-relative macOS file admission now rejects component links, traversal, hard links, special objects and device crossings while admitting checked nested files. The shared hostile file-discovery and installed-Skill corpora execute their successful/rejection paths on macOS. Discovery is not Guard dispatch. Native debug active-process PTYs exercise both linear and inline TERM, HUP, Ctrl+C, suspension/resumption and failed-call restoration, with independently captured empty stdout, closed cancelled/failed replay and no accepted assistant message. The fixture owns its session/controlling terminal and close-on-exec descriptors. It exposed fullscreen startup's cursor-preserving clear querying stdout and timing out when stdout is piped; startup now clears stderr directly. Shared restoration observation normalizes only Darwin's kernel-maintained PENDIN bit and compares every other field. The tiny child pre-exec exception was reviewed separately for owned fd 0, allocation-free native syscalls and failure propagation.

The existing explicitly activated TLS owner passes valid chain/name, wrong-name rejection and unrelated-chain rejection in debug and release using generated certificates, loopback-bound OpenSSL 3 servers and a client-local test anchor. It changes no OS trust settings and does not prove default system-trust or actual compiled-endpoint HTTPS journeys on macOS.

Final locked offline native suites execute 258 debug and 255 release tests, with zero failures and 17/16 explicitly ignored gates respectively. Formatting, strict all-target Clippy in both profiles and diff checks pass. The isolated Keychain gate completes exactly one parent/child test in debug; the named-pasteboard and TLS gates each complete exactly one activated owner in both profiles. These checks use physical TMPDIR=/private/tmp and synthetic current/legacy account roots; native loopback and OS-service gates require execution outside the development sandbox. Cargo audit/deny remain unavailable, so no fresh advisory/license-policy pass is inferred.

Full granted file/command/runtime-Skill/MCP enforcement remains an implementation gap: no native profile attests the common aggregate memory/process/CPU/storage bounds and complete descendant ownership. A harmless Seatbelt execution probe passed outside the development sandbox, but Seatbelt plus process groups alone does not establish that profile. macOS effects still fail closed. Optimized active-CLI/account setup needs isolated effective-user roots; remaining terminal loss/fault/picker/tmux, actual compiled-endpoint/default-trust TLS, Keychain setup/refresh/logout/cross-user scope, VoiceOver, Linux re-verification and distribution provenance remain separate. [Next steps](../../NEXT_STEPS.md#beta-2-and-broader-release) and [the active native plan](../../planning/native-macos/README.md) retain the unresolved isolation design and remaining evidence without a complete macOS support claim.

## 19. Decisions that supersede or sharpen earlier research

The existing CLI and architecture research is directionally correct but its “smallest verification matrix” can be misread as dozens of individual tests. This report sharpens it as follows:

- every matrix section becomes a scenario, table, or corpus owner rather than a test-count requirement;
- exact boundary and boundary + 1 checks share tables instead of separate functions;
- the deterministic fake proves the composition in-process, while the real binary owns OS channels and replay; no hidden fake CLI is added;
- each ignored live smoke should spawn the shipped executable so it is the genuinely complete Provider path;
- user-output goldens are few, explicit, and manually reviewed without a snapshot dependency;
- 100-run repeatability and latency loops are release measurements, not 100 permanent test cases;
- property/model/fuzz tools are triggered by state-space growth or escaped bugs rather than installed preemptively; and
- test deletion is part of every behavior change, so the suite can become smaller when a stronger proof replaces weaker examples.

## 20. Final recommendation

Begin implementation with the deterministic journey and make it display the exact human stdout/stderr and JSONL facts the user will see. Add the real `show` subprocess as soon as SQLite replay exists. Add compact adversarial tables only at the boundaries named by the architecture and security reports. Finish the core proof with deterministic cancellation and one real OS signal test per supported platform. Only then run each explicitly paid actual-binary smoke for an adapter Arany intends to claim.

This testing model keeps the suite small because each test is broad in composition but precise in ownership. It remains strong because storage, rendering, process behavior, hostile input, and cancellation are real; only the stochastic Provider is replaced in the default journey, at the exact seam the production architecture already requires.

## Primary sources

### Rust and Cargo

- [The Rust Programming Language: Writing Automated Tests](https://doc.rust-lang.org/book/ch11-00-testing.html)
- [The Rust Programming Language: Test Organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html)
- [The Rust Programming Language: ignored tests](https://doc.rust-lang.org/book/ch11-02-running-tests.html#ignoring-some-tests-unless-specifically-requested)
- [Cargo targets and integration tests](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#tests)
- [`cargo test`](https://doc.rust-lang.org/cargo/commands/cargo-test.html)
- [Cargo Continuous Integration guide](https://doc.rust-lang.org/cargo/guide/continuous-integration.html)
- [`std::process`](https://doc.rust-lang.org/std/process/)
- [`std::process::Command`](https://doc.rust-lang.org/std/process/struct.Command.html)
- [rustc platform support](https://doc.rust-lang.org/rustc/platform-support.html)

### Runtime and concurrency

- [Tokio: Graceful Shutdown](https://tokio.rs/tokio/topics/shutdown)
- [`tokio_util::sync::CancellationToken`](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html)
- [`tokio::time::pause`](https://docs.rs/tokio/latest/tokio/time/fn.pause.html)
- [`loom`](https://docs.rs/loom/latest/loom/)

### Persistence

- [SQLite: How SQLite Is Tested](https://www.sqlite.org/testing.html)
- [SQLite: Atomic Commit](https://www.sqlite.org/atomiccommit.html)
- [SQLite: How To Corrupt Your Database](https://www.sqlite.org/howtocorrupt.html)
- [SQLite database defensive configuration](https://www.sqlite.org/c3ref/c_dbconfig_defensive.html)

### Generative testing and snapshots

- [Proptest README](https://github.com/proptest-rs/proptest/blob/main/proptest/README.md)
- [Proptest: State Machine testing](https://proptest-rs.github.io/proptest/proptest/state-machine.html)
- [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz)
- [Kani Rust Verifier](https://model-checking.github.io/kani/)
- [Insta snapshot review workflow](https://insta.rs/docs/quickstart/)
- [Insta advanced snapshot lifecycle](https://insta.rs/docs/advanced/)

### Provider

- [OpenAI: Structured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs)
- [OpenAI: Your data](https://developers.openai.com/api/docs/guides/your-data)
- [OpenAI Responses API](https://developers.openai.com/api/reference/cli/resources/responses/methods/create)
