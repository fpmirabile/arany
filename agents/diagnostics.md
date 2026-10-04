# Development diagnostics

Load for `src/diagnostics.rs`, local debug-log admission or Provider failure instrumentation. [security.md](./security.md) owns secret exclusion; [store.md](./store.md) owns private file admission. User operation is documented in [development diagnostics](../docs/development-diagnostics.md).

- Enable the concrete diagnostic owner only after ordinary attached Session or `exec` admission. Help/version, read-only inspection and internal helpers keep their existing side-effect and channel contracts. Optimized builds leave it disabled.
- Record compiled failure stages and numeric counters, never arbitrary strings, upstream errors, bodies, headers, identifiers, requests or conversation data. Stack frames omit source locations and controls; a local call stack is not an upstream or async causal stack.
- Keep the fixed log and each record bounded through checked StateRoot handles. Nonblocking contention, unsafe files and write failures drop diagnostics rather than changing a Provider disposition. This lossy local log is neither canonical state nor telemetry; do not export it or feed it into context.
- Instrumentation must preserve strict acceptance, deadlines, billing isolation and replay categories. A synthetic distinction between causes does not establish the cause or repair of a user's live failure.
