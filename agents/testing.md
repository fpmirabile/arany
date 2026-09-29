# Testing

Load when adding or changing behavior, fixing a defect, editing tests or fixtures, changing user-visible output, or making a correctness, security, durability, provider, performance, or platform-support claim. The detailed rationale and V1 evidence register live in `docs/research/testing-strategy-for-rust-cli-harness.md`; keep this file as the concise working contract.

## Evidence budget

- Optimize for evidence per test, not test count or coverage percentage.
- Add a test only when it protects a user-visible contract, durable or security invariant, protocol boundary, concurrency or failure mode, or demonstrated regression; no existing test may already catch the same defect with equally useful diagnostics.
- State the owned claim and the defect the test would catch. If the test cannot be shown to fail for that defect, strengthen an existing test, type, or parser instead.
- Prefer adding a row to an existing scenario, table, corpus, or failpoint harness over adding another test function or file.
- Keep one authoritative test owner per claim. Duplicate a claim at another layer only when real process, platform, durability, or security evidence is materially different.
- Refactors with unchanged observable behavior normally rely on existing tests. Do not test private helpers, getters, type-enforced facts, third-party behavior, mock call order outside the protocol, or abstractions created only for testing.
- Delete or merge tests and fixtures when a stronger owner subsumes them, the contract disappears, or the failure becomes structurally impossible. Keep durable and security regressions in the narrowest stable corpus.

## User-visible proof

- Acceptance scenarios produce an `ObservationBundle`: exit class, exact stdout bytes, exact stderr bytes, Events reopened from a closed SQLite store, and the RunView reduced from those Events.
- A successful assertion is incomplete when it checks only an Engine return, a Provider call, stdout, or live in-memory state.
- Compare human output as exact bytes. Canonicalize only parsed UUIDs, timestamps, and temporary paths explicitly declared volatile; preserve identity relationships, ordering, counts, and every user-controlled byte.
- Parse JSONL one line at a time, compare it semantically with persisted Events, and verify its exact newline and channel contract.
- Use a fresh product process for argument parsing, exit codes, stdout/stderr separation, signal wiring, and `show`. Use Cargo's absolute binary path, an explicit working directory, a cleared allowlisted environment, isolated temporary roots, a parent deadline, and guaranteed kill/reap cleanup.
- The deterministic successful `run` exercises the real CLI adapter, Engine, SQLite file, reducer, and renderers in-process with the strict scripted Provider. Do not expose a fake-provider CLI mode or add a second product binary for tests.
- Keep a few manually reviewed goldens only for public human/JSONL output. CI never rewrites them, and an agent updates them only with an intentional contract change and a summarized diff.

## Test doubles and fixtures

- Provider is the only Engine behavior double. Its scripted fake matches semantic expectations, controls concurrency with gates, fails on missing/duplicate/extra calls, and proves cancellation with drop guards.
- Use real temporary filesystem objects and real file-backed SQLite for state, reopen, permission, journaling, replay, and recovery evidence. `:memory:` is not durability evidence.
- Use explicit gates and paused Tokio time for schedules and deadlines. Never coordinate with wall sleeps, randomness, repeated attempts, or assumed task order.
- Keep provider response fixtures minimal and hand-reviewed from the official wire contract. Never capture production responses, credentials, prompts, or user data.
- Use one bounded canary assertion across Provider observations, Events, database bytes, stdout, stderr, controlled panic/error text, and optional telemetry without echoing the canary on failure.

## Evidence lanes

- Default `cargo test` is offline, deterministic, isolated, retry-free, free of wall sleeps and hardware-sensitive timing assertions, and quiet on success.
- Keep V1 integration scenarios in one `tests/team_run.rs` target. Focused owning-module tests use compact transition, boundary, hostile-input, or parser tables only where the central journey cannot prove the claim.
- One ignored, explicitly paid `live_openai_team_run` launches the shipped executable and asserts structural bounds, persistence, channels, and replay—not prose, latency, or model quality. Missing credentials fail only when that test is selected.
- Cross-platform claims require native behavior tests for filesystem, ACL, signal, process, SQLite reopen, and terminal semantics. A compile-only target or silent skip is not evidence.
- Process-death, disk-fault, fuzz, repeated-flake, and named-host performance work may run as scheduled or release gates when too expensive for the default loop.
- Stochastic model quality belongs to evaluations; latency and resource distributions belong to benchmarks. Neither can override a failed deterministic invariant.
- Add property, fuzz, model-checking, snapshot, process-test, or concurrency frameworks only when their named boundary has outgrown readable tables or a tool replaces material repeated machinery.

## Failure and maintenance discipline

- Failure output names the scenario and case, expected and observed exit, bounded escaped stdout/stderr, semantic Event difference, and safe fixture identifier. It never dumps secrets, raw hostile controls, provider bodies, or database payloads.
- Every test runs independently and owns its directories, files, ports, runtime, database, and processes. Do not mutate shared process environment from parallel tests.
- A flaky deterministic test is a product-or-test synchronization bug. Fix it; do not retry until green or weaken the assertion. Temporary quarantine names an owner, the missing claim, and a removal condition and does not count as passing evidence.
- For behavior changes, identify the current owner, demonstrate the failure, update the smallest scenario or corpus row, review exact output changes, and remove superseded evidence.
- Report verification by claims proved and lanes run, not by the number of tests added. State explicitly when live, platform, release, or performance gates were not run.
