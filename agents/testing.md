# Testing

Load when continuing beta 1, adding or changing behavior, fixing a defect, editing tests or fixtures, changing user-visible output, or making a correctness, security, durability, provider, performance, or platform-support claim. For documentation-only work, use [documentation and instruction checks](#documentation-and-instruction-changes). The [testing strategy](../docs/research/testing-strategy-for-rust-cli-harness.md) explains the rationale; this file is the working contract.

## Load for the affected task

- Local verification or CI reproduction → [commands and native prerequisites](./references/testing-details.md#local-verification-and-ci).
- Release-policy workflow or dependency audits → [release-policy CI](./references/testing-details.md#release-policy-ci).
- Live Provider or named-host performance claims → [native and live evidence](./references/testing-details.md#native-and-live-evidence).
- Process, account, OS-service, or private-state fixtures → [host and account isolation](./references/testing-details.md#host-and-account-isolation).
- PTY input, signals, restoration, terminal faults, or tmux → [terminal process fixtures](./references/testing-details.md#terminal-process-fixtures).
- Storage, Workspace faults, response RSS, or paused deadlines → [storage, Workspace and resource faults](./references/testing-details.md#storage-workspace-and-resource-faults).
- Native Guard topology, PID or pidfd fixtures → [native Guard fixtures](./references/testing-details.md#native-guard-fixtures).

## Documentation and instruction changes

Use the [shared documentation checks](../docs/development.md#verification). When changing instruction routing, follow representative tasks from `AGENTS.md` (terminal output, Provider decoding, Session persistence) to their contract, editing rules, and evidence owner. Check inbound links after moving or renaming a heading. These checks establish documentation consistency, not agent quality or platform support.

## Evidence budget

- Optimize for evidence per test, not count or coverage. Add a test only for a user-visible contract, durable or security invariant, protocol boundary, concurrency or failure mode, or demonstrated regression that no existing test catches with equally useful diagnostics.
- State the owned claim and the defect the test catches. If it cannot be shown to fail for that defect, strengthen an existing test, type, or parser instead.
- Prefer a row in an existing scenario, table, corpus, or failpoint harness over a new test function or file. Keep one authoritative owner per claim; duplicate it only for materially different process, platform, durability, or security evidence.
- Refactors with unchanged behavior rely on existing tests. Do not test private helpers, getters, type-enforced facts, third-party behavior, or test-only abstractions; delete or merge tests a stronger owner subsumes.
- Record completed checks in the handoff, PR, and CI. Keep only unresolved defects and missing verification in `NEXT_STEPS.md`, removing closed items.

## User-visible proof

- Noninteractive acceptance scenarios produce an `ObservationBundle`: exit class, exact stdout and stderr bytes, Events reopened from a closed SQLite store, and the SessionView or RunView reduced from them. An Engine return, Provider call, stdout, or in-memory state alone is incomplete proof; interactive scenarios add a separate PTY observation.
- Compare `exec` text, screen-reader output, receipts, and `show` as exact bytes. Canonicalize only explicitly volatile UUIDs, timestamps, and temporary paths. Parse JSONL one line at a time and compare it semantically with persisted Events.
- Use a fresh product process for argument parsing, exit codes, channel separation, signals, and `show`: absolute Cargo binary path, explicit working directory, cleared allowlisted environment, isolated roots, a parent deadline, and guaranteed kill and reap. Drain piped channels while waiting and cap each capture, or a valid large export can deadlock. The credential-owner rule applies to every observation process: a bounded parent wait and output plus kill and reap, in addition to the descendant timeout.
- The deterministic successful `exec` runs the real CLI adapter, Engine, SQLite file, reducer, and renderers with the strict scripted Provider. Do not add a fake-provider CLI mode or a second product binary for tests.
- Keep a few manually reviewed goldens for public human and JSONL output. CI never rewrites them; update them only with an intentional contract change and a summarized diff. Do not duplicate Engine scenarios per width or key.

## Test doubles and fixtures

- Provider is the only Engine behavior double. The scripted fake matches semantic expectations, gates concurrency, fails on missing, duplicate, or extra calls, and proves cancellation with drop guards. Check request semantics and exact counts outside Engine panic handling, so an accidental fake panic cannot satisfy an expected failed Run.
- A refusal peer must return a valid success if a forbidden request arrives, so the rejection assertion fails; a peer crash followed by generic failure is not refusal proof. A native OOM owner proves its payload reached execution, the kernel OOM-kill counter advanced, and cleanup was observed. A timeout, malformed output, or command failure is not disappearance.
- Use real temporary filesystem objects and real file-backed SQLite for state, reopen, permission, replay, and recovery evidence. `:memory:` is not durability evidence.
- Use explicit gates and paused Tokio time for schedules and deadlines. Never coordinate with wall sleeps, randomness, repeated attempts, or assumed task order.
- Pause Tokio time only after real subprocess or Guard I/O has finished: while paused, an idle runtime auto-advances timers, so Guard timeouts can fire during a Tool call. Pause from a Provider hook after the Tool result, as the Goal Finish tests do. Advance the clock inside the Provider call that needs it; a progress observer races the Engine, which can pass several store round trips inside one poll before the observer runs.
- Keep provider response fixtures minimal and hand-reviewed from the official wire contract. Never capture production responses, credentials, prompts, or user data.
- Use one bounded canary assertion across Provider observations, Events, database bytes, stdout, stderr, panic or error text, and opt-in telemetry, without echoing the canary on failure.

## Evidence lanes

- Default `cargo test` is offline, deterministic, isolated, retry-free, free of wall sleeps and hardware-sensitive timing, and quiet on success.
- Heavy native gates use `cfg_attr(not(arany_ci), ignore)`, so CI's `--cfg arany_ci` runs them in its ordinary test step. Explicitly ignored release gates need a named opt-in lane that proves a nonzero exact test count. Gates in one test binary hold `crate::test_process::NATIVE_GATE`. A locally ignored gate is not evidence. Check commands live in the [development guide](../docs/development.md#verification).
- Before activating an ignored native parent, reconcile its fixture with the current Provider wire, Tool admission, Goal lifecycle, and persisted success criteria; a stale fixture is not native evidence.
- Cross-platform claims need native behavior tests for filesystem, ACL, signal, process, SQLite reopen, and terminal semantics. A compile-only target or silent skip is not evidence.
- Process-death, disk-fault, fuzz, repeated-flake, and named-host performance work may run as scheduled or release gates. Stochastic model quality belongs to evaluations and latency to benchmarks; neither overrides a failed deterministic invariant.
- Add property, fuzz, model-checking, snapshot, or concurrency frameworks only when their named boundary has outgrown readable tables.
- Security test coverage: hostile repository/Git configuration, every path-component type, state-path links and permissions, omitted-input egress canaries per adapter, credential isolation, broker-route drift, terminal-control payloads, oversized input/output, malformed replay, cancellation cleanup, and unavailable-provider/telemetry failure.

## Corpus placement

Keep scenarios that launch `arany` or drive a terminal in `crates/arany-cli/tests/`: Session journeys stay in `crates/arany-cli/tests/session_run.rs`, and Guard-launching Engine cases belong to `crates/arany-cli/tests/guard_runs.rs`. In-process Engine, Session, Store, Provider and Tool scenarios stay in `crates/arany/tests/` or their owning module. Focused module tests use compact transition, boundary, hostile-input or parser tables only where the central journey cannot prove the claim.

Reuse shared process/Git fixtures and semantic Provider support through the library's `test-support` feature; do not copy fixture or Provider-rendering knowledge between corpora.

## Deferred evidence

- A check needing manual interaction, real credentials, paid usage, an unavailable OS service, another machine, or an unavailable toolchain is recorded once in [NEXT_STEPS.md](../NEXT_STEPS.md) with its unverified claim and user-owned procedure. Continue the next implementable task; pending checks are not evidence.
- Never bypass security or resource bounds to obtain evidence. Manual screen-reader checks are deferred evidence, not silent skips.

## Diagnosing failures

- Read the development log ([diagnostics spec](../docs/specs/diagnostics.md)) before adding instrumentation.
- Review new instrumentation at typed boundaries against the [diagnostic privacy contract](../docs/specs/diagnostics.md).
- A synthetic distinction between causes does not establish the cause of a live failure.

## Failure and maintenance discipline

- After a hosted CI failure, consolidate confirmed failures and review findings from that exact head before correcting. Dispatch a [focused CI lane](./references/testing-details.md#local-verification-and-ci) only once diagnosis names the OS and profile; its pass is diagnostic, not verification. Before marking a change ready, the complete `ci.yml` workflow must pass on the exact final head, as an automatic PR run on any base branch, a push run or a manual dispatch.
- Focused Cargo and test-binary invocations report their intended nonzero test count. A self-spawned helper filtered with `--exact` asserts exactly one completed test, with a bounded wait, output limit, and kill and reap. Helper-only tests are ignored by default and need their parent's private fixture. Zero-test or ignored-child success provides no evidence.
- Failure output names the scenario, case, expected and observed exit, bounded escaped output, the semantic Event difference, and a safe fixture identifier. It never dumps secrets, raw hostile controls, provider bodies, or database payloads.
- Every test owns its directories, files, ports, runtime, database, and processes, and never mutates shared process environment.
- A flaky deterministic test is a product-or-test synchronization bug. Fix it; do not retry until green or weaken the assertion. Quarantine names an owner, the missing claim, and a removal condition, and never counts as passing evidence.
- For behavior changes, identify the owner, demonstrate the failure, update the smallest scenario or corpus row, review exact output changes, and remove superseded evidence.
- Report verification by claims proved and lanes run, not tests added. State explicitly when live, platform, release, or performance gates did not run.
