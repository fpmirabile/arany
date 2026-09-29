# CLI inputs, configuration, credentials, cancellation, and operability

**Status:** research and concrete recommendation for the minimum demonstrator  
**Research date:** 2026-09-29  
**Question:** What is the smallest safe and testable CLI contract for the first Rust harness, including bounded workspace input, provider configuration, credentials, terminal feedback, cancellation, diagnostics, fixtures, and proof thresholds?

## Reading guide

This report distinguishes four kinds of statements:

- **Fact**: behavior documented by a primary source such as official documentation, a specification, or upstream source.
- **Inference**: a conclusion derived from those facts and the repository's current architecture.
- **Recommendation**: the exact version-1 decision proposed for this repository.
- **Gate**: evidence implementation must produce before the behavior is claimed.

The current architecture is authoritative about product scope: one Cargo package, one process, one root orchestrator, exactly two read-only workers on the success path, one Provider behavior seam, one SQLite event table, and no effectful Tool. The optional OTLP second patch adds one private module, not a runtime service or another Engine seam.

> **Security amendment — 2026-09-29:** [Harness security lessons](./harness-security-lessons-and-controls.md) and D-14 in the [decision register](./next-step-decision-register.md) strengthen the concrete CLI contract. Resolve and admit the private state root before repository input; process no repository/Git configuration or `.env`; escape terminal and bidi controls deterministically; keep replayed text as data; reserve one aggregate Run budget before `RunStarted`; and treat dependency/build/Skill provenance as a release gate.

> **Testing amendment — 2026-09-29:** [Testing strategy for the Rust CLI harness](./testing-strategy-for-rust-cli-harness.md) is authoritative for how this report's verification matrix is implemented. Each area is a compact scenario, table, corpus, or failpoint owner rather than a requirement for one test function per row. User-visible acceptance captures exit status, exact stdout/stderr, and Events/RunView reopened from SQLite; deterministic, live-provider, evaluation, and performance evidence remain separate lanes.

## Executive conclusion

The minimum CLI should be deliberately less interactive than Claude Code or Codex. It should prove the multi-agent Engine without first building a terminal application framework.

**Recommendation:** version 1 has two commands, six run options, two output modes, four exit codes, no configuration file, no login flow, no keychain integration, no terminal redraw, no color, no width calculation, and no Engine retry:

```text
arany [--state-dir <DIR>] run \
  [--workspace <DIR>] \
  [--include <RELATIVE_PATH>]... \
  [--model <MODEL_ID>] \
  [--otlp-endpoint <URL>] \
  [--jsonl] \
  <OBJECTIVE>

arany [--state-dir <DIR>] show [--jsonl] <RUN_ID>
```

The important design decisions are:

1. `run` validates and snapshots every input before `RunStarted`; the model never receives a path it can open later.
2. Workspace-relative files are opened through pinned directory handles with no-follow traversal. Absolute paths, `..`, symlinks, junctions/reparse points, non-regular files, invalid UTF-8, duplicates, and over-limit inputs fail before provider I/O.
3. Human progress is append-only on stderr and the final answer is plain text on stdout. JSONL writes persisted Events to stdout in sequence order. TTY detection does not change bytes in version 1.
4. Version 1 emits no ANSI styling. `NO_COLOR`, terminal resize, CJK width, grapheme width, alternate screens, and cursor control therefore need no dependency or special case.
5. The OpenAI model is explicit through `--model` or `ARANY_OPENAI_MODEL`; there is no changing compiled-in model default. The API key comes only from `OPENAI_API_KEY`; no `--api-key`, `.env`, config file, or database field exists.
6. First `Ctrl-C` requests structured cancellation of the root token and every child. A second `Ctrl-C` or a two-second cleanup deadline aborts remaining tasks, then the supervisor records terminal cancellation and returns exit code 130.
7. The scripted fake is a strict expectation machine, not a second product mode. It injects outcomes at the Provider seam, can hold calls behind gates, and fails on unexpected, duplicate, unclaimed, or incompletely resolved expectations.
8. One integration-test file covers the architecture, boundary failures, replay, rendering, and cancellation. One ignored live smoke test proves the OpenAI adapter with explicit credentials and model.
9. OTLP traces are disabled unless `run` receives an explicit loopback Collector endpoint through CLI or the supported standard environment variables. Export failure never changes canonical Run truth or exit status.

This is the Ponytail result: delete every terminal feature that does not prove the team loop, and spend the saved complexity on trustworthy path handling, cancellation, and deterministic evidence.

## 1. Scope, assets, and trust boundaries

### 1.1 Assets

The first CLI handles five assets:

- the user's provider credential;
- bounded repository instruction and include-file snapshots;
- the local canonical event database, including objective and model output;
- provider budget: requests, tokens, elapsed time, and concurrency.
- when explicitly enabled, content-free operational timing and correlation identifiers exported to a local Collector.

There is no Tool, child process, MCP server, writable Workspace, remote Client, or durable Memory in this slice.

### 1.2 Attacker-controlled or failure-prone inputs

Treat these as untrusted:

- every command-line string and environment variable;
- the Workspace path and every directory entry under it;
- `AGENTS.md`, `CLAUDE.md`, and every included file;
- symlinks, reparse points, file type, concurrent path replacement, and concurrent file mutation;
- provider HTTP status, headers, body, structured output, and model-generated text;
- data already present in the SQLite file;
- stdout/stderr availability, including a closed pipe;
- signal timing and task completion races.

Repository content and model output are data. They do not gain filesystem authority or permission to change runtime limits.

### 1.3 Authorization decision

The user authorizes one ambient path only: the `--workspace` directory, defaulting to the current directory. The Engine converts that authority into an open directory handle. Every instruction and `--include` file is resolved relative to that handle. A CLI path string never becomes continuing authority for the model.

The OpenAI endpoint is fixed to the official Responses endpoint. Version 1 has no user-supplied provider base URL, redirect target, hosted Tool, or provider plugin. The separate optional OTLP endpoint accepts only numeric loopback `http`; it is not a Provider endpoint and the Collector owns every remote connection.

### 1.4 Failure posture

Fail before `RunStarted` when command syntax, configuration—including explicit OTLP configuration—credentials, Workspace resolution, instruction discovery, include validation, byte limits, or UTF-8 validation fails. After `RunStarted`, every terminal outcome must be represented in the event stream before the CLI returns whenever the store remains writable. A valid but unavailable Collector is a non-fatal telemetry diagnostic, never a Run failure.

## 2. Exact CLI contract

### 2.1 Commands

