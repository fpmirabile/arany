# Evaluation and quality strategy for the harness

**Research and source access date:** 2026-09-28  
**Question:** How should a provider-neutral, durable agent harness evaluate deterministic runtime correctness, model behavior, multi-agent coordination, memory, security, performance, and user feedback without confusing tests, benchmarks, traces, and model-graded judgments?

> **Scope amendment — 2026-09-29:** Version 1 evaluates `arany` as the sole product Client through one deterministic durable-Session journey that covers both a direct answer and a bounded `team(N)` Run, plus focused storage, input, cancellation, provider, and runtime-opt-in OTLP gates. The evaluation runtime, hosted service, web Clients, and broader suites below remain triggered future work. Engine is tested in-process; no protocol seam is implemented yet.

> **Testing amendment — 2026-09-29:** [Testing strategy for the Rust CLI harness](./testing-strategy-for-rust-cli-harness.md) owns deterministic V1 verification, exact user-output evidence, the ignored live Provider smoke, and test-admission/deletion rules. This report owns stochastic behavioral evaluation after its trigger; an eval score never substitutes for a failed deterministic test.

## Executive conclusion

Evaluation must be a first-class product capability, but it must not become part of the production agent loop. The production engine records typed events, outcomes, artifacts, and version manifests. A separate evaluation module replays fixtures or runs isolated trials against the same public engine interface, applies several kinds of graders, and compares a candidate with a fixed baseline.

The harness needs five complementary evidence layers:

1. **Deterministic verification:** reducers, policy algebra, event replay, protocol compatibility, effect idempotency, cancellation, and resource bounds.
2. **Adapter conformance:** every provider, tool, memory, guard, and protocol adapter runs the same recorded and fault-injected contract suites.
3. **Behavioral evaluation:** repeated end-to-end trials grade environment outcomes, visible trajectories, final responses, cost, and latency.
4. **Human and accessibility evaluation:** users must understand ownership, progress, blockers, decisions, and controls under realistic streaming load.
5. **Production evidence:** privacy-preserving telemetry, user feedback, incident examples, and sampled review feed new regression cases without becoming the canonical source of truth.

Outcome checks should dominate. A statement such as “the migration succeeded” is weaker than the resulting database or repository state. Deterministic graders should dominate model graders. Model graders are appropriate for qualities such as clarity, synthesis, and groundedness, but require calibration against humans and cannot authorize effects or override hard failures.

Agent behavior is stochastic. One successful run is not evidence of reliability. Each behavioral case therefore has multiple trials where cost permits, and release decisions compare distributions and failure modes rather than a single aggregate score. Anthropic explicitly distinguishes tasks, trials, graders, trajectories, and outcomes, and recommends inspecting transcripts and calibrating model-based graders rather than accepting scores at face value. [[Anthropic: Demystifying evals for AI agents](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)] **Fact**

