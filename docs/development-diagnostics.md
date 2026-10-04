# Local development diagnostics

`cargo run` uses this repository's development build; `cargo run --release` uses its optimized build. Development chat and `exec` record subscription failures in `development.log` beneath their admitted Session State directory, outside the Workspace. With an explicit `--state-dir`, that is `STATE_DIR/development.log`; Linux's ordinary default is `$XDG_STATE_HOME/arany/development.log`, or `$HOME/.local/state/arany/development.log` when that variable is absent. This Session path is distinct from the stable OS-user account root. The optimized build does not enable this log.

For a deliberate account retry, start the development executable normally:

```sh
cargo run --locked
```

An actual submitted task can consume plan usage. Arany does not retry, switch billing routes, read another harness's credentials or run a preliminary model check. Existing failed journals cannot reconstruct this new diagnostic detail: the log records only subsequent observed failures.

The log distinguishes transport/deadline errors, HTTP rejection, response media/encoding/length checks, EOF before `response.completed`, stream bounds, SSE framing/JSON/type rejection, `response.incomplete`, a remote failed event, completed-response/outcome/usage rejection, and the local output cap. It includes a local timestamp, process ID, accepted stream-byte/event counters where available, numeric HTTP status when observed, and a bounded local stack trace. This is not an upstream stack trace or the full asynchronous request history.

Only compiled categories and counters enter the record. Tokens, headers, raw errors, request/response bodies, model/account identities, prompts, answers and environment dumps are excluded. Stack source locations are omitted. A failed remote event's raw message/code is not logged. Ordinary expected Provider failures do not panic, so their captured stack is the rejection call site rather than a panic stack. There is no panic-payload or HTTP-body logger.

One no-follow, single-link, owner-checked `0600` file is capped at 256 KiB; each record is capped at 24 KiB. At capacity the next record truncates older diagnostics. Busy locks, unsafe files or storage errors discard diagnostics without changing the Run result; the file is best-effort and not crash-durable. File modes do not encrypt it or exclude another same-user or privileged process. Nothing is sent over the network or printed into chat, stdout, stderr or JSONL. Help/version, `show`, the credential helper and Tool Guard do not enable it.

The user's repeated live subscription failure remains undiagnosed. Offline product tests prove that interrupted and incomplete streams remain rejected and produce distinct safe diagnostics in development, while optimized processes keep the same rejection and channel/replay contract without logging. A new actual failure can narrow the investigation; it does not justify accepting partial output or bypassing identity, TLS, endpoint or usage checks.
