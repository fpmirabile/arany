# Rust foundation and minimum Engine contract

> **Session/team amendment — 2026-09-29:** [Beta Sessions, teams, terminal, providers, and license](./beta-sessions-teams-terminal-providers-and-license.md) supersedes the fixed four-call/two-child shape. Beta adds focused `session.rs`, Session-scoped Events, explicit resume/fork/compaction, and one primary plus ordered budget-bounded `0..N` direct children. A direct answer uses one Provider call; a team with `k` children uses `k + 2`. Native OpenAI/Anthropic remain; exact custom endpoint/model profiles require data-free conformance before Workspace disclosure.

Status: recommended foundation for the first executable demonstrator  
Scope: one Cargo package, one process, CLI only, durable Sessions, one accountable primary with ordered budget-bounded direct children, SQLite event journal, one scripted Provider, native OpenAI/Anthropic, and exact conformance-gated custom profiles
Research date: 2026-09-29

> **Security amendment — 2026-09-29:** [Harness security lessons](./harness-security-lessons-and-controls.md) and D-14 in the [decision register](./next-step-decision-register.md) supplement this contract. Startup pins private state and Workspace authority before repository input; SQLite opens no-follow in defensive mode with ownership/ACL and aggregate-growth checks; replay remains data-only; terminal output is inert; one aggregate Run budget is reserved before `RunStarted`; and dependency/build/Skill provenance is a release gate.

> **Multi-provider amendment — 2026-09-29:** [Beta multi-provider routing and adapters](./beta-multi-provider-routing-and-adapters.md) supersedes the OpenAI-only adapter, configuration, egress, provenance, and live-test details below. The non-streaming semantic `Delegate | Finish` contract remains, with one direct call or `N + 2` team calls; native OpenAI and Anthropic launch first, exact custom profiles require data-free conformance, and built-in OpenRouter/Z.AI remain gated.

> **Terminal amendment — 2026-09-29:** [Beta terminal interface and multi-agent feedback](./beta-terminal-interface-and-multi-agent-feedback.md) supersedes direct rendering in `main.rs` and the absence of terminal dependencies below. [Interactive CLI conventions](./interactive-cli-conventions-and-command-surface.md) defines the durable bare-entry Session and closed slash registry. Add pure `presentation.rs` and terminal-owned `terminal.rs`; Ratatui/Crossterm types never enter Engine interfaces. D-12/D-18/D-20 in the [decision register](./next-step-decision-register.md) are authoritative.

## Decision

Rust is a good fit for the harness core, but Rust alone will not make model generation faster. The useful latency win is to keep the local path short and predictable: one current-thread Tokio runtime, a reused HTTP client, a bounded response body, one coordinator loop, and one SQLite connection on a dedicated standard-library thread. The Engine should expose one real behavior seam, `Provider`; everything else remains concrete and private until a second implementation or an isolation requirement exists.

The core proof should have six source files:

- `main.rs`: parse CLI input, select Provider and presentation, install signal handling, and compose the process.
- `lib.rs`: public Engine command/data contract, private orchestration loop, event reducer, and handle-relative instruction/workspace input handling.
- `provider.rs`: the `Provider` trait plus scripted and private beta adapters.
- `store.rs`: a private SQLite actor that owns its connection and migration.
- `presentation.rs`: pure bounded `RunView -> PresentationModel` projection and deterministic linear rendering.
- `terminal.rs`: Ratatui widgets, Crossterm event/resize handling, and one RAII terminal owner.

After the Session, team, replay, and cancellation proof passes, the final beta OTLP slice adds one more private file:

- `telemetry.rs`: typed content-free span mapping, OTLP/HTTP-protobuf export, bounded batching, and shutdown.

This is deliberately a smaller physical design than the eventual product. The public data types make another client possible later without creating a daemon, client protocol, scheduler trait, repository layer, or renderer framework now.

## Why this runtime shape

### Tokio, current-thread

Use Tokio with the current-thread scheduler, but let synchronous `main` own its full lifetime:

```rust
fn main() -> ExitCode {
    let telemetry = Telemetry::from_config(/* validated config */);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("current-thread runtime");

    let exit = runtime.block_on(run_cli(/* &telemetry */));
    drop(runtime);
    telemetry.shutdown_with_timeout(/* at most 750 ms */);
    exit
}
```

