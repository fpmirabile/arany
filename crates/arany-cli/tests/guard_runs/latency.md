# Linux Tool latency corpus

The explicitly ignored `guard_runs::linux_tool_latency_corpus` reports single-Read,
four-Read-step, admitted in-process Edit and guarded native no-op Command rows offline.
This is measurement evidence for [Tools](../../../../docs/specs/tools.md)
and [Execution](../../../../docs/specs/execution.md#committed-event-stream), not a
new runtime deadline, CLI reporting feature or native support claim.

## Measurement

`tool_receipt_to_commit_us` is a monotonic interval from the Engine receiving a
complete typed primary Provider response to the Store acknowledging the step's
final `ToolFinished` append. It includes validation, permission journaling,
execution and result fitting; Command includes Guard preparation, selection, GO,
execution, capture and cleanup. Provider inference, transport, startup, final
answer, hooks, rendering and `show` are outside it. The four-Read row runs its
reads concurrently. The recorder exists only under `test-support`.

Each sample uses fresh private State, Workspace and runtime, then audits the
committed result, strict replay of the reopened journal and a fresh `arany show`.
`journal_interval_ms` reports persisted wall-clock differences, sampled before
commit; negative differences become `clock_invalid`. Monotonic durations cannot be
rebuilt from journal bytes, so preserve `report.json` with the closed journals.

Percentiles are nearest-rank (`ceil(p*n)-1`) over measured successes in attempt
order. Only the single-Read and Edit rows carry the provisional target verdict;
the four-Read and Command rows report null objective and verdict, and the Command
objective is compared under the [owner procedure](#owner-procedure). Denial,
failure, cancellation, uncertainty or a missing acknowledgement is never a
sample: the invocation stops without retry and emits no percentile or verdict. A
missed target requires diagnosis, not a higher threshold. [The decision register](../../../../docs/research/next-step-decision-register.md#d-23--tool-latency-measures-durable-acknowledgement)
owns the rationale and peer comparison.

## Bounds and retained evidence

Warm-up, measured and retained-sample counts are named in
[`test_support/latency.rs`](../../../../crates/arany/src/test_support/latency.rs);
the target, invocation timeout and report, sample and evidence caps are named in
[`latency.rs`](./latency.rs). Overlapping or unmatched steps and any overflow
invalidate capture. Warm-up fixtures are removed only after replay and inspection
succeed; measured and failed fixtures remain private.

## Owner procedure

Use the named host, the owner's Arch Linux workstation (owner decision 2026-10-28),
on stable power and local disk. The accepted no-op Command objective is host-specific:
each of the three invocations' Command p95 must not exceed `PROVISIONAL_P95_US` in
`latency.rs`, the same value as the Read/Edit objective, with no separate number. The
owner compares each report's `p50_p95_max_us` p95 against it and records the result and
any miss with the run artifact; the report's `provisional_p95_objective_us` and
`objective_met` stay null for Command. No
sandbox bypass or fake Guard is allowed. Build in this worktree's `target` or
through the team Cargo shim; never put build output in the evidence directory or
set `CARGO_TARGET_DIR`. The corpus refuses debug execution, unsupported hosts and
tmpfs evidence storage. A missing prerequisite is a failed invocation.

Review the selected revision's native namespace bootstrap and obtain its required
host authorization before running Command. The [namespace-entry contract](../../../../docs/specs/tools.md#ubuntu-namespace-entry-diagnosis)
owns the administrator prerequisite, including executable-specific profiles on
restricted Ubuntu hosts. The owner runs any required exact administrator commands
by hand; this corpus never changes host policy. Installing Bubblewrap alone is
insufficient. Stock-policy refusal is failed measurement evidence, with no host
command, retired-copy or fake-Guard fallback. Use the implementation and overlay/cache
profile present at the selected revision. The helper records the canonical no-op
pin and device/inode with its prerequisites and effective bounds.

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
ownership, and keep failed or partial evidence. [Next steps](../../../../NEXT_STEPS.md#daily-use-first)
owns the remaining native evidence.

The existing dispatch-only measurement remains separate and unchanged:

```sh
cargo test -p arany --lib --release --locked --offline tools::read_tests::in_process_repeated_reads_all_succeed_on_a_large_file -- --exact --nocapture --test-threads=1
```

Cold disk, long history, large snapshots, concurrent load, live Provider latency
and macOS latency are outside this corpus. Linux measurements cannot establish
those claims.

## Git write rows

The Git corpora emit separate `Git write latency ` JSON lines. These are single
diagnostic samples, without warm-ups, percentiles, objectives or timing assertions;
they do not join the four-row Linux report above. Retain the complete successful
test output with the exact revision, toolchain, OS, debug/release profile, device
and load. A partial or failed journey is invalid evidence, even if it printed a row.

On macOS the existing branch, commit and switch journeys emit six rows: one
branch creation, paths and staged commits, a switch, its same-branch no-op and
the return switch. They use the same opt-in receipt-to-durable-commit recorder
described above. The interval includes both unit envelopes, view preparation,
Git, bounded frame relay, executor teardown, publication and publisher cleanup;
the no-op row still traverses both units. It also includes the fixture's immediate
one-use approval response, with no human interaction or Provider inference.
Each sample is matched to its single Tool and audited against Events reopened
after Engine closure, beside the existing receipt, unchanged staging and
replay assertions. Unit/volume cleanup is checked outside the timed interval too.
The recorder classifies these calls as `Other`; the Git row checks the successful
disposition and durable endpoint explicitly and never uses the Read/Edit target
or percentile classifier. No new process, fixture, retry or storage budget is added.

Owner-run capture on macOS: select exactly these three parent tests individually
with `--exact --nocapture` in both debug and release, so the forwarded rows are
printed rather than captured by the broad CI suites:

```text
tools_git::guarded_native_git_branch_creation_publishes_through_two_units_and_replays
tools_git::guarded_native_git_commit_publishes_through_two_units_and_replays
tools_git::guarded_native_git_switch_publishes_through_two_units_and_replays
```

Each selection must complete one parent and its existing supervised helper.
Release uses CI's disposable account; a debug account override is never release
evidence. These rows measure the small journey fixtures, not the maximum switch.

On Linux, `tools::git::write::frame::tests::largest_switch_and_long_paths_fit_the_derived_frame_bound`
adds a `largest_switch_frame` row to its existing boundary scenario. It measures
only the framed write/read round-trip, including encoding, chunk hashing,
verification and decoding, with the maximum index, aggregate replacement bytes,
file count, ref count and admitted path lengths. Fixture construction and the
extra equality digests stay outside the interval. Its existing round-trip and
overflow assertions remain authoritative; the production Linux path continues
through this codec. Run exactly that one library test with `--release --exact --nocapture`
(owner-run, not a CI lane). The reported sizes derive from the owning constants.
This codec row does not measure Git execution or filesystem publication and
cannot be compared to the macOS receipt-to-commit rows. Real-device approval and
Seatbelt observations remain in [next steps](../../../../NEXT_STEPS.md#daily-use-first).
