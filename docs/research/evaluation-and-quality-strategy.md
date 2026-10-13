# Behavioral evaluation: comparisons and limits

**Source baseline:** 2026-09-28; retained methodology, not an executed benchmark or release result.

[Testing rules](../../agents/testing.md) own required verification and
[the testing strategy](./testing-strategy-for-rust-cli-harness.md) explains the
deterministic evidence lanes. This report covers the separate question of how
reliably a model, prompt or collaboration policy solves a distribution of tasks.
It does not require an evaluation framework for ordinary repository changes.

## Evaluate an outcome before optimizing its cost

A plausible final answer cannot prove a filesystem change, valid durable history
or correct application state. Deterministic outcome and safety checks therefore
precede subjective quality scores. Model grading can help assess synthesis,
clarity or groundedness when objective assertions are insufficient; it cannot
make a violated invariant acceptable. [Anthropic's evaluation guidance](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)
distinguishes task outcomes from the trajectory and explains why several grading
methods can be needed.

**Application to Arany:** retain the existing deterministic contract tests as
hard requirements. Introduce a behavioral comparison only for a named decision,
such as changing a prompt, default model, collaboration policy or retrieval
strategy. Do not infer model quality from one live Provider smoke test.

## Make comparisons answer the proposed decision

Keep the task inputs, repository/environment revision and resource budgets
comparable. Record the model/profile and prompt variants; separate infrastructure
failures from incorrect task outcomes. Useful baselines depend on the change:

| Proposed change | Comparison that isolates its value |
|---|---|
| Prompt or harness policy | Baseline and candidate on the same tasks and model |
| Multiple agents | An equal-budget single-agent baseline |
| Retrieval or durable Memory | No-retrieval or lexical-only baseline, as applicable |
| Default model | The same task distribution and declared operating constraints |

Model-dependent outcomes vary. Repeated trials, randomized variant order and
paired win/loss/tie results are more informative than an isolated success.
Report uncertainty and failures alongside success rate, cost per successful
outcome and latency. Anthropic distinguishes success within several attempts
from consistent success across repeated attempts; those metrics answer different
product questions. [Evaluation metrics](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents).

**Inference:** more agents or a higher average score do not alone justify a new
default. A cost reduction that increases unsafe behavior or contract failures is
not an acceptable improvement. The exact sample size and acceptance threshold
need the actual decision and budget; this report supplies no universal target.

## Calibrate subjective judges

A model judge is another model-dependent measurement. Keep its model, prompt and
rubric identifiable; compare its judgments against human review, inspect
agreement and disagreement, and revisit calibration when the judge changes.
Avoid exposing candidate identity when it could bias grading. Review a sample
of apparent passes as well as failures. These measures address judge error,
not only candidate error. [Anthropic evaluation guidance](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents).

Untrusted custom grading code needs the same execution protection as other
untrusted code. The reviewed [Google Agents CLI evaluation guide](https://google.github.io/agents-cli/guide/evaluation/)
distinguishes code-execution metrics from model judges and warns about local
grader privileges. A score cannot grant capabilities or authorize disclosure.

## Preserve useful cases without creating another history store

A reviewed failure can become a small synthetic case with expected outcomes and
an identifiable contract or risk. Avoid copying private conversations,
credentials or real customer repositories into a suite. Keep case provenance and
review ambiguous, saturated or time-sensitive cases rather than accumulating
uninterpretable test totals.

The reviewed [OpenAI evaluation guide](https://developers.openai.com/api/docs/guides/agent-evals)
and [Anthropic evaluation guidance](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)
support repeatable datasets and maintained evaluation. For Arany, canonical
Events remain production truth; trial artifacts must not become another mutable
Session authority. [Context rationale](./context-memory-and-compaction.md)
explains the distinction between canonical and derived state.

A proposed evaluation needs finite trials, usage and retention, explicit owners
and the existing consent requirements for paid/live work. Plans hold that work;
[next steps](../../NEXT_STEPS.md) holds current gaps. No evaluator service,
durable-job schema, new runtime seam or additional release suite is implied by
this methodology.
