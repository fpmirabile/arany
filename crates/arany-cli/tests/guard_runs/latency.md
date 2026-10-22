# Linux Tool latency corpus

The explicitly ignored `guard_runs::linux_tool_latency_corpus` reports the Read
row offline. Guarded Edit and command rows remain in
[slice 2](../../../../planning/linux-tool-latency/README.md#2-pr-complete-the-same-report-with-guarded-edit-and-command-rows).
This is measurement evidence for [Tools](../../../../docs/specs/tools.md#acceptance-scenarios-and-evidence-owners)
and [Execution](../../../../docs/specs/execution.md#committed-event-stream), not a
new runtime deadline, CLI reporting feature or native support claim.

## Measurement

`tool_receipt_to_commit_us` uses `std::time::Instant`. It starts when the Engine
receives a complete typed primary Provider response, before response validation,
`ProviderCallRecorded`, intent admission and `ToolStarted`. It ends immediately
after the Store acknowledges the matching `ToolFinished` durable append. This
includes permission journaling, admitted Read execution and result fitting. Read
uses its real in-process path; it does not launch a Guard. No timestamps come from
the lossy Journal subscription or telemetry.

Provider inference, transport and wire decoding, Engine/fixture startup, final
answer, post-result hooks, rendering and `show` are outside the interval. The
scripted Provider is the only behavior double. There are no hooks, approval waits,
telemetry, children, concurrent Tools, retries, compactions or fallback routes.
The recorder is opt-in under `test-support`, disabled by default and absent from
production builds. Its timed callbacks do no I/O or async sends and retain only
scoped identities, closed operation/outcome values, sequences and timing.

Each serial invocation has 10 labeled warm-ups followed by 100 measured Reads,
without outlier removal or replacement attempts. Every sample uses a fresh private
State and Workspace, a fresh current-thread Engine runtime, identical short
history and the existing 53,248-byte, 4,096-line fixture. Setup reads the complete
fixture before timing to warm filesystem data. The Read requests the default first
2,000 lines. The corpus checks the exact numbered page, full-file digest, committed
permission and `in_process` attestation. It closes the Engine, reopens SQLite for
strict replay and compares every JSONL envelope from a fresh `arany show` process.
Replay and inspection perform no Tool execution. SQLite journaling and durability
settings stay unchanged.

The report joins by Session, Run, agent, primary call index, intent and Event
sequences. `journal_interval_ms` separately reports persisted
`ProviderCallRecorded → ToolFinished` and `ToolStarted → ToolFinished` differences.
`created_at_ms` is a millisecond Unix wall-clock timestamp sampled **before** reducer
validation, insertion and commit. It is not a commit acknowledgement timestamp.
Negative differences become `clock_invalid` with no reported negative interval;
the monotonic duration remains unchanged. Exact monotonic durations cannot be
reconstructed from journal bytes: preserve `report.json` with the closed journals.

Integer microsecond samples stay in attempt order. Sorting their measured-success
durations reproduces nearest-rank p50/p95/max (`ceil(p*n)-1`, p95 index 94 for 100).
Only a complete, audited invocation gets a target comparison. Invalid proposals,
denial, failure, cancellation, uncertainty and missing acknowledgements cannot
become successful samples. Failure stops the invocation without retry, preserves
partial counts/captures and emits no valid percentile or target verdict. A missed
provisional target retains a valid report; it requires diagnosis, not a higher
threshold. [The decision register](../../../../docs/research/next-step-decision-register.md#d-23--tool-latency-measures-durable-acknowledgement)
owns the rationale and peer comparison.

## Bounds and retained evidence

The recorder allows one pending measurement and at most 330 retained attempts;
overlap, unmatched completion and overflow invalidate capture. Slice 1 attempts
at most 110 Reads, two scripted calls per successful Run (a finite four-call Goal
ceiling), one 53,248-byte Workspace file, the ordinary 64 KiB Tool result bound,
32,768 Goal tokens, 120 seconds active time and one Run. The report records the
effective Tool limits and default resource settings. There is no checkpoint or
publication for Read.

The parent serially supervises each helper for at most 120 seconds and the whole
invocation for 30 minutes. Existing bounded process owners kill and reap helpers
on timeout/capture failure; `show` has its separate 10-second bound. Each captured
channel is limited to 64 KiB. Sample evidence above 8 MiB fails; the parent reserves
8 MiB before each attempt within a 256 MiB aggregate retention ceiling. Timing and
metadata reports are capped at 1 MiB. Warm-up fixtures are removed only after
closed replay and inspection succeed; measured and failed fixtures remain private.
No cleanup of failed evidence is implied by a missing timing endpoint.

## Owner procedure

Use the selected quiet native Linux host on stable power and local disk. No
sandbox bypass or fake Guard is allowed. Build in this worktree's `target` or
through the team Cargo shim; never put build output in the evidence directory or
set `CARGO_TARGET_DIR`. The corpus refuses debug execution, unsupported hosts and
tmpfs evidence storage. A missing prerequisite is a failed invocation.

Create an existing local-disk evidence directory and a `host.json` object containing
these nonempty reviewed string fields (at most 1,024 bytes each, 16 KiB total):
`date`, `revision`, `toolchain`, `host`, `cpu`, `ram`, `kernel`, `architecture`,
`filesystem`, `device`, `power`, `load`, `native_prerequisites`. Unknown fields reject;
`revision` must be the exact 40-character hexadecimal commit identity. Include core count
with CPU, storage/State/Workspace location with filesystem/device, and current
enforcement availability with native prerequisites. Supply no credentials,
environment dumps or private repository data. Metadata is owner-supplied evidence,
not automatic host attestation.

After building the release target, first list the exact selection and require
exactly one `linux_tool_latency_corpus: test`. Then run:

```sh
ARANY_LATENCY_ROOT=/absolute/local/evidence cargo test -p arany-cli --test guard_runs --release --locked --offline linux_tool_latency_corpus -- --exact --ignored --nocapture --test-threads=1
```

The report also hashes the actual product and helper executables and records the
package version. The parent launches only the exact private helper and verifies one completed test
per attempt. Its cleared environment isolates HOME, XDG roots and Git configuration;
no real Git configuration or ssh-agent is inherited. Each report lives in a new
private `arany-tool-latency-*` directory beneath the evidence root. Retain its digest,
the binary identity, revision/toolchain, invocation summary and closed-journal audit
in the owner's run artifact. Perform three separate invocations and report each
p95; do not pool them or keep only the fastest. Record target misses and diagnosis
ownership. [NEXT_STEPS item 11](../../../../NEXT_STEPS.md#daily-use-first) owns the
accepted host, objectives and remaining native evidence.

The existing dispatch-only measurement remains separate and unchanged:

```sh
cargo test -p arany --lib --release --locked --offline tools::tests::in_process_repeated_reads_all_succeed_on_a_large_file -- --exact --nocapture --test-threads=1
```

Cold disk, long history, large snapshots, concurrent load, live Provider latency
and macOS latency are outside this corpus. Linux measurements cannot establish
those claims.