OpenAI recommends traces for discovering workflow failures and datasets plus repeatable eval runs for regression testing. Its examples include tool choice, handoffs, instruction violations, and routing changes. [[OpenAI: Evaluate agent workflows](https://developers.openai.com/api/docs/guides/agent-evals)] **Fact** Google similarly separates trace generation from grading and exposes metrics for tool use, multi-turn trajectories, task success, grounding, and final response quality. [[Google Agents CLI: Evaluation guide](https://google.github.io/agents-cli/guide/evaluation/)] **Fact**

**Recommendation:** build a local, provider-neutral evaluation runner around the harness's public command/event protocol. Export to vendor evaluation systems when useful, but keep cases, manifests, raw deterministic grades, and baseline comparisons portable.

## 1. Terms and evidence hierarchy

Use the following terms consistently:

| Term | Meaning | Not the same as |
|---|---|---|
| **Test** | A deterministic or tightly controlled check with a binary invariant. | Stochastic model quality evaluation. |
| **EvalCase** | One versioned scenario with inputs, environment, budgets, and success criteria. | One execution. |
| **Trial** | One attempt at an `EvalCase` under a fully recorded system manifest. | The case itself. |
| **Trajectory** | The user-visible and machine-observable sequence of commands, messages, events, tool calls, and results in a trial. | Hidden provider reasoning. |
| **Outcome** | The terminal external and domain state produced by a trial. | A final-answer claim. |
| **Grader** | Versioned logic that maps evidence to structured grade facts. | An authority or policy grant. |
| **EvalRun** | A bounded set of trials for one suite and one or more system variants. | A production `Run`. |
| **Benchmark** | A controlled measurement of performance, resource use, cost, or quality distributions. | A correctness proof. |
| **Audit** | A scoped investigation of compliance, security, or behavior using evidence from tests, evals, and runtime state. | Continuous telemetry. |

Evidence strength, from strongest to weakest for a specific claim:

1. invariant checked against authoritative domain or environment state;
2. executable deterministic grader;
3. structured comparison against a reviewed reference;
4. calibrated human rubric;
5. calibrated model rubric;
6. uncalibrated model judgment;
7. self-reported agent success.

A weaker layer may add context. It must not turn a failure at a stronger layer into a pass.

## 2. What is being evaluated

“The agent” is not only the model. The result depends on a complete versioned system:

```text
engine implementation
+ domain/event schema
+ provider and model
+ agent instructions
+ tool schemas and implementations
+ instruction snapshot
+ policy and effective protection profile
+ context compiler and memory state
+ workspace/environment fixture
+ orchestration topology
+ budgets and sampling settings
= system under evaluation
```

Anthropic makes the same distinction between an evaluation harness and the agent harness being evaluated: model and scaffold are evaluated together. [[Anthropic: Demystifying evals for AI agents](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)] **Fact**

Every trial therefore records an immutable `SystemManifest` containing at least:

```text
engine_build_id
domain_schema_version
protocol_version
provider_adapter_version
provider_model_id and resolved model revision when available
agent_definition_digest
tool_catalog_digest
instruction_snapshot_digest
policy_profile_digest
context_compiler_version
memory_fixture_digest
workspace_fixture_digest
topology and join-policy version
budgets and concurrency limits
provider sampling configuration
grader versions
```

Secrets and raw credentials are never part of the manifest. Provider release aliases that can move must be recorded as aliases plus any provider-returned concrete version metadata.

## 3. Evaluation architecture

The evaluation runner is a client of the same engine interface as the CLI or web client:

```text
EvalSuite + Candidate/Baseline manifests
                    │
                    ▼
          isolated evaluation runner
          ├─ fixture provisioner
          ├─ trial scheduler
          ├─ event/trajectory collector
          └─ grader pipeline
                    │ commands/events
                    ▼
             public engine seam
                    │
          production-equivalent engine
                    │
           disposable environments
```

This placement provides three important properties:

- eval-only code cannot alter production scheduling or authorization;
- CLI, web, and engine behavior are exercised through the actual public seam;
- provider or deployment-specific evaluation services remain export adapters rather than architectural owners.

Recommended module shape:

```text
harness-eval
  load_suite(...)
  run_comparison(baseline, candidate, suite, limits)
  grade(recorded_trial, graders)
  compare(eval_runs, policy)

private implementation modules
  fixtures / trials / graders / statistics / reports / exporters

adapters
  local engine / process protocol / remote deployment
  deterministic graders / human review / model judge
  OpenAI / Google / other export formats
```

Do not introduce a generic “evaluation provider” interface that erases the differences between code graders, state checks, human rubrics, and model judges. They produce a shared `GradeResult`, but have distinct configuration and trust.

## 4. Canonical evaluation data model

An indicative domain model:

```rust
pub struct EvalCase {
    pub id: EvalCaseId,
    pub version: u32,
    pub objective: String,
    pub fixture: FixtureSpec,
    pub input: EvalInput,
    pub budgets: EvalBudgets,
    pub repetitions: RepetitionPolicy,
    pub graders: Vec<GraderSpec>,
    pub tags: BTreeSet<Tag>,
}

pub struct TrialRecord {
    pub trial_id: TrialId,
    pub case_ref: VersionedCaseRef,
    pub system_manifest: SystemManifest,
    pub engine_run_id: RunId,
    pub event_range: EventRange,
    pub outcome: TrialOutcome,
    pub usage: UsageSummary,
    pub timing: TimingSummary,
    pub artifact_refs: Vec<ArtifactRef>,
}

pub struct GradeResult {
    pub grader_id: GraderId,
    pub grader_version: String,
    pub verdict: Verdict,
    pub score: Option<f64>,
    pub reason_codes: Vec<ReasonCode>,
    pub evidence_refs: Vec<EvidenceRef>,
    pub confidence: Option<Confidence>,
}
```

The trial refers to the production journal rather than copying every event into a second canonical store. A retention policy can preserve an evaluation artifact bundle when production-style history is ephemeral.

`Verdict` should include `Pass`, `Fail`, `Inconclusive`, and `InfrastructureError`. A provider outage or broken fixture is not a model failure. An ambiguous task is not a pass.

## 5. Grader order and composition

Apply graders in this order:

1. **Fixture integrity:** was the initial environment the expected one?
2. **Safety and invariant checks:** did any forbidden effect, scope escape, leak, or irreversible violation occur?
3. **Outcome checks:** does authoritative final state satisfy the task?
4. **Trajectory constraints:** required approvals, tool semantics, evidence use, step limits, and recovery behavior.
5. **Final result checks:** correctness, completeness, citations, and communication quality.
6. **Efficiency metrics:** cost, tokens, latency, turns, tool calls, agent count, retries, and peak resources.

Hard safety failures short-circuit the overall verdict but not evidence collection. Remaining graders may still run in read-only mode to aid diagnosis.

### 5.1 Deterministic graders

Prefer:

- unit/integration tests against produced code;
- repository diff and invariant inspection;
- database or application state assertions;
- event sequence and policy receipt checks;
- JSON Schema or typed result validation;
- exact or set equality when the problem has objective answers;
- artifact hashes and provenance checks;
- resource, latency, and budget thresholds.

### 5.2 Human graders

Use humans for ambiguous utility, usability, and rubric calibration. Review tools should hide variant identity where practical, randomize order, capture rationale and disagreement, and sample both passes and failures.

### 5.3 Model graders

Model judges are suitable for groundedness, clarity, evidence coverage, decomposition quality, and synthesis faithfulness when deterministic checks are insufficient. Requirements:

- version the judge prompt and model;
- give the judge only the minimum redacted evidence required;
- require structured scores and reason codes;
- measure agreement against expert judgments;
- use repeated or multiple judges for high-variance rubrics;
- monitor drift when the judge model changes;
- never grade its own hidden reasoning;
- never grant capabilities, release secrets, or mutate the trial environment.

Google's evaluation tooling explicitly distinguishes code-execution metrics from LLM-as-judge metrics, and warns that local custom grading code runs with the CLI's privileges unless a remote sandbox is selected. [[Google Agents CLI: Evaluation guide](https://google.github.io/agents-cli/guide/evaluation/)] **Fact** The harness should always execute untrusted custom graders through the same protection runtime as tools.

## 6. Stochastic results and comparisons

For model-dependent cases:

- use multiple trials per variant when cost permits;
- randomize baseline/candidate order to reduce time-correlated provider effects;
- pin the environment and budgets;
- report successes and failures, not only a mean score;
- retain seeds where providers expose meaningful seeded behavior, but do not assume seeds guarantee determinism;
- report confidence intervals for rates and effect sizes;
- separate infrastructure errors from failed trials;
- correct for multiple comparisons when many metrics drive one release decision.

Useful result views include:

- `pass@1`: expected first-attempt usefulness;
- repeated success or `pass^k`: reliability across repeated trials;
- cost per successful outcome;
- p50/p95 latency per successful outcome;
- safety failure rate, which should generally have a zero-tolerance release threshold;
- paired win/loss/tie counts for baseline versus candidate;
- failure-mode counts by stable taxonomy.

Anthropic describes both `pass@k` and `pass^k` and emphasizes that the latter exposes consistency requirements that an average success rate can hide. [[Anthropic: Demystifying evals for AI agents](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)] **Fact**

## 7. Required suites

### 7.1 Domain and journal suite

- every legal and illegal state transition;
- command idempotency and optimistic sequence conflict;
- deterministic replay and snapshot-plus-tail equivalence;
- terminal-state immutability;
- schema migration and old-fixture compatibility;
- assignment DAG acyclicity and readiness;
- capability narrowing algebra;
- event redaction and visibility projection.

Use example tests for known edge cases and property tests for algebraic invariants and generated histories. Proptest provides generated inputs and shrinking to minimal counterexamples. [[Proptest documentation](https://docs.rs/proptest/latest/proptest/)] **Fact** Fuzz parsers, protocol decoders, event upcasters, instruction policy blocks, provider streams, and untrusted tool output using `cargo-fuzz`. [[Rust Fuzz Book](https://rust-fuzz.github.io/book/)] **Recommendation**

### 7.2 Durability and supervision suite

- crash before and after every durable append/effect-dispatch boundary;
- duplicate and reordered delivery where the contract permits it;
- lost heartbeats, expired leases, and stale fenced results;
- engine restart during provider, tool, approval, and join waits;
- cancellation at every lifecycle state;
- process tree cleanup and resource permit release;
- slow or disconnected subscribers with cursor resume;
- bounded restart intensity and escalation.

Workflow replay is only deterministic when external operations are excluded from the replay path and represented by recorded results. Temporal documents this command/event separation explicitly. [[Temporal: Workflow definition](https://docs.temporal.io/workflow-definition)] **Fact**

### 7.3 Provider conformance suite

Every provider adapter runs identical logical scenarios:

- direct answer and streaming deltas;
- multiple and parallel tool calls;
- structured output success and schema violation;
- usage arriving early, late, estimated, or absent;
- provider-specific content preserved in an extension envelope;
- malformed, truncated, duplicated, and unknown stream events;
- retryable and terminal errors;
- rate limits, backoff, and cancellation;
- context-limit rejection and compaction retry;
- credential and payload redaction.

Recorded fixtures run on every change. Opt-in live contracts detect upstream drift. A live test failure blocks an adapter release, not unrelated offline development.

### 7.4 Tool and protection suite

- argument arrays never become shell interpolation;
- approval binds to the exact request digest and maximum scope;
- denied effects fail before execution;
- tool output and artifacts respect byte/count/time limits;
- malicious terminal, Markdown, HTML, and SVG output is sanitized at render time;
- descendants cannot widen filesystem, network, secret, process, MCP, or memory authority;
- strict profiles fail closed when the platform cannot enforce them;
- idempotent effects survive duplicate dispatch; non-idempotent effects require a recovery contract;
- native process, MCP, and future WASI adapters satisfy equivalent logical contracts.

### 7.5 Context and memory suite

- same canonical state and inputs produce the same context manifest before provider-specific rendering;
- scope filters apply before ranking and prompt assembly;
- retrieved untrusted text cannot become instructions or policy;
- compaction retains named decisions, constraints, unresolved questions, artifact references, and provenance;
- deletions and tombstones disappear from new contexts and derived indexes;
- stale or contradicted memory is not silently preferred;
- cross-workspace and cross-user retrieval leakage remains zero;
- quality comparisons cover no-memory, lexical, and any candidate dense/hybrid retrieval under equal budgets;
- long-running sessions remain within latency, token, and storage budgets.

### 7.6 Single-agent behavioral suite

Include realistic coding, research, explanation, debugging, and planning tasks. Grade:

- environment outcome;
- instruction and policy compliance;
- tool choice and argument correctness;
- recovery from tool/provider errors;
- evidence and citation quality;
- final response correctness and calibration;
- unnecessary work and over-engineering;
- turns, tokens, cost, latency, and approvals.

### 7.7 Multi-agent behavioral suite

Every multi-agent case has a single-agent baseline with the same total cost/time/token ceiling. Grade:

- whether spawning was justified;
- decomposition coverage and overlap;
- assignment clarity and dependency correctness;
- capability and context narrowing;
- result/evidence completeness;
- synthesis faithfulness;
- conflict, duplicate work, and error containment;
- stop decisions and orphan cleanup;
- coordination overhead per successful result;
- user-visible progress accuracy.

A topology is enabled by default only when it improves a named suite by a predefined effect while staying inside reliability, cost, and legibility budgets. Sequential tasks should not be forced through fan-out.

### 7.8 User-feedback and accessibility suite

Measure whether a user can:

- identify owner, current activity, wait reason, and blocker within a target time;
- find the exact assignment and evidence behind a status;
- distinguish provider wait, approval wait, retry, lost worker, and completed work;
- steer or cancel the intended scope without collateral effects;
- recover orientation after reconnect;
- operate the TUI at 80 columns and the web client with keyboard and screen reader;
- receive critical status messages without token-by-token focus disruption;
- understand cost and authority before approving.

These require moderated usability sessions plus automated accessibility checks. Automated checks are necessary but do not prove comprehension.

### 7.9 Performance and soak suite

Measure separately:

- command admission;
- journal append and projection lag;
- context compilation and retrieval;
- provider byte-to-client relay;
- tool spawn/cancel/reap;
- snapshot recovery and full replay;
- multi-client fan-out;
- scheduler fairness and attention latency;
- CPU, RSS, disk/WAL growth, file descriptors, and child processes.

Report warm/cold and p50/p95/p99 distributions. Provider generation and external tool time are separate from harness-owned time. Saturate every queue, stop provider reads and client consumption, and verify bounds rather than relying on a short throughput benchmark.

## 8. Failure taxonomy

Every failed or inconclusive trial receives one primary category and optional contributing categories:

```text
fixture.invalid
infrastructure.provider_unavailable
infrastructure.tool_unavailable
engine.transition_invalid
engine.durability
engine.cancellation
engine.resource_bound
adapter.protocol
adapter.capability_mismatch
policy.denied_expected
policy.violation
agent.decomposition
agent.tool_selection
agent.tool_arguments
agent.recovery
agent.grounding
agent.synthesis
agent.stop_decision
memory.retrieval
memory.compaction
feedback.inaccurate
feedback.unusable
grader.invalid
```

Taxonomy changes are versioned. Preserve the original reason and map to newer categories through a derived projection.

## 9. Baselines and release policy

Maintain at least these baselines:

1. last released harness with the current default model;
2. candidate harness with the same model;
3. candidate harness with a fake deterministic provider for runtime-only tests;
4. single-agent topology for every multi-agent comparison;
5. no-memory or lexical-only baseline for retrieval changes.

A release policy should support hard and statistical gates:

| Gate | Example treatment |
|---|---|
| Security or cross-scope leak | Zero tolerance; any confirmed failure blocks release. |
| Journal/replay/cancellation invariant | Deterministic pass required. |
| Protocol compatibility | Every advertised version pair passes. |
| Core task success | Candidate lower confidence bound must stay above the agreed floor. |
| Regression suite | No named critical case may regress without an explicit accepted decision. |
| Multi-agent default | Must beat equal-budget single-agent baseline on the target suite. |
| Cost and latency | Must stay within absolute budgets and agreed relative regression limits. |
| Model-judge score | Advisory until calibrated; never the only release gate. |

Do not collapse all metrics into one “agent score.” A composite score hides safety failures and permits cheap improvements in one dimension to compensate for unacceptable regressions in another.

## 10. Dataset lifecycle and production feedback

Evaluation cases are reviewed, versioned repository assets. Each case states why it exists and which incident, requirement, risk, or capability it covers.

Production-to-eval flow:

```text
redacted production signal or user report
  → authorized human review
  → minimal synthetic/reproducible fixture
  → regression case and expected evidence
  → baseline run
  → review for ambiguity and grader validity
  → versioned suite
```

Do not commit raw user prompts, repositories, credentials, or memory. Prefer synthetic fixtures; otherwise require consent, minimization, access controls, retention, and deletion propagation.

Suites need explicit owners and health checks:

- remove or revise ambiguous cases;
- detect saturation and add harder representative tasks;
- inspect disagreements and a sample of apparent passes;
- recalibrate model judges after model or prompt changes;
- track coverage by capability and failure category;
- quarantine flaky infrastructure without hiding product failures;
- expire time-sensitive factual cases or pin their source corpus.

OpenAI recommends moving from trace inspection to datasets and repeatable eval runs once desired behavior is known. [[OpenAI: Evaluate agent workflows](https://developers.openai.com/api/docs/guides/agent-evals)] **Fact** Anthropic recommends treating suites as living artifacts with clear ownership and combining automated evals with production monitoring, feedback, A/B tests, and human review. [[Anthropic: Demystifying evals for AI agents](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)] **Fact**

## 11. Observability versus evaluation

The production event journal is authoritative operational history. Traces are diagnostic projections. Eval records refer to those facts and add case/manifests/grades. Metrics aggregate behavior. None substitutes for another.

```text
domain events ───────► projections and recovery
      │
      ├──────────────► diagnostic traces
      │
      └──────────────► trial evidence ─► graders ─► eval report
                                            │
                                            └──────► release decision
```

Sampling may be acceptable for operational traces; it is not acceptable for events required to reconstruct a selected evaluation trial. Raw content capture remains opt-in and access-controlled.

## 12. Implementation phases

### Phase 0 — evaluation vocabulary and deterministic fixtures

- define `EvalCase`, `Trial`, `SystemManifest`, `GradeResult`, and failure taxonomy;
- create fake provider/tool/clock/ID/journal adapters;
- add reducer, policy, replay, protocol, and redaction suites;
- encode the first ten critical behavioral cases before implementing their target features;
- make every failure reproducible by case ID, manifest, and trial ID.

**Exit gate:** deterministic suites replay identically; infrastructure failure is distinguished from product failure; every proposed Phase 1 feature has at least one outcome-based case.

### Phase 1 — local end-to-end runner

- provision disposable workspaces;
- drive the engine through the process protocol;
- collect journal ranges, outcomes, usage, timing, and artifacts;
- implement code/state/schema/trajectory graders;
- compare baseline and candidate;
- generate a local Markdown/JSON report.

**Exit gate:** a candidate regression in tool execution, cancellation, replay, or final outcome reliably blocks the suite and points to bounded evidence.

### Phase 2 — behavioral and multi-agent evaluation

- repeated trials and statistical summaries;
- human-review queue and blinded pairwise comparison;
- calibrated model graders for groundedness, synthesis, and feedback accuracy;
- equal-budget single-agent versus multi-agent suites;
- memory and long-session suites.

**Exit gate:** the first multi-agent default is supported by outcome, reliability, cost, and feedback evidence rather than a demo.

### Phase 3 — production learning loop

- privacy-preserving case nomination from incidents and feedback;
- remote/deployed target adapter;
- scheduled provider live-contract and model-upgrade runs;
- trend reports, flaky infrastructure quarantine, and suite health metrics;
- optional exports to vendor evaluation platforms.

**Exit gate:** every shipped model/provider/topology change has a reproducible comparison, critical regressions cannot be averaged away, and production examples can become sanitized tests without copying sensitive data.

## 13. Decisions resolved by this research

1. Evaluation is a separate module and deployment workflow, not a branch inside the production agent loop.
2. It consumes the same public engine interface and authoritative events as real clients.
3. Outcome and invariant graders outrank trajectories, final claims, and model judges.
4. A complete system manifest is required for every trial.
5. Multi-agent features require an equal-budget single-agent baseline.
6. Vendor eval platforms are optional adapters; repository-owned cases and results remain portable.
7. Hidden chain-of-thought is neither required nor retained as canonical evaluation evidence.
8. Security, cross-scope leakage, durability, and protocol compatibility remain separate hard gates, never terms in a composite score.

## 14. Implementation gates, not research gaps

The following require code and measurements:

- minimum repetitions and statistical thresholds for each suite;
- exact release-regression tolerances;
- reference hardware and load profiles;
- human/model grader agreement targets;
- evaluation cost budgets and provider quotas;
- which vendor export adapters are worth maintaining;
- the first task corpora for coding, research, memory, and multi-agent work.

These should be decided in implementation plans using pilot data. More literature review cannot supply product-specific thresholds.

## Final recommendation

Build the first evaluation fixtures at the same time as the domain events and fake adapters. Before enabling any adaptive behavior—memory promotion, model routing, multi-agent spawning, generated summaries, or peer collaboration—require a named suite that can show the feature improves the intended outcome without violating safety, reliability, cost, latency, or user-legibility budgets.

The defining invariant is:

> Every quality claim names the system manifest, case set, grader versions, outcome evidence, and comparison baseline that support it; no self-report or aggregate score can override a failed deterministic invariant.

## Primary sources

- [OpenAI: Evaluate agent workflows](https://developers.openai.com/api/docs/guides/agent-evals)
- [OpenAI: Trace grading](https://developers.openai.com/api/docs/guides/trace-grading)
- [Anthropic: Demystifying evals for AI agents](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)
- [Anthropic: Writing effective tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents)
- [Anthropic: How we built our multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)
- [Google Agents CLI: Evaluation guide](https://google.github.io/agents-cli/guide/evaluation/)
- [Google Agents CLI reference](https://google.github.io/agents-cli/cli/)
- [Temporal: Workflow definition and deterministic replay](https://docs.temporal.io/workflow-definition)
- [Proptest documentation](https://docs.rs/proptest/latest/proptest/)
- [Rust Fuzz Book](https://rust-fuzz.github.io/book/)
- [Tokio testing](https://tokio.rs/tokio/topics/testing)
- [WCAG 2.2](https://www.w3.org/TR/WCAG22/)