Use `clap` derive with a required subcommand. Its derive API maps structs and enums to arguments and subcommands, supports repeated `Vec<T>` options, and performs value validation before application logic. [`clap` derive reference](https://docs.rs/clap/latest/clap/_derive/) **Fact**

```text
arany [GLOBAL_OPTIONS] <COMMAND>

Commands:
  run   Start one bounded multi-agent Run
  show  Replay one stored Run
  help  Print help

Global options:
      --state-dir <DIR>  Override the platform state directory
  -h, --help             Print help
  -V, --version          Print version
```

`run`:

```text
Usage: arany [--state-dir <DIR>] run [OPTIONS] <OBJECTIVE>

Arguments:
  <OBJECTIVE>  Non-empty UTF-8 objective, at most 8 KiB

Options:
      --workspace <DIR>          Workspace root [default: .]
      --include <RELATIVE_PATH>  Include one read-only UTF-8 file; repeatable
      --model <MODEL_ID>         OpenAI model; overrides ARANY_OPENAI_MODEL
      --otlp-endpoint <URL>      Export traces to a numeric-loopback HTTP Collector
      --jsonl                    Emit ordered Event objects instead of human output
  -h, --help                     Print help
```

`show`:

```text
Usage: arany [--state-dir <DIR>] show [--jsonl] <RUN_ID>

Arguments:
  <RUN_ID>  Exact Arany Run identifier

Options:
      --jsonl  Emit the stored Event objects in sequence order
  -h, --help   Print help
```

### 2.2 Intentionally absent options

Version 1 has no:

- `--provider`: OpenAI is the only product adapter; the fake is injected by tests.
- `--api-key`: secrets in command arguments are too easy to expose through shell history, process inspection, test failure output, or copied commands.
- `--config`: two environment variables and one state-directory override do not justify a file format, discovery rules, migrations, or merge semantics.
- timeout, retry, concurrency, input-limit, or output-limit options: all are fixed constants until real usage demonstrates that a specific bound must vary.
- `--color`, `--no-color`, quiet, verbose, log-level, or terminal-layout flags: version 1 has no ANSI styling, redraw, or debug log stream.
- prompt-from-stdin mode: it complicates TTY ownership and piping semantics without helping the initial proof.
- recursive include, glob, URL, directory include, ignore-file expansion, or model-driven file discovery.

### 2.3 Parsing and validation order

The command boundary performs work in this order:

1. `clap` parses syntax and basic values.
2. Resolve the state directory before repository input; require an outside-Workspace, local, no-follow, current-user private root; open SQLite no-follow in defensive mode and verify aggregate headroom.
3. For `show`, parse the Run ID and replay; no provider credential, Workspace, or telemetry exporter is read or constructed.
4. For `run`, validate objective, model, and OTLP configuration. Reject unsupported OTLP variables by name without reading or printing their values.
5. Construct disabled telemetry or the bounded blocking exporter before entering the current-thread Tokio runtime.
6. Load `OPENAI_API_KEY` into a secret wrapper without formatting it.
7. Open and pin the Workspace directory.
8. Resolve the exact root instruction file and every explicit include into immutable byte snapshots.
9. Validate aggregate bounds and compile the initial provider input.
10. Append `RunStarted`; only now has a Run begun.

The ordering ensures a rejected input creates no partial Run and performs no provider request.

### 2.4 Exit codes

Rust's `ExitCode` supports the platform's canonical success/failure values and arbitrary `u8` codes, while warning that numeric meanings are not universally portable. [`std::process::ExitCode`](https://doc.rust-lang.org/std/process/struct.ExitCode.html) **Fact** `clap` uses 2 for errors printed to stderr and 0 for help/version output. [`clap::Error::exit_code`](https://docs.rs/clap/latest/clap/error/struct.Error.html#method.exit_code) **Fact**

Use only four observable codes:

| Code | Meaning | Examples |
|---:|---|---|
| `0` | success or requested help/version | completed Run, successful replay, `--help` |
| `1` | operational failure after valid invocation | provider error, timeout, corrupt state, unknown Run, SQLite failure, broken output pipe |
| `2` | invalid invocation or pre-Run configuration/input | clap error, missing model/key, invalid include, oversized input |
| `130` | user cancellation | first Ctrl-C completed cleanup, second Ctrl-C forced task abort |

Do not create a numeric code per error kind. Human and JSON diagnostics carry a stable symbolic code; the process code answers only what scripts normally need: success, invalid request, failed operation, or interrupted operation.

## 3. Output and terminal behavior

### 3.1 Channel contract

Human `run` mode:

- stdout contains only the sanitized root final result followed by one newline;
- stderr contains append-only state transitions and safe diagnostics;
- every visible transition is derived from a persisted Event;
- no partial model token stream is printed in the first slice.

Human `show` mode:

- stdout contains a deterministic text rendering of the reconstructed `RunView`;
- stderr contains only diagnostics;
- rendering the same stored Events twice produces identical stdout.

JSONL mode:

- stdout contains only Event envelopes;
- stderr contains only diagnostic envelopes;
- no human prefix, spinner, ANSI sequence, progress table, or final plain-text copy is added.

This separation lets `arany run ... > answer.txt` capture the answer while the user still sees team progress. It also lets automation request the canonical event stream explicitly.

### 3.2 Human progress grammar

Use one line for each semantic state transition:

```text
run 01J... started
root 01J... running
worker 01J... spawned: review persistence trade-offs
worker 01J... running
worker 01J... finished: SQLite fits the single-process journal
root 01J... waiting for 1 worker
root 01J... running
root 01J... finished
run 01J... finished
```

The prefix is ASCII and machine-testable. Objectives and summaries are untrusted fields: flatten CR/LF to spaces for progress, visibly escape or replace ESC/CSI/OSC/DCS, C0/C1 controls, carriage return, backspace, bidi overrides/isolation controls, and any forged line-prefix delimiter, and enforce stored size limits. The final result preserves the deliberate LF/TAB policy but cannot carry terminal or bidi control actions. JSONL preserves underlying strings as JSON data and is emitted only by the serializer as exactly one complete object per line; never construct it by interpolation or apply human-terminal rewriting to the serialized representation.

Do not render Markdown, OSC hyperlinks, model-provided ANSI, or terminal titles. The CLI is a text adapter, not a trusted rich renderer.

### 3.3 TTY and non-TTY

Rust's `IsTerminal` reports whether a descriptor or handle refers to a terminal and returns false on unsupported platforms or unexpected detection errors. [`std::io::IsTerminal`](https://doc.rust-lang.org/std/io/trait.IsTerminal.html) **Fact**

**Recommendation:** do not call `is_terminal()` or branch output in version 1. A TTY and redirected stream receive the same bytes. This removes an unused observation, avoids snapshot-test divergence, and makes CI logs match local behavior.

The first version therefore has:

- no spinner;
- no cursor movement or line erasure;
- no alternate screen or raw mode;
- no keyboard handling beyond the OS/Tokio Ctrl-C signal;
- no terminal-size query or resize subscription.

`crossterm` can report terminal size and resize events, but adopting those APIs would create a live layout lifecycle. [`crossterm::terminal::size`](https://docs.rs/crossterm/latest/crossterm/terminal/fn.size.html) [`crossterm::event`](https://docs.rs/crossterm/latest/crossterm/event/) **Fact** The minimum proof does not need that lifecycle.

### 3.4 Color and `NO_COLOR`

The `NO_COLOR` convention says that a present, non-empty `NO_COLOR` disables default ANSI color. [NO_COLOR](https://no-color.org/) **Fact**

**Recommendation:** emit no ANSI color under any condition. This satisfies `NO_COLOR` without reading it and avoids a `--color` policy. Add color only when user testing proves that semantic attribution is materially harder without it. At that point use `auto|always|never`, with an explicit CLI choice overriding `NO_COLOR` as the convention permits.

### 3.5 Unicode and width

Accept objectives and file contents only as valid UTF-8. Preserve Unicode text in provider input, Events, and JSON. Structural labels and status markers remain ASCII.

Do not calculate display width or truncate to terminal columns. The `unicode-width` crate has separate ordinary and CJK width behavior, and string width may differ from the sum of scalar widths for emoji sequences. [`unicode-width` source and rules](https://docs.rs/unicode-width/latest/src/unicode_width/lib.rs.html) [`UnicodeWidthChar`](https://docs.rs/unicode-width/latest/unicode_width/trait.UnicodeWidthChar.html) **Fact** The terminal is allowed to wrap append-only lines. This avoids choosing an incorrect locale, grapheme, emoji, or font model.

### 3.6 JSONL semantics

JSON Lines requires UTF-8, one valid JSON value per line, and a line terminator after each value. [JSON Lines](https://jsonlines.org/) **Fact**

Each stdout line is a compact Event envelope:

```json
{"event_version":1,"sequence":1,"run_id":"run-example","agent_run_id":null,"kind":"RunStarted","created_at_ms":1780000000000,"payload":{}}
```

Fixed semantics:

- UTF-8, no BOM;
- exactly one compact object followed by `\n`;
- flush after each Event;
- strict ascending `sequence` for one Run;
- persisted Event values only, not ephemeral token deltas;
- `show --jsonl` emits the same stored envelopes and order as the original completed run;
- schema field names are stable; payload interpretation is selected by `event_version` and `kind`;
- no ANSI and no raw control bytes outside JSON escaping.

Diagnostics are a separate stderr JSONL stream in JSONL mode:

```json
{"type":"diagnostic","schema_version":1,"code":"H_INPUT_TOO_LARGE","message":"included input exceeds 262144 bytes"}
```

Diagnostics never masquerade as canonical Events. A runtime failure that the Engine could persist appears as terminal `AgentFinished`/`RunFinished` data on stdout and may also receive one concise stderr diagnostic.

### 3.7 Output I/O failure

All writes use explicit locked writers and return `io::Result`; do not use `println!` as the rendering boundary. On `BrokenPipe` during an active JSONL run, request run cancellation because the only Client has detached and continued provider cost would be invisible. Finalize cancellation best-effort and return 1. Other write failures follow the same operational-failure path.

## 4. Workspace and `--include` security

### 4.1 Why `canonicalize` plus `open` is insufficient

`std::fs::canonicalize` resolves an absolute path and symbolic links. [`std::fs::canonicalize`](https://doc.rust-lang.org/std/fs/fn.canonicalize.html) **Fact** A later `File::open(path)` performs another name lookup. **Inference:** if policy checks the canonical string and later reopens the original path, another process can swap a path component between those operations. The checked object and the opened object need not be the same.

Rust's own file documentation warns that files may be concurrently modified even while a handle exists. [`std::fs::File`](https://doc.rust-lang.org/std/fs/struct.File.html) **Fact** Path containment and immutable content therefore require two separate decisions:

- containment is enforced while acquiring a handle;
- the exact bytes read from that one handle become the immutable Run snapshot.

### 4.2 Handle-relative resolver

`cap-std` provides directory capabilities and rejects attempts to escape a supplied `Dir` through absolute paths, `..`, or symlinks outside the directory; on modern Linux it uses `openat2` in common cases and otherwise walks components using directory handles. [`cap-std` README](https://github.com/bytecodealliance/cap-std/blob/main/README.md) **Fact** Linux `openat2` supplies `RESOLVE_BENEATH`, `RESOLVE_NO_MAGICLINKS`, and `RESOLVE_NO_SYMLINKS`; the kernel reports escape and race failures rather than returning a handle it could not safely resolve. [`openat2(2)`](https://man7.org/linux/man-pages/man2/openat2.2.html) **Fact**

The existing instruction research requires rejecting every symlink/reparse component, not merely external escapes. Use `cap-std` plus `cap-fs-ext` to walk each component from a pinned `Dir`: `open_dir_nofollow` fails when its final directory component is a symlink, while `FollowSymlinks::No` controls the final file component. [`cap_fs_ext::DirExt`](https://docs.rs/cap-fs-ext/latest/cap_fs_ext/trait.DirExt.html) [`cap_fs_ext::FollowSymlinks`](https://docs.rs/cap-fs-ext/latest/cap_fs_ext/enum.FollowSymlinks.html) **Fact**

**Recommendation:** use one private Engine resolver, with no trait:

```text
open workspace with explicit ambient authority
for each lexical relative path:
  reject non-UTF-8, absolute/prefix/root, empty, `.` and `..` components
  walk parent components with open_dir_nofollow from the pinned parent Dir
  open final component once with follow_symlinks = No
  obtain metadata from the open handle
  require a regular file
  stream at most limit + 1 bytes from that handle
  require valid UTF-8
  compute SHA-256 over the exact bytes read
  retain bytes, normalized relative path, identity, size, mtime and digest
```

Do not pass the path forward for a later reopen. Provider input receives the immutable in-memory snapshot.

### 4.3 Exact path rules

For every `--include`:

- the argument must be valid UTF-8;
- it must be a relative path with at least one normal component;
- reject root, platform prefix, parent (`..`), and current-directory (`.`) components rather than normalizing them;
- reject repeated lexical paths after separator normalization;
- reject any symlink, junction, reparse point, magic link, socket, FIFO, device, or directory component where a normal directory/file is expected;
- the final handle must report a regular file;
- its bytes must be valid UTF-8;
- an empty regular file is valid;
- preserve include order as supplied by the user.

Rejecting even contained symlinks is stricter than `cap-std` containment alone. The benefit is one portable and testable meaning for “the user included this repository file,” consistent with instruction-file policy. Relax it only after a real symlink-heavy use case and cross-platform conformance tests exist.

### 4.4 Concurrent mutation and snapshot semantics

Once opened, read from the same file handle into a bounded buffer and hash those exact bytes. A concurrent writer can change the file while it is being read; portable advisory locks cannot guarantee cooperation from an attacker. The contract is therefore:

- the Run uses exactly the bytes it obtained before `RunStarted`;
- the digest identifies those bytes, not a promise that the path remained unchanged;
- size metadata is advisory; the streaming `limit + 1` check is authoritative;
- a read error fails the Run before start;
- the Engine never rereads the path during the Run.

This closes authority and allocation races without pretending to provide an atomic repository snapshot. A true multi-file snapshot would require a VCS tree, filesystem snapshot, or copied content-addressed Artifact and is deferred.

### 4.5 Instruction resolution

At the exact Workspace root:

1. Attempt exact `AGENTS.md` with no-follow semantics.
2. Only an actual `NotFound` result permits exact `CLAUDE.md` lookup.
3. If `AGENTS.md` exists but is a symlink, non-regular, unreadable, oversized, or invalid UTF-8, fail; do not fall back.
4. If both are absent, continue with no repository instruction file.
5. If both exist, load only `AGENTS.md`.
6. Record selected relative path, file identity metadata, byte count, and SHA-256 digest in `RunStarted`; do not persist the credential or provider request headers.

The instruction snapshot has its own byte limit and does not consume an include slot, but it does consume the compiled-input byte limit.

### 4.6 Cross-platform conformance gate

Before declaring Windows and macOS support, run the same resolver suite on each platform:

- final file symlink;
- intermediate directory symlink;
- external symlink escape;
- Windows junction/reparse point and reserved device names;
- FIFO/socket/device or closest platform equivalent;
- path swap during repeated open attempts;
- file growth beyond the byte limit during read;
- case-ambiguous `AGENTS.md` aliases on case-insensitive filesystems.

`cap-std` has had a Windows device-name sandbox advisory in older versions, so pin a patched current release and include dependency audit evidence. [Bytecode Alliance advisory GHSA-hxf5-99xg-86hw](https://github.com/bytecodealliance/cap-std/security/advisories/GHSA-hxf5-99xg-86hw) **Fact** Unsupported enforcement fails closed rather than silently reverting to `canonicalize` plus reopen.

## 5. Fixed bounds and overflow behavior

The first demonstrator needs constants, not tunables. The values below comfortably hold the two-document proof while putting a finite ceiling on memory, provider cost, event rows, and elapsed time.

| Resource | Fixed version-1 bound | Overflow behavior |
|---|---:|---|
| objective | 8 KiB UTF-8 bytes | exit 2 before Run |
| include count | 16 | exit 2 before Run |
| one include file | 128 KiB | read at most 128 KiB + 1; exit 2 |
| all include bytes | 256 KiB | exit 2 before Run |
| selected instruction file | 64 KiB | exit 2; never fallback from invalid `AGENTS.md` |
| compiled provider input | 384 KiB serialized UTF-8 | exit 2 before first call; fail Run before later call |
| one child objective | 2 KiB | reject Provider outcome; fail root |
| children requested by root | exactly 2 | reject every other delegate count |
| concurrent workers | 2 | fixed semaphore/scheduler rule |
| AgentUpdated summary | 2 KiB | reject Provider outcome; no silent truncation |
| one worker result | 16 KiB | reject Provider outcome |
| root final result | 32 KiB | reject Provider outcome |
| Provider HTTP response body | 1 MiB | stop reading, drop request, fail AgentRun |
| Event JSON payload | 64 KiB | reject append and fail Run |
| provider calls per Run | 4 | root delegate + 2 workers + root finish; fifth is invariant failure |
| `max_output_tokens` per call | 4,096 | `incomplete` is failure, not valid semantic output |
| requested output-token ceiling per Run | 16,384 | implied by 4 calls × 4,096 |
| one provider-call deadline | 120 s | fail that AgentRun; cancel descendants |
| whole-Run deadline | 300 s | cancel tree, record `timed_out`, exit 1 |
| graceful cancellation drain | 2 s | abort remaining JoinSet tasks, finalize cancellation |
| automatic provider retries | 0 | expose safe diagnostic and retry hint; user chooses rerun |
| SQLite busy wait | 250 ms | operational failure; do not hang CLI |
| SQLite database pages | 65,536 × 4 KiB = 256 MiB | reject growth; never prune canonical Events |
| SQLite headroom before new Run | 4 MiB | reject admission before `RunStarted` |
| recent Events in human `show` | all events for this bounded proof | add pagination only after a measured event-volume issue |

Before `RunStarted`, one Engine-owned budget reserves the four Provider call slots and the worst-case 16,384 requested output tokens. Root and children consume that same budget; delegation never creates a new allowance. Version 1 does not need an unbounded Event queue. Append and reduce each Event before notifying the renderer. If the implementation uses a Tokio channel to decouple rendering, make it a 32-item bounded channel and await capacity; canonical Events are never dropped or coalesced. A closed receiver follows the output-I/O failure policy, and a slow receiver applies backpressure. This keeps memory finite and makes detachment visible instead of silently spending provider budget.

Set `rusqlite::Connection::busy_timeout(Duration::from_millis(250))` explicitly on the connection. `rusqlite` currently installs a five-second default, but its documentation says that default may change; relying on it would make the CLI deadline an accidental dependency-version property. [`rusqlite::Connection::busy_timeout`](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html#method.busy_timeout) **Fact**

### 5.1 Token-bound nuance

OpenAI's `max_output_tokens` is an upper bound that includes visible output and reasoning tokens. [OpenAI Responses API](https://developers.openai.com/api/reference/cli/resources/responses/methods/create) [OpenAI token counting](https://developers.openai.com/api/docs/guides/token-counting) **Fact** Set it to 4,096 on every call and treat an `incomplete` response caused by the limit as a failed semantic outcome.

There is no honest model-independent exact input-token counter in the Rust standard library. OpenAI offers a token-counting endpoint, but calling it before every generation would add network latency, requests, and another failure surface. [OpenAI token counting](https://developers.openai.com/api/docs/guides/token-counting) **Fact** A bundled tokenizer would need model-specific mapping that changes independently of the Engine.

**Recommendation:** version 1 preflights input in bytes and leaves API truncation disabled. If the selected model rejects context size, fail visibly; never ask the API to discard earlier input automatically. Record provider-reported input/output usage after each response. Add exact preflight token counting only when observed context failures justify either a model-aware local tokenizer or one bounded count request per distinct compiled prompt.

This resolves the token policy without claiming false precision:

- generation is hard-capped by tokens;
- provider-call count hard-caps the requested Run output budget;
- prompt allocation and transmission are hard-capped by bytes;
- actual input tokens are measured and persisted after each successful response;
- provider context rejection is a classified operational failure.

## 6. Configuration, state location, and model selection

### 6.1 Precedence

There is no general configuration object or config file. Resolve only four settings:

| Setting | Precedence |
|---|---|
| state directory | `--state-dir` > `ARANY_STATE_DIR` > platform default |
| OpenAI model | `--model` > `ARANY_OPENAI_MODEL` > error |
| OTLP traces endpoint | `--otlp-endpoint` > `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` > `OTEL_EXPORTER_OTLP_ENDPOINT` > disabled |
| OpenAI credential | `OPENAI_API_KEY` > error |

An explicitly present but empty CLI or environment value is invalid; it does not fall through. This prevents a typo or cleared secret from silently selecting another source.

`--workspace` has no environment counterpart and defaults to the process current directory. Output mode comes only from `--jsonl`. The trace-specific environment value is an exact trace URL; the generic endpoint is a base to which `/v1/traces` is appended. The CLI value is a validated base URL. Only numeric loopback `http` endpoints are accepted.

Other OTLP environment variables—including headers, certificates, client keys, compression, protocol selectors, and resource attributes—are unsupported. Detect them by variable name and fail before `RunStarted`; never read or render their values. Queue, batch, timeout, retry, privacy, Resource, and sampling settings remain fixed constants. The local Collector owns remote authentication, TLS, routing, and vendor configuration.

### 6.2 State directory

The XDG Base Directory specification defines `$XDG_STATE_HOME` for state that persists across application restarts, including history, with `$HOME/.local/state` as the default. [XDG Base Directory Specification 0.8](https://specifications.freedesktop.org/basedir/0.8/) **Fact** Apple designates `~/Library/Application Support/<app>` for app-specific support and user data. [Apple macOS Library directory guidance](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/FileSystemProgrammingGuide/MacOSXDirectories/MacOSXDirectories.html) **Fact** Microsoft identifies `FOLDERID_LocalAppData` for machine-local application data. [Microsoft known folders](https://learn.microsoft.com/windows/win32/shell/knownfolderid) **Fact**

The `directories` crate exposes `ProjectDirs`, including Linux state directories and platform local-data directories. [`directories::ProjectDirs`](https://docs.rs/directories/latest/directories/struct.ProjectDirs.html) **Fact**

Use:

```text
Linux:  ProjectDirs::state_dir()/events.sqlite3
macOS:  ProjectDirs::data_local_dir()/events.sqlite3
Windows: ProjectDirs::data_local_dir()/events.sqlite3
```

with `ProjectDirs::from("dev", "Arany", "arany")`. `ARANY_STATE_DIR` or `--state-dir` replaces only the parent directory; the filename remains `events.sqlite3`.

Resolve the directory before any repository input and require it to be outside the Workspace on a verified local filesystem. Open no-follow, reject links/reparse points and unexpected types, verify the current user owns it, and require private access: directory `0700` and database/sidecars `0600` on Unix, or an equivalent effective ACL on Windows. Do not merely assume Local AppData is private and do not silently chmod an existing broad directory. State-directory failure is operational error 1; an invalid explicit configuration remains exit 2.

The override exists for tests, portable development, backup, and recovery. It still obeys every security property above and does not justify a configuration file.

### 6.3 Model selection

Do not compile a default model ID. Model names, availability, pricing, context, and feature support change independently of the binary. A silent default also hides a meaningful cost and quality decision.

`--model` and `ARANY_OPENAI_MODEL` accept a non-empty ASCII identifier of at most 128 bytes. The exact value is recorded in `RunStarted`. The Provider must require Structured Outputs support for the semantic `Delegate|Finish` schema; strict Structured Outputs conforms to a supplied JSON Schema and rejects unsupported schemas/models rather than silently approximating. [OpenAI Structured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs) **Fact**

If the chosen model does not support the contract, fail before accepting a semantic outcome. Do not fall back to another model.

### 6.4 Provider request defaults

Fixed OpenAI behavior:

- endpoint: `POST https://api.openai.com/v1/responses`;
- foreground request;
- `store: false`;
- `truncation: disabled`;
- strict JSON Schema output for `Delegate|Finish`;
- no built-in Tool, web search, file search, MCP, or code execution;
- `max_output_tokens: 4096`;
- one 120-second attempt;
- no automatic retry.

OpenAI documents that Responses default to stored application state when `store` is omitted/true and that `store: false` disables response storage subject to the documented abuse-monitoring/data-control policy. [OpenAI data controls](https://developers.openai.com/api/docs/guides/your-data) [Responses `store`](https://developers.openai.com/api/reference/cli/resources/responses/methods/create) **Fact** The local event journal remains canonical.

The direct HTTP client is also part of the security contract. With `reqwest`, configure Rustls, `https_only(true)`, `redirect(Policy::none())`, `retry(reqwest::retry::never())`, `no_proxy()`, `referer(false)`, no cookie store, and the 120-second total timeout. Mark the Authorization `HeaderValue` sensitive before insertion. Do not enable invalid-certificate/hostname exceptions, TLS key logging, connection-verbose body logging, or a configurable base URL. `reqwest` otherwise follows up to ten redirects, inherits system proxy variables, and retries some protocol NACKs; its builder supplies explicit controls for all three behaviors. [`reqwest::ClientBuilder`](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html) [`reqwest::retry::never`](https://docs.rs/reqwest/latest/reqwest/retry/fn.never.html) **Fact**

Disabling inherited proxy configuration is deliberate: an ambient `HTTPS_PROXY` must not silently change the credential's network path. It does mean version 1 will not run in environments where OpenAI is reachable only through an enterprise proxy. Add explicit, auditable proxy configuration only after that is a real deployment requirement; never regain proxy support merely by accepting every process environment variable.

## 7. Credentials

### 7.1 Minimum credential source

OpenAI recommends keeping API keys out of source and exposing them through an environment variable or secret-management service; its quickstart uses `OPENAI_API_KEY`. [OpenAI production best practices](https://developers.openai.com/api/docs/guides/production-best-practices) [OpenAI SDKs and CLI](https://developers.openai.com/api/docs/libraries) **Fact**

Version 1 reads only `OPENAI_API_KEY` from the inherited environment. It never:

- loads `.env`;
- accepts a key argument or stdin prompt;
- writes the key to SQLite, a config file, crash report, Event, metric, or diagnostic;
- includes request headers or a full HTTP client debug dump in an error;
- forwards the environment to another process, because no child process exists.

Wrap the key immediately in `secrecy::SecretString`, expose it only while building the Authorization header, and do not enable its serde feature. The crate's stated goals are explicit secret exposure, prevention of debug-log leakage, and zeroization on drop. [`secrecy` crate](https://docs.rs/secrecy/latest/secrecy/) **Fact** This dependency earns its place by making the most damaging accidental formatting error harder.

An absent or empty environment value is diagnostic `H_CONFIG_OPENAI_API_KEY` and exit 2. An API 401 is a runtime provider failure and exit 1; report “OpenAI authentication failed” without reflecting the key or response body.

### 7.2 Why keychain support is deferred

Platform stores are valuable but materially different. Apple's Keychain stores small secrets in an encrypted database and can impose access controls. [Apple Keychain Services](https://developer.apple.com/documentation/security/keychain-services) **Fact** Windows provides Credential Manager APIs and an encrypted per-user Vault. [Microsoft Credentials Management](https://learn.microsoft.com/en-us/windows/win32/secauthn/credentials-management) **Fact** Linux desktop environments commonly expose the Secret Service D-Bus API, whose collections may be locked and may prompt the user. [Secret Service API](https://specifications.freedesktop.org/secret-service/latest-single/) **Fact**

Adding “use the keychain” therefore means defining service/account naming, headless behavior, locked-session behavior, prompts, migration, deletion, CI behavior, backend availability, and native dependencies on three platforms. That is not needed to prove the Engine.

**Trigger:** add an `arany auth login/logout/status` flow and one credential-store adapter only when users need persisted credentials or OAuth. At that point environment input remains an explicit higher-precedence deployment override, and the keychain failure mode must be visible rather than falling back to plaintext storage.

## 8. Provider errors, deadlines, and retries

### 8.1 Error classification

Map provider failures to safe internal classes:

| Provider observation | Run result | Safe diagnostic fields |
|---|---|---|
| DNS/TLS/connect failure | failed | class, elapsed time |
| 400/schema/context error | failed | HTTP status, provider error code, request ID |
| 401/403 | failed | authentication/permission class, request ID |
| 404 model | failed | selected model, request ID |
| 429 temporary rate limit | failed, retryable hint | error code, bounded `Retry-After`, request ID |
| 429 quota/billing/spend | failed, not retryable | error code, request ID |
| 500/503 overload | failed, retryable hint | status, error code, bounded `Retry-After`, request ID |
| malformed/oversized body | failed | status, byte count, request ID |
| incomplete/refusal/non-schema output | failed semantic outcome | response status/reason, request ID |
| deadline | timed out | deadline duration, request ID if known |

OpenAI distinguishes temporary `slow_down` and `server_is_overloaded` conditions from billing/spend/quota errors and instructs clients to inspect status plus `error.code`. It may provide `Retry-After`. [OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits) [OpenAI error codes](https://developers.openai.com/api/docs/guides/error-codes) **Fact**

Never print the response body, prompt, output, Authorization header, or all headers as a diagnostic. Parse the typed error envelope under the same 1 MiB response cap and allowlist only the fields above.

### 8.2 No automatic retry in the demonstrator

OpenAI recommends bounded exponential backoff with jitter for eligible temporary failures and warns that unsuccessful requests still count against rate limits. [OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits) **Fact**

**Recommendation:** keep the architecture's zero-retry rule for the proof. Automatic retries would require attempt Events, cumulative deadlines, cancellation during backoff, request identity, duplicate-accounting rules, and deterministic fake behavior. The CLI instead reports whether the class is retryable and a capped `retry_after_seconds` value when present. The user explicitly reruns.

**Trigger:** add at most two automatic attempts only after live evidence shows temporary failure is a meaningful usability problem. Then honor valid `Retry-After`, add jitter when absent, cap total retry time inside the 300-second Run deadline, and record each attempt without nesting SDK retries.

### 8.3 Deadlines

Wrap each Provider future in a 120-second deadline and the Engine run in a 300-second deadline. Tokio's `timeout` returns an error and cancels the wrapped future when the deadline elapses, although a future that never yields can overrun. [`tokio::time::timeout`](https://docs.rs/tokio/latest/tokio/time/fn.timeout.html) **Fact**

The HTTP adapter must be genuinely asynchronous and yield while connecting, sending, and reading. Stream the response with a 1 MiB cap; do not call an unbounded `bytes()`/`text()` convenience that allocates the complete body first.

## 9. Ctrl-C and cascading cancellation

### 9.1 Cancellation tree

Tokio describes graceful shutdown as detecting shutdown, notifying tasks, and waiting for them. Its `CancellationToken` wakes all clones/children, and a child token can be cancelled without cancelling its parent. [Tokio graceful shutdown](https://tokio.rs/tokio/topics/shutdown) [`CancellationToken`](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html) **Fact**

Use one root `CancellationToken` per Run and one `child_token()` per AgentRun:

```text
Run token
├── root AgentRun token
├── worker A token
└── worker B token
```

Only the supervisor owns task handles and appends lifecycle Events. Provider tasks return typed outcomes; they do not append terminal state on their own. This gives cancellation one writer and prevents a late provider completion from racing a cancelled terminal Event.

At every provider wait use a cancellation-prioritized selection:

```text
select biased:
  cancellation -> drop provider future, return Cancelled
  deadline     -> drop provider future, return TimedOut
  response     -> validate outcome
```

Tokio `select!` cancels remaining branches by dropping them and documents cancellation-safety requirements; biased order makes the developer responsible for placing shutdown where it cannot starve. [`tokio::select!`](https://docs.rs/tokio/latest/tokio/macro.select.html) **Fact**

### 9.2 Signal behavior

`tokio::signal::ctrl_c()` is cross-platform, but on Unix it replaces the default SIGINT behavior for the life of the process after first polling. [`tokio::signal::ctrl_c`](https://docs.rs/tokio/latest/tokio/signal/fn.ctrl_c.html) **Fact** The CLI must therefore implement both first- and second-signal behavior explicitly.

First Ctrl-C:

1. stop admitting work;
2. cancel the Run token;
3. cancel active Provider futures and workers;
4. wait for tasks to acknowledge;
5. append cancelled `AgentFinished` entries for unfinished agents in stable ID order;
6. append cancelled `RunFinished`;
7. flush/commit the store;
8. return 130.

While draining, race task completion against:

- a second Ctrl-C; or
- a two-second cleanup deadline.

Either condition calls `JoinSet::shutdown()`, which aborts all tasks and waits until the set is empty. [`tokio::task::JoinSet`](https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html#method.shutdown) **Fact** The supervisor then performs the terminal event writes itself. Do not call `process::exit`, because Rust documents that immediate exit skips destructors; return `ExitCode::from(130)` from `main` after cleanup. [`std::process::ExitCode`](https://doc.rust-lang.org/std/process/struct.ExitCode.html) **Fact**

### 9.3 Cancellation invariants

Tests must prove:

- no Provider completion is accepted after the supervisor observes cancellation;
- no new child is spawned after cancellation;
- every active AgentRun has one terminal outcome;
- root and Run reach cancelled terminal state even when Provider futures never resolve;
- all JoinSet tasks are reaped before `main` returns;
- semaphore permits and SQLite transactions are released;
- replay reconstructs cancelled state exactly;
- a timeout produces `timed_out` and exit 1, not user-cancelled/130.

There are no child OS processes in this slice. Process-tree supervision remains a trigger for the first external Tool.

## 10. Diagnostics, observability, and redaction

### 10.1 Canonical Events plus optional trace projection

Version 1 has:

- canonical Events in SQLite;
- user-facing progress derived from those Events;
- one-line safe diagnostics on stderr;
- an optional private OTLP/HTTP-protobuf trace exporter to a numeric-loopback Collector; and
- no rotating log file, OTLP logs, OTLP metrics, analytics, crash upload, global `tracing` subscriber, or `RUST_LOG` contract.

SQLite avoids building a second history beside the journal. OTLP is a lossy operational projection that accepts only typed, content-free fields and can be dropped without changing the Run. Add structured logs only when diagnosing real field failures requires information that is neither canonical state nor safe user feedback.

### 10.2 Stable diagnostic envelope

Every diagnostic has:

```text
code       stable ASCII identifier
message    safe bounded sentence
run_id     optional
agent_id   optional
details    allowlisted scalar map
source     top-level component only
```

Examples:

```text
H_INPUT_PATH_ESCAPE
H_INPUT_SYMLINK
H_INPUT_NOT_REGULAR
H_INPUT_TOO_LARGE
H_INPUT_INVALID_UTF8
H_CONFIG_MODEL
H_CONFIG_OPENAI_API_KEY
H_CONFIG_OTLP
H_STORE_OPEN
H_PROVIDER_AUTH
H_PROVIDER_RATE_LIMIT
H_PROVIDER_TIMEOUT
H_PROVIDER_SCHEMA
H_CANCELLED
H_TELEMETRY_EXPORT
```

Messages are at most 1 KiB; each detail string is at most 256 bytes; at most 16 details exist. Unknown errors collapse to a safe class plus Run/Agent ID. Error chaining is available to tests and internal debugging but is not rendered recursively to users.

### 10.3 Allowlist instead of scrub-after-formatting

Do not format a rich request/client error and then apply regex redaction. Construct diagnostics from allowlisted fields:

- Run/Agent IDs;
- normalized Workspace-relative path;
- byte counts and fixed limits;
- model ID;
- elapsed milliseconds;
- HTTP status;
- typed OpenAI error code;
- provider request ID;
- bounded `Retry-After` seconds.

Never include:

- API key or Authorization value;
- inherited environment;
- absolute home/state/workspace path unless the user supplied it explicitly and the error requires it;
- file content, prompt, raw model output, or response body;
- full HTTP headers;
- database payload;
- secret length or prefix.

The SQLite journal is canonical local history, not a debug log. It legitimately stores objective, attributed summaries, and results because replay requires them; it does not store included file contents in the minimum slice, only their normalized paths, sizes, and digests.

## 11. Deterministic fake fixture

### 11.1 Strict fake, not probabilistic stub

The fake implements the same Provider interface but is compiled as test support rather than exposed as a CLI provider. It owns a finite expectation set:

```text
ExpectedCall
├── matcher: role + objective + call ordinal + required input digests
└── behavior: return(outcome, optional release gate) | pend until cancelled
```

On invocation it:

1. finds exactly one unclaimed matching expectation;
2. atomically claims it before the first await and records a sanitized request observation;
3. waits on its optional deterministic release gate;
4. either marks normal completion and returns the scripted semantic outcome, or remains pending with a drop guard for the cancellation case.

It fails the test on no match, multiple matches, duplicate call, unexpected call, or unclaimed expectation. At teardown, every ordinary expectation must also have completed; a `pend until cancelled` expectation must instead have fired its drop guard. It must not sleep for real time, use randomness, call the network, inspect ambient environment, or assign production IDs.

### 11.2 Concurrency without order flakiness

Do not script workers as “next response A, then next response B,” because two concurrent tasks can arrive in either order. Match them by their distinct objective and role. Use release gates to control completion order explicitly:

```text
root call 1 -> Delegate(worker-a, worker-b)
worker-a    -> wait gate A -> Finish(A)
worker-b    -> wait gate B -> Finish(B)
root call 2 -> Finish(final)
```

The test releases B before A and proves:

- completion order may differ from spawn order;
- Event sequences remain total and monotonic;
- RunView child ordering is stable by spawn sequence, not completion race;
- root call 2 is impossible until both child terminal Events exist.

For cancellation, use a fake Provider future with a drop guard that notifies the test when dropped. This proves that cancellation reaches the actual in-flight future rather than merely changing the RunView.

### 11.3 Time and identity

Do not create public Clock or ID traits solely for tests. Tests discover generated IDs from Events and assert relationships, uniqueness, ordering, and replay equality rather than fixed literal values. Use Tokio's dev-only `test-util` feature and call `tokio::time::pause()` inside an ordinary test's explicitly constructed current-thread runtime for deterministic deadline paths. This preserves production's no-macro runtime lifecycle. [`tokio::time::pause`](https://docs.rs/tokio/latest/tokio/time/fn.pause.html) **Fact**

This preserves Provider as the only Engine behavior seam.

## 12. Live test opt-in

Cargo/Rust supports ignored tests that run only when explicitly requested. [Rust test execution](https://doc.rust-lang.org/book/ch11-02-running-tests.html#ignoring-some-tests-unless-specifically-requested) **Fact**

Put one ignored test in `tests/team_run.rs`:

```text
live_openai_team_run
```

Run it with:

```text
cargo test --test team_run live_openai_team_run -- --ignored --exact
```

Requirements:

- `OPENAI_API_KEY` and `ARANY_OPENAI_MODEL` must be present; otherwise the explicitly selected test fails immediately with a fixed setup instruction that prints neither value.
- use a temporary Workspace and temporary state directory;
- use two tiny deterministic text fixtures;
- send `store: false`, no Tools, and the same strict semantic schema as production;
- assert structural invariants only: one root, two workers, join-all, terminal root synthesis, valid Events, replay equality, non-empty bounded results;
- do not assert exact natural-language text or latency;
- never run by default in CI or `cargo test`;
- do not retry automatically;
- document that it incurs provider cost.

One successful live smoke proves connectivity and current adapter compatibility, not model quality or release reliability.

## 13. Smallest verification matrix

Keep all end-to-end scenarios in `tests/team_run.rs`; Cargo compiles files under `tests/` as integration-test crates, and one file avoids a separate executable per scenario. [Cargo test targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#tests) **Fact** Use `tempfile::TempDir` for isolated Workspace/state roots; it deletes the directory when dropped. [`tempfile::TempDir`](https://docs.rs/tempfile/latest/tempfile/struct.TempDir.html) **Fact**

The minimum release-blocking matrix is:

| Area | Required cases |
|---|---|
| happy path | root delegates two workers, workers finish in controlled reverse order, root synthesizes, Run finishes |
| loop invariants | same loop handles root/workers; children cannot delegate; third child rejected; root cannot finish early; fifth provider call rejected |
| instructions | `AGENTS.md` wins; `CLAUDE.md` only on NotFound; both absent allowed; invalid/oversized/symlink `AGENTS.md` fails without fallback |
| includes | exact files only; order preserved; duplicates, absolute, `.`, `..`, symlink component, external escape, non-regular, invalid UTF-8, per-file and aggregate overflow rejected |
| snapshot | Provider observes only immutable bytes and digests captured before Run start; later path modification does not alter request |
| state | event sequences monotonic; replay after reopening SQLite equals live RunView; show of unknown/corrupt Run fails safely |
| rendering | human stdout/stderr golden output; JSONL parses line-by-line; JSONL facts equal human RunView facts; untrusted ANSI/control input cannot affect terminal |
| credentials | missing/empty key and model fail before Run; secret never appears in Debug, Event, human error, or JSON diagnostic |
| provider failures | 401, 429 temporary, 429 quota, 503, oversized body, invalid schema, incomplete response, request deadline |
| cancellation | cancel before delegation; cancel with two pending workers; second-signal/cleanup-timeout abort; no leaked task; replay shows cancellation |
| limits | exact boundary accepted and boundary + 1 rejected for every byte/count/result/event bound |
| CLI | help/version 0; syntax/config 2; success 0; runtime failure 1; Ctrl-C 130; state-dir/model precedence |
| live | one ignored OpenAI structural smoke |

Unit tests inside the owning source files cover pure parsers, bounds, reducer transitions, sanitizer, and diagnostic formatting. The integration file proves composition. No property-testing, snapshot framework, mock HTTP server crate, or benchmark harness is required initially; plain fixtures and fake Provider behavior are enough.

### 13.1 Why not `assert_cmd` initially

`assert_cmd` can locate Cargo-built binaries, configure environment/current directory, impose timeouts, and assert output/status. [`assert_cmd`](https://docs.rs/assert_cmd/latest/assert_cmd/) **Fact** It is useful once binary-level scenarios grow. For the first slice, call the Engine and renderer in-process for fake injection, and use Cargo's `CARGO_BIN_EXE_arany` only for the small parsing/exit-code smoke if needed. Cargo documents that variable for integration tests. [Cargo targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#integration-tests) **Fact**

**Trigger:** add `assert_cmd` when three or more binary-level process tests repeat setup or when real Ctrl-C delivery must be tested as an OS process rather than through the supervisor API.

## 14. Success and performance thresholds

Model generation dominates live latency, so the local harness should be both bounded and measurably negligible. Do not put hardware-sensitive millisecond assertions in ordinary unit tests. Record a small release measurement on one named reference environment.

### 14.1 Functional success

The demonstrator is successful only when all of these are true:

1. The deterministic team scenario passes 100 consecutive times with identical semantic Events and RunView after normalizing generated IDs/timestamps.
2. Root delegates exactly two bounded objectives; both workers use the same loop; root synthesis begins only after both terminal child Events.
3. Human and JSONL renderers expose the same Run, AgentRun, state, summary, result, and order facts.
4. Replay after process/connection teardown is exactly equal to the live terminal RunView.
5. All boundary and failure cases in the matrix pass on Linux; path/cancellation subsets pass on every claimed platform.
6. Cancellation drops every pending fake Provider future, empties the JoinSet, records terminal state, and returns 130.
7. The ignored live OpenAI smoke passes once with an explicitly selected model before the adapter is called working.
8. Secret canary scanning finds zero API-key bytes in stdout, stderr, Events, SQLite bytes, panic output, and test snapshots.

### 14.2 Local performance gates

On a named developer reference machine, release build, warm filesystem, fake Provider:

| Measurement | Required threshold | Method |
|---|---:|---|
| CLI start to `RunStarted` with two 32 KiB files | p95 <= 50 ms over 100 runs | wall-clock external runner |
| complete fake two-worker Run including SQLite commits | p95 <= 100 ms over 100 runs | wall-clock external runner |
| `show` replay for 100 Events | p95 <= 25 ms over 1,000 runs | wall-clock external runner |
| cancellation request to terminal cancellation with cooperative fake | p95 <= 100 ms over 100 runs | monotonic clock |
| peak resident memory for 256 KiB included input | <= 32 MiB above idle baseline | platform measurement |
| stored Event payload | never exceeds 64 KiB | invariant test |

These are gates, not permanent product promises. If a threshold fails, profile before adding caches, snapshots, channels, or a daemon. Provider live latency has no fixed threshold in this phase because it is dominated by model and network behavior; instead record time-to-first/complete response and distinguish it from local pre/post-processing.

### 14.3 Cost bound

The request-count and output-token constants give a visible ceiling of four requests and 16,384 requested output tokens per Run. Actual input and output usage from every provider response is persisted as safe numeric metadata and summarized at Run finish. The CLI should show usage after the final answer on stderr, never hide it in verbose mode.

## 15. Failure-mode contract

| Failure | Persisted state | Diagnostic | Exit |
|---|---|---|---:|
| clap syntax/help | no Run | clap help/error | `0`/`2` |
| missing/empty model or key | no Run | safe config code | `2` |
| Workspace/instruction/include rejected | no Run | relative path + bound/type class | `2` |
| state DB cannot open | no Run | state error without payload | `1` |
| Provider auth/model/schema/rate/transport failure | failed AgentRun and Run when store writable | typed safe provider class | `1` |
| Provider timeout | timed-out AgentRun/Run | deadline | `1` |
| whole-Run timeout | cancelled children, timed-out Run | 300 s bound | `1` |
| first Ctrl-C | cancelled active agents and Run | cancellation | `130` |
| second Ctrl-C/drain deadline | aborted/reaped tasks, supervisor writes cancelled state best-effort | forced cancellation | `130` |
| SQLite append fails after Run start | durable prefix only; no false terminal claim | store failure | `1` |
| stdout/stderr write fails | cancel active Run, persist best-effort | no recursive output attempt on failed stream | `1` |
| malformed/corrupt stored Event on show | no mutation | sequence/kind class, never raw payload | `1` |
| unknown Run ID | no mutation | not found | `1` |

If the store fails during terminalization, return 1 even for Ctrl-C because the cancellation record is not durable. Do not claim successful or clean cancellation when canonical state could not be written.

## 16. Minimal implementation shape

Keep the four-file core proof, then add one private module in the optional OTLP patch:

```text
src/main.rs
  clap types
  env/config resolution
  human and JSONL writers
  signal-to-Engine cancellation

src/lib.rs
  fixed Limits constants
  Workspace snapshot resolver
  instruction/include validation
  cancellation supervisor
  RunView and diagnostic types

src/provider.rs
  Provider trait
  strict scripted fake under test support
  OpenAI HTTP adapter
  SecretString credential ownership

src/store.rs
  state-path resolution helper
  SQLite append/replay

src/telemetry.rs
  Disabled | Otlp concrete mode
  typed privacy allowlist and span mapping
  bounded OTLP/HTTP-protobuf export and shutdown

tests/team_run.rs
  deterministic matrix
  ignored live smoke
```

`Workspace`, renderer, limits, cancellation, and config are private functions/types until their implementation gains enough depth to deserve a file. `cap-std`/`cap-fs-ext` are mechanisms hidden inside `lib.rs`, not adapter traits. `directories` is a path helper hidden by `store.rs`, not a state-location interface.

Suggested direct dependencies earned by the minimum behavior:

- `clap` with derive;
- `tokio` plus `tokio-util` for runtime, signal, time, task tracking, and cancellation;
- `serde`/`serde_json` for Event and provider schemas;
- `reqwest` with Rustls and bounded incremental response-body reads;
- `rusqlite` with the repository's selected SQLite linkage policy;
- `cap-std` and `cap-fs-ext` for handle-relative no-follow Workspace access;
- `directories` for platform state location;
- `secrecy` for credential handling;
- the selected SHA-256 implementation for required snapshot digests.
- `opentelemetry`, `opentelemetry_sdk`, and `opentelemetry-otlp` with trace-only minimal features for the OTLP patch.

Dev dependency:

- `tempfile`;
- `opentelemetry_sdk` with the `testing` feature for in-memory span assertions; and
- Tokio's `test-util` feature for paused time in ordinary tests with an explicitly constructed current-thread runtime.

Do not add `crossterm`, `unicode-width`, `indicatif`, `console`, `keyring`, `dotenv`, `figment`, `config`, `tracing-subscriber`, `assert_cmd`, `insta`, `proptest`, or a tokenizer in the first implementation.

## 17. Triggers for future configurability

Add a setting only when its named trigger occurs:

| Trigger | Then add |
|---|---|
| users repeatedly need different limits for valid repositories | one named limit with validated range; do not expose the entire Limits struct |
| exact context failures occur despite byte bounds | model-aware local token counter or bounded count endpoint, with measured latency |
| temporary 429/503 materially harms completion | bounded two-attempt retry policy with recorded attempts |
| users need persisted credential UX or OAuth | `auth` commands and platform credential store |
| a second hosted Provider ships | explicit `--provider` and provider-scoped model config |
| a deployment requires an enterprise egress proxy | explicit proxy URL/trust configuration with redaction and credential-path tests |
| append-only progress is proven insufficient | TTY-only renderer behind the same RunView, then width/resize/color policy |
| terminal color materially improves attribution | `--color auto|always|never` and `NO_COLOR` precedence |
| large output is common | Artifact storage and Event references; do not just raise 64 KiB rows |
| event replay misses its 25 ms/100-Event gate at real scale | measured snapshot strategy |
| three binary-level process tests repeat machinery | `assert_cmd` or a small process-test helper |
| users need repeatable per-project defaults | one explicit project config path and schema; no implicit upward search until specified |
| remote/daemon Client exists | protocol-level cancellation, subscriptions, authentication, and detached-run policy |
| first external process Tool ships | process-tree supervision and the separate deterministic Guard before release |
| users must bypass a local Collector | direct-remote OTLP design with explicit TLS, authentication, secrets, proxy, and SSRF policy |
| a daemon or aggregate SLO cannot be served by traces | bounded OTLP metrics design |
| a concrete field failure cannot be diagnosed from Events, traces, and safe diagnostics | separately allowlisted structured logs; never mirror Event payloads |

## 18. Concrete version-1 contract

An implementer should be able to work from this checklist without another design decision:

### Commands and configuration

- [ ] Implement only `run` and `show`.
- [ ] Support global `--state-dir`; support `ARANY_STATE_DIR` beneath it in precedence.
- [ ] Support `--workspace`, repeated `--include`, `--model`, `--otlp-endpoint`, and `--jsonl` on `run`.
- [ ] Resolve model as CLI then `ARANY_OPENAI_MODEL`; error if absent/empty.
- [ ] Resolve OTLP as CLI, trace-specific environment, generic environment, then disabled; accept only numeric-loopback `http` and reject unsupported OTLP variable names without reading their values.
- [ ] Read credential only from non-empty `OPENAI_API_KEY` into `SecretString`.
- [ ] Use four exit codes: 0, 1, 2, 130.

### Inputs and bounds

- [ ] Validate all inputs before `RunStarted` and before provider I/O.
- [ ] Use one pinned Workspace `Dir` and no-follow component traversal.
- [ ] Reject every symlink/reparse component and every non-regular final input.
- [ ] Read once from the opened handle, cap with `limit + 1`, validate UTF-8, hash exact bytes.
- [ ] Load exact root `AGENTS.md`; use exact root `CLAUDE.md` only on `NotFound`.
- [ ] Enforce the fixed bounds table without silent truncation.

### Provider and cancellation

- [ ] Use fixed OpenAI Responses endpoint, `store:false`, truncation disabled, strict schema, 4,096 output tokens.
- [ ] Use HTTPS-only Rustls transport with no redirect, retry, inherited proxy, cookies, or unsafe TLS override; mark Authorization sensitive.
- [ ] Permit at most four Provider calls, two workers, and two concurrent worker futures.
- [ ] Use 120-second call and 300-second Run deadlines with zero retry.
- [ ] Propagate a root cancellation token to every child and prioritize it at awaits.
- [ ] Drain for two seconds; second Ctrl-C or deadline aborts and reaps the JoinSet.
- [ ] Persist terminal state through the supervisor, then return rather than calling `process::exit`.

### Output and safety

- [ ] Human progress is append-only stderr; final root result is stdout.
- [ ] JSONL stdout is one compact persisted Event plus newline, flushed per Event.
- [ ] JSONL stderr diagnostics use a distinct typed envelope.
- [ ] Emit no color, cursor control, alternate screen, width calculation, or resize handling.
- [ ] Escape or reject ANSI/ECMA-48 and OSC sequences, C0/C1 controls, carriage return, backspace, bidirectional controls, and forged line prefixes; allowlist diagnostic fields.
- [ ] Never persist or render credential, headers, raw provider body, inherited environment, or included file contents.

### Optional OTLP patch

- [ ] Add it only after the core team-run, replay, and cancellation proof passes.
- [ ] Keep OpenTelemetry types inside `telemetry.rs`; define no telemetry trait or global subscriber.
- [ ] Export traces only, with the exact nine-span successful topology and typed content-free fields.
- [ ] Disable redirects and ambient proxies; keep remote TLS, authentication, secrets, and routing in the local Collector.
- [ ] Bound queue, batch, request, retries, memory, and post-runtime shutdown exactly as the OTLP report specifies.
- [ ] Treat an unavailable Collector, queue drop, export error, or shutdown timeout as a safe non-fatal diagnostic.

### Proof

- [ ] Implement the strict gated fake without a public fake CLI mode.
- [ ] Put the deterministic matrix and ignored OpenAI smoke in `tests/team_run.rs`.
- [ ] Run the happy path 100 times and compare normalized semantic output.
- [ ] Prove path resolution on every claimed platform.
- [ ] Prove secret canary absence across output and persisted bytes.
- [ ] Measure the four local latency gates in release mode on a named machine.
- [ ] Prove telemetry topology, canary absence in spans and encoded protobuf, loopback protocol behavior, cancellation progress, thread cleanup, saturation, and disabled-path overhead.

No research question remains open for the minimum CLI. The cross-platform path suite, performance measurements, and live OpenAI smoke are implementation gates because they require the real code and runtime environment; they are not reasons to add more architecture before the tracer bullet exists.

## Primary sources

### Rust and CLI

- [`clap` derive reference](https://docs.rs/clap/latest/clap/_derive/)
- [`clap::Error`](https://docs.rs/clap/latest/clap/error/struct.Error.html)
- [`std::io::IsTerminal`](https://doc.rust-lang.org/std/io/trait.IsTerminal.html)
- [`std::process::ExitCode`](https://doc.rust-lang.org/std/process/struct.ExitCode.html)
- [Cargo test targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#tests)
- [Rust ignored tests](https://doc.rust-lang.org/book/ch11-02-running-tests.html#ignoring-some-tests-unless-specifically-requested)
- [`assert_cmd`](https://docs.rs/assert_cmd/latest/assert_cmd/)

### Terminal and data formats

- [NO_COLOR](https://no-color.org/)
- [JSON Lines](https://jsonlines.org/)
- [`unicode-width` rules](https://docs.rs/unicode-width/latest/src/unicode_width/lib.rs.html)
- [`UnicodeWidthChar`](https://docs.rs/unicode-width/latest/unicode_width/trait.UnicodeWidthChar.html)
- [`crossterm` events](https://docs.rs/crossterm/latest/crossterm/event/)
- [`crossterm::terminal::size`](https://docs.rs/crossterm/latest/crossterm/terminal/fn.size.html)

### Filesystem and platform directories

- [`std::fs::canonicalize`](https://doc.rust-lang.org/std/fs/fn.canonicalize.html)
- [`std::fs::File`](https://doc.rust-lang.org/std/fs/struct.File.html)
- [`cap-std` README](https://github.com/bytecodealliance/cap-std/blob/main/README.md)
- [`cap_fs_ext::DirExt`](https://docs.rs/cap-fs-ext/latest/cap_fs_ext/trait.DirExt.html)
- [`cap_fs_ext::FollowSymlinks`](https://docs.rs/cap-fs-ext/latest/cap_fs_ext/enum.FollowSymlinks.html)
- [Bytecode Alliance `cap-std` advisory GHSA-hxf5-99xg-86hw](https://github.com/bytecodealliance/cap-std/security/advisories/GHSA-hxf5-99xg-86hw)
- [Linux `openat2(2)`](https://man7.org/linux/man-pages/man2/openat2.2.html)
- [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir/0.8/)
- [Apple Application Support guidance](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/FileSystemProgrammingGuide/MacOSXDirectories/MacOSXDirectories.html)
- [Microsoft known folders](https://learn.microsoft.com/windows/win32/shell/knownfolderid)
- [`directories::ProjectDirs`](https://docs.rs/directories/latest/directories/struct.ProjectDirs.html)

### Credentials and provider

- [OpenAI production best practices](https://developers.openai.com/api/docs/guides/production-best-practices)
- [OpenAI SDKs and CLI](https://developers.openai.com/api/docs/libraries)
- [OpenAI Responses API](https://developers.openai.com/api/reference/cli/resources/responses/methods/create)
- [OpenAI Structured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs)
- [OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits)
- [OpenAI error codes](https://developers.openai.com/api/docs/guides/error-codes)
- [OpenAI token counting](https://developers.openai.com/api/docs/guides/token-counting)
- [OpenAI data controls](https://developers.openai.com/api/docs/guides/your-data)
- [`reqwest::ClientBuilder`](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html)
- [`reqwest::retry::never`](https://docs.rs/reqwest/latest/reqwest/retry/fn.never.html)
- [`secrecy`](https://docs.rs/secrecy/latest/secrecy/)
- [Apple Keychain Services](https://developer.apple.com/documentation/security/keychain-services)
- [Microsoft Credentials Management](https://learn.microsoft.com/en-us/windows/win32/secauthn/credentials-management)
- [Secret Service API](https://specifications.freedesktop.org/secret-service/latest-single/)

### Async runtime and testing

- [Tokio graceful shutdown](https://tokio.rs/tokio/topics/shutdown)
- [`tokio::signal::ctrl_c`](https://docs.rs/tokio/latest/tokio/signal/fn.ctrl_c.html)
- [`CancellationToken`](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html)
- [`tokio::select!`](https://docs.rs/tokio/latest/tokio/macro.select.html)
- [`tokio::time::timeout`](https://docs.rs/tokio/latest/tokio/time/fn.timeout.html)
- [`tokio::task::JoinSet`](https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html)
- [`tokio::test`](https://docs.rs/tokio/latest/tokio/attr.test.html)
- [`rusqlite::Connection::busy_timeout`](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html#method.busy_timeout)
- [`tempfile::TempDir`](https://docs.rs/tempfile/latest/tempfile/struct.TempDir.html)
