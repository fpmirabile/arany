# A durable multi-agent loop with user-first feedback

**Status:** research and architectural recommendation  
**Research date:** 2026-09-28  
**Question:** How should this Rust-first harness implement a reusable, durable agent loop that can supervise a team of subagents while showing the user what every agent is doing, why it is doing it, what needs attention, and what the team has produced?

> **Scope amendment — 2026-09-29:** Version 1 exposes this runtime only through `arany`: exactly one root and two children, fixed join-all, append-only human output or JSONL, and one RunView. Interactive terminal rendering, three-worker phases, recursive teams, Assignment DAGs, web, desktop, IDE, and standalone SDK presentations below are triggered future analysis, not current implementation scope. The [closure audit](./research-closure-audit.md) and [system overview](../architecture/system-overview.md) are canonical.

## Reading guide

This report uses four labels deliberately:

- **Fact**: directly supported by a cited primary source: official documentation or source, a specification, or an original paper.
- **Inference**: a conclusion drawn from facts, but not itself promised by a source.
- **Recommendation**: the proposed design for this repository.
- **Gate**: a claim that must be demonstrated by a spike, conformance test, evaluation, or benchmark before it becomes a product guarantee.

The report extends the vocabulary in [`CONTEXT.md`](../../CONTEXT.md), the dependency direction in [the modular architecture study](./modular-harness-architecture.md), the capability boundary in [the deterministic protection study](./deterministic-harness-protection.md), and the instruction snapshot defined in [the instruction Markdown study](./instruction-markdown-and-policy-enforcement.md).

## Executive conclusion

Build **one reusable single-agent loop** and let multi-agent work be composition around that loop. An orchestrator is an ordinary agent running the same loop with additional team-management tools. A worker is the same loop with a narrower assignment, context, budget, and capability grant. Do not build separate control logic for “main agents,” “subagents,” and “teams.”

The engine should own a durable event journal and pure state reducers. Provider calls, tools, approvals, child-agent execution, and timers are fallible external activities. Every accepted state transition is committed before it is presented as fact. Clients consume a snapshot plus an ordered stream and construct read models optimized for humans; they do not reconstruct truth by scraping terminal output.

The default coordination pattern should be **manager with agents-as-tools**:

- one orchestrator remains accountable to the user;
- workers receive bounded assignments and typed return contracts;
- independent assignments may run concurrently;
- the orchestrator joins, validates, and synthesizes their results;
- every transfer of final-answer ownership is explicit and journaled.

Handoffs and peer-to-peer messages remain available, but they are not the default. Current research and product experience both show that multi-agent coordination helps most when work is genuinely parallel and degrades sequential work when communication fragments the reasoning path. Google Research reports gains on parallelizable tasks and losses on sequential planning in a controlled study of 180 configurations; Anthropic reports that its research system benefits from breadth-first parallel work but uses far more tokens and is a poor fit for tightly dependent domains. [[Google Research: scaling agent systems](https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/)] [[Anthropic multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)] **Fact**

User feedback should be a first-class product surface, not debug logging. The canonical journal feeds five projections:

1. **Team overview:** outcome, current phase, active/idle/waiting counts, elapsed time, budget, last meaningful activity.
2. **Team tree:** supervision and final-answer ownership.
3. **Assignment graph:** dependencies, parallel branches, joins, conflicts, and critical path.
4. **Activity timeline:** durable events with filters and drill-down.
5. **Attention inbox:** approvals, user questions, blockers, conflicts, lost workers, budget limits, and failures that require a decision.

Every agent card should show an explicitly authored `ProgressSummary`: current objective, completed work, present action or wait, next checkpoint, and blocker. It must never expose or claim to reconstruct private chain-of-thought. Detailed model prompts, hidden reasoning, secrets, and raw tool payloads are not user feedback; they are sensitive diagnostic material subject to separate access and retention policy.

The central architecture is:

```text
user Command
    │
    ▼
Session ── Run ── Team ── root AgentRun (orchestrator)
                           │
                  creates Assignments (DAG)
                           │
                  spawns AgentRuns (tree)
                           │
              provider / tool / approval activities
                           │
                      canonical Events
                           │
                 journal + pure reducers
                           │
             snapshot + resumable event stream
                           │
          CLI / TUI / web / IDE / automation clients
```

The **supervision structure is a tree** because ownership, capability inheritance, cancellation, and orphan prevention need one parent. The **work structure is a DAG** because an assignment may depend on outputs from several other assignments. Mixing these two relationships into one “agent graph” creates ambiguous cancellation and final-answer ownership.

**Recommendation:** the smallest useful vertical slice is one durable run, one orchestrator, at most three worker assignments, parallel fan-out, one join, typed worker results, cancellation, one approval pause, journal replay, and a live terminal team view. Prove this before adding nested teams, decentralized negotiation, remote agents, or elaborate role marketplaces.

## 1. What “Firstmate” most likely means

