# Native Guard

Load when changing `src/tools/guard.rs` or its native adapters. [Tools](./tools.md)
owns Policy and effects; [the Tool spec](../docs/specs/tools.md) owns observable
limits and compatibility.

- Keep native enforcement private to the separate same-binary worker. The macOS
  adapter admits typed files and pinned Skill data only; it must reject command
  and MCP grants before Provider invocation. This file worker's fork/spawn/exec
  prohibition does not replace the required ordinary command-launch contract.
- Close inherited descriptors at the single-threaded worker entry before Rust
  opens persistent handles. Reset and unblock SIGXCPU and install inherited hard
  limits before Seatbelt prohibits changes. Attest denial before GO; no payload
  may run under a partially installed profile.
- Retain the exact owned child until reap. Stop PID observation immediately
  after reap. Darwin task information disappears during exit; BSD inspection
  must include zombies (`arg = 1`) and positively establish kernel exit before
  retiring resource observation. Missing live-process information fails closed.
- Only the native adapter allows unsafe Rust. Match the installed SDK and libc
  ABI, validate exact initialized output sizes, bound descriptor/profile buffers,
  retain C string lifetimes through each call and free native error allocations
  once. Never parse or print arbitrary native diagnostics. The syscall-number
  filter and explicit Darwin syscall numbers support only reviewed 64-bit ABIs.
- A dedicated FFI review covers allocation, alignment, initialization, ownership,
  variadic argument types, signal state and unreaped PID lifetime. Native profile
  conformance exercises denied host file/network/spawn access and immutable CPU
  controls in isolated child processes; the activated CPU owner proves actual
  expiry. The native Engine journey owns successful typed effects and closed
  replay. Passing either alone does not establish the other boundary.
