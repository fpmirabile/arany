# Multi-agent supervision: reasons and expansion gates

**Research baseline:** 2026-09-28.

[Execution](../specs/execution.md) owns the bounded primary/direct-child protocol;
[Session spec](../specs/session.md) own lineage and context;
[terminal behavior](../specs/terminal.md) owns inspection and feedback. This report
retains orchestration alternatives and conditions for a broader design.

## Keep one accountable owner

A manager that delegates and synthesizes fits a user who expects one coherent
answer. It centralizes budgets, cancellation, result validation and conflict
resolution. Assignment order and completion order are different: explicit causal
assignments keep the result stable when parallel children finish in another order.

A handoff instead transfers responsibility for subsequent interaction. It may
fit a specialist with a distinct scope, but needs a visible ownership transition
and cannot transfer Engine authority. Peer collaboration can help challenge
findings, but adds communication cost and ambiguous completion. Neither follows
merely from admitting more children.

| Pattern | Useful condition | Added responsibility |
|---|---|---|
| Single agent | Sequential or tightly coupled work | No team coordination |
| Primary plus bounded workers | Independent questions and one final owner | Explicit briefs, join, aggregate budgets |
| Handoff | Another agent must own subsequent interaction | Visible final-owner and continuation semantics |
| Peer messages | Workers need bounded exchange before joining | Recipient scope, mailbox limits, causal history |
| Recursive supervision | Direct children cannot express a demonstrated task | Depth limits and descendant cleanup |
| Assignment DAG | Dependencies or ownership transfer are real | Attempts, leases, fencing and stale-result rejection |

The broader patterns remain research options. A graph, scheduler trait or mailbox
module is not earned by an illustrative interface sketch.

## Lessons retained from other systems

The original source review distinguished a few useful precedents:

- Firstmate's visible workers and explicit decisions address a real user need,
  but its terminal backends and status-file watchers reflect orchestration over
  other harnesses. Arany can own typed facts directly.
- OpenAI's manager-versus-handoff distinction clarifies answer ownership. Live
  operational events, saved state and post-turn traces serve different purposes;
  one cannot stand in for all three.
- Claude Code and OpenCode show the value of isolated child context and transcript
  inspection. A visible child session or shared task list alone does not prove
  cancellation, accurate state or durable recovery.
- ADK and A2A supply workflow and remote-task vocabulary. A transport's transient
  status messages are not a canonical internal replay model.
- Temporal and Erlang/OTP illustrate explicit worker ownership, deterministic
  history and bounded escalation. Adopting those ideas does not require embedding
  either system or implementing every restart strategy.

The research also found that parallelism helps decomposable work but can amplify
errors and coordination load. More agents is not a quality metric. Compare task
correctness, total usage and elapsed time against a single-agent baseline before
claiming a benefit. These are dated source observations, not current support or
feature claims about another product.

## Ownership before scheduling

The Engine must remain able to explain who owns admitted work, which result is
eligible and what a cancellation or interrupted prefix means. Progress is a
projection of accepted facts, not another writer of lifecycle state. A trace or
process-liveness hint cannot manufacture successful completion.

If retries, leases or recursive work become necessary, first define stale-attempt
rejection and reconciliation of effects whose result was lost. Automatically
restarting a worker can repeat billable calls or external effects. Existing
[Tool uncertainty](../specs/tools.md) and [execution recovery](../specs/execution.md)
must survive any extension.

Concurrent writable workers would need isolated Workspace views and an explicit
integration owner. A process or worktree per agent does not itself establish
capability confinement, conflict handling or authority to publish changes.

## Feedback and evidence

Users need meaningful state changes and bounded drill-down, not every low-level
scheduler event or private reasoning. Primary-screen activity and agent inspection
are already owned by [the terminal contract](../specs/terminal.md). Keep proposed
handoff/peer interactions in a concrete plan instead of repeating UI wireframes.

The real Engine with a strict scripted Provider proves completion order, join,
aggregate admission and cancellation. Corpus rows own illegal outcomes and
capacity boundaries; native processes and PTYs prove their distinct lifecycle
claims. [Testing rules](../../agents/testing.md) own the procedures and
[evaluation research](./evaluation-and-quality-strategy.md) separates model
quality from deterministic correctness. [Next steps](../../NEXT_STEPS.md) owns gaps.

## Primary sources

- Firstmate: [architecture](https://github.com/kunchenguid/firstmate/blob/main/docs/architecture.md), [README](https://github.com/kunchenguid/firstmate/blob/main/README.md)
- OpenAI: [orchestration](https://developers.openai.com/api/docs/guides/agents/orchestration), [running agents](https://developers.openai.com/api/docs/guides/agents/running-agents), [session events](https://developers.openai.com/api/docs/guides/agents-api/sessions/events)
- Claude Code: [subagents](https://code.claude.com/docs/en/sub-agents), [agent teams](https://code.claude.com/docs/en/agent-teams)
- [OpenCode session schema](https://github.com/anomalyco/opencode/blob/dev/packages/schema/src/v1/session.ts)
- [Google ADK workflows](https://google.github.io/adk-docs/agents/workflow-agents/sequential-agents/), [A2A specification](https://github.com/a2aproject/A2A/blob/main/docs/specification.md)
- [Temporal workflow replay](https://docs.temporal.io/workflow-definition), [Erlang supervision](https://www.erlang.org/doc/system/sup_princ.html)
- [Anthropic multi-agent research](https://www.anthropic.com/engineering/multi-agent-research-system), [Google scaling study](https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/), [Anthropic coordination study](https://www.anthropic.com/research/multiagent-systems)
