# Testing strategy for the Rust CLI harness

This document explains the testing design and its trade-offs. [Testing rules](../../agents/testing.md)
own the working requirements; [fixture details](../../agents/references/testing-details.md)
own native procedures. [Domain specs](../specs/README.md) own behavior and acceptance
scenarios, and [next steps](../../NEXT_STEPS.md) owns unresolved defects and missing
verification. Completed work and past check results belong in Git and CI history.

## Evidence per test

The useful unit is a claim with a clear owner, not a test count or coverage
percentage. One connected journey can prove coordination, durable writes,
replay and visible output. Small boundary corpora complement that journey where
its successful path cannot exercise rejection or a specific failure.

Prefer extending the existing owner over adding another layer that asserts the
same fact. A new framework, fixture abstraction or generative tool needs a
reachable gap that the existing suite cannot cover clearly. A private helper's
shape is not itself a product claim.

This keeps changes reviewable: the test name, scenario or corpus row explains
what breaks, while its observation shows the user-visible or security consequence.
An implementation fix can make a test more valuable; completion alone is no
reason to delete a durable/security regression.

## Composition and process evidence

The deterministic Session journey uses the real Engine, temporary file-backed
SQLite, reducers and presentation, with a strict scripted Provider at the
existing substitution boundary. It exercises direct and bounded-team Runs,
controlled child completion, resume/fork, compaction and cancellation.

Provider scripts match complete semantic requests, reject missing or extra
calls and expose controlled gates. A mock panic followed by generic Run failure
cannot prove product refusal. Forbidden-call peers return valid success if the
request reaches them, so rejection assertions depend on Arany's boundary.

A separate product-process lane owns argument parsing, signals, exit status,
channel separation and replay after process exit. It executes the shipped
binary rather than adding a fake-provider mode or second product executable.
These lanes establish different facts; neither is advertised as proof of the
other.

For a noninteractive acceptance scenario, compare one connected observation:

- exit class and exact stdout/stderr bytes;
- Events reopened from the closed SQLite store;
- SessionView/RunView reduced from those Events.

Interactive scenarios add semantic frames, terminal settings, keys/signals and
the post-restoration receipt. Ratatui TestBackend owns deterministic layout;
native PTY tests own terminal acquisition, restoration and OS input behavior.
Raw escape sequences are not public output goldens, and automated frame tests
do not establish assistive-technology usability.

## Durability and failure

Use real filesystem objects and file-backed SQLite for ownership, permission,
transaction, quota, reopen and recovery claims. An in-memory database cannot
prove persistence after a connection or process ends. Acknowledged Events must
survive reopen; failure before acknowledgement must leave an honest prefix.

Fault owners choose explicit boundaries, such as before commit or after append
acknowledgement, then kill/reap the owned process and inspect the closed state.
This proves the selected application boundary, not arbitrary power-loss timing.
The same distinction applies to interrupted effects: durable reporting must
preserve uncertainty rather than infer rollback from missing completion.

Cancellation owners observe dropped futures, joined tasks and positively owned
child cleanup. Gates and paused time control scheduling; sleeping until an
assumed order is not synchronization. Native supervisors need bounded observation
of actual process identity and resource enforcement, independently of Engine
return values.

Fixture isolation is part of the evidence. A temporary Session directory and a
changed HOME do not isolate an account derived from the effective OS user.
Native account/OS-service gates follow the dedicated fixture rules and fail
visibly when their isolation prerequisites are unavailable.

## Behavioral evaluation

Methodology baseline: 2026-09-28; no executed model benchmark is claimed.
[Evidence lanes](../../agents/testing.md#evidence-lanes) own required checks.
Behavioral comparisons answer a named model, prompt or collaboration decision:

- Check actual outcomes and deterministic invariants before subjective quality.
  A model score cannot excuse unsafe disclosure or invalid durable history.
- Compare the same tasks, environment and budgets, including an equal-budget
  single-agent baseline for team changes. Repeat trials, vary order and report
  failures and uncertainty alongside cost per success and latency.
- Calibrate identifiable model judges against human review, including apparent
  passes. Preserve useful failures as small synthetic cases, with finite usage
  and retention; private conversations are not a benchmark corpus.

Source basis: [Anthropic evaluation guidance](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents),
[OpenAI agent evaluations](https://developers.openai.com/api/docs/guides/agent-evals).
These are methodology sources, not a mandate for another framework or release
suite. [Testing rules](../../agents/testing.md#failure-and-maintenance-discipline)
own maintenance and deletion of superseded evidence.

## Primary sources

- [Rust: Writing Automated Tests](https://doc.rust-lang.org/book/ch11-00-testing.html)
- [Cargo: test targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#tests)
- [Tokio: Graceful Shutdown](https://tokio.rs/tokio/topics/shutdown)
- [Tokio: paused time](https://docs.rs/tokio/latest/tokio/time/fn.pause.html)
- [SQLite: How SQLite Is Tested](https://www.sqlite.org/testing.html)
- [SQLite: Atomic Commit](https://www.sqlite.org/atomiccommit.html)