The name is ambiguous: several unrelated products use “FirstMate.” The behavior described by the user—one liaison supervising autonomous coding workers, visible sessions, isolated worktrees, and completed changes or reports—matches the current [`kunchenguid/firstmate`](https://github.com/kunchenguid/firstmate) project very closely. Its own README calls it an **agent distro**, not a model, harness, skill, MCP server, or CLI; it delegates to workers running in visible tmux, zellij, cmux, Herdr, or Orca surfaces and gives workers isolated worktrees. [[Firstmate README](https://github.com/kunchenguid/firstmate/blob/main/README.md)] **Fact**

Firstmate's current architecture uses append-only status records, process-aware liveness checks, event-driven watcher wakeups, explicit open-decision reconciliation, and direct worker steering through its runtime backends. [[Firstmate architecture](https://github.com/kunchenguid/firstmate/blob/main/docs/architecture.md)] [[Firstmate tmux backend](https://github.com/kunchenguid/firstmate/blob/main/docs/tmux-backend.md)] **Fact**

**Inference:** Firstmate validates the product desire—one user-facing liaison, visible parallel workers, isolated workspaces, and intervention—but its file, watcher, and terminal-backend coordination exists because it is layered over other harnesses. This repository can make those concepts native: typed events instead of status-line parsing, engine-owned cancellation instead of terminal-process inference, and protocol projections instead of a particular multiplexer.

**Recommendation:** copy the user contract, not the implementation topology:

- one accountable liaison;
- workers that are independently visible and steerable;
- quiet supervision that surfaces only meaningful change;
- work isolation;
- persistent decisions and outcomes;
- a clear distinction between working, waiting, blocked, failed, and done.

## 2. Evidence from current systems

### 2.1 OpenAI Agents, Codex, and observability

OpenAI's documented SDK loop calls the active agent's model, executes tool calls, follows handoffs, and stops on a final answer. A paused approval returns interruptions plus resumable state and must resume the same run rather than start a new turn. Streaming exposes events while the same loop is running. [[OpenAI: running agents](https://developers.openai.com/api/docs/guides/agents/running-agents)] [[OpenAI: results and state](https://developers.openai.com/api/docs/guides/agents/results)] **Fact**

OpenAI documents two ownership patterns. With an agent as a tool, the manager retains responsibility for the final reply. With a handoff, control and reply ownership move to the specialist. [[OpenAI: orchestration and handoffs](https://developers.openai.com/api/docs/guides/agents/orchestration)] **Fact**

The managed Agents API exposes a real-time session event stream with turn progress, item updates, text deltas, required actions, failures, cancellation, completion, and idle state. Its multi-agent stream includes subagent-created events, subagent lifecycle events for becoming active again or closing, and coordination items for create, message, wait, and interrupt actions. Saved items and per-subagent histories support later inspection, but the live stream is not a durable replay log: on disconnect, OpenAI instructs clients to subscribe again, fetch saved state, merge buffered events by item ID, and accept that some intermediate events cannot be recovered. [[OpenAI: session events and items](https://developers.openai.com/api/docs/guides/agents-api/sessions/events)] [[OpenAI: multi-agent](https://developers.openai.com/api/docs/guides/agents-api/multi-agent)] [[OpenAI: streaming event types](https://developers.openai.com/api/reference/python/resources/beta/subresources/agents/subresources/sessions/subresources/events/methods/stream)] **Fact**

Detailed Agents API traces group model responses, tools, and subagents into spans, but those traces are assembled after the turn ends and usage may arrive later. Live operational events, saved session state, and post-turn traces are therefore three related but distinct observability surfaces. [[OpenAI: observability and usage](https://developers.openai.com/api/docs/guides/agents-api/observability)] [[OpenAI: tracing](https://developers.openai.com/api/docs/guides/agents-api/tracing)] **Fact**

Codex subagent workflows can spawn specialized agents in parallel, collect their results, and display subagent activity in supported Codex and ChatGPT clients. Each subagent adds its own model and tool cost. [[Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents)] **Fact**

**Inference:** live operational feedback and post-run diagnostic tracing are different products. A trace optimized for exhaustive debugging is too late and too dense to serve as the primary live team interface. The harness needs both, fed from common identifiers but with different retention and presentation.

### 2.2 Anthropic Claude Code

Claude Code subagents run in separate context windows, can have distinct tools and permissions, and return a summary to the caller. Current configuration includes turn limits, model choice, skills, MCP servers, background execution, persistent memory, and optional worktree isolation. [[Claude Code subagents](https://code.claude.com/docs/en/sub-agents)] **Fact**

Claude Code's experimental agent teams use a lead, independent teammate sessions, a shared task list, and mailboxes for direct messages. The in-process display shows teammate rows and lets the user open a transcript, send a message, or interrupt work; split-pane mode exposes all terminal outputs simultaneously. [[Claude Code agent teams](https://code.claude.com/docs/en/agent-teams)] **Fact**

The same documentation states current limitations: in-process teammates are not restored by session resume/rewind, task status can lag, shutdown can wait on an active request or tool, a session has one team, and nested teams are unsupported. [[Claude Code agent-team limitations](https://code.claude.com/docs/en/agent-teams#limitations)] **Fact**

Claude Code hooks expose lifecycle interception points including `SubagentStart`, `SubagentStop`, and `TeammateIdle`; the idle hook can require a teammate to continue or stop it with a user-visible reason. Separately, Claude Code can export opt-in OpenTelemetry metrics, events, and traces with subagent and parent-agent identifiers, timing, token, cost, tool, and error data. [[Claude Code hooks](https://code.claude.com/docs/en/hooks)] [[Claude Code monitoring](https://code.claude.com/docs/en/monitoring-usage)] **Fact**

Claude Desktop's task pane shows subagents, shell commands, and workflows running within a session; users can inspect output or stop an entry. [[Claude Code Desktop](https://code.claude.com/docs/en/desktop#watch-background-tasks)] **Fact**

**Inference:** visible teammate transcripts, lifecycle hooks, and exported telemetry are valuable control and drill-down surfaces, but none by itself establishes a durable, replayable team state. Recovery, accurate current-state projection, and information overload still require a canonical state model outside the model transcript.

### 2.3 Google ADK and A2A

Google ADK exposes deterministic sequential, parallel, and loop workflow constructs in addition to model-driven agents, and newer ADK guidance is moving toward graph-based workflows. [[ADK sequential workflow](https://google.github.io/adk-docs/agents/workflow-agents/sequential-agents/)] [[ADK agents overview](https://google.github.io/adk-docs/agents/)] **Fact**

The A2A specification separates `Message`, stateful `Task`, and output `Artifact`; defines terminal and interrupted task states; and supports polling, ordered streaming, and push notifications. It warns that transient status messages are not necessarily persisted, so critical information cannot rely only on a live stream. [[A2A specification](https://github.com/a2aproject/A2A/blob/main/docs/specification.md)] **Fact**

**Inference:** A2A is a useful remote-agent adapter and vocabulary check, not a sufficient internal team runtime. Internally this harness needs stronger causal ordering, assignment dependencies, lease ownership, capability inheritance, and replay. Externally it can map selected assignments and artifacts to A2A tasks without forcing the domain model to become the wire protocol.

### 2.4 OpenCode

OpenCode represents subagent work as child sessions and lets the TUI navigate from a parent session to child sessions and among siblings. Its server exposes a server-sent event stream. Current source models pending/running/completed/error tool states, step boundaries, retries, token/cost data, compaction, patches, and subtask parts. [[OpenCode agents](https://opencode.ai/docs/agents)] [[OpenCode server](https://opencode.ai/docs/server/)] [[OpenCode session schema](https://github.com/anomalyco/opencode/blob/dev/packages/schema/src/v1/session.ts)] **Fact**

**Inference:** child-session navigation is a good drill-down interaction. The parent/child session hierarchy alone is not the assignment graph, and a raw message-part schema is not a complete durable team state machine.

### 2.5 Durable workflow and supervision precedents

Temporal reconstructs workflow state by replaying an event history. Non-deterministic I/O such as APIs, model calls, and database queries executes outside the replay path as activities. Child workflows have separate histories; parent-close policy determines how closure affects children. [[Temporal workflow definition](https://docs.temporal.io/workflow-definition)] [[Temporal child workflows](https://docs.temporal.io/child-workflows)] **Fact**

Temporal activities can retry and carry heartbeat checkpoint data between attempts. Temporal's AI guidance explicitly treats model/tool loops, long approval waits, fan-out, isolated retries, and recovery as durable workflow concerns. [[Temporal tasks](https://docs.temporal.io/tasks)] [[Temporal durable AI](https://docs.temporal.io/ai)] **Fact**

Erlang/OTP supervisors separate workers from supervisors, define one-for-one and grouped restart strategies, bound restart intensity, and escalate when a lower supervisor cannot recover a failing child. [[Erlang supervisor behavior](https://www.erlang.org/doc/system/sup_princ.html)] **Fact**

**Inference:** the harness should borrow the principles—event history, deterministic reduction, activities, child ownership, bounded retries, escalation—not embed Temporal or emulate every OTP feature in the first version.

### 2.6 Research evidence and its limits

Anthropic's production research system uses an orchestrator-worker pattern: the lead decomposes a query, sends explicit tasks to parallel workers, gathers their outputs, decides whether more work is necessary, and synthesizes a cited result. Anthropic reports that vague briefs produced duplicated work and gaps, and that parallel subagents plus parallel tool calls reduced elapsed time substantially for complex research. [[Anthropic multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)] **Fact**

Google Research evaluated independent, centralized, decentralized, hybrid, and single-agent configurations. In that study, centralized orchestration best balanced success and error containment; independent parallel agents amplified errors more strongly, and all multi-agent variants degraded a sequential planning benchmark. [[Google Research: scaling agent systems](https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/)] **Fact**

Anthropic's more recent multi-agent experiments show that peer coordination can discover complementary results, but tightly coupled software projects remain difficult and fleets can create harmful emergent load such as aggressive polling. [[Anthropic: patterns and problems in multiagent systems](https://www.anthropic.com/research/multiagent-systems)] **Fact**

**Recommendation:** multi-agent execution is a policy decision, not a success metric. The orchestrator must justify fan-out with decomposability, expected parallel speedup, context isolation, or independent verification. “More agents” is never a default optimization.

## 3. Domain vocabulary and relationships

### 3.1 Proposed terms

| Term | Exact meaning | Relationship to existing vocabulary |
|---|---|---|
| **Team** | The bounded coordination aggregate created inside one `Run`, containing one root `AgentRun`, zero or more descendant `AgentRun`s, assignments, messages, budgets, and ownership state. A single-agent run is a degenerate team of one. | New. It is not a `Session` and cannot span unrelated `Run`s in the first version. |
| **Orchestrator** | The team role responsible for decomposition, dispatch, join decisions, escalation, and final synthesis. It is played by an `AgentRun`; it is not a new infrastructure service. | New role. It must not replace **Engine**, which remains the policy-owning runtime. |
| **Agent** | An immutable, versioned role specification: instructions, provider/model policy, tools, output contract, budget defaults, and delegation permissions. | New. It is a definition, not a running actor and not a provider model. |
| **Assignment** | A durable work contract with an objective, boundaries, inputs, expected result schema, dependencies, priority, budget, workspace scope, and one current owner at most. | New preferred internal term. It is not a user `Command`. |
| **Task** | A compatibility term used only at an adapter boundary when an external protocol calls its unit of work a task, such as A2A or Temporal. | Avoid as an unqualified internal domain noun because Tokio, tools, A2A, Temporal, and products use it differently. Map it explicitly to `Assignment`, `AgentRun`, or adapter operation. |
| **AgentRun** | One execution attempt of an `Agent` against one `Assignment`. A retry creates a new `AgentRun` attempt; it does not rewrite the failed attempt. | Nested within the existing `Run`. Avoid “session” and “job.” |
| **InterAgentMessage** | Immutable authored content addressed from one team participant to one or more participants, with delivery state and causal references. | A specialized existing `Message`, not an `Event`. Sending/delivery are events about it. |
| **ProgressSummary** | Deliberately authored, user-safe status: objective, completed work, current action or wait, next checkpoint, blockers, and evidence references. | New durable content/projection input. It is not chain-of-thought and never claims to explain hidden reasoning. |
| **AttentionItem** | A projection of a condition requiring human or orchestrator action: approval, question, conflict, stale/lost worker, exhausted budget, or non-retryable failure. | New read-model concept. Source events remain authoritative. |
| **Approval** | The existing recorded decision granting or denying a requested capability for a defined scope. | Existing term unchanged. An approval can pause an `AgentRun`, `Assignment`, or whole `Run` depending on scope. |

### 3.2 Relationship model

```text
Session
  └── Run
       ├── accepted Command
       ├── Team
       │    ├── root AgentRun ──plays──> Orchestrator
       │    ├── descendant AgentRuns ──execute──> Assignments
       │    ├── Assignment dependency DAG
       │    └── InterAgentMessages
       ├── Events
       ├── Messages
       ├── Approvals
       └── Artifacts
```

**Recommendation:** one `Run` has at most one `Team` in the first version. A `Team` has exactly one root `AgentRun`. Every non-root `AgentRun` has exactly one supervision parent, even if its assignment depends on several assignments.

### 3.3 Conflicts with `CONTEXT.md`

1. **Engine versus orchestrator.** `CONTEXT.md` defines the Engine as the policy-owning lifecycle component. “Orchestrator” must remain a domain role executed under the Engine; otherwise model behavior and trusted lifecycle policy become conflated.
2. **Run versus AgentRun.** `Run` already means one bounded engine attempt from an accepted command. A worker execution therefore needs the qualified name `AgentRun`.
3. **Command versus assignment.** A `Command` comes from a client and asks the Engine to change state. An `Assignment` is internal work authorized by a run. An orchestrator proposes it; the Engine validates and commits it.
4. **Message versus event.** The assignment brief, progress summary, and inter-agent note are authored content. Creation, delivery, and acknowledgement are immutable events.
5. **Artifact versus worker output.** A worker result is a typed return contract that may reference `Artifact`s. A large patch or report is not embedded in the event.
6. **Task ambiguity.** The repository should not add an unqualified `Task` entity. Adapter documentation must name the mapping, for example “A2A `Task` maps to a remote `Assignment` plus its current `AgentRun`.”

**Recommendation:** add these terms to `CONTEXT.md` only when implementation begins and an ADR approves their exact semantics. This research report alone should not silently change repository-wide vocabulary.

## 4. Core invariants

The following invariants should hold regardless of provider, client, storage backend, or team size.

### 4.1 Ownership and authority

1. Every live `AgentRun` belongs to exactly one `Run` and one `Team`.
2. Every non-root `AgentRun` has exactly one supervision parent.
3. Every `Assignment` has zero or one active lease owner.
4. The final-answer owner is explicit at all times.
5. A child receives capabilities by intersection with its parent and assignment; delegation can never widen authority. This follows the repository security invariant in [`agents/security.md`](../../agents/security.md).
6. An orchestrator may propose an assignment, message, join, retry, or cancellation. The Engine validates and performs the state transition.

### 4.2 Durability and replay

1. State changes become visible only after their events are committed.
2. Reducers are deterministic and side-effect free.
3. Provider calls, tools, sandboxes, timers, and remote agents are external activities with stable invocation IDs.
4. Retried activities are idempotent or explicitly marked unsafe to retry.
5. A replay reconstructs the same aggregate state and projection inputs from the same event sequence.
6. Every aggregate has monotonically increasing sequence numbers; a client detecting a gap resynchronizes from a snapshot.

### 4.3 Completion and cancellation

1. A root `AgentRun` cannot successfully complete while an owned, non-detached child remains nonterminal.
2. Detached descendants are not supported in the initial version.
3. Cancellation propagates down the supervision tree, cancels pending joins and approvals, and eventually reaps owned processes.
4. A canceled or superseded attempt cannot commit a result after its lease epoch is invalidated.
5. Retrying an assignment creates a new attempt with new IDs and preserved causal linkage.
6. Terminal states are immutable.

### 4.4 Feedback integrity

1. “Working,” “waiting,” “blocked,” “lost,” and “done” have machine-defined meanings.
2. `ProgressSummary` is authored status, never hidden reasoning.
3. An inferred UI label is visibly distinguished from an agent-authored statement and from an engine fact.
4. Critical attention conditions are durable; they never exist only as transient stream messages.
5. Usage, cost, and time values state whether they are measured, provider-reported, estimated, or unknown.

## 5. The reusable single-agent loop

OpenAI's documented SDK loop and the original ReAct pattern both alternate model decisions with environment observations, but a production harness needs explicit durability, policy, and interruption boundaries around that conceptual loop. [[OpenAI: running agents](https://developers.openai.com/api/docs/guides/agents/running-agents)] [[ReAct paper](https://arxiv.org/abs/2210.03629)] **Fact**

### 5.1 Loop inputs

An `AgentRun` starts with an immutable launch envelope:

```text
AgentSpecRef
AssignmentRef
parent AgentRunRef (absent only for root)
InstructionSnapshotRef
ContextSnapshotRef
WorkspaceSnapshotRef
CapabilityGrantRef
Budget
ReturnContract
lease epoch
```

The loop never receives ambient authority merely because its process inherited it. Provider credentials remain in the provider adapter; tool authority remains behind the tool router and guard.

### 5.2 One durable step

**Recommendation:** each iteration follows this sequence:

1. **Recover and reduce.** Load the latest snapshot plus subsequent events. Refuse to advance if the lease epoch, run state, or policy snapshot is stale.
2. **Admit.** Reserve bounded provider, token, tool, and workspace capacity. If unavailable, enter a typed wait with a reason and deadline.
3. **Assemble context.** Build a bounded context from the assignment brief, applicable instructions, selected session messages, mailbox deliveries, dependency results, tool definitions, and prior step summaries. Record only references and a digest in the canonical event.
4. **Start provider activity.** Commit `provider.call.started`, call the provider with a stable invocation ID, and stream safe authored deltas through an ephemeral low-latency channel. Hidden reasoning is neither requested for display nor emitted as progress.
5. **Normalize output.** Convert provider-specific content into a closed engine result: `Final`, `ToolCalls`, `Delegations`, `Handoff`, `Progress`, or protocol failure. Provider adapters may preserve extra data in a restricted artifact.
6. **Validate intent.** Check schemas, budgets, current ownership, capabilities, assignment boundaries, and terminal conditions.
7. **Perform effects.** For tools, request deterministic authorization, pause for `Approval` where needed, execute through the protected runtime, and commit bounded results. Independent tool calls may run concurrently only when their effect declarations do not conflict.
8. **Perform delegation.** Validate assignment briefs, dependency edges, concurrency budget, capability narrowing, and workspace strategy; then create child assignments and `AgentRun`s.
9. **Wait or continue.** A join, approval, timer, user answer, provider retry, tool completion, or child completion parks the run without consuming a model turn. A durable signal wakes it.
10. **Complete.** Validate the return contract, commit the result and artifacts, release the lease, and notify the parent. The root additionally produces the session-facing `Message` and final run outcome.

### 5.3 Loop outcomes

```text
Continue        another model step is necessary
Wait(reason)    durable pause; no polling model turn
Succeeded       valid return contract committed
Failed          terminal or retry-policy input
Canceled        cancellation acknowledged and resources reaped
Lost            lease expired or execution ownership cannot be proven
```

“Blocked” should be a `Wait` reason plus an `AttentionItem`, not a terminal result. “Retrying” should be scheduler state, not an agent claim.

### 5.4 Stop conditions

The loop must stop on any of:

- valid final result;
- user cancellation;
- deadline;
- step, token, cost, tool, or child-count budget exhaustion;
- non-retryable provider or tool failure;
- repeated failure beyond bounded retry policy;
- invalid model output beyond repair attempts;
- policy revocation or capability enforcement failure;
- orphaned/lost execution that cannot safely resume;
- engine shutdown after durable suspension.

**Recommendation:** no prompt is the sole owner of a stop condition. The Engine enforces hard bounds even if the agent asks to continue.

## 6. The multi-agent orchestration loop

The orchestrator runs the same loop but receives narrow team tools:

```text
assignment.create
assignment.cancel
assignment.retry
agent.message
agent.steer
agent.join
team.progress
team.complete
```

These are commands into the Engine, not privileged in-process method calls.

### 6.1 Orchestration cycle

```text
understand goal
     │
     ▼
propose bounded assignments ── invalid/over-budget ──> revise or escalate
     │
     ▼
Engine validates DAG, grants, workspaces, budgets
     │
     ├──> worker A loop ──┐
     ├──> worker B loop ──┼──> durable result set
     └──> worker C loop ──┘
                              │
          messages / user steering / attention
                              │
                              ▼
                  join policy and validation
                              │
                  gaps? conflicts? failures?
                    │ yes              │ no
                    ▼                  ▼
             revise / retry /      synthesize
             ask user / cancel         │
                                       ▼
                                 final outcome
```

### 6.2 Delegation contract

Anthropic reports that workers need a specific objective, output format, source/tool guidance, and boundaries; vague briefs caused duplication and gaps. [[Anthropic multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)] **Fact**

**Recommendation:** an assignment is accepted only when it contains:

- one objective and explicit non-goals;
- expected output schema;
- inputs and evidence/artifact references;
- dependency IDs and join key;
- authorized workspace and write mode;
- capability and tool subset;
- time, token, cost, step, and retry budgets;
- progress checkpoint policy;
- success, partial-success, and failure criteria;
- conflict owner and final consumer.

### 6.3 Parallel branches and joins

The scheduler may start all `Ready` assignments whose dependency, workspace, capability, and budget constraints are satisfied. A join has an explicit policy:

- `all_required`: fail or request a decision if any required branch fails;
- `min_success(n)`: continue after at least `n` valid results;
- `first_valid`: cancel remaining branches after one result passes validation;
- `best_effort_until(deadline)`: synthesize available valid results and disclose omissions;
- `quorum`: appropriate only with a defined vote and adjudication rule.

**Recommendation:** the join policy is configuration/domain data, not improvised prose in the orchestrator prompt.

### 6.4 Failures and retries

Failure classification belongs to the Engine:

- transient provider/network failure: retry with bounded backoff and provider rate-limit signals;
- worker process loss: invalidate lease and start a new `AgentRun` attempt from committed state;
- invalid return contract: one bounded repair step, then fail;
- deterministic tool denial: do not retry unchanged input;
- ambiguous side-effect outcome: stop and surface attention unless an idempotency receipt proves safe retry;
- assignment-quality failure: orchestrator revises the assignment, producing a new assignment revision or replacement;
- systemic repeated failure: supervisor escalates instead of creating a restart storm.

### 6.5 User steering

A message to an idle `Session` starts a new `Run`; a steering `Command` addressed to an active run changes that run. OpenAI's managed sessions similarly distinguish a new idle turn from a message that steers an active turn. [[OpenAI Agents API sessions](https://developers.openai.com/api/docs/guides/agents-api/sessions)] **Fact**

**Recommendation:** every steering command declares its target and delivery mode:

```text
target: orchestrator | assignment | agent_run | team
mode: interrupt_at_safe_point | queue_for_next_step | cancel_and_replace
content: MessageRef
```

The Engine journals acceptance, delivery, acknowledgement, and any superseded work. Direct worker steering is allowed as a product feature, but the orchestrator is notified and retains final-answer accountability unless an explicit handoff changes ownership.

## 7. Coordination patterns and ownership

### 7.1 Manager with agents as tools

Use when:

- one coherent final answer is required;
- assignments are bounded and results can be typed;
- centralized budgets, policy, and conflict resolution matter;
- the user wants one liaison;
- workers should not need the full conversation.

OpenAI recommends agents-as-tools when the manager should synthesize the final response; Anthropic's research system uses a lead that delegates and synthesizes. [[OpenAI orchestration](https://developers.openai.com/api/docs/guides/agents/orchestration)] [[Anthropic multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)] **Fact**

**Recommendation:** this is the default for the harness.

### 7.2 Handoff

Use when:

- a specialist must own subsequent user interaction;
- the specialist needs a materially different tool/policy/context surface;
- forwarding every exchange through a manager adds no value;
- ownership transfer is legible to the user.

**Recommendation:** a handoff emits `final_owner.changed` with source, destination, scope, reason, and continuation context. A handoff never changes Engine ownership, team supervision, or policy authority.

### 7.3 Decentralized or peer collaboration

Use selectively when:

- workers must challenge findings or exchange discoveries before the join;
- the dependency graph changes based on peer results;
- communication is bounded by named recipients, topics, and budgets.

Avoid when work is sequential, assignments edit the same state, or a central result contract is sufficient. Current Claude agent teams support peer messages and a shared task list, but document status lag and recovery limitations; research also finds that uncontrolled peers can create coordination overhead and error propagation. [[Claude agent teams](https://code.claude.com/docs/en/agent-teams)] [[Google Research: scaling agent systems](https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/)] **Fact**

**Recommendation:** peer messages are routed through the journal and mailbox projection. Do not give workers a shared mutable transcript or unrestricted broadcast channel.

## 8. State machines and structural invariants

### 8.1 Team lifecycle

```text
Created -> Planning -> Active -> Draining -> Succeeded
              │          │          ├------> Failed
              │          │          └------> Canceled
              │          ├------> Paused -> Active
              │          └------> Canceling -> Canceled
              └------> Failed | Canceled
```

`AttentionRequired` is an orthogonal projection flag, not a team lifecycle state. One worker may need approval while other independent work continues.

Team invariants:

- exactly one root and one final-answer owner;
- no new assignments after `Draining` except explicit recovery work;
- `Succeeded` requires a valid root result and no owned live descendants;
- `Failed` preserves partial results and failure causes;
- cancellation is monotonic.

### 8.2 Assignment lifecycle

```text
Proposed -> Queued -> Ready -> Leased -> Running
                         ^        │        │
                         │        │        ├──> Waiting -> Running
                         │        │        ├──> Succeeded
                         │        │        ├──> Failed
                         │        │        └──> Canceled
                         │        └── lease expires -> Ready or Failed
                         └── dependencies satisfied

Any nonterminal state -> Canceled
Dependency failure -> Skipped | AttentionRequired, according to join policy
```

Assignment invariants:

- dependency edges form an acyclic graph;
- an assignment becomes `Ready` only when its dependency predicate is satisfied;
- only the holder of the current fencing epoch may complete it;
- a retry returns the assignment to `Ready` or creates a replacement according to policy, while retaining attempt history;
- result schema validation precedes `Succeeded`.

### 8.3 AgentRun lifecycle

Use a small lifecycle plus an orthogonal wait reason:

```text
Created -> Starting -> Active -> Completing -> Succeeded
              │          │            ├------> Failed
              │          │            └------> Canceled
              │          ├------> Failed | Canceled | Lost
              └------> Failed | Canceled | Lost
```

While `Active`, `wait_reason` may be:

```text
none | provider | tool | approval | user_input | child_join |
rate_limit | capacity | timer | workspace_conflict
```

This prevents a combinatorial enum such as `WaitingForToolAfterRetry` while retaining exact feedback.

### 8.4 Supervision tree versus assignment DAG

**Recommendation:** use two explicit structures:

- **Supervision tree:** `parent_agent_run_id`. It controls capability inheritance, lifecycle ownership, cancellation, restart escalation, and who receives a child's terminal result.
- **Assignment DAG:** `depends_on_assignment_ids`. It controls readiness, dataflow, joins, and the critical path.

An assignment may depend on results owned by several sibling workers, but its worker still has one supervisor. No process has two cancellation parents.

### 8.5 Restart policy

Borrow OTP's bounded escalation principle, not its exact process API. A worker attempt can be restarted independently when failure is isolated. A repeated systemic failure escalates to the orchestrator/team rather than restarting forever. Erlang/OTP explicitly bounds restart intensity to prevent repeated crash loops and escalates failure to higher supervisors. [[Erlang supervisor behavior](https://www.erlang.org/doc/system/sup_princ.html)] **Fact**

**Recommendation:** retry budgets exist at activity, `AgentRun`, assignment, team, and provider levels. A lower-level retry consumes upper-level budget. Identical deterministic denials are never retried automatically.

## 9. Canonical event model

### 9.1 Event envelope

```rust
pub struct EventEnvelope<P> {
    pub event_id: EventId,
    pub schema_version: u16,
    pub aggregate: AggregateRef,
    pub sequence: u64,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub team_id: Option<TeamId>,
    pub assignment_id: Option<AssignmentId>,
    pub agent_run_id: Option<AgentRunId>,
    pub actor: ActorRef,
    pub causation_id: Option<EventId>,
    pub correlation_id: CorrelationId,
    pub trace_id: Option<TraceId>,
    pub occurred_at: Timestamp,
    pub recorded_at: Timestamp,
    pub visibility: Visibility,
    pub sensitivity: Sensitivity,
    pub payload: P,
}
```

Semantics:

- `aggregate + sequence` gives strict aggregate ordering.
- `causation_id` identifies the event or command that directly caused this fact.
- `correlation_id` groups the accepted command and all resulting work.
- trace/span identifiers bridge to diagnostics without making tracing the source of truth.
- `occurred_at` is the adapter-observed time; `recorded_at` is authoritative journal time.
- visibility and sensitivity are mandatory render/export controls, not optional tags added later.

### 9.2 Event taxonomy

Use past-tense facts with closed, versioned payloads.

| Family | Canonical events |
|---|---|
| Run | `run.accepted`, `run.started`, `run.steered`, `run.pause_requested`, `run.paused`, `run.resumed`, `run.cancel_requested`, `run.succeeded`, `run.failed`, `run.canceled` |
| Team | `team.created`, `team.phase_changed`, `team.budget_changed`, `final_owner.changed`, `team.draining_started` |
| Assignment | `assignment.proposed`, `assignment.created`, `assignment.ready`, `assignment.leased`, `assignment.started`, `assignment.wait_changed`, `assignment.succeeded`, `assignment.failed`, `assignment.skipped`, `assignment.canceled` |
| AgentRun | `agent_run.created`, `agent_run.started`, `agent_run.heartbeat_recorded`, `agent_run.progress_reported`, `agent_run.wait_changed`, `agent_run.succeeded`, `agent_run.failed`, `agent_run.lost`, `agent_run.canceled` |
| Communication | `inter_agent_message.sent`, `inter_agent_message.delivered`, `inter_agent_message.acknowledged`, `steering.delivered` |
| Provider | `provider_call.started`, `provider_call.completed`, `provider_call.failed`, `provider_retry.scheduled`, `usage.recorded` |
| Tool | `tool_call.proposed`, `tool_call.authorized`, `tool_call.started`, `tool_call.progressed`, `tool_call.succeeded`, `tool_call.failed`, `tool_call.canceled` |
| Approval | `approval.requested`, `approval.resolved`, `approval.expired`, `approval.revoked` |
| Artifact | `artifact.committed`, `artifact.redacted`, `artifact.retention_changed` |
| Scheduler | `lease.granted`, `lease.renewed`, `lease.expired`, `capacity.wait_started`, `budget.exhausted` |
| Workspace | `workspace.reserved`, `workspace.conflict_detected`, `change_set.ready`, `change_set.integrated` |

### 9.3 What is not a canonical event

- every provider token delta;
- animated spinner ticks;
- inferred “thinking” text;
- repeated heartbeat rows that do not change state;
- raw stdout chunks without bounds and retention policy;
- client hover, selection, or layout state;
- a projection's current label.

Provider text can stream immediately to clients, but the journal stores bounded chunks, the final authored message, or an artifact according to retention policy. The durable state transition is completion/failure, not every character.

### 9.4 Example wire event

```json
{
  "type": "agent_run.progress_reported",
  "event_id": "evt_01K...",
  "schema_version": 1,
  "sequence": 42,
  "session_id": "ses_01K...",
  "run_id": "run_01K...",
  "team_id": "team_01K...",
  "assignment_id": "asn_01K...",
  "agent_run_id": "arun_01K...",
  "causation_id": "evt_01J...",
  "correlation_id": "cor_01K...",
  "recorded_at": "2026-09-28T14:12:09.184Z",
  "visibility": "team",
  "sensitivity": "internal",
  "payload": {
    "summary": {
      "objective": "Verify cancellation behavior",
      "completed": ["Mapped process ownership", "Added failing race test"],
      "current": "Reproducing a child-process leak",
      "next": "Patch the supervisor and rerun the targeted test",
      "blocker": null
    },
    "evidence": ["artifact://test-output/sha256:..."],
    "valid_until": "2026-09-28T14:14:09.184Z"
  }
}
```

### 9.5 Journal versus telemetry

OpenTelemetry's GenAI semantic conventions currently define development-stage `invoke_agent`, `invoke_workflow`, `plan`, inference, and `execute_tool` spans. They explicitly model a workflow as a coordinated process and distinguish it from a standalone agent invocation. [[OpenTelemetry GenAI agent spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-agent-spans.md)] [[OpenTelemetry GenAI spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-spans.md)] **Fact**

**Recommendation:** the canonical event journal is product truth; OpenTelemetry is an export/diagnostic projection. Map team/run IDs onto spans, but do not make trace sampling or exporter availability affect recovery. Record prompt and output content only through explicit opt-in because those fields can contain sensitive data and are high-cardinality.

## 10. User-feedback product design

### 10.1 Information hierarchy

The interface should answer, in order:

1. **Is the team making progress?**
2. **Does anything need me?**
3. **Who owns each piece and what are they doing?**
4. **What changed, what did it cost, and what remains?**
5. **What exact evidence or diagnostic event explains this?**

This produces four levels of progressive disclosure:

| Level | Default content | User action to reveal more |
|---|---|---|
| 0: Outcome bar | overall state, concise summary, attention count, elapsed/cost | none |
| 1: Team view | agent cards, assignment graph, current waits, last meaningful change | open team |
| 2: Activity | authored messages, tool names, files/artifacts, state transitions | select agent or assignment |
| 3: Diagnostics | normalized provider/tool events, retries, policy receipts, timing | explicit drill-down |
| 4: Sensitive raw data | retained prompts, outputs, stdout, trace payloads when policy permits | privileged explicit action |

### 10.2 Team overview

Show:

- run objective and current orchestrator-authored summary;
- `active / waiting / needs attention / succeeded / failed` counts;
- assignments completed versus total known assignments;
- provider/tool capacity and budget status;
- elapsed wall time, active model time, tool time, and queue time;
- measured usage and estimated cost, each labelled;
- last committed meaningful event and projection freshness;
- one primary action: review attention, steer, pause, resume, or cancel.

Do not display a fabricated percentage when the orchestrator does not know the full task set. Use counts, phases, or “scope still being discovered.” Do not fabricate an ETA; show a deadline or recent pace only when supported by facts.

### 10.3 Team tree and agent cards

The tree shows supervision and final-answer ownership. Each card shows:

- stable human-readable role and short agent-run ID;
- assignment title and explicit owner;
- lifecycle plus wait reason;
- latest fresh `ProgressSummary`;
- last meaningful event time;
- parent, child count, and dependency count;
- workspace/write mode and conflict state;
- token/cost/time budget used;
- unread messages and open attention;
- actions: open, message, steer, cancel, retry where authorized.

The card should not continuously replay raw stdout. “View transcript/output” is drill-down.

### 10.4 Assignment graph

Use the DAG for dependencies and joins, not the supervision tree. The graph should make critical relationships visible:

- blocked by dependency;
- running in parallel;
- join policy;
- failed or skipped prerequisite;
- workspace overlap;
- partial result accepted;
- assignment replaced or retried.

On narrow terminals, render an ordered list grouped by phase and indentation rather than forcing a wide graph.

### 10.5 Activity timeline and communication

The default timeline contains meaningful events:

- assignment created/started/completed;
- authored progress summary;
- orchestrator assignment brief;
- inter-agent message sent/delivered;
- approval requested/resolved;
- tool started/completed at semantic granularity;
- retry, failure, cancellation, or ownership change;
- artifact/change set produced.

Filters include agent, assignment, event family, attention-only, errors, and time window. Each row links cause and result. Orchestrator-to-agent messages must be visible because they explain why work changed, but secrets and private payloads are redacted before projection.

### 10.6 Safe summaries, not chain-of-thought

**Recommendation:** the product exposes three safe feedback forms:

1. **Assignment brief:** what the orchestrator asked, with constraints and expected output.
2. **Progress summary:** what the agent says it has completed, is doing, will do next, and what blocks it.
3. **Engine facts:** tool/provider state, files changed, tests run, budgets, approvals, errors, and timestamps.

It never presents hidden reasoning, internal scratchpads, or reconstructed “thoughts.” If a provider returns a reasoning summary intended for users, store it as provider-authored summary content with provenance; do not label it as the agent's full reasoning.

### 10.7 Attention inbox

An `AttentionItem` is derived from durable events and has:

```text
kind, severity, title, concise reason, affected scope,
available decisions, default/timeout behavior, opened_at,
last_changed_at, provenance, resolution state
```

Kinds:

- approval required;
- user input required;
- workspace conflict;
- non-retryable failure;
- retry storm/systemic outage;
- stale or lost worker;
- budget/deadline limit;
- policy enforcement unavailable;
- synthesis conflict or insufficient valid results.

Opening an item does not acknowledge it. Resolution is a `Command`; the resulting event closes or changes the item. A client can track local read state separately.

### 10.8 Freshness, stale, and hung

Freshness is not the same as progress.

- A provider call can be healthy but quiet; the Engine knows the call ID and deadline.
- A process can emit heartbeats while making no semantic progress.
- A `ProgressSummary` can become stale while tools still produce events.
- A worker is `Lost` only when ownership/lease cannot be proven, not merely because no text arrived.

**Recommendation:** expose three timestamps:

```text
last_state_change
last_activity
last_progress_summary
```

Stale rules are typed and contextual. A provider wait is stale after its declared deadline or transport heartbeat failure. A local worker is lost after lease expiry and failed liveness confirmation. A progress summary is stale after its `valid_until`, which asks for a new check-in but does not kill work. “Hung” is a diagnosis produced only after the relevant activity's liveness contract fails.

### 10.9 Snapshot, stream, and reconnect

The protocol flow should be:

```text
GET snapshot -> snapshot_version = N
SUBSCRIBE events after N
apply N+1, N+2, ...
deduplicate by event_id
gap or retention miss -> fetch a new snapshot
```

The server may deliver at least once. Events are ordered per aggregate and include a global stream cursor for the projection. The subscription sends transport heartbeats during idle periods. Critical state remains retrievable after disconnect.

A2A similarly requires generated event order and offers streaming plus polling/reconnect, while warning that transient status messages may be missed and must not carry critical information alone. [[A2A specification](https://github.com/a2aproject/A2A/blob/main/docs/specification.md)] **Fact**

**Gate:** measure local commit-to-visible p50/p95/p99 latency. Proposed initial targets are p95 under 250 ms for meaningful state events and under one second during high-volume output, excluding provider generation latency.

### 10.10 Coalescing and rate limits

**Recommendation:**

- flush approvals, failures, ownership changes, cancellations, and terminal outcomes immediately;
- coalesce token/text deltas into at most 10 visual frames per second;
- coalesce repeated tool progress by call ID and retain the newest bounded sample;
- never coalesce across a semantic state transition;
- cap per-client buffered events and force resnapshot on overflow;
- preserve exact canonical events even when presentation is coalesced;
- let users choose quiet, normal, and diagnostic presentation without changing execution.

### 10.11 Terminal and web clients

Terminal/TUI:

- degrade from graph to list based on width;
- keep stdout machine-readable in non-interactive mode and send progress to stderr;
- avoid redraw when content is unchanged;
- preserve keyboard navigation and a stable selected agent across updates;
- offer a static `--watch` table and JSONL event mode in addition to full TUI.

Web:

- virtualize long timelines;
- give stable DOM keys and preserve focus during streaming updates;
- use text/icon/shape as well as color for state;
- make status updates programmatically available without stealing focus;
- respect reduced motion and allow auto-updating panels to pause.

W3C guidance requires that color not be the only status cue and that status messages be programmatically determinable without forced focus; it also documents reduced-motion and pause/stop behavior for changing interfaces. [[WCAG use of color](https://www.w3.org/WAI/WCAG22/Understanding/use-of-color)] [[WCAG status messages](https://www.w3.org/WAI/WCAG21/Understanding/status-messages)] [[WCAG reduced motion technique](https://www.w3.org/WAI/WCAG22/Techniques/css/C39)] **Fact**

## 11. Scheduler and supervisor behavior

### 11.1 Admission and concurrency budgets

Use hierarchical permits:

```text
global
  ├── provider/account/model
  ├── workspace
  ├── session/run/team
  └── capability class (model, shell, browser, remote MCP)
```

An assignment starts only after acquiring all required permits in a stable order. The scheduler reserves capacity for the root orchestrator, cancellations, approval resolution, and terminal reporting so workers cannot starve the control plane.

### 11.2 Fairness and provider limits

**Recommendation:** use weighted fair queuing across sessions/teams, then priority within a team. Enforce:

- request and token rate limits per provider/account/model;
- maximum concurrent provider calls;
- maximum active workers per team and workspace;
- per-run cost/token/tool budgets;
- backoff and `Retry-After` compliance;
- no starvation of small interactive runs by one large autonomous run.

Temporal's current AI guidance similarly separates priority from fairness so urgent work can run ahead of bulk work without letting one tenant starve others. [[Temporal durable AI](https://docs.temporal.io/ai)] **Fact**

### 11.3 Leases and fencing

Every active assignment has a lease:

```text
assignment_id
agent_run_id
lease_epoch
owner_instance
expires_at
last_renewed_at
```

Completion and state-changing worker commands must present the current epoch. After expiry, the scheduler confirms liveness, marks the attempt lost, increments the epoch, and may create a replacement. A late result from the old attempt is retained as diagnostic evidence but cannot change assignment state.

Heartbeats prove lease-owner contact, not semantic progress. Long tools can attach a bounded checkpoint reference. Temporal's activities use heartbeats both for liveness/cancellation and to carry resumable checkpoint data across retries. [[Temporal tasks](https://docs.temporal.io/tasks)] **Fact**

### 11.4 Cancellation and orphan prevention

Cancellation proceeds:

1. journal request and invalidate new work admission;
2. notify root and descendants in tree order;
3. cancel provider streams, pending joins, queued tools, timers, and approvals;
4. request graceful tool/process shutdown;
5. after a bounded deadline, terminate the owned process tree through the guard;
6. release permits, leases, workspace reservations, and temporary resources;
7. run an orphan sweep and commit the terminal result.

The repository's security rules already require supervision of complete process trees and resource release on cancellation. **Recommendation**

### 11.5 Capability inheritance

Child authority is:

```text
effective_child = parent_grant
                ∩ team_policy
                ∩ agent_spec_limit
                ∩ assignment_request
                ∩ workspace_policy
                ∩ current approvals
```

The scheduler does not grant capabilities. It carries a guard-issued grant reference and refuses to dispatch without an effective enforcement attestation, following [the deterministic protection design](./deterministic-harness-protection.md).

### 11.6 Workspace and file-conflict strategy

**Recommendation:**

- read-only research workers may share one immutable workspace snapshot;
- each write-capable worker receives an isolated worktree or equivalent copy-on-write workspace by default;
- assignments declare intended path scopes when known;
- overlapping declared write scopes create an attention/conflict event before concurrent start;
- undeclared overlap discovered from produced change sets is detected before integration;
- one explicit integration owner rebases/merges and runs combined validation;
- no worker silently writes into another worker's active workspace.

Firstmate demonstrates the practical value of per-worker worktrees, and current Claude subagents also support worktree isolation. [[Firstmate README](https://github.com/kunchenguid/firstmate/blob/main/README.md)] [[Claude Code subagents](https://code.claude.com/docs/en/sub-agents)] **Fact**

**Gate:** measure worktree creation, disk use, large-repository behavior, and integration conflict rates before making worktrees mandatory on every platform.

## 12. Context and memory between parent and children

### 12.1 Isolation by default

Claude subagents and agent-team workers use separate context windows rather than inheriting the whole parent conversation; Anthropic reports context isolation as a reason to use subagents. [[Claude Code subagents](https://code.claude.com/docs/en/sub-agents)] **Fact**

**Recommendation:** a child receives an explicit `ContextSnapshot`, not the parent's live context buffer. It contains:

- assignment brief and return contract;
- applicable `InstructionSnapshot` and security policy digest;
- selected user messages necessary for the assignment;
- relevant dependency results and artifact references;
- workspace snapshot and change policy;
- allowed tool schemas;
- addressed mailbox messages;
- a concise parent-authored context note.

### 12.2 Scoped sharing

Context has named scopes:

```text
session-shared
team-shared
assignment-private
agent-run-private
artifact-reference
memory-candidate
```

Private scratch, hidden reasoning, and raw provider buffers are never automatically promoted. A worker shares information by returning a typed result, sending an `InterAgentMessage`, or proposing a memory entry with provenance.

### 12.3 Worker return contract

```rust
pub struct AgentResult {
    pub status: ResultStatus,
    pub summary: UserSafeSummary,
    pub claims: Vec<Claim>,
    pub evidence: Vec<ArtifactRef>,
    pub change_set: Option<ChangeSetRef>,
    pub checks: Vec<CheckResult>,
    pub unresolved: Vec<OpenIssue>,
    pub suggested_followups: Vec<AssignmentDraft>,
    pub usage: UsageReport,
}
```

`Claim` links to evidence or is marked as an unsupported agent assertion. The orchestrator does not receive only prose when downstream validation requires exact files, tests, citations, or structured data.

### 12.4 Compaction and provenance

Compaction preserves:

- accepted user commands and steering;
- assignment revisions and ownership;
- decisions and approvals;
- final progress summaries;
- tool receipts and side-effect identifiers;
- worker results and artifact references;
- unresolved attention items;
- source event ranges and content digests.

It may omit redundant token deltas, repeated progress samples, and superseded presentation text. A compacted summary is a new derived artifact with source ranges, model/version if generated, and validation status; it never replaces canonical events.

### 12.5 Durable memory

Memory is not shared mutable team state. A memory proposal includes author, scope, source events/artifacts, confidence or verification status, retention, and sensitivity. The memory adapter authorizes scope on every read and write. Derived indexes and summaries remain rebuildable, consistent with [`agents/security.md`](../../agents/security.md).

## 13. Deep module decomposition

### 13.1 Domain

Owns:

- IDs and vocabulary;
- Team, Assignment, AgentRun, message, approval, and result state;
- legal transitions and invariants;
- reducers from events to state;
- dependency graph validation;
- join and terminal semantics;
- capability requirements in domain terms.

Does not know Tokio, SQL, HTTP, provider schemas, OpenTelemetry, terminal layout, or A2A.

### 13.2 Engine

Owns the deep use case:

```text
accept command -> recover -> advance loops -> commit events -> publish
```

It assembles context, invokes provider/tool/guard ports, manages approvals and user steering, validates return contracts, and decides when a run has a terminal outcome. Callers do not manually sequence these steps.

### 13.3 Scheduler/supervisor

An internal Engine module initially. Owns:

- admission, hierarchical permits, fairness, and provider limits;
- assignment readiness and leases;
- worker lifecycle and heartbeat evaluation;
- bounded retry/escalation;
- cancellation propagation and orphan sweep;
- workspace reservations.

It may become a separate crate or process only when deployment, scaling, or independent ownership requires it.

### 13.4 Feedback/projection module

Consumes canonical events and builds:

- run/team snapshot;
- agent cards;
- assignment DAG;
- activity timeline;
- attention inbox;
- usage/cost/time rollups;
- freshness and staleness indicators.

It contains no lifecycle authority. Rebuilding it from the journal must reproduce the same read model. Agent-authored `ProgressSummary` is input; optional generated display summaries are labelled derived and never become execution state.

### 13.5 Protocol

Owns versioned commands, snapshots, event envelopes, cursors, error shapes, and compatibility rules. It exposes the same semantics over local stdio/socket, HTTP/SSE or WebSocket, IDE transport, and test adapter. It does not expose Rust implementation enums without a stable wire representation.

### 13.6 Adapters

- providers normalize streaming, tool calls, usage, retry hints, and errors;
- tools and MCP map requests/results into the common effect model;
- journal/snapshot/artifact stores provide durability;
- protection adapters enforce capabilities;
- workspace adapters create isolated views and integrate change sets;
- telemetry maps canonical identifiers to OpenTelemetry;
- A2A maps remote tasks/messages/artifacts at the system edge.

### 13.7 Clients

CLI, TUI, web, desktop, and IDE clients submit commands and render projections. They own presentation state, keybindings, accessibility, and local preferences. They never decide a domain transition by mutating their local snapshot.

### 13.8 Initial crate shape

**Recommendation:** do not create one crate per box. Start with:

```text
arany-domain        pure state, events, reducers, invariants
arany-engine        loop, scheduler/supervisor, projections behind private modules
arany-protocol      stable wire commands/events/snapshots
arany-adapters      initial provider/tool/storage/workspace/telemetry adapters
aranyd              composition root
client(s)           initially one CLI/TUI exercising the protocol
```

Split projection or scheduler crates only after they need independent consumption, deployment, dependency exclusion, compile locality, or ownership.

## 14. Rust-shaped interface sketches

These sketches communicate ownership, not final API names.

```rust
pub enum AgentDecision {
    Final(AgentResultDraft),
    ToolCalls(Vec<ToolCallDraft>),
    Delegate(Vec<AssignmentDraft>),
    Handoff(HandoffDraft),
    Progress(ProgressSummaryDraft),
}

pub enum WaitReason {
    Provider(ProviderCallId),
    Tool(ToolCallId),
    Approval(ApprovalId),
    UserInput(AttentionItemId),
    ChildJoin(JoinId),
    RateLimit { retry_at: Timestamp },
    Capacity(CapacityClass),
    WorkspaceConflict(ConflictId),
}

pub enum AdvanceOutcome {
    Progressed { committed_through: u64 },
    Waiting(WaitReason),
    Terminal(RunOutcome),
}
```

Provider and journal ports live outside the domain:

```rust
#[async_trait]
pub trait ProviderRuntime: Send + Sync {
    async fn start(
        &self,
        request: NormalizedProviderRequest,
        sink: ProviderStreamSink,
        cancel: CancellationToken,
    ) -> Result<ProviderCompletion, ProviderError>;
}

#[async_trait]
pub trait EventJournal: Send + Sync {
    async fn append(
        &self,
        aggregate: AggregateRef,
        expected_sequence: u64,
        events: Vec<NewEvent>,
    ) -> Result<Commit, AppendError>;

    async fn load_after(
        &self,
        aggregate: AggregateRef,
        sequence: u64,
    ) -> Result<EventPage, JournalError>;
}
```

The public Engine interface remains small:

```rust
#[async_trait]
pub trait HarnessEngine: Send + Sync {
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, EngineError>;
    async fn snapshot(&self, query: SnapshotQuery) -> Result<Snapshot, EngineError>;
    async fn subscribe(&self, after: StreamCursor) -> Result<EventStream, EngineError>;
}
```

**Recommendation:** do not expose `spawn_agent()` as an unrestricted library primitive. Delegation is a validated command under a `Run`, with budgets, grants, parentage, and event provenance.

## 15. Performance model

### 15.1 Where time goes

For independent branches:

```text
T_team ≈ T_plan
       + max(T_branch_1 ... T_branch_n)
       + T_join
       + T_synthesis
       + T_harness_critical_path
```

The speedup disappears when dependencies serialize the branches or coordination requires repeated model turns. Google Research's observed sequential penalty and Anthropic's token/cost observations make this an evaluation question, not an architectural assumption. [[Google Research](https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/)] [[Anthropic multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)] **Fact**

### 15.2 Keep harness overhead off the provider path

**Recommendation:**

- use in-memory ownership for live aggregates with journal-backed recovery;
- batch logically related events in one optimistic append;
- publish committed events through bounded broadcast channels;
- update projections incrementally rather than rescanning a run;
- store large tool output and artifacts out of line;
- avoid a durable write per text token;
- reuse stable prompt prefixes and tool definitions where provider caching supports it;
- keep cancellation, approval, and attention paths independent of busy worker queues;
- use structured IDs and indexes so team snapshots are proportional to visible items, not total history;
- cap timeline pages, mailbox delivery, and context assembly.

### 15.3 Initial performance gates

**Gate:** before claiming that only token generation is slow, measure:

- command accepted to durable receipt;
- event commit to CLI/TUI/web visibility;
- scheduling delay by resource class;
- context assembly and serialization time/bytes;
- provider time to first token and completion;
- tool queue, execution, and cancellation latency;
- projection rebuild and snapshot size;
- fan-out/join overhead for 1, 3, 10, and 50 no-op workers;
- memory and file-descriptor use per live `AgentRun`;
- reconnect time after 10 thousand, 100 thousand, and one million events;
- cancellation-to-no-owned-processes latency.

Proposed budgets for a local three-worker slice: median engine transition below 5 ms, p95 commit-to-visible below 250 ms, and harness CPU below one core while workers are provider-bound. These are hypotheses until benchmarked.

## 16. Security, privacy, and adversarial scenarios

Assets include provider credentials, user data, repository contents, durable memory, policy/grant integrity, private prompts, inter-agent messages, and the correctness of attention/approval routing. Inputs from models, workers, tools, workspaces, remote agents, and rendered Markdown are untrusted, following [`agents/security.md`](../../agents/security.md).

Test at least:

1. a worker forges another agent's ID or lease epoch;
2. a late worker commits after cancellation or replacement;
3. a model creates a dependency cycle or assignment explosion;
4. a child requests broader tools, files, network, secrets, or memory than its parent;
5. an inter-agent message contains prompt injection aimed at the orchestrator;
6. a progress summary falsely claims tests passed;
7. tool output contains secrets or malicious terminal/HTML escape sequences;
8. two workers edit the same file or migration in separate workspaces;
9. a provider times out after a side-effecting tool result was accepted;
10. a tool completes but its completion event is retried or delivered twice;
11. a projection misses or reorders events after reconnect;
12. the orchestrator finishes while a child is still running;
13. cancellation occurs during approval resolution;
14. repeated worker crashes create a restart storm;
15. a client with insufficient scope subscribes to another workspace's team;
16. raw trace/export configuration leaks prompts, secrets, or hidden reasoning;
17. a remote A2A worker sends oversized, malformed, or stale updates;
18. an agent sends messages fast enough to starve execution or the user interface.

**Recommendation:** progress summaries and worker claims are untrusted assertions until backed by Engine-observed receipts or referenced evidence. The UI can show “agent reports tests passed” versus “test tool recorded success.”

## 17. Verification strategy

### 17.1 State and replay tests

- table-test every legal and illegal transition;
- property-test that terminal states cannot leave terminality;
- property-test assignment DAG acyclicity and readiness;
- replay random event sequences and compare live versus rebuilt snapshots;
- fuzz event/schema decoding and version upgrades;
- test optimistic append conflicts and duplicate command idempotency;
- verify a new attempt cannot reuse an old fencing epoch.

### 17.2 Loop conformance tests

Run the same fake-provider scenarios through every provider adapter:

- direct final answer;
- one and multiple tool calls;
- parallel independent tools;
- approval pause/resume;
- handoff;
- subagent fan-out/join;
- provider stream cancellation;
- rate limit and retry;
- malformed structured output;
- usage arriving late or not at all;
- context overflow and compaction;
- user steering during provider and tool waits.

### 17.3 Supervisor and chaos tests

- kill engine before/after every event append boundary;
- kill worker, provider stream, tool process, and projection process;
- partition journal, provider, and client transports independently;
- expire leases with late heartbeats and late completions;
- cancel trees at every lifecycle state and assert no orphans;
- inject repeated failures and verify escalation/restart caps;
- saturate provider and workspace permits and verify fairness;
- reconnect clients with duplicates, gaps, and old cursors.

### 17.4 Model/team evaluations

Evaluate more than final answer quality:

- fan-out decision precision: did multi-agent work actually help?
- decomposition coverage and overlap;
- assignment clarity and contract validity;
- dependency correctness;
- worker result completeness and evidence quality;
- synthesis faithfulness to worker results;
- error containment and conflict detection;
- stop-decision quality;
- progress-summary accuracy against events;
- attention precision/recall and time-to-human-action;
- steering compliance;
- tokens, cost, elapsed time, and coordination messages per successful outcome.

Always compare against a single-agent baseline with the same total budget. Google Research's findings show that architecture choice depends on task decomposability, tool count, and sequential dependencies. [[Google Research: scaling agent systems](https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/)] **Fact**

### 17.5 User-feedback tests

- a user identifies who is blocked and why within five seconds;
- a user finds the orchestrator's exact assignment brief;
- a user distinguishes “waiting on provider” from “hung” and “needs approval”;
- a user steers one worker without accidentally changing the whole team;
- screen-reader output announces critical attention without narrating every token;
- state remains understandable without color and at 80-column terminal width;
- high-volume tools do not move focus or make cancellation unreachable;
- reconnect restores the same selected entity and no event is silently lost.

## 18. Phased implementation plan

### Phase 0 — vocabulary, invariants, and event fixtures

- approve `Team`, `Agent`, `Assignment`, `AgentRun`, `InterAgentMessage`, `ProgressSummary`, and `AttentionItem` semantics;
- define state machines and canonical event schemas;
- implement pure reducers and JSON fixtures;
- define final-answer ownership and cancellation invariants;
- create fake provider, tool, clock, ID, and journal adapters.

**Exit gate:** random replay produces deterministic snapshots, invalid transitions fail, and every UI state maps to committed facts.

### Phase 1 — durable single-agent vertical slice

- one `Run`, degenerate one-agent `Team`, provider/tool loop, final result;
- journal, snapshot, resumable event stream;
- one approval pause/resume;
- user cancellation and process-tree cleanup;
- terminal client showing state, current activity, attention, and timeline.

**Exit gate:** kill/restart recovery, reconnect, approval, and cancellation conformance tests pass.

### Phase 2 — manager plus three workers

- orchestrator team tools;
- bounded assignment contracts and typed results;
- parallel fan-out, dependency DAG, and one join policy;
- capability narrowing and isolated write workspaces;
- team overview, supervision tree, assignment list/graph, and worker drill-down;
- direct addressed steering with orchestrator notification.

**Exit gate:** a three-worker research or code-review scenario survives one worker loss, reports partial/final outcomes correctly, and leaves no orphan.

### Phase 3 — supervision and attention

- leases, fencing, heartbeats, lost detection, bounded retry/escalation;
- attention inbox and stale-summary policy;
- cancellation at every tree level;
- workspace conflict detection and explicit integration owner;
- projection rebuild and schema migration tests.

**Exit gate:** chaos suite passes and no stale attempt can mutate state.

### Phase 4 — scheduling, budgets, and rich feedback

- hierarchical concurrency limits, weighted fairness, provider rate limits;
- token/cost/time attribution and budget enforcement;
- coalesced live streams and freshness measurements;
- full TUI plus web projection with accessibility checks;
- OpenTelemetry export with safe content defaults.

**Exit gate:** one large team cannot starve interactive work, feedback targets hold under load, and sensitive fixtures do not leak into default traces or clients.

### Phase 5 — advanced coordination and remote agents

- explicit handoffs and ownership transfer;
- bounded peer messages and optional hybrid collaboration;
- additional join strategies;
- A2A adapter and remote-worker trust boundary;
- long-history compaction and optional nested supervisors.

**Exit gate:** each feature beats the simpler manager pattern on a named evaluation set without violating cost, recovery, or legibility budgets.

## 19. Open decisions requiring user choice

1. **User communication authority.** May the user message workers directly, or only through the orchestrator? Recommendation: direct addressed steering is allowed, journaled, and mirrored to the orchestrator.
2. **Final answer ownership.** Is the orchestrator always the final liaison, or can explicit handoffs transfer the user conversation? Recommendation: orchestrator-only for the first version.
3. **Default team size.** Recommendation: start with a maximum of three concurrent workers and a larger queued assignment set; increase only from benchmarks/evals.
4. **Workspace isolation.** Is one worktree per write-capable worker acceptable in disk and Git workflow terms? Recommendation: yes for the first coding vertical slice, behind a workspace adapter.
5. **Partial success policy.** Should a failed optional branch permit synthesis automatically? Recommendation: every assignment declares required/optional and the join policy decides; never infer after failure.
6. **Progress cadence.** Recommendation: meaningful checkpoints plus a configurable maximum silence window, not fixed “still working” messages.
7. **Transcript retention.** Which prompts, provider outputs, tool output, and inter-agent messages may be retained, for how long, and who may view them? Default recommendation: minimal canonical content, bounded diagnostic artifacts, raw provider content off by default.
8. **Cost authority.** What can the orchestrator spend without approval, and are budgets per run, team, agent, provider, or workspace? Recommendation: all, with the effective limit being the minimum.
9. **Remote topology.** Is the first product a local single-user daemon or must the initial protocol support authenticated remote/multi-tenant use? Recommendation: local daemon first, but keep principal/scope fields in the protocol.
10. **Detached work.** May child work survive root completion? Recommendation: no until a separate background-run product and ownership model exists.
11. **Generated progress summaries.** Should the harness call a cheaper model to summarize raw activity when a worker has not authored a fresh summary? Recommendation: optional and visibly labelled derived; never on the critical path and never authoritative.
12. **Peer collaboration.** Which initial scenario genuinely requires worker-to-worker messages instead of orchestrator-mediated results? Recommendation: defer until a named eval proves value.

## 20. Final recommendation

Proceed with a **durable manager-worker engine** rather than a free-form swarm:

```text
arany-domain
  Team / Assignment / AgentRun / Event / result invariants

arany-engine
  one reusable agent loop, orchestration use cases, context assembly

scheduler-supervisor (private engine module first)
  readiness, budgets, fairness, leases, retries, cancellation, workspaces

feedback projections (private engine module first)
  team snapshot, tree, DAG, timeline, attention, usage, freshness

arany-protocol
  commands, snapshots, ordered events, cursor/reconnect contract

adapters
  providers, tools, guard, journal, artifacts, workspaces, telemetry, A2A

clients
  CLI/TUI/web/IDE presentations of the same projections
```

The defining product promise should be:

> At any moment, the user can see who owns the work, what each agent was asked to do, its last verified or authored progress, what it is waiting for, what needs attention, what it produced, and how the facts connect—without reading raw logs or exposing private reasoning.

The defining runtime invariant should be:

> Every agent is the same bounded loop; every delegation is a durable assignment; every effect is policy-mediated; every child has one supervisor; every state transition is replayable; and every user-visible status is either a committed fact, an attributed authored summary, or a clearly labelled inference.

## Primary sources consulted

- [Firstmate README](https://github.com/kunchenguid/firstmate/blob/main/README.md)
- [Firstmate architecture](https://github.com/kunchenguid/firstmate/blob/main/docs/architecture.md)
- [Firstmate tmux backend](https://github.com/kunchenguid/firstmate/blob/main/docs/tmux-backend.md)
- [OpenAI Agents overview](https://developers.openai.com/api/docs/guides/agents)
- [OpenAI Agents SDK: running agents](https://developers.openai.com/api/docs/guides/agents/running-agents)
- [OpenAI Agents SDK: orchestration and handoffs](https://developers.openai.com/api/docs/guides/agents/orchestration)
- [OpenAI Agents SDK: results and state](https://developers.openai.com/api/docs/guides/agents/results)
- [OpenAI Agents SDK: guardrails and human review](https://developers.openai.com/api/docs/guides/agents/guardrails-approvals)
- [OpenAI Agents API: sessions](https://developers.openai.com/api/docs/guides/agents-api/sessions)
- [OpenAI Agents API: session events and items](https://developers.openai.com/api/docs/guides/agents-api/sessions/events)
- [OpenAI Agents API: multi-agent](https://developers.openai.com/api/docs/guides/agents-api/multi-agent)
- [OpenAI Agents API: observability and usage](https://developers.openai.com/api/docs/guides/agents-api/observability)
- [OpenAI Agents API: tracing](https://developers.openai.com/api/docs/guides/agents-api/tracing)
- [OpenAI Agents API: streaming event types](https://developers.openai.com/api/reference/python/resources/beta/subresources/agents/subresources/sessions/subresources/events/methods/stream)
- [Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents)
- [Claude Code subagents](https://code.claude.com/docs/en/sub-agents)
- [Claude Code agent teams](https://code.claude.com/docs/en/agent-teams)
- [Claude Code hooks](https://code.claude.com/docs/en/hooks)
- [Claude Code monitoring](https://code.claude.com/docs/en/monitoring-usage)
- [Claude Code Desktop](https://code.claude.com/docs/en/desktop)
- [Anthropic: Building effective agents](https://www.anthropic.com/engineering/building-effective-agents)
- [Anthropic: How we built our multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)
- [Anthropic: Patterns and problems in multiagent systems](https://www.anthropic.com/research/multiagent-systems)
- [Google Research: Towards a science of scaling agent systems](https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/)
- [Google ADK agents](https://google.github.io/adk-docs/agents/)
- [Google ADK sequential workflow](https://google.github.io/adk-docs/agents/workflow-agents/sequential-agents/)
- [A2A specification](https://github.com/a2aproject/A2A/blob/main/docs/specification.md)
- [OpenCode agents](https://opencode.ai/docs/agents)
- [OpenCode server](https://opencode.ai/docs/server/)
- [OpenCode session schema](https://github.com/anomalyco/opencode/blob/dev/packages/schema/src/v1/session.ts)
- [Temporal workflow definition](https://docs.temporal.io/workflow-definition)
- [Temporal child workflows](https://docs.temporal.io/child-workflows)
- [Temporal tasks](https://docs.temporal.io/tasks)
- [Temporal durable AI](https://docs.temporal.io/ai)
- [Erlang/OTP supervisor behavior](https://www.erlang.org/doc/system/sup_princ.html)
- [OpenTelemetry GenAI agent and framework spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-agent-spans.md)
- [OpenTelemetry GenAI spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-spans.md)
- [ReAct: Synergizing Reasoning and Acting in Language Models](https://arxiv.org/abs/2210.03629)
- [WCAG 2.2: use of color](https://www.w3.org/WAI/WCAG22/Understanding/use-of-color)
- [WCAG: status messages](https://www.w3.org/WAI/WCAG21/Understanding/status-messages)
- [WCAG: reduced motion technique](https://www.w3.org/WAI/WCAG22/Techniques/css/C39)
