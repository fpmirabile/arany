# Native macOS process unit

Load when changing `src/tools/guard/macos/unit.rs`. [Guard](./guard.md) owns the
native worker and FFI constraints; [Tools](../docs/specs/tools.md) owns effects,
resource semantics and unavailable-capability rejection.

- One temporary Background launchd job owns a distinct kernel resource coalition.
  Bind its reported PID, socket peer credentials, process unique ID and coalition
  before accepting a task. Keep the private control root outside the Workspace.
- Use the shared [private filesystem admission](./private-fs.md) without creating
  State records or history locks. Retain its directory handle for the unit lifetime.
  Read the newline-delimited task with bounded buffering, capped at 96 KiB plus
  its delimiter; reject truncation and buffered trailing input before Ready.
- Serialize the fixed typed job dictionary with Core Foundation into a bounded
  binary plist; never interpolate markup. Create it exclusively with mode 0600
  inside the private control root. Arguments remain separate literal strings.
- Observe coalition membership through bounded kernel metadata, never process
  arguments or environments. Filter foreign coalitions before full identity
  queries, then revalidate each admitted member. A process unique ID survives
  exec; audit-token versions can change. Resource samples retain the process identity across exec,
  while every signal selects the exact current PID/version atomically. Revalidate
  identity after failed resource samples too: confirmed reaping or PID reuse
  retires that member; unavailable observation of a live member still rejects.
- Convert native CPU counters through the validated Mach timebase. Aggregate
  resident memory and live threads, and enforce the accepted per-process CPU
  threshold outside the payload. Missing live observation fails closed.
- Cleanup repeatedly stops and kills current coalition members with audit-token
  signals and requires positively observed emptiness within its deadline. Job
  removal alone does not terminate an escaped orphan. An exact-target `bootout`
  success or `ESRCH` is required even after automatic one-use job retirement.
- Parent disconnect, cancellation and helper failure own cleanup. No task runs
  before the authenticated one-use GO gate. A missing post-GO receipt stays
  uncertain; fixed bootstrap stages are the only diagnostic wire payload.
- The caller awaits a bounded blocking owner with its own reactor; unit teardown
  and synchronous Drop must stay off the terminal reactor. Dropping the awaiting
  future cancels the worker. Preserve the GO marker across worker failure so
  execution uncertainty is never reported as a pre-dispatch rejection. Cleanup
  polls at the shared supervision interval rather than spinning with yield.
- Private storage uses two fixed-size, case-sensitive HFS+ images. Require
  kernel-only attachment: killing a userspace disk-image helper before unmount
  causes an improper-eject warning. Validate opened mounts, ownership, writable
  `NOSUID` flags and exact capacity. Terminate and positively empty the payload
  coalition before detaching either fixed mount, including partial preparation
  and unwind. Never recursively remove a mounted or uncertain control root;
  retain an unfinished-attachment marker when cleanup cannot be established.
  Create that marker immediately before attachment, after image creation and
  validation. Never sweep roots by filename prefix: a retained uncertain root
  requires positive ownership and device/mount evidence before deletion.
- The activated native unit owner exercises escaped-descendant cleanup,
  stale-token refusal and actual aggregate memory/task and ignored-signal CPU
  termination, fixed-capacity exhaustion, supervisor termination and storage
  cleanup during unwind. Its private command phase exercises the checked
  snapshot, compiled native code and real MCP, with denied host/network/signal
  and coalition/persona operations. Its signed-actor phase proves dynamic
  entitlement denial after a successful unsandboxed baseline while ordinary
  launch remains available; it does not confer an Apple private entitlement.
- Emit bounded fixed-stage failures while the owned control writer is live.
  Dropping Tokio's owned Unix write half shuts down the shared socket; a cloned
  descriptor cannot publish a failure afterward. Retire the bootstrap diagnostic
  clone before entering the reactor; publish its result through the live writer
  exactly once. Require positive helper exit
  before inspecting its exact completed-test summary; control EOF may precede
  the test runner's final output.