The workload is predominantly socket and timer waiting. The demonstrator has at most two child provider calls in flight, and SQLite is kept off the runtime thread. Tokio documents the current-thread runtime as a scheduler that runs tasks on the current thread and enables I/O and time drivers separately; its bridging guide calls it a good fit when concurrency is limited. Explicit ownership also ensures the blocking OpenTelemetry batch processor shuts down only after `runtime.block_on(...)` returns. A multi-thread runtime would add scheduling and shutdown surface without making remote token generation faster ([Tokio runtime](https://docs.rs/tokio/latest/tokio/runtime/), [bridging guide](https://tokio.rs/tokio/topics/bridging)).

All queues are bounded. Tokio explicitly recommends bounded channels as backpressure and notes that an unbounded channel permits arbitrary buffering ([Tokio channels tutorial](https://tokio.rs/tokio/tutorial/channels)). Recommended initial capacities are private constants, not configuration:

| Queue | Capacity | Overflow behavior |
| --- | ---: | --- |
| Engine event/update path | 32 messages | producer awaits capacity; closed receiver cancels the Run |
| SQLite request path | 64 requests | Engine awaits capacity |

Do not enable `rt-multi-thread` or Tokio's `full` feature. Add the multi-thread scheduler only after profiling shows CPU work or runnable-task contention on the coordinator thread. CPU-heavy work should first be identified and bounded; it should not be hidden in an unbounded blocking pool.

### SQLite gets a dedicated thread, not `spawn_blocking`

Use `rusqlite` and exactly one dedicated `std::thread` that opens and exclusively owns one `rusqlite::Connection`. `Connection` is `Send` but not `Sync`, which matches transfer of ownership to one thread and rules out concurrent shared access without synchronization ([rusqlite `Connection`](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html)).

The integration is fixed as follows:

1. `Store::open` starts one named standard-library thread.
2. That thread opens the database, applies the migration and pragmas, then reports startup through a capacity-one `std::sync::mpsc::sync_channel`. `Store::open` waits on this short, pre-work handshake; it does not call a Tokio blocking API from the runtime thread.
3. It calls `blocking_recv` on a bounded `tokio::sync::mpsc` receiver.
4. Each private `StoreRequest` carries a `tokio::sync::oneshot::Sender` for its result.
5. Only the Engine coordinator submits `AppendBatch`; child tasks submit domain messages to the coordinator and never write the journal themselves.
6. The store assigns SQLite sequence numbers inside a transaction. The Engine applies and publishes those committed envelopes only after the reply succeeds.
7. Shutdown sends `Close`, waits for its reply, and joins the thread.

A request accepted by the store is not cancellation-sensitive: its transaction runs to commit or rollback. This produces a crisp invariant—an event is visible only after a completed transaction—and avoids an ambiguous half-cancelled write.

Do not use `tokio::task::spawn_blocking` for the connection. Tokio documents that a blocking task cannot be aborted once it has started and that the blocking queue can grow very large; those semantics are a poor ownership and shutdown contract for the journal ([Tokio `spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)). Do not use SQLx or a pool in this slice. SQLx's pool is useful when multiple asynchronous connections and database portability are requirements, neither of which applies to a single local writer ([SQLx `SqlitePool`](https://docs.rs/sqlx/latest/sqlx/type.SqlitePool.html)).

Use rusqlite's `bundled` feature. Its project recommends bundled SQLite for applications that control their database deployment, and it avoids relying on an arbitrary system SQLite build ([rusqlite repository](https://github.com/rusqlite/rusqlite), [rusqlite manifest](https://github.com/rusqlite/rusqlite/blob/master/Cargo.toml)).

## Minimum execution protocol

The demonstrator is a bounded workflow, not an open-ended autonomous loop:

1. The primary AgentRun performs planning and may return a direct `Finish` or `Delegate` with the number of child objectives admitted by the pinned `single|auto|team` policy.
2. For a team Run, the Engine creates the admitted ordered collection of child `AgentRun`s and invokes at most the pinned concurrency limit at once.
3. Each child performs `ChildWork` and must return `Finish`. A child cannot delegate.
4. The Engine waits for every admitted child. If any terminally fails, the Run fails; it does not synthesize from partial results.
5. The primary performs synthesis using all ordered child results and must return `Finish`.
6. The Engine persists the primary terminal Event and then the Run terminal Event.

A direct success uses one Provider invocation; a team success with `N` children uses `N + 2`. This proves bounded delegation, concurrency, join, replay, cancellation, and feedback without introducing recursion, work stealing, leases, daemon recovery, or an unbounded model-driven loop.

The semantic outcomes are deliberately narrow:

```rust
pub enum ProviderOutcome {
    Delegate(Delegate),
    Finish(Finish),
}

pub struct Delegate {
    pub children: [ChildObjective; 2],
}

pub struct ChildObjective {
    pub objective: String,
}

pub struct Finish {
    pub summary: String,
    pub result: String,
}
```

The Engine validates phase and outcome together. `RootPlan + Delegate`, `ChildWork + Finish`, and `RootSynthesis + Finish` are the only valid pairs. It rejects empty objectives, duplicate objectives, oversized strings, a `Finish` during planning, and a `Delegate` from a child or synthesis call. These are domain validation failures, not provider transport failures.

Use the Responses API Structured Outputs mechanism for this boundary. OpenAI documents strict JSON Schema output through `text.format`; strict schemas support only a subset of JSON Schema, require all object fields to be listed as required, and require `additionalProperties: false` ([Structured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs), [Responses create reference](https://developers.openai.com/api/reference/resources/responses/methods/create)). Keep the two small schemas adjacent to the Rust outcome types and protect them with golden serialization tests. Do not add a schema-generation dependency merely to avoid maintaining these two fixtures.

## Engine interface

The CLI calls one concrete Engine. Engine itself is not a trait.

```rust
pub struct Engine<P> {
    // Provider, store actor, and private limits.
    provider: P,
}

pub struct RunRequest {
    pub objective: String,
    // An authority-selection path, pinned to a Dir before any input resolution.
    pub workspace: PathBuf,
    pub includes: Vec<PathBuf>,
}

pub struct RunCancellation {
    // Private fields; the CLI retains handles that request and force cancellation.
}

impl<P: Provider> Engine<P> {
    pub fn open(path: PathBuf, provider: P) -> Result<Self, OpenError>;

    pub async fn run(
        &self,
        request: RunRequest,
        updates: mpsc::Sender<RunUpdate>,
        cancellation: RunCancellation,
    ) -> Result<RunView, EngineError>;

    pub async fn show(&self, run_id: RunId) -> Result<RunView, EngineError>;

    pub async fn close(self) -> Result<(), EngineError>;
}

pub struct RunUpdate {
    pub event: EventEnvelope,
    pub view: RunView,
}
```

The Engine persists an Event, applies that same committed Event to the reducer, and only then sends `RunUpdate`. Interactive, linear, and JSONL presentations therefore observe journal truth rather than optimistic in-memory state. A slow presentation backpressures through the bounded update channel instead of consuming unlimited memory. A closed receiver is an output failure: the Engine cancels and terminalizes the Run instead of silently spending Provider budget with no attached CLI.

`show` replays the run and invokes the same reducer. There is no separately maintained projection table in the demonstrator. A failure to replay is surfaced; the Engine never skips an event it does not understand. `close` sends the store's terminal command, awaits its acknowledgement, and joins the SQLite thread. Dropping without `close` closes the last request sender and joins as a defensive fallback, but the CLI must use the explicit path so shutdown failures are reportable.

### Workspace input is handle-relative and no-follow

The workspace path in `RunRequest` is an authority-selection input, not a string prefix used for later access. At the beginning of `run`, open it once as a `cap_std::fs::Dir` and retain that directory handle for the whole input-loading phase. All instruction and `--include` reads after that point are relative to the retained handle. `cap-std` is explicitly capability-oriented: `Dir` operations are relative to an open directory and prevent `..`, absolute paths, and symlinks from escaping the selected tree ([cap-std filesystem API](https://docs.rs/cap-std/latest/cap_std/fs/), [cap-std capability model](https://github.com/bytecodealliance/cap-std)).

The harness policy is stricter than containment: it rejects every symlink or Windows reparse component even when the target would remain inside the workspace. Implement one private `open_workspace_file` function with this exact algorithm:

1. Reject an empty path and every component except `Component::Normal`; absolute prefixes, roots, `.`, and `..` never reach the filesystem.
2. Start from a clone of the pinned workspace `Dir`.
3. For each parent component, call `cap_fs_ext::DirExt::open_dir_nofollow` with that single component and retain the returned `Dir`. The extension documents that it fails when the named component is a symlink ([`DirExt::open_dir_nofollow`](https://docs.rs/cap-fs-ext/latest/cap_fs_ext/trait.DirExt.html#tymethod.open_dir_nofollow)). Never pass a multi-component remainder to this operation.
4. Open the final single component with `cap_std::fs::OpenOptions`, `read(true)`, and `cap_fs_ext::OpenOptionsFollowExt::follow(FollowSymlinks::No)`. The extension controls following of the final component ([`OpenOptionsFollowExt`](https://docs.rs/cap-fs-ext/latest/cap_fs_ext/struct.OpenOptions.html)).
5. Validate metadata from the opened handle, not from a pathname. It must be a regular file. On Windows, also reject the handle when `MetadataExt::file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0`; import that constant from the target-specific `windows-sys` dependency.
6. Check the handle's length before allocation, then read through the already-open handle into a bounded buffer and reject growth beyond the limit. Compute SHA-256 over those exact bytes and decode UTF-8 only after the byte bound succeeds.

Each lookup is therefore bound to the previous directory handle, and the checked object is the object that was opened. There is no `canonicalize`, `Path::starts_with`, pre-open `symlink_metadata`, or check-then-open window. `cap-fs-ext` supplies the missing portable no-follow directory and file options; a direct `rustix` dependency would solve only the Unix half and would require a separate hand-written Windows handle implementation. The cap-std project supports Linux, macOS, FreeBSD, and Windows and internally uses `openat2` where available or component-wise resolution elsewhere ([cap-std implementation notes](https://github.com/bytecodealliance/cap-std#how-fast-is-it)).

Opening the workspace root is the single ambient-authority grant. The CLI must make that grant explicit to the user; after the `Dir` is pinned, private Engine code must not reconstruct an ambient path from it. If the product later requires rejecting symlinks in the user-supplied workspace-root path itself, add a separate root-acquisition contract that starts from an already-open parent/root capability. Do not pretend that canonicalizing the root provides that guarantee.

This input loader remains a concrete private Engine helper, not a `Workspace` trait or a new module. Its synchronous reads happen before provider tasks are launched. If profiling later shows that large allowed inputs stall active tasks, move the whole bounded load onto one owned input thread; do not scatter pathname reads into Tokio's blocking pool.

### Provider is the only Engine behavior seam

There are two real provider implementations: a deterministic scripted fake and OpenAI. That justifies one trait. Use a static generic rather than `dyn Provider` and do not add `async-trait`:

```rust
pub trait Provider: Send + Sync + 'static {
    fn invoke(
        &self,
        request: ProviderRequest,
    ) -> impl Future<Output = Result<ProviderResponse, ProviderError>> + Send;
}
```

Rust stabilized `async fn` and return-position `impl Trait` in traits in 1.75, but the resulting traits are not dyn-compatible without an additional erasure strategy. Static dispatch avoids boxing, macros, and an object-safety design that the first process does not need ([Rust announcement](https://blog.rust-lang.org/2023/12/21/async-fn-rpit-in-traits/), [Rust Reference: traits](https://doc.rust-lang.org/reference/items/traits.html), [The Rust Book: trait objects](https://doc.rust-lang.org/book/ch18-02-trait-objects.html)).

The request is semantic, not an OpenAI request body:

```rust
pub struct ProviderRequest {
    pub agent_run_id: AgentRunId,
    pub phase: AgentPhase,
    pub instructions: String,
    pub objective: String,
    pub documents: Vec<InputDocument>,
    pub child_results: Vec<ChildResult>,
    pub max_output_tokens: u32,
}

pub enum AgentPhase {
    RootPlan,
    ChildWork,
    RootSynthesis,
}

pub struct ProviderResponse {
    pub provider_response_id: Option<String>,
    pub outcome: ProviderOutcome,
    pub usage: Option<TokenUsage>,
}
```

The Engine owns instruction resolution, include validation, workflow phase, output bounds, and child result ordering. The provider owns authentication, the model identifier, HTTP translation, bounded body parsing, and translation from provider errors into the types above. OpenAI request/response structs stay private to its adapter.

`model` and `max_output_tokens` must be explicit in the adapter/request rather than inherited from a mutable global default. Resolve the model as non-empty `--model`, then non-empty `ARANY_OPENAI_MODEL`, otherwise fail before starting a Run; do not compile a default that can silently change cost or capability. This named precedence does not justify a general configuration framework. Provider capabilities, model discovery, routing, and fallbacks wait for a second provider or a demonstrated requirement.

## Provider response contract: no token stream in version 1

Version 1 requests one non-streaming Responses result per provider call. Its only provider semantics are `Delegate` and `Finish`, and the CLI deliberately exposes lifecycle Events rather than partial tokens. OpenAI streaming would reduce time to first token, but without a user-visible incremental behavior it would add an SSE state machine, forward-compatibility policy, channel, dependency, and cancellation surface without changing the accepted outcome ([OpenAI streaming guide](https://developers.openai.com/api/docs/guides/streaming-responses)).

There is therefore no `ProviderEvent`, provider progress channel, or unknown-provider-event retention contract in this slice. Unknown SSE events cannot occur because `stream` is false. The JSON response envelope is forward-compatible at the provider-wire layer: ordinary unknown top-level fields are ignored, while the strict `Delegate|Finish` payload rejects unknown or missing semantic fields with Serde's `deny_unknown_fields` ([Serde container attributes](https://serde.rs/container-attrs.html)). Never persist the raw provider response.

Streaming becomes earned when the CLI or another real client must display partial model text/reasoning, or when measured time-to-first-useful-output is a product requirement. That change must define, together, a bounded SSE parser, known-event normalization, unknown-event preservation/redaction, slow-consumer behavior, partial-output retry semantics, and cancellation tests. Until then, removing `futures-util`, `sse-core`, and `serde_json/raw_value` is the smaller and safer contract.

## Direct OpenAI HTTP adapter

OpenAI lists official SDKs for several languages but not Rust; its library page lists Rust under community libraries. Direct HTTPS therefore avoids binding the Engine to an unofficial SDK's data model ([OpenAI libraries](https://developers.openai.com/api/docs/libraries)).

Use one reused `reqwest::Client`. Reqwest documents that `Client` holds a connection pool and should be reused rather than constructed per request. It is the right level for request building, rustls, incremental body reads, and timeouts; direct Hyper would require assembling lower-level connection, body, TLS, and pooling components ourselves ([reqwest](https://docs.rs/reqwest/latest/reqwest/), [reqwest TLS](https://docs.rs/reqwest/latest/reqwest/tls/), [Hyper client guide](https://hyper.rs/guides/1/client/basic/)).

Build the client once with these fixed properties:

- `https_only(true)` and fixed origin `https://api.openai.com/v1/responses`;
- rustls selected explicitly, with reqwest default features disabled;
- redirects disabled;
- a bounded connect timeout and whole-invocation deadline owned by the Engine;
- system proxy discovery disabled in the first slice;
- no cookies and no automatic compression features;
- `retry(reqwest::retry::never())` so transport behavior cannot create a hidden duplicate request;
- `bearer_auth` for the key. Reqwest marks the resulting `Authorization` header as sensitive, and `HeaderValue` debug output masks sensitive values ([reqwest `RequestBuilder`](https://docs.rs/reqwest/latest/src/reqwest/async_impl/request.rs.html), [`HeaderValue`](https://docs.rs/http/latest/http/header/struct.HeaderValue.html)).

Send `stream: false`, `store: false`, `truncation: "disabled"`, the strict `text.format` schema, and `max_output_tokens: 4096`. Responses are stored by default unless storage is disabled, so `store: false` is required for the harness's default data boundary ([Responses create reference](https://developers.openai.com/api/reference/resources/responses/methods/create), [OpenAI data controls](https://developers.openai.com/api/docs/guides/your-data)). Do not enable provider tools, background execution, or provider-side conversation state in the demonstrator.

Read the HTTP response with repeated `Response::chunk()` calls into a buffer capped at 1 MiB; fail immediately on byte 1 MiB + 1. `chunk()` is available without reqwest's optional `stream` feature ([reqwest `Response::chunk`](https://docs.rs/reqwest/latest/reqwest/struct.Response.html#method.chunk)). `Content-Length` may reject an obviously oversized response early but is not authoritative. Do not call `bytes()`, `text()`, or `json()` on an unbounded body. After the byte bound succeeds, deserialize the typed envelope and strict semantic outcome.

Capture OpenAI's request ID in a redacted provider error and accept an Engine-generated client request ID for correlation. OpenAI documents `x-request-id` and `X-Client-Request-Id` for troubleshooting ([request IDs](https://platform.openai.com/docs/api-reference/backward-compatibility)). Neither identifier is a domain event ID.

## IDs and time

Use typed UUIDv7 identifiers:

```rust
pub struct RunId(Uuid);
pub struct AgentRunId(Uuid);
pub struct EventSequence(i64);
```

UUIDv7 is time-ordered and `Uuid::now_v7` provides monotonic ordering within a process, while retaining the standardized UUID representation ([uuid crate](https://docs.rs/uuid/latest/uuid/), [RFC 9562](https://www.rfc-editor.org/rfc/rfc9562)). Treat IDs as opaque identity only. SQLite `sequence` is the sole authoritative event order, and persisted event timestamps remain the audit time. ULID would add a different identifier format without solving a requirement UUIDv7 does not already meet.

Do not add an ID-generator trait. Tests should assert identity relationships and ordering by event sequence, not exact random values.

Use the standard library for time:

- `SystemTime` is converted once to signed Unix milliseconds when creating a persisted event. Wall time can move backward and is never used to order events.
- `Instant` measures deadlines, elapsed provider latency, graceful shutdown, and retry budgets. It is monotonic and intentionally cannot be serialized.
- SQLite `sequence` orders events.

The standard library documents exactly this distinction: `Instant` is nondecreasing and opaque, while `SystemTime` reflects the system clock and can move backward ([`Instant`](https://doc.rust-lang.org/std/time/struct.Instant.html), [`SystemTime`](https://doc.rust-lang.org/std/time/struct.SystemTime.html)). Do not add `chrono`, `time`, or `jiff` until a user-visible RFC 3339 timestamp, calendar arithmetic, or time-zone conversion is required. Rendering raw Unix milliseconds is sufficient for the first JSONL contract.

## State location

Resolve the database parent with this fixed precedence: `--state-dir`, then non-empty `ARANY_STATE_DIR`, then the platform default. An explicitly present but empty override is invalid rather than a signal to fall through. The filename is always `events.sqlite3`. Use `directories::ProjectDirs::from("dev", "Arany", "arany")`; on Linux choose `state_dir()`, and on macOS/Windows choose `data_local_dir()`. `ProjectDirs` maps these methods to XDG state, macOS Application Support, and Windows Local AppData conventions and may return `None` when the platform cannot identify a usable user directory ([directories crate](https://docs.rs/crate/directories/latest), [`ProjectDirs`](https://docs.rs/directories/latest/directories/struct.ProjectDirs.html)). Treat that absence as a configuration error rather than falling back to the current directory.

The override replaces only the parent directory. Resolve it before repository input and require it to remain outside the Workspace on a verified local filesystem. Open it no-follow and verify current-user ownership plus private access: directory `0700` and database/sidecars `0600` on Unix, an equivalent effective ACL on Windows. Refuse links/reparse points, unexpected types, owners, or access; do not silently repair broad permissions. The resolver is one private function in `store.rs`, not a state-location trait or general configuration system.

## Journal schema and versioning

Use one strict events table and one run-ordering index:

```sql
PRAGMA journal_mode = DELETE;
PRAGMA synchronous = EXTRA;
PRAGMA trusted_schema = OFF;
PRAGMA busy_timeout = 250;
PRAGMA page_size = 4096;
PRAGMA max_page_count = 65536;

CREATE TABLE events (
    sequence       INTEGER PRIMARY KEY,
    run_id         TEXT    NOT NULL,
    agent_run_id   TEXT,
    kind           TEXT    NOT NULL,
    event_version  INTEGER NOT NULL,
    payload         TEXT    NOT NULL CHECK (json_valid(payload)),
    created_at_ms  INTEGER NOT NULL
) STRICT;

CREATE INDEX events_by_run ON events(run_id, sequence);
PRAGMA user_version = 1;
```

`INTEGER PRIMARY KEY` already aliases the SQLite rowid. Do not use `AUTOINCREMENT`: SQLite documents that it adds CPU, memory, disk, and I/O overhead and is normally unnecessary. The harness never deletes event rows, so rowid reuse is not a concern ([SQLite AUTOINCREMENT](https://www.sqlite.org/autoinc.html)).

Open read-write/create with `SQLITE_OPEN_NOFOLLOW` and enable `SQLITE_DBCONFIG_DEFENSIVE` before migrations or statements. Rollback-journal `DELETE` mode is the smaller policy because the active CLI receives committed Events directly from the Engine and `show` is not required to poll during a live Run. `synchronous=EXTRA` includes the `FULL` sync behavior and also syncs the containing directory after unlinking a rollback journal, which SQLite documents as extra durability for this mode ([SQLite synchronous pragma](https://sqlite.org/pragma.html#pragma_synchronous), [SQLite 3.11.1 release note](https://sqlite.org/releaselog/3_11_1.html)). `trusted_schema=OFF` follows SQLite's guidance for applications that do not need schema objects to invoke application-defined functions ([SQLite security guidance](https://sqlite.org/security.html), [`trusted_schema`](https://sqlite.org/pragma.html#pragma_trusted_schema)).

Check the returned `journal_mode`, numeric synchronous value (`3` for `EXTRA`), defensive configuration, page size, and page cap; do not assume configuration took effect. Require 4 MiB of database headroom before admitting a Run and never prune canonical Events implicitly. Configure a 250 ms busy timeout on the connection and surface expiry as `StoreError::Busy`. WAL is deferred until a second live reader is required or benchmarks show rollback-journal commits miss the durable append budget; that change also requires checkpoint and backup semantics ([SQLite WAL](https://sqlite.org/wal.html)).

There are two independent version axes:

- `PRAGMA user_version` versions the physical SQLite schema. `Store::open` migrates supported older versions in a transaction and rejects a database with a newer version.
- `event_version` versions the payload for one event kind. Dispatch on `(kind, event_version)` before deserialization. Unsupported pairs return `StoreError::UnsupportedEventVersion`; they are never skipped.

The JSONL form carries the same kind and event version. Persisted event readers should tolerate additive fields within a supported version, while required fields and semantic invariants remain validated. A breaking rename, meaning change, or required-field change gets a new event version and an explicit upgrader. No migration framework is needed for version 1; one private migration function is enough.

The database transaction is the publication boundary. Before append, serialize the typed payload and reject anything over 64 KiB; never truncate canonical state. On append failure, no reducer state or UI update advances. On replay, every stored event is checked by the same reducer used live.

## Event envelope and reducer

The stable Engine-to-client contract is a domain envelope, not an OpenAI event:

```rust
pub struct EventEnvelope {
    pub sequence: EventSequence,
    pub run_id: RunId,
    pub agent_run_id: Option<AgentRunId>,
    pub event_version: u16,
    pub created_at_ms: i64,
    pub event: Event,
}

pub enum Event {
    RunStarted(RunStarted),
    AgentSpawned(AgentSpawned),
    AgentUpdated(AgentUpdated),
    AgentFinished(AgentFinished),
    RunFinished(RunFinished),
}
```

The storage codec writes `Event::kind()` to `kind` and the variant body to `payload`; JSONL uses the same adjacent representation. The five domain event kinds are sufficient:

- `RunStarted`: objective, root agent identity, resolved instruction path and SHA-256 digest, and included-file manifests/digests.
- `AgentSpawned`: child identity, parent identity, role, objective, and deterministic sibling position.
- `AgentUpdated`: state transition and bounded human-readable summary.
- `AgentFinished`: terminal agent state, result or a redacted failure descriptor, usage, and provider request ID if present.
- `RunFinished`: terminal run state. The successful final result remains on the root `AgentFinished` event and is not duplicated.

The reducer rejects, at minimum, a sequence gap or regression; an Event before `RunStarted`; a duplicate start; an update or finish for an unknown AgentRun; an illegal state transition; a second terminal Event; children above the pinned limit or hard ceiling; a child with a non-primary parent; primary success before every admitted child succeeds; and Run success before primary success. Reducer failure while replaying is `StoreError::CorruptJournal`, not a best-effort partial view.

## RunView

`RunView` is a small reconstructed read model suitable for both terminal and future client adapters:

```rust
pub struct RunView {
    pub run_id: RunId,
    pub objective: String,
    pub state: RunState,
    pub agents: Vec<AgentRunView>,
    pub final_result: Option<String>,
    pub events: Vec<EventEnvelope>,
}

pub struct AgentRunView {
    pub agent_run_id: AgentRunId,
    pub parent_agent_run_id: Option<AgentRunId>,
    pub role: AgentRole,
    pub objective: String,
    pub state: AgentState,
    pub summary: Option<String>,
    pub result: Option<String>,
}

pub enum RunState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

pub enum AgentState {
    Running,
    WaitingForChildren,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}
```

Agents are ordered by their spawn event sequence, not by completion time or map iteration. During an active Run, nonterminal state is `Running` or `WaitingForChildren`. Read-only `show` has no live owner; if replay ends without `RunFinished`, it maps those nonterminal presentation states to `Interrupted` without inventing or appending an Event. For this bounded proof, `RunView.events` contains all stored envelopes in sequence order. `show --jsonl` serializes that vector directly, so JSONL is neither a second projection nor a second store query; human rendering uses the reduced fields from the same replay. Replace the vector with an explicit paged history only after measured event volume makes this bounded representation inadequate. Private reducer state may contain indexes and sequence bookkeeping that are not part of the public view.

## Cancellation and task ownership

`RunCancellation` contains one graceful `CancellationToken` and one force token behind narrow `request()`/`force()` handles retained by `main.rs`. The Engine owns propagation and task handles:

- The first Ctrl-C requests graceful cancellation. The second Ctrl-C forces cleanup.
- Each agent call uses a child token. Root cancellation reaches every child; child cancellation does not cancel the root token. Tokio documents these parent/child semantics ([`CancellationToken`](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html)).
- Each provider task uses cancellation-prioritized `tokio::select!` between the invocation, its 120-second call deadline, and graceful cancellation. The supervisor also owns a 300-second whole-Run deadline. Its expiry cancels the task tree and records a distinct `TimedOut` terminal outcome rather than misreporting user cancellation. Dropping the reqwest response ends local consumption; it does not prove the remote provider stopped computing.
- Child tasks are owned by the supervisor's `JoinSet`; tasks return typed outcomes and never append terminal Events themselves.
- After graceful cancellation, the supervisor waits for acknowledgements. A second Ctrl-C or a fixed two-second cleanup deadline calls `JoinSet::shutdown()`, which aborts and reaps all remaining tasks ([`JoinSet`](https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html)).
- Once the set is empty, the supervisor appends cancelled `AgentFinished` Events in stable agent order and then `RunFinished::Cancelled`. Forced cleanup changes how tasks stop, not the final domain state.
- The SQLite actor finishes any accepted transaction. Orderly Engine shutdown sends `Close` and joins it before `main` returns.

Installing Tokio's Ctrl-C handler changes the process's default Ctrl-C behavior for the lifetime of the process, so initialization failure and both signal phases must be handled deliberately ([Tokio `ctrl_c`](https://docs.rs/tokio/latest/tokio/signal/fn.ctrl_c.html)). Do not call `process::exit` on the second signal: it skips destructors and could strand the store contract. Return exit code 130 only after task reaping and best-effort terminal writes.

Cancellation is a persisted terminal domain outcome, not an `EngineError`. If terminalization fails, return the store error rather than claiming clean cancellation. `EngineError` means the requested operation could not be represented faithfully, for example store loss, output detachment, or an invariant violation.

## Error taxonomy

Keep errors typed and messages safe. `thiserror` can derive the repetitive `Display`, `Error`, and `source` implementations without changing public behavior ([thiserror](https://docs.rs/thiserror/latest/thiserror/)). Distinguish operation errors from terminal run failures. An operation error means the Engine cannot faithfully return a view; a provider or semantic-output failure after `RunStarted` is journaled and returned as `Ok(RunView { state: Failed, .. })`.

```rust
pub enum EngineError {
    Input(InputError),
    Store(StoreError),
    UpdateSinkClosed,
    Invariant(InvariantError),
}

pub enum RunFailure {
    Provider(ProviderFailure),
    InvalidOutcome(InvalidOutcome),
}

pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub status: Option<u16>,
    pub request_id: Option<String>,
    pub retry_after: Option<Duration>,
    pub request_may_have_been_sent: bool,
    pub safe_message: String,
}

pub enum ProviderErrorKind {
    InvalidRequest,
    Authentication,
    Permission,
    RateLimited,
    Quota,
    Unavailable,
    TransportBeforeSend,
    TransportAfterSend,
    Protocol,
    Refusal,
    Incomplete,
    Deadline,
}
```

Store errors distinguish open, migration, busy, append, replay, corruption, unsupported schema, unsupported event version, and actor termination. Input errors distinguish containment, path type, size, encoding, and read failure. CLI parse and render errors remain in `main.rs`; they are not Engine domain errors.

`ProviderError` is the adapter-to-Engine diagnostic used to construct a bounded `ProviderFailure` event payload. `ProviderFailure` keeps only the stable kind, HTTP status when safe, request ID, whether the request may have been sent, and a safe message; it never embeds the source error or response body. Because version 1 accepts no semantic output until the complete bounded body passes strict parsing, it has no "partial semantic output" state. If the Engine cannot persist that terminal failure, `run` returns the `StoreError` instead of pretending a failed view is durable. Configuration needed to construct the OpenAI adapter belongs to `OpenError`, before a run starts.

Do not put raw HTTP bodies, request headers, prompts, file contents, or raw provider events in `safe_message`. Persist a bounded stable code and safe summary for failed agent events. Retain a source error in memory where useful, but custom `Debug` implementations must not disclose secrets or raw bodies.

OpenAI distinguishes authentication, permission, rate limit, quota, server, and connection failures; incomplete Responses also need explicit handling because output-token limits can be consumed before visible output is produced ([OpenAI error codes](https://developers.openai.com/api/docs/guides/error-codes), [reasoning guidance](https://developers.openai.com/api/docs/guides/reasoning)).

## Retry boundary

There is no automatic retry in the demonstrator. Configure reqwest with `retry::never()` and surface a typed failure. This keeps the admitted call budget, lifecycle event sequence, cancellation, and cost behavior intelligible.

Reqwest has its own retry layer and documents a default safe protocol retry policy, so disabling it is necessary if the Engine is to be the only future retry owner ([reqwest retry module](https://docs.rs/reqwest/latest/reqwest/retry/)). OpenAI recommends exponential backoff with jitter for transient rate limits and notes that failed requests still count toward rate limits ([OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits)). Those recommendations support a later bounded Engine policy, not nested provider and HTTP retries.

If retry is introduced later, all of these conditions must hold:

1. A measured transient failure rate justifies the complexity.
2. The Engine owns one total deadline and one attempt budget.
3. Authentication, permission, quota, invalid request, refusal, protocol, and invariant failures are never retried.
4. Rate-limit and unavailable failures honor bounded server hints, then exponential backoff with jitter.
5. Automatic retry is allowed only when the adapter can prove the request was not sent. An ambiguous post-send transport failure requires provider idempotency/resume support or fails; receiving no semantic outcome does not prove the provider did no work.
6. Every attempt and final exhaustion becomes visible diagnostic metadata, without persisting secret response bodies.

## Secret and redaction boundary

The initial secret surface is one environment variable, `OPENAI_API_KEY`. Read it once into `secrecy::SecretString`, expose it only while constructing the sensitive Authorization header, and do not enable secrecy's Serde feature. Never accept it as a CLI argument, persist it, place it in provider requests, or include it in errors. OpenAI recommends environment variables or a secret-management service rather than embedding keys in source ([OpenAI production practices](https://developers.openai.com/api/docs/guides/production-best-practices)). Secrecy makes exposure explicit, redacts formatting, and zeroizes its owned secret on drop, reducing the most likely accidental disclosure paths ([secrecy](https://docs.rs/secrecy/latest/secrecy/)).

Do not add `dotenv`, a config-file parser, a direct `zeroize` dependency, or a keychain abstraction in the first slice. `SecretString` cannot erase copies inside environment and TLS machinery, so this is defense against accidental application exposure rather than a whole-process erasure guarantee. Add platform credential storage only with a real login/persisted-credential workflow.

The data boundaries are:

- The journal intentionally stores objectives, agent summaries, and final results because they are product state.
- Included file bodies are sent only for the invocation and are not journaled; the start event stores validated relative paths, sizes, and SHA-256 digests of the exact bytes read.
- Raw provider responses and API error bodies are bounded to 1 MiB in memory, reduced to typed safe fields, and discarded.
- `store: false` prevents default provider-side Response storage.
- No general tracing/logging façade is present. The runtime-opt-in OTLP module exports only typed allowlisted spans; it never mirrors journal Events or arbitrary diagnostic fields. JSONL is a deliberate user output mode, not an internal dump.
- The OpenAI origin is fixed; redirects, arbitrary base URLs, cookies, and ambient proxies are absent. A custom endpoint becomes a separate security decision.

## What stays private

Only `Provider` is a trait. These remain concrete private implementation:

| Concern | Initial form | Trigger for a seam |
| --- | --- | --- |
| SQLite | `Store` actor in `store.rs` | second durable backend or separate privilege/process boundary |
| Event reduction | private pure functions in `lib.rs` | reused by a second package or independently versioned protocol |
| Scheduling | bounded primary plus flat `0..N` direct-child coordinator | recursive or queued work with a real scheduling policy |
| Cancellation | Engine-owned token tree | remote execution with acknowledged cancellation |
| Retry | none | measured transient failures and an idempotency/resume contract |
| Instruction resolution | private Engine function | a second resolution strategy with materially different policy |
| Workspace input | pinned `cap_std::fs::Dir` plus private component-wise no-follow loader | remote/blob workspace or separate privilege boundary |
| State location | private `ProjectDirs`/override resolver in `store.rs` | a second state profile or system-wide installation |
| Time and IDs | `SystemTime`, `Instant`, `Uuid::now_v7` | deterministic simulation that cannot test through persisted fixtures |
| HTTP | private reqwest client in OpenAI adapter | a second transport for the same provider or test need not met by scripted Provider |
| Presentation | pure semantic/linear functions in `presentation.rs`; Ratatui/Crossterm ownership in `terminal.rs` | second independently consumed Client or measured need for a renderer protocol |
| Configuration | named CLI/environment values with fixed per-setting precedence | multiple layered sources or a configuration file with merge semantics |
| Telemetry | private concrete `Disabled | Otlp` module with typed safe fields | a second independently owned instrumentation implementation with genuinely different behavior |

Do not create `Engine`, `Store`, `Repository`, `Scheduler`, `Memory`, `Clock`, `IdGenerator`, `Renderer`, `RetryPolicy`, `HttpClient`, `InstructionResolver`, `Workspace`, or `Telemetry` traits. Two presentation files do not justify a renderer trait. Do not split the package into crates. A future web client first justifies a daemon/client protocol; it does not justify one in advance.

## Verification gates for implementation

The foundation is accepted only after these tests pass:

### Contract and orchestration

- Scripted direct and `team(2)` successes produce the admitted call counts, overlapping child calls where applicable, and replay-identical `SessionView`/`RunView` values.
- Invalid phase/outcome pairs and delegation above the pinned child limit or hard ceiling are rejected.
- One child failure prevents synthesis and produces deterministic failed terminal events.
- Slow `RunUpdate` consumers apply backpressure without unbounded growth or changing journal results.

### Persistence

- Crash/reopen after every committed event prefix either replays a valid prefix or a valid terminal run; no committed row is invisible to the reducer.
- An append failure produces no `RunUpdate` and no reducer advance.
- Unknown schema and event versions fail closed.
- Corrupt JSON, sequence/order violations, and illegal transitions fail replay.
- Startup verifies the returned `journal_mode=delete` and numeric `synchronous=3`; a busy external writer expires after 250 ms with `StoreError::Busy`.
- Store shutdown joins its thread; a dropped reply receiver does not interrupt the accepted transaction.

### Provider and HTTP

- The adapter always sends `stream: false`; there is no provider progress channel or SSE parser in the dependency graph.
- Response bodies split at every tested chunk boundary accept exactly 1 MiB and reject byte 1 MiB + 1. Missing, false, or changing `Content-Length` cannot bypass the streaming read limit.
- Structured output accepts each valid semantic result and rejects unknown/missing semantic fields, wrong phase, oversize strings, refusal, incomplete response, and completion without the expected terminal result. Additive unknown fields in the outer provider envelope are tolerated.
- Reqwest redirects, inherited proxies, and retries are disabled, the client is reused, and only the fixed HTTPS origin is reachable.
- Debug/error snapshots contain no authorization value, prompt, included-file content, or raw response body.

### Cancellation and resources

- Cancellation before a call, during both concurrent child calls, during a bounded response read, while waiting on a full update channel, and during SQLite append reaches a deterministic terminal state.
- No async task or SQLite thread remains after shutdown.
- Store and update channels remain at their declared capacities under stress.
- A second Ctrl-C and the two-second cleanup deadline both abort and reap the `JoinSet`, then let the supervisor best-effort persist stable cancellation events before exit 130; neither path calls `process::exit`.

### Final beta telemetry slice

- Disabled telemetry starts no worker and performs no network I/O; its fixed scripted Run overhead stays within 1% median and 100 µs absolute.
- A direct Run and a `team(2)` Run produce the topology-derived span counts with exact parentage, overlapping children where applicable, one Provider span per admitted call, safe Session correlation, and no trace from `arany show`.
- A canary corpus spanning every objective, instruction, include, result, path, Event payload, provider body/header, and credential is absent from in-memory spans, encoded protobuf, diagnostics, and snapshots.
- A bounded loopback receiver proves OTLP/HTTP protobuf method, path, content type, status/retry classification, redirect rejection, proxy isolation, and request size limits.
- Queue saturation drops telemetry without blocking Engine state, while queued telemetry remains within the 4 MiB target.
- Slow or unavailable export cannot stall the current-thread runtime; Engine/store cleanup precedes a post-runtime telemetry shutdown bounded to 750 ms.

### Workspace input

- Reject absolute, prefix, root, `.`, `..`, empty, and non-regular include paths.
- Test a symlink in every parent position and in the final position. The read fails even when the link target is inside the workspace.
- Race replacement of every component between repeated opens; the retained handle must either read the object it opened or fail, never escape through a check-then-open path.
- On Windows, test file symlinks, directory symlinks, junctions, and another available reparse-point type; every opened handle with `FILE_ATTRIBUTE_REPARSE_POINT` is rejected.
- Test a file that grows after metadata inspection and prove the streaming byte ceiling still stops the read.

### Dependency gates

- Run `cargo tree -e features` and verify that Tokio multi-thread runtime and macro features, reqwest's `stream` feature, `sse-core`, native TLS, cookies, compression, SQLx, OpenTelemetry gRPC/Tonic, HTTP/JSON, runtime integration, log-provider bridges, and gzip are absent. Review and minimize Ratatui, Crossterm, and PTY-test features separately. `http-proto` unavoidably activates internal OTLP metric protocol code, but no meter provider or reader is constructed. `futures-util` may be an unavoidable OpenTelemetry transitive dependency; it is not a direct application dependency or an authorization to add streaming. Reqwest internally includes Tower's retry machinery, so also test that every Provider client uses `reqwest::retry::never()`; dependency-feature inspection alone cannot prove that runtime policy.
- Review `cargo deny` or equivalent advisory/license output before merging.
- Run the handle-relative/no-follow resolver suite on Linux, macOS, and Windows before claiming those targets; never replace an unsupported target with path canonicalization plus reopen.
- Commit `Cargo.lock`; dependency updates are deliberate changes with the same gates.

## Exact dependency recommendation

The following block is the original Engine/storage/network baseline. The canonical complete set additionally includes reviewed pinned Ratatui and Crossterm runtime dependencies plus a reviewed development-only PTY helper such as `expectrl`, as required by D-12. Exact terminal versions and enabled features are selected only after the terminal report's dependency/security review; `Cargo.lock` records the resolution.

```toml
[package]
edition = "2024"
rust-version = "1.88"

[dependencies]
cap-fs-ext = { version = "4.0", default-features = false, features = ["std"] }
cap-std = { version = "4.0", default-features = false }
clap = { version = "4.6", features = ["derive"] }
directories = "6.0"
opentelemetry = { version = "0.33.0", default-features = false, features = ["trace"] }
opentelemetry_sdk = { version = "0.33.0", default-features = false, features = ["trace"] }
opentelemetry-otlp = { version = "0.33.0", default-features = false, features = ["trace", "http-proto", "reqwest-blocking-client"] }
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls"] }
rusqlite = { version = "0.40", default-features = false, features = ["bundled"] }
secrecy = { version = "0.10", default-features = false }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
sha2 = { version = "0.11", default-features = false }
thiserror = "2.0"
tokio = { version = "1.53", default-features = false, features = ["rt", "signal", "sync", "time"] }
tokio-util = { version = "0.7", default-features = false, features = ["rt"] }
uuid = { version = "1.26", features = ["serde", "v7"] }

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.61", features = ["Win32_Storage_FileSystem"] }

[dev-dependencies]
opentelemetry_sdk = { version = "0.33.0", default-features = false, features = ["trace", "testing"] }
tempfile = "3"
tokio = { version = "1.53", default-features = false, features = ["rt", "signal", "sync", "test-util", "time"] }
```

`clap` supplies typed derive-based command parsing and validation ([clap derive tutorial](https://docs.rs/clap/latest/clap/_derive/_tutorial/)). `cap-std` 4.0.3 and `cap-fs-ext` 4.0.3 provide the cross-platform handle-relative/no-follow input mechanism; their published feature lists show that cap-std has no default feature additions and cap-fs-ext's `std` feature selects its cap-std implementation ([cap-std features](https://docs.rs/crate/cap-std/latest/features), [cap-fs-ext features](https://docs.rs/crate/cap-fs-ext/latest/features)). `windows-sys` is target-only and supplies the authoritative reparse attribute constant; it adds no non-Windows build surface. `directories` implements the platform state-location mapping; `secrecy` provides an intentionally non-serializable/redacted credential wrapper; and RustCrypto's `sha2` provides the required SHA-256 file digests without a native dependency ([ProjectDirs](https://docs.rs/directories/latest/directories/struct.ProjectDirs.html), [secrecy features](https://docs.rs/crate/secrecy/latest/features), [sha2](https://docs.rs/sha2/latest/sha2/)). Rusqlite 0.40 currently sets Rust 1.88 as its minimum supported compiler, which determines this package's initial `rust-version` ([rusqlite manifest](https://github.com/rusqlite/rusqlite/blob/master/Cargo.toml)).

The three OpenTelemetry dependencies are confined behind `telemetry.rs`; default features are disabled because the upstream defaults include broader signals and transports. The OTLP `http-proto` feature currently activates internal metric-protocol support even though this application constructs no meter provider; this exception must remain recorded and re-reviewed on upgrades. The SDK `testing` feature is dev-only.

Explicitly absent as direct application choices are `anyhow`, `async-trait`, SQLx, a connection pool, Hyper and rustls as direct dependencies, direct `rustix`, an OpenAI community SDK, `futures-util`, `sse-core`, BLAKE3, `chrono`, `time`, `jiff`, ULID, `schemars`, a migration framework, a retry crate, `tracing`, `dotenv`, a general configuration crate, direct `zeroize`, `dashmap`, `parking_lot`, a terminal component framework beyond Ratatui/Crossterm, and a web framework. OpenTelemetry may pull `futures-util` transitively; application code still does not use it.

## Evolution triggers

Change this foundation only on evidence:

- Enable Tokio multi-thread when profiling shows coordinator starvation from CPU work or materially higher runnable concurrency.
- Add a database pool or SQLx when there are multiple concurrent writers, a remote database, or database portability—not merely because the API is asynchronous.
- Add provider streaming only when a real client consumes incremental model output or measured time-to-first-useful-output is a product requirement; introduce the bounded parser, forward-compatibility policy, and partial-output cancellation/retry semantics together.
- Add nested or recursive scheduling only after the bounded flat `0..N` workflow is correct and evaluated; introduce explicit depth, fan-out, token, time, and concurrency budgets first.
- Add automatic retry only under the six retry conditions above.
- Add a daemon and transport only when a second client or detached execution exists.
- Add another crate only for measured compile pressure, independent release/ownership, or privilege isolation.
- Add Policy and a separate Guard before the first effectful tool. Provider output and Markdown rules are inputs to deterministic authorization, not authorization by themselves.
- Add time-formatting, schema-generation, secret-management, or general configuration dependencies only when their concrete behavior is required and tested.
- Expand telemetry beyond the private trace-only loopback-Collector module only on the triggers in [OTLP observability](./otlp-observability.md): a required receiver transport, direct-remote trust design, long-running metric need, or concrete field-diagnostic log need.

The intended performance property is not “Rust makes tokens arrive faster.” It is that the harness adds bounded, inspectable overhead before the first event and between subsequent events, while preserving a replayable truth and deterministic cancellation. That is the right foundation for making provider velocity—not local orchestration—dominate end-to-end latency.
