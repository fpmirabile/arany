# Context, memory, retrieval, and compaction for a durable agent harness

**Research and source-access date:** 2026-09-28  
**Question:** How should a Rust-first, multi-agent harness preserve canonical history, construct bounded model context, retrieve durable memory, compact long-running sessions, isolate agent branches, and give users reliable control without making provider-specific behavior part of the domain?

> **Scope amendment — 2026-09-29:** Version 1 keeps bounded input assembly private inside Engine and runs exactly one root plus two children. Cross-Run Memory, retrieval, compaction, lineage for larger teams, and persistent caches below are triggered future designs. The [system overview](../architecture/system-overview.md) and [next-step decision register](./next-step-decision-register.md) are canonical for implementation.

## Reading guide

This report distinguishes four kinds of statements:

- **Fact** describes behavior established by a primary source.
- **Inference** derives an architectural consequence from facts and the repository's existing direction.
- **Recommendation** proposes the initial harness design; it is not an implemented decision.
- **Gate** names evidence required before a recommendation becomes implementation or product truth.

Provider documentation changes quickly. Model names, token limits, cache thresholds, retention periods, and feature availability belong in capability discovery and compatibility tests, not in durable domain types.

## Executive conclusion

The harness should not have one undifferentiated “memory.” It needs six deliberately different data products:

1. **Canonical history:** append-only `Event` envelopes and versioned authored `Message` records that explain what the Engine accepted and what happened.
2. **Working state:** deterministic projections rebuilt from canonical events, including active assignments, approvals, budgets, and tool state.
3. **Working context:** a bounded, provider-rendered input for one model call. It is ephemeral and reproducible from a recorded compilation manifest, but it is not the Session.
4. **Durable Memory:** scoped, inspectable knowledge selected for reuse beyond the event that introduced it. It is versioned, provenance-bearing, correctable, expirable, and deletable.
5. **Artifacts:** immutable, addressable large content held outside hot event rows and model prompts, with bounded excerpts selected into context.
6. **Derived data:** summaries, snapshots, lexical/vector indexes, embeddings, prompt-cache state, and provider continuation/compaction handles. Every derived structure is disposable or explicitly provider-owned; none is the local source of truth.

**Recommendation:** implement a deterministic `ContextCompiler` as a deep Engine module. Given an immutable input snapshot, policy snapshot, provider capability snapshot, exact budgets, and ordered candidates, it produces a `ContextManifest` plus logical context items. A Provider adapter then renders those items, performs provider-specific token accounting, caching, and optional native compaction, and records what was actually sent. The same inputs and compiler version must produce the same manifest and exclusion diagnostics.

**Recommendation:** preserve stable prompt prefixes when doing so does not weaken freshness, scope, or correctness. Prompt caching is an optimization, not memory. OpenAI, Anthropic, and Gemini all expose materially different cache controls, minimums, isolation, usage fields, and retention behavior, proving that cache policy belongs behind the Provider seam ([OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching), [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), [Gemini context caching](https://ai.google.dev/gemini-api/docs/caching)). **Fact**

**Recommendation:** compact in layers. First remove or replace bulky reproducible tool output with typed artifact references; next select bounded verbatim history and retrieved records; only then use provenance-bearing summaries. Provider-native opaque compaction may continue one provider branch efficiently, but it must never replace the local journal, Memory, or user-visible history. OpenAI explicitly describes its compaction item as opaque and not human-interpretable, while Anthropic says its server-side context editing leaves the client's full history unchanged ([OpenAI compaction](https://developers.openai.com/api/docs/guides/compaction), [Anthropic context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing)). **Fact**

**Recommendation:** each `AgentRun` gets an isolated context lineage. A child receives a bounded assignment capsule, instruction/policy snapshot references, explicitly selected Memory and Artifacts, and narrower-or-equal capabilities. It does not inherit the parent's entire provider conversation or share a mutable prompt. OpenAI's current multi-agent documentation likewise gives each subagent its own context and recommends bounded independent tasks ([OpenAI multi-agent](https://developers.openai.com/api/docs/guides/agents-api/multi-agent)). **Fact supporting the pattern; harness design is a Recommendation**

**Recommendation:** start with SQLite plus FTS5 and exact scope filters. Do not add embeddings, a vector database, or autonomous cross-session memory writes until an evaluation corpus proves a material end-to-end gain over lexical retrieval and recent-history baselines. LongMemEval separates indexing, retrieval, and reading and evaluates extraction, multi-session reasoning, temporal reasoning, knowledge updates, and abstention; LoCoMo adds long conversations and temporal/causal dynamics. Neither benchmark substitutes for project-specific agent traces ([LongMemEval](https://proceedings.iclr.cc/paper_files/paper/2025/hash/d813d324dbf0598bbdc9c8e79740ed01-Abstract-Conference.html), [LoCoMo](https://aclanthology.org/2024.acl-long.747/)). **Fact and Recommendation**

The smallest responsible vertical slice is therefore not a vector-memory product. It is one durable Session with a canonical journal, a deterministic context compiler, bounded recent history, out-of-line Artifacts, explicit user-authored Memory, lexical retrieval with hard scope filters, replayable compilation manifests, and provider-specific cache metrics. Compaction, generated Memory, embeddings, and cross-session automation follow only after their gates pass.

## 1. Reconciliation with the existing architecture

This report refines rather than replaces the existing research:

- [Rust core feasibility](./rust-core-feasibility.md) already separates canonical history, working state, working context, long-term memory, artifacts, and derived data; recommends SQLite WAL plus FTS5; and treats provider state as an optimization.
- [Modular harness architecture](./modular-harness-architecture.md) establishes `clients → protocol adapters → engine → domain`, keeps external technologies behind narrow adapters, and warns against crate-per-noun and trait-per-dependency designs.
- [Deterministic protection](./deterministic-harness-protection.md) makes `memory.read/write` capability-governed effects, requires complete mediation, and keeps the harness state database out of tool-visible filesystems.
- [Instruction Markdown and policy enforcement](./instruction-markdown-and-policy-enforcement.md) produces immutable instruction/policy snapshots with provenance and digests; ordinary retrieved text is data, never authority.
- [Multi-agent loop and user feedback](./multi-agent-loop-and-user-feedback.md) makes the event journal product truth, separates the supervision tree from the Assignment DAG, gives each `AgentRun` a bounded contract, and prohibits presenting hidden reasoning as user feedback.

The combined invariant is:

```text
canonical Events and Messages
        │ deterministic replay
        ▼
working state ───────────────┐
                            │
instruction/policy snapshot │       scoped Memory and Artifact candidates
                            │                         │
current Command             ├── ContextCompiler ◄────┘
provider capabilities       │          │
exact budgets ──────────────┘          ▼
                               ContextManifest
                                      │
                                      ▼
                             Provider adapter rendering
                                      │
                                      ▼
                         one provider request / stream
```

The journal does not become a prompt, the prompt does not become Memory, and the provider's conversation object does not become canonical history.

### 1.1 Domain vocabulary

This report uses the current [CONTEXT.md](../../CONTEXT.md) terms:

- A **Session** is the durable conversation and working context containing multiple Runs.
- A **Run** is one bounded attempt to advance the Session to a terminal outcome.
- An **Event** is an immutable Engine fact.
- A **Message** is authored input or durable output.
- **Memory** is durable, scoped knowledge selected for reuse beyond its originating Event.
- An **Artifact** is immutable addressable output retained outside an Event payload because of size or lifecycle.

The following are proposed implementation terms, not additions to the glossary yet:

- `ContextLineage`: the ancestry and provider-state lineage for one AgentRun's model context.
- `ContextManifest`: a reproducible record of inputs selected, excluded, ordered, budgeted, and rendered for one provider call.
- `MemoryRecord`: one versioned Memory statement plus scope, provenance, status, and lifecycle metadata.
- `ContentObject`: separately retained message/event content that can be redacted or erased while a minimal Event envelope remains.
- `CompactionRecord`: a derived summary or provider-native compacted state with source coverage and provenance.

**Gate:** add these to `CONTEXT.md` only after Phase 0 fixtures demonstrate that each term has distinct behavior and the implementation plan approves the semantics.

## 2. Authority and lifecycle model

### 2.1 Canonical history

Canonical history answers “what did the harness accept and observe?” It contains:

- accepted Commands and resulting state-transition Events;
- authored Messages with actor, role, visibility, sensitivity, and content reference;
- provider request/completion identities and normalized outcomes;
- tool calls, approvals, cancellations, assignments, and inter-agent communication;
- Memory lifecycle Events and Artifact descriptors;
- causation, correlation, per-aggregate ordering, and schema versions.

It does **not** contain every token delta, spinner tick, cache entry, embedding, projection label, or model-generated speculation. This matches the canonical-event rules in the multi-agent report.

Provider-hosted conversation state cannot be canonical. OpenAI Conversations can persist messages, tool calls, and other items across sessions or jobs, but their storage and retention semantics are endpoint-specific; Responses may be stored for a default period while Conversation items persist until deleted ([OpenAI conversation state](https://developers.openai.com/api/docs/guides/conversation-state), [OpenAI data controls](https://developers.openai.com/api/docs/guides/your-data)). Anthropic Messages is stateless from the application's point of view for ordinary calls, while managed or client-side memory features have different storage behavior ([Anthropic memory tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool), [Anthropic API retention](https://platform.claude.com/docs/en/manage-claude/api-and-data-retention)). **Fact**

**Inference:** provider IDs are continuation accelerators and audit metadata. The harness must retain enough local content and normalized events to recover, change Provider, rebuild context, enforce deletion, and explain behavior without relying on a provider-side object.

### 2.2 Working state

Working state is a pure reduction of canonical Events plus a versioned snapshot optimization. It includes current Session/Run/AgentRun state, pending calls, active policy and instruction snapshot digests, budgets, leases, open approvals, and current Assignment dependencies.

Snapshots record their reducer version and last covered sequence. Replay from zero and snapshot-plus-tail must produce the same state. A snapshot can be deleted and rebuilt; deleting it cannot delete user data or change meaning.

### 2.3 Working context

Working context is the exact bounded material rendered for one inference. It has a one-call lifecycle and a recorded manifest. It may include:

- trusted system/developer instructions and tool schemas;
- current Command and selected working state;
- recent verbatim Messages and semantic Events;
- bounded Artifact excerpts;
- retrieved Memory records and their provenance labels;
- derived summaries or a provider-native compacted state;
- response schema, tool choice, and Provider settings.

It excludes content for reasons that must be observable: wrong scope, policy denied, expired, superseded, deleted, untrusted for the requested slot, duplicate, too large, lower priority than the remaining budget, or unsupported by the Provider.

Working context is not durable Memory merely because the model saw it. Conversely, Memory need not be loaded into every context.

### 2.4 Durable Memory

Memory answers “what knowledge may be reused later, by whom, and why?” A `MemoryRecord` needs at least:

```text
memory_id, version, kind, status
author and asserting actor
scope and visibility
statement or content_ref
source Event/Message/Artifact references
source hashes and observed_at
valid_from, valid_until, supersedes
created_at, reviewed_at, expires_at
sensitivity and retention class
writer capability/grant and policy snapshot digest
extractor/summarizer model and prompt versions, when generated
```

Confidence is advisory metadata, never authorization. A model claiming high confidence cannot widen scope, bypass review, or convert retrieved data into instructions.

### 2.5 Artifacts and derived data

Artifacts hold large or independently useful content: command output, patches, images, documents, trace exports, datasets, and generated reports. Summaries, chunks, text extraction, thumbnails, embeddings, and search indexes are derived from Artifacts and can be regenerated from the retained source.

The Open Container Initiative's content descriptor is a useful precedent: media type, digest, and byte size identify content, and consumers verify digest and size after retrieval ([OCI descriptor specification](https://github.com/opencontainers/image-spec/blob/main/descriptor.md)). **Fact**

**Recommendation:** use an `ArtifactRef` containing a versioned digest algorithm, digest, byte size, media type, storage generation, sensitivity, and retention class. Content addressing detects corruption and deduplicates identical bytes; it does not authorize access. Authorization remains a lookup on the reference plus caller, workspace, Session, policy, and current retention state.

## 3. Non-negotiable invariants

1. **Canonical-before-derived:** an acknowledged semantic mutation is journaled before projections, indexes, summaries, or embeddings claim it exists.
2. **No implicit authority:** retrieved text, summaries, provider compaction items, and model-written Memory are untrusted data. Only the instruction resolver and policy compiler create trusted guidance or restrictions.
3. **Scope before similarity:** authentication, authorization, tenant/workspace/session visibility, validity, deletion, and sensitivity filters run before lexical/vector ranking and again before context emission.
4. **No mutable shared prompt:** each AgentRun has one context lineage; collaboration occurs through journaled Messages, Assignment results, and explicit Artifact/Memory references.
5. **Deterministic compilation:** the same immutable inputs, versions, and capability snapshot yield the same logical `ContextManifest` and exclusion reasons.
6. **Bounded everything:** candidate counts, bytes, tokens, query time, summarization work, index lag, cache size, Artifact excerpt size, provider retries, and per-run memory writes have configured limits and defined overflow behavior.
7. **Deletion takes precedence over cache:** once a tombstone is committed, queries and context compilation exclude the content immediately even if physical purge or provider deletion is still running.
8. **Derived means rebuildable:** a summary, embedding, index row, prompt cache, or provider continuation handle can disappear without corrupting canonical history or Memory lifecycle truth.
9. **Compaction is not erasure:** shortening a model context never implies deletion of canonical content. Retention/erasure is a separate authorized workflow.
10. **User-visible provenance:** every durable Memory and every non-verbatim summary exposed to users identifies who or what authored it and what source range it covers.
11. **No cache-driven staleness:** cache-hit optimization cannot keep a superseded instruction, deleted Memory, stale tool schema, or wrong policy snapshot in use.
12. **Provider state is branch-local:** a continuation or opaque compaction handle is bound to Provider, model/configuration, tenant/project, ContextLineage, and source manifest digest.

## 4. Deterministic context compilation

### 4.1 Why compilation is a module

Context assembly hides substantial policy: trust ordering, freshness, deduplication, retrieval, token reservation, summary selection, Provider capabilities, and explainability. If every agent loop or Provider adapter performs those choices, behavior diverges and tests cannot explain why one fact was present. The deletion test therefore justifies a deep `ContextCompiler` module: removing it would spread complex selection policy across the Engine, Providers, agent orchestration, and tests.

Its external interface should remain close to:

```rust
#[async_trait]
pub trait ContextCompiler {
    async fn compile(
        &self,
        request: CompileContext,
    ) -> Result<CompiledContext, ContextCompileError>;
}

pub struct CompileContext {
    pub lineage: ContextLineageSnapshot,
    pub run: RunSnapshot,
    pub command: CommandRef,
    pub instructions: InstructionSnapshotRef,
    pub policy: PolicySnapshotRef,
    pub provider: ProviderContextCapabilities,
    pub budget: ContextBudget,
    pub memory_query: AuthorizedMemoryQuery,
}

pub struct CompiledContext {
    pub items: Vec<LogicalContextItem>,
    pub manifest: ContextManifest,
}
```

The illustrative types are not a committed public interface. The module may query journal, Memory, and Artifact implementations internally. Callers should not coordinate retrieval, ranking, truncation, and token packing themselves.

### 4.2 Two-stage compilation

Provider tokenization and request rendering can vary. A deterministic design therefore has two stages:

1. **Logical compilation:** select typed items, trust labels, source references, ordering groups, and byte/token reservations using a frozen Provider capability snapshot and deterministic candidate order.
2. **Provider rendering:** translate logical items into provider roles/content blocks/tool definitions, perform the provider's exact or conservative token count, place supported cache controls, and return a render receipt.

If exact token counting is available only through a Provider endpoint, the logical compiler emits a bounded provisional plan and the Provider adapter performs a preflight. An over-budget result returns a typed `BudgetRecompileRequired` with measured section counts; the Engine recompiles with those immutable measurements. It does not silently drop arbitrary messages inside the adapter.

### 4.3 Budget model

One total context-window number is insufficient. Use explicit reservations:

```text
provider maximum
  - maximum requested output
  - provider-required reasoning/tool overhead
  - safety margin for tokenizer/render drift
  = maximum rendered input

rendered input budget
  = mandatory trusted instructions and schemas
  + current command and critical working state
  + recent verbatim continuity
  + selected artifacts/retrieval
  + summaries/compaction state
```

If mandatory material alone exceeds the safe input budget, compilation fails with section-level diagnostics. It must not truncate policy, the current user Command, or a tool schema into syntactically valid but semantically incomplete content.

**Recommendation:** initial safety margins are Provider-adapter configuration validated by contract tests, not a universal percentage in domain code.

### 4.4 Selection order

Within one immutable compilation attempt:

1. Resolve active instruction and policy snapshots for the target scope.
2. Load the current Command and reducer snapshot at a fixed journal sequence.
3. Determine Provider/model/render capabilities and exact budget reservations.
4. Select mandatory trusted items.
5. Select the newest minimal verbatim continuity window without crossing deletion or lineage boundaries.
6. Execute a scoped Memory/Artifact retrieval query with fixed limits and deadline.
7. Normalize candidates to stable IDs, remove unauthorized/deleted/expired/superseded items, and deduplicate by canonical source/content hash.
8. Rank by a versioned deterministic function. Break equal scores by stable ID, never database iteration order.
9. Pack candidates by priority and measured cost; record every exclusion reason.
10. Add provenance delimiters and trust labels as structured logical metadata.
11. Hash the source revisions, compiler/ranker versions, capability snapshot, budget, ordered item IDs, and render-relevant configuration into a `manifest_digest`.
12. Ask the Provider adapter to render/count. Record the final request digest and actual usage when available.

Model-generated query expansion or reranking is permissible only as a separately versioned derived stage. Its output becomes an input to deterministic filtering and packing; it does not make the overall model call reproducible in the mathematical sense. The manifest must identify that nondeterministic stage and its model/prompt versions.

### 4.5 Context precedence is not authority precedence

Prompt ordering influences a model but cannot enforce policy. The compiler should render clear typed sections, but deterministic authorization still occurs at every effect adapter. The recommended logical order is:

1. trusted harness/provider instructions;
2. selected repository guidance with source envelopes;
3. stable tool schemas;
4. Assignment and active Run state;
5. prior authored Messages and results;
6. retrieved Memory and Artifact excerpts, explicitly labelled as untrusted data;
7. current user input in the Provider-appropriate position.

Never place retrieved text inside a trusted developer/system slot merely to improve compliance or caching. OpenAI's agent-safety guidance warns that putting untrusted variables into developer messages grants them stronger influence and recommends structured outputs between workflow nodes ([OpenAI agent safety](https://developers.openai.com/api/docs/guides/agent-builder-safety)). **Fact**

### 4.6 Explainability and replay

`ContextManifest` should include:

```text
context_id, lineage_id, run_id, agent_run_id
journal_sequence, compiler_version, ranker_version
instruction_snapshot_digest, policy_snapshot_digest
provider capability/configuration digest
total and per-section budgets
ordered included item descriptors
excluded candidate descriptors and reason codes
summary/compaction coverage ranges
retrieval query/filter digest and index revision
logical manifest digest and rendered request digest
actual token/cache usage when returned
```

Raw sensitive text does not belong in the manifest. References and digests enable privileged inspection without leaking content into routine telemetry.

## 5. Prompt caching without semantic coupling

### 5.1 What providers establish

OpenAI prompt caching matches reusable rendered prefixes; changing model, tools, response format, reasoning settings, or context management can change reuse. It recommends stable content first and append-only conversation history, while noting that summarization, compaction, or truncation can reset prefix reuse ([OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)). **Fact**

Anthropic caches tools, system blocks, messages, images/documents, and tool interactions; its hierarchy is tools → system → messages, so an earlier change can invalidate later cache portions. Its current controls include automatic or explicit breakpoints and provider/model-specific minimums and lifetimes ([Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)). **Fact**

Gemini supports implicit caching for current model families and also has API-surface-dependent explicit caching. Its documentation recommends placing large shared content at the beginning and reports cached token usage ([Gemini context caching](https://ai.google.dev/gemini-api/docs/caching), [Gemini explicit caching](https://ai.google.dev/gemini-api/docs/generate-content/caching)). **Fact**

**Inference:** “cache breakpoint,” TTL, minimum cacheable length, cache key, cache isolation, write/read pricing, and supported input blocks are Provider capabilities. A common domain cache abstraction would either expose vendor vocabulary everywhere or erase behavior the Engine needs to measure.

### 5.2 Provider capability snapshot

The Provider adapter should translate its current documentation/configuration into a normalized internal snapshot such as:

```rust
pub struct ProviderContextCapabilities {
    pub max_input_tokens: TokenLimit,
    pub max_output_tokens: TokenLimit,
    pub token_counting: TokenCountingMode,
    pub cache: PromptCacheCapabilities,
    pub compaction: CompactionCapabilities,
    pub continuation: ContinuationCapabilities,
    pub supported_content: ContentCapabilities,
    pub retention: ProviderRetentionCapabilities,
    pub capability_version: CapabilityVersion,
}
```

This value is recorded for each request. Discovery failure or unknown limits fails closed for a configured strict profile; it does not assume yesterday's model behavior.

The domain may understand general facts such as “this request has a safe input budget” and “provider state is deletable or not.” It must not contain model-name branches, cache-control JSON, opaque compaction item schemas, or provider retention constants.

### 5.3 Stable-prefix strategy

Prefer this physical arrangement when supported:

```text
stable provider/tool definitions
stable harness and repository instructions
append-only prior rendered history or compacted branch state
dynamic retrieved/contextual suffix
current input
```

But correctness wins over cache reuse:

- a changed policy or instruction snapshot invalidates the affected prefix;
- a revoked tool schema is removed immediately;
- corrected/deleted Memory is excluded even if that causes a cache miss;
- user- or tenant-specific content never moves into a cross-tenant shared prefix;
- unstable timestamps, request IDs, and live counters stay after reusable material;
- do not pad prompts merely to cross a cache minimum without a measured net benefit.

### 5.4 Cache identity and telemetry

Local cache accounting keys should include tenant/workspace, Provider endpoint/project, model, tool-schema digest, instruction snapshot digest, response schema, reasoning mode, compiler/render version, and retention profile. Provider keys remain adapter details.

Observe per call:

- eligible, written, hit, and uncached tokens;
- cache-hit ratio by stable section;
- time to first provider byte/token;
- cache invalidation reason;
- cache write/read cost where reported;
- context bytes and tokens before/after compaction;
- net cost and latency versus an uncached control sample.

Cache hit rate alone is not a success metric. A smaller non-cached prompt may be cheaper and faster than a large cached one.

## 6. Truncation, summarization, and compaction

### 6.1 Separate four operations

| Operation | Meaning | Lossy? | Canonical? |
|---|---|---:|---:|
| Omission | Do not select an otherwise retained item for this call | Yes for this call | No |
| Truncation | Cut content to a declared bounded excerpt | Yes | No |
| Summarization | Create a new derived representation covering sources | Yes | No |
| Provider compaction | Use provider-native condensed/opaque branch state | Provider-defined | No, except as continuation state for that branch |

Calling all four “compaction” hides different correctness, provenance, and deletion behavior.

### 6.2 Layered pressure response

When approaching a budget:

1. Replace repeated or bulky tool output with an Artifact descriptor and small relevant excerpt.
2. Remove ephemeral deltas already represented by a final Message or semantic Event.
3. Omit candidates outside hard scope, validity, freshness, and task relevance.
4. Retain the minimal recent verbatim conversational window and unresolved decisions.
5. Use an existing valid summary whose source hashes and compiler policy still match.
6. Generate a new summary only before the source window reaches the Provider hard limit.
7. Use provider-native compaction when its semantics, retention, and compatibility meet the active profile.
8. If mandatory context still does not fit, stop with an actionable `ContextBudgetExceeded`; do not blind-FIFO away constraints.

Anthropic says all request material—including tool definitions, messages, images/documents, and output—counts toward context, and warns that more context is not automatically better. Its current guidance makes server-side compaction primary for long-running workflows ([Anthropic context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows)). OpenAI's standalone compact endpoint returns a compacted window that must be carried forward as returned, and its opaque item is not a human summary ([OpenAI compaction](https://developers.openai.com/api/docs/guides/compaction)). **Fact**

**Recommendation:** model these as different adapters. A harness summary is inspectable derived content. An OpenAI opaque item is provider branch state. Neither is durable Memory unless a separately authorized Memory write creates a provenance-bearing record.

### 6.3 Summary contract

A summary must declare:

- exact source Event/Message sequence ranges and content hashes;
- summary purpose and granularity;
- author type (`model`, `user`, or deterministic extractor);
- model, prompt, schema, and compiler versions;
- created time, token/byte count, scope, sensitivity, and retention class;
- facts, decisions, unresolved work, constraints, failures, approvals, and evidence references in typed fields where practical;
- known omissions and validation results.

Summaries are immutable versions. A correction creates a new summary or invalidates the old one; it never edits provenance in place. A summary becomes ineligible when a covered source is corrected, erased, or no longer authorized.

### 6.4 Hierarchical summaries

Use levels only after evaluation justifies them:

```text
verbatim Messages/Events
        ↓
turn or tool-cycle summaries
        ↓
Assignment/run summaries
        ↓
Session/project Memory candidates
```

Every upward edge records exact coverage. Retrieval may select the narrowest level that answers the query within budget, but it should prefer original sources for exact identifiers, quotes, security decisions, and recent changes.

### 6.5 Long-running session checkpoints

Before compaction, a durable checkpoint should confirm:

- all acknowledged Events are committed;
- Artifact writes referenced by retained events are finalized;
- open tools/approvals and unresolved assignments are represented in working state;
- the summary source range is closed and hashable;
- the next ContextLineage revision is linked to the prior manifest;
- cancellation during compaction leaves either the old or new valid lineage, never a half-state.

Compaction is an asynchronous derived job. The active agent loop can continue only when the Engine commits which old or new context revision it will use.

## 7. Durable Memory lifecycle

### 7.1 Writes are effects

A durable Memory write can influence future Runs and agents, so it is a capability-governed effect. The model may propose a `MemoryCandidate`; the Engine authorizes and commits a `MemoryRecord` according to scope and review policy.

Recommended defaults:

- explicit user requests such as “remember this for this Workspace” may commit after typed confirmation of scope and content;
- model-inferred preferences, procedures, or lessons remain pending until user review or a narrowly configured policy allows that class;
- observed execution facts remain in canonical history and derived search unless deliberately promoted to Memory;
- secrets, credentials, raw environment values, and hidden reasoning are never Memory;
- children may propose Memory only within inherited scope and capability; promotion beyond the Assignment requires an authorized parent/user decision.

Anthropic's memory tool is client-side, supports persistent create/read/update/delete operations, and makes the application responsible for storage, size limits, expiry, sensitive-data handling, and path confinement ([Anthropic memory tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)). **Fact**

**Inference:** provider-provided memory tools validate the product need but do not remove the harness's policy, provenance, review, and deletion responsibilities.

### 7.2 Memory kinds

Use a closed initial catalog rather than arbitrary tags:

- `UserPreference`: explicit stable preference, usually user-scoped and user-authored.
- `WorkspaceFact`: durable fact about the authorized Workspace with source evidence.
- `Decision`: an accepted product/architecture/workflow decision with status and supersession.
- `Procedure`: repeatable steps approved for reuse; it remains guidance, not authority.
- `Episode`: bounded account of an earlier outcome useful for analogous work.

“Summary” is not a Memory kind by default; it is a representation. A summary becomes Memory only through an explicit lifecycle transition with scope and review.

### 7.3 Scope and visibility

Represent scope as an intersection of typed dimensions, not a path-like free string:

```text
principal/tenant
workspace
session (optional)
team/agent visibility (optional)
purpose or task class (optional)
valid time interval
sensitivity/retention class
```

User-global Memory is high blast radius and must be explicitly user-authored or approved. Workspace Memory never crosses to another Workspace merely because embeddings are similar. Session-private or agent-private scratch information is not visible to siblings unless a Message or result explicitly shares it.

### 7.4 Correction and supersession

A correction creates a new record version linked by `supersedes`. The old version becomes ineligible for ordinary retrieval but remains visible in privileged history until its retention policy erases it. Queries choose the latest authorized valid version at a fixed catalog revision.

Conflicting records are not silently averaged by a model. The read model exposes a conflict, source provenance, and review action. The context compiler may include both with a conflict marker or exclude them under a configured policy.

### 7.5 Expiry and forgetting

Expiry is eligibility, not immediate physical erasure. At `expires_at`, the record becomes query-ineligible. A background purge removes content/index rows according to retention policy and emits a receipt. Access may refresh a lease only where policy explicitly allows; a model read must not silently convert temporary Memory into permanent Memory.

## 8. Scoped retrieval

### 8.1 Retrieval pipeline

The safe ordering is:

```text
authenticate caller
  → authorize Memory/Artifact scopes
  → apply deletion, validity, sensitivity, and purpose filters
  → generate lexical query and optional semantic query
  → retrieve bounded candidates
  → optional rerank/diversify within the authorized set
  → verify scope again
  → budget-aware context packing
  → provenance-labelled emission
```

Filtering after a global vector search can leak existence through timing, scores, counts, logs, or mistaken post-filter behavior. The storage query itself must constrain the authorized corpus, and the Engine verifies returned records again.

### 8.2 Lexical-first baseline

SQLite FTS5 supports full-text search, column filters, snippets, and BM25 ranking ([SQLite FTS5](https://www.sqlite.org/fts5.html)). **Fact**

**Recommendation:** the initial index stores normalized searchable text beside canonical Memory IDs and authorization/filter columns in ordinary SQLite tables. Execute hard relational filters and FTS joins in one controlled query plan. Candidate count, query length, result bytes, and execution time are bounded. Search exact paths, symbols, error messages, identifiers, and explicit facts before adding embeddings.

### 8.3 Embedding/vector gate

Approximate nearest-neighbor search trades recall for latency. `pgvector` documents exact search as the default and approximate HNSW/IVFFlat indexes as recall/speed trade-offs; filtering can require iterative scans or partitioning ([pgvector](https://github.com/pgvector/pgvector)). **Fact**

For every embedding store, retain:

```text
source Memory/Artifact ID and version
source content hash and chunk span
scope/filter columns
embedding provider/model/version and dimension
normalization and distance metric
chunker/extractor version
index generation and created time
```

**Gate:** add dense retrieval only if, on a held-out project corpus, it materially improves end-to-end task correctness or required recall over tuned FTS while meeting cross-scope leakage, p95 latency, index-lag, storage, deletion, and cost budgets. Retrieval recall alone is insufficient.

### 8.4 Freshness under asynchronous indexing

Indexing uses an outbox committed with the canonical mutation. Retrieval combines the latest completed index generation with a bounded scan of newer eligible Memory changes. A failed or lagging index cannot make fresh corrections or tombstones disappear.

Tombstones are checked in the authoritative catalog before returning any candidate. An old vector hit for deleted content resolves to “unavailable,” not stale text.

### 8.5 Provenance in context

Each retrieved item carries a structured header that the Provider renderer can express without upgrading its trust:

```text
record ID and version
kind and author
scope and observed/valid time
source references
retrieval method and score class
trust label: untrusted_data
```

Scores are not shown as confidence in truth. BM25/vector relevance means similarity to the query, not factual correctness or instruction authority.

## 9. Multi-agent and branch context isolation

### 9.1 One lineage per AgentRun

The reusable agent loop should accept a `ContextLineageId`. Root and child AgentRuns use the same loop but different lineages. A lineage records:

- parent lineage and fork sequence, if any;
- Assignment ID and exact assignment brief;
- inherited instruction/policy snapshots;
- capability/budget snapshot, never wider than the parent grant;
- explicit Memory/Artifact visibility grants;
- journal cursor and selected Messages;
- Provider continuation/compaction state bound to this branch;
- latest ContextManifest.

OpenAI's current multi-agent API documents independent contexts for subagents, and Codex documentation describes separate agent threads so noisy intermediate work does not pollute the main conversation ([OpenAI Agents multi-agent](https://developers.openai.com/api/docs/guides/agents-api/multi-agent), [Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents)). **Fact**

### 9.2 Assignment capsule

A child starts with a bounded capsule:

1. identity, role, Assignment objective, expected result schema, and stop conditions;
2. current user goal and only the decisions/constraints relevant to the Assignment;
3. instruction/policy snapshot references and effective capabilities;
4. explicitly shared Memory and Artifact references;
5. dependencies and parent contact route;
6. time, token, cost, tool, and output budgets.

It does not receive the parent's full transcript, hidden reasoning, unrelated Memory, every tool schema, or sibling output by default.

### 9.3 Communication and joins

Inter-agent Messages are authored, journaled, bounded, scope-checked input to a future compilation. They do not mutate a context already being processed by a Provider. Steering creates a new lineage revision or subsequent model call.

Child completion returns a typed result:

```text
status and concise authored summary
claims with evidence/Artifact references
changed resources
open risks or questions
usage and budget outcome
proposed Memory candidates, if authorized
```

The parent compiler selects that result, not the child's entire context. Join summaries identify every included or rejected child result and remain attributable. A provider continuation ID is never shared between sibling lineages.

### 9.4 Branch correction and cancellation

If a user corrects or deletes source data:

- new compilations across affected lineages exclude the old version immediately;
- active calls whose continued output could cause a sensitive effect are cancelled under policy;
- completed child claims derived from invalidated content are marked stale and must be revalidated before synthesis;
- provider-side branch deletion is attempted where supported and tracked separately;
- the canonical correction/tombstone Event remains visible according to retention policy.

## 10. Artifact handling

### 10.1 Storage contract

An Artifact store needs a small interface:

```rust
#[async_trait]
pub trait ArtifactStore {
    async fn put(&self, request: PutArtifact) -> Result<ArtifactRef, ArtifactError>;
    async fn open(&self, request: OpenArtifact) -> Result<BoundedArtifactReader, ArtifactError>;
    async fn tombstone(&self, request: TombstoneArtifact) -> Result<PurgeReceipt, ArtifactError>;
}
```

`put` streams into a size-bounded temporary object, computes digest, validates declared media type against detection policy, atomically publishes, and returns a descriptor. `open` authenticates and authorizes before reading, supports bounded ranges/previews, verifies size/digest when trust requires it, and never returns an unbounded byte vector. `tombstone` makes the reference immediately unavailable and schedules physical purge according to retention.

### 10.2 Extraction

Text extraction, OCR, archive inspection, syntax parsing, and thumbnailing are untrusted transformations with their own limits:

- maximum input, decompressed bytes, entries, nesting, dimensions, pages, and CPU/wall time;
- sandboxed process or memory-safe parser appropriate to the format;
- extractor name/version and source digest on every output;
- no active HTML/SVG/script execution;
- URLs and embedded files are data, not automatically fetched;
- extracted text remains at the Artifact's scope and sensitivity.

The compiler selects an excerpt descriptor with byte/line/page spans. It does not place an entire large Artifact into context because a Provider accepts files.

### 10.3 Message and Event content erasure

An immutable journal conflicts with legal/product deletion if sensitive raw text is stored inline forever. **Recommendation:** journal immutable semantic envelopes and store redactable authored/raw content as encrypted or separately addressable `ContentObject`s. The envelope records that content existed and its lifecycle; a deletion Event makes the object unavailable and starts physical erasure. Replay yields a typed redacted placeholder after erasure.

Small non-sensitive structural payloads may stay inline. The line between inline and separately retained content is a versioned retention policy, not an arbitrary byte threshold alone.

## 11. User inspection, correction, export, and deletion

### 11.1 Product surface

Users need a Memory and data-control view independent of the activity timeline:

- list Memory by scope, kind, author, status, source, and expiry;
- inspect exact content, provenance, and every version;
- see whether a record was user-authored, model-proposed, imported, or derived;
- correct by creating a superseding version;
- narrow scope or shorten retention;
- tombstone/delete one record or a whole scope;
- export canonical Messages, Memory, and Artifact descriptors in a documented format;
- see derived index, cache, and provider-cleanup status without implying they are Memory;
- review pending model-proposed Memory candidates.

No hidden “personalization” store may influence context without appearing in this inventory.

### 11.2 Deletion workflow

Deletion is a state machine, not one SQL statement:

```text
Requested
  → Authorized
  → Tombstoned (query/context denial is immediate)
  → LocalContentPurged
  → DerivedDataPurged
  → ProviderDeletionCompleted | ProviderDeletionUnsupported | ProviderDeletionFailed
  → CompletedWithReceipt
```

The receipt reports exact scope, counts, local completion, outstanding adapters, and provider limitations. It never says “deleted everywhere” when a third party lacks deletion or retains abuse/security logs.

OpenAI documents materially different retention across Responses, Conversations, files, caches, and hosted tools; Anthropic separately documents stateless APIs, caches, managed sessions, and ZDR eligibility; Gemini distinguishes API state, files, explicit caches, implicit in-memory caches, and Live session resumption ([OpenAI data controls](https://developers.openai.com/api/docs/guides/your-data), [Anthropic data retention](https://platform.claude.com/docs/en/manage-claude/api-and-data-retention), [Gemini ZDR](https://ai.google.dev/gemini-api/docs/zdr)). **Fact**

**Inference:** the harness needs adapter-reported retention and deletion capabilities plus honest partial-completion receipts. A single `retention_days` field cannot describe provider behavior.

### 11.3 Immediate non-resurrection guarantee

After the tombstone commit:

- authoritative reads deny the content;
- context caches include the tombstone/catalog revision in their validity key;
- lexical/vector hits resolve through the catalog and cannot return old content;
- summaries covering the source are invalidated;
- pending indexing jobs check tombstones before writing;
- physical backup retention is disclosed and handled by the configured storage policy;
- restoring a backup replays later tombstones before serving queries.

This is stronger and more testable than promising immediate physical disappearance from every medium.

## 12. Security and trust analysis

### 12.1 Assets and attackers

Assets include user/workspace data, canonical history, Memory correctness, Artifact content, provider credentials, deletion guarantees, instruction/policy integrity, and the confidentiality of one agent's context from unauthorized users or siblings.

Attacker-controlled inputs include user/model text, repository content, web pages, MCP/tool output, imported documents, Memory proposals, summaries, embeddings, metadata, filenames, media types, Provider responses, and inter-agent Messages.

### 12.2 Main attack paths

1. **Persistent prompt injection:** malicious tool/web/repository text is promoted to Memory and re-enters future Sessions.
2. **Scope confusion:** retrieval returns another tenant, Workspace, Session, or private child record.
3. **Instruction laundering:** retrieved text or a summary is rendered as developer/system guidance.
4. **Poisoned summarization:** a malicious source causes a summary to omit constraints or invent authority.
5. **Stale resurrection:** a corrected/deleted fact survives in an index, summary, prompt cache, provider conversation, or backup.
6. **Inference leakage:** counts, scores, cache hits, or timing reveal existence of inaccessible content.
7. **Artifact parser attack:** archive bombs, malformed documents, path traversal, active HTML/SVG, or parser vulnerabilities affect the trusted Engine.
8. **Budget denial:** a source expands context, Memory writes, chunks, embeddings, or summaries until cost/storage is exhausted.
9. **Cross-agent contamination:** a child or sibling sends malicious instructions disguised as evidence or progress.
10. **Deletion overclaim:** the UI treats local tombstoning as third-party physical deletion.

OpenAI defines prompt injection as malicious untrusted content attempting to override agent instructions and recommends limiting access, structured data flow, and approvals; Anthropic's memory-tool guidance specifically requires sensitive-data filtering, size limits, expiry, and path-traversal protection ([OpenAI agent safety](https://developers.openai.com/api/docs/guides/agent-builder-safety), [Anthropic memory security](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)). **Fact**

### 12.3 Required controls

- typed provenance and trust class on every context item;
- separate instruction discovery from retrieval;
- closed Memory kinds and scope fields; deny unknown fields;
- `memory.read` and `memory.write` capability checks in the Engine and storage adapter; correction and deletion are typed write operations unless a later effect-catalog decision adds a narrower `memory.delete` class;
- restrict-only descendant capability inheritance;
- pre-ranking and post-retrieval authorization;
- model Memory candidates held pending by default;
- secret/PII scanning appropriate to the configured policy before durable writes;
- deterministic maximum writes, bytes, records, candidates, chunks, and background jobs;
- sandboxed/bounded extraction and sanitized rendering;
- no raw prompts, Memory, Artifact text, or embeddings in ordinary logs/traces;
- deletion/tombstone revision checked by every cache and index read;
- adversarial evaluation of injection persistence and cross-scope retrieval;
- explicit effective provider retention/deletion report.

No classifier or model output can grant Memory scope, trust, or capability. Classifiers may flag content for review or denial.

## 13. Rust module placement and typed interfaces

### 13.1 Logical modules, not immediate crates

| Module | Responsibility hidden behind its interface | Initial placement | Reason for later split |
|---|---|---|---|
| `domain` | IDs, Memory/Artifact lifecycle vocabulary, Events, invariants, pure reducers | `arany-domain` | Already justified as dependency-control core |
| `context` | deterministic selection, ordering, budgets, provenance, manifest/exclusion reasons | private Engine module | Split only if reused independently or compile ownership demands it |
| `memory` | Memory lifecycle commands, scoped query, retention/tombstone coordination | private Engine + SQLite adapter | Split when a second backend or independent ownership exists |
| `retrieval` | lexical index/query and optional derived semantic indexing | initial adapter module | Split after a second real index or heavy native dependency exists |
| `artifacts` | bounded content-addressed put/open/tombstone | initial adapter module | Split for object-store backend or independent lifecycle |
| `providers` | render/count/cache/continuation/compaction/retention capability translation | Provider adapters | Real external seam already exists |
| `protocol` | user commands, Memory views, Context diagnostics, deletion receipts | `arany-protocol` DTOs | Existing cross-language seam |

This reconciles the richer module map in the Rust feasibility report with the smaller initial workspace in the modular architecture report. A module is warranted now; a crate is not automatically warranted.

### 13.2 Domain types

The domain owns stable semantics only:

```rust
pub struct MemoryId(/* opaque */);
pub struct MemoryVersion(u64);
pub struct ArtifactRef { /* digest, size, media type, identity */ }
pub struct ContextId(/* opaque */);
pub struct ContextLineageId(/* opaque */);

pub enum MemoryStatus {
    PendingReview,
    Active,
    Superseded,
    Expired,
    Tombstoned,
}

pub enum ContextExclusionReason {
    Unauthorized,
    WrongScope,
    Expired,
    Superseded,
    Deleted,
    Duplicate,
    OverBudget,
    UnsupportedContent,
    StaleDerivedData,
}
```

It does not own SQL rows, embeddings, tokenizers, provider role enums, cache JSON, filesystem paths, or vector scores.

### 13.3 Engine commands and events

User and agent operations enter through the existing small Engine command interface. Indicative Commands:

```text
ProposeMemory
AcceptMemory
CorrectMemory
TombstoneMemory
ChangeMemoryScope
ExportSessionData
DeleteSessionData
RequestContextExplanation
```

Indicative Events:

```text
memory.proposed
memory.recorded
memory.superseded
memory.expired
memory.tombstoned
memory.purge_progressed
memory.purge_completed
context.compiled
context.compaction_created
context.compaction_invalidated
artifact.recorded
artifact.tombstoned
```

Events carry references, reason codes, and digests by default rather than raw content.

### 13.4 Storage seams

Do not define a generic repository trait. Journal append/replay, Memory catalog, Artifact storage, and derived retrieval have different consistency and failure semantics.

An indicative internal Memory interface is:

```rust
#[async_trait]
pub trait MemoryCatalog {
    async fn apply(
        &self,
        expected_revision: CatalogRevision,
        command: MemoryMutation,
    ) -> Result<MemoryCommit, MemoryError>;

    async fn query(
        &self,
        query: AuthorizedMemoryQuery,
    ) -> Result<BoundedMemoryPage, MemoryError>;
}
```

`AuthorizedMemoryQuery` is constructed by trusted Engine policy code, not accepted directly from a client/model. The SQLite implementation should be tested as SQLite in temporary databases rather than hidden behind extensive mocks. A deterministic in-memory fake is useful for reducer/Engine scenarios only if it implements the same observable revision, scope, and tombstone semantics.

### 13.5 Provider seam

Extend the Provider seam with context operations without exposing vendor schemas:

```rust
#[async_trait]
pub trait ProviderContextRuntime {
    async fn capabilities(&self, model: &ProviderModelRef)
        -> Result<ProviderContextCapabilities, ProviderError>;

    async fn render_and_count(&self, logical: &CompiledContext)
        -> Result<RenderedProviderRequest, ProviderError>;

    async fn delete_state(&self, state: &ProviderStateRef)
        -> Result<ProviderDeletionReceipt, ProviderError>;
}
```

Native compaction and explicit-cache preparation may be capability-gated optional operations on this interface or private adapter behavior invoked by one normalized Engine command. Avoid a method per vendor feature on the public Engine interface.

### 13.6 Error semantics

Typed errors must distinguish:

- `MandatoryContextTooLarge`;
- `ProviderCountMismatch`;
- `CapabilitySnapshotStale`;
- `RetrievalDeadlineExceeded`;
- `IndexLagBeyondBound`;
- `UnauthorizedMemoryScope`;
- `DerivedDataStale`;
- `ContentTombstoned`;
- `ProviderDeletionUnsupported`;
- `ProviderRetentionIncompatible`;
- `ArtifactLimitExceeded`;
- `CompactionFailedButPriorContextValid`.

Failures never fall back to a wider scope, older policy, unsandboxed extractor, or unbounded prompt. Degraded operation may omit optional retrieval or use the still-valid prior context only when the active profile permits it and the result is surfaced.

## 14. Latency, cost, and resource bounds

### 14.1 Cost model

For model call `i`:

```text
T_i = T_compile + T_retrieve + T_render/count
    + T_network/queue + T_prefill + T_decode + T_stream

C_i = uncached_input + cache_write + cache_read
    + output/reasoning + retrieval/embedding + summarization
```

Rust can make local selection, filtering, storage, and streaming predictable. It cannot remove remote retrieval, prompt prefill, provider queueing, or generation. Longer context may increase latency even when prompt caching reduces billed input.

### 14.2 Initial bounds to make explicit

These are configuration categories, not final values:

- maximum recent Events/Messages scanned and selected;
- maximum retrieval queries, candidate rows, returned records, and bytes;
- maximum per-item and aggregate context bytes/tokens;
- maximum Artifact size, excerpt size, decompressed size, entries, pages, and extraction time;
- maximum Memory records and bytes per scope, writes per Run, and pending proposals;
- maximum summary source range, output, retries, concurrent jobs, and queue depth;
- maximum index lag by Events/time;
- maximum provider preflight/compaction attempts;
- maximum local cache entries and bytes;
- deletion/purge deadlines and retry caps.

Every maximum needs overflow behavior: reject, page, truncate with an explicit descriptor, spill to Artifact, omit optional retrieval, or pause for user action.

### 14.3 Proposed performance hypotheses

Measure on a declared corpus and reference machine before treating these as SLOs:

| Stage | Initial warm hypothesis | Notes |
|---|---:|---|
| Logical compile without retrieval | p95 ≤ 10 ms | fixed Run snapshot and bounded items |
| SQLite scoped FTS + packing | p95 ≤ 50 ms | target corpus and warm page cache |
| Full local context preparation | p95 ≤ 75 ms | excludes remote embedding/rerank |
| Tombstone to query invisibility | p99 ≤ 50 ms | local authoritative catalog |
| Context explanation lookup | p95 ≤ 100 ms | manifest only, no raw content fetch |
| Snapshot/tail Session recovery | p95 ≤ 200 ms | aligns with Rust feasibility report |

**Gate:** benchmark cold/warm, small/large Session, compaction boundary, concurrent AgentRuns, index lag, and deletion. Report distributions and bytes; do not merge Provider wait into “compiler overhead.”

### 14.4 Backpressure

- One bounded storage writer queue owns canonical SQLite writes.
- Retrieval has separate read permits so a slow query cannot starve cancellation/approval.
- Background summary/embedding/extraction queues are lower priority and discard/rebuild work safely on overflow.
- A Context compile is cancelled when its Run revision or tombstone catalog revision changes before dispatch.
- Provider token deltas do not trigger context or index writes per token.
- Artifact streaming applies byte and time bounds without buffering whole content in memory.

## 15. Evaluation plan

### 15.1 Evaluate the pipeline by layer

End-to-end quality alone cannot locate failures. LongMemEval explicitly decomposes memory into indexing, retrieval, and reading stages ([LongMemEval paper](https://proceedings.iclr.cc/paper_files/paper/2025/file/d813d324dbf0598bbdc9c8e79740ed01-Paper-Conference.pdf)). **Fact**

Record separate results for:

1. write/proposal decision;
2. canonicalization/chunking/indexing;
3. authorization and retrieval;
4. context selection/positioning;
5. model reading/reasoning;
6. final task outcome;
7. correction/deletion behavior.

### 15.2 Correctness metrics

**Context compiler**

- manifest determinism across repeated runs;
- mandatory-item inclusion and budget compliance;
- provenance coverage;
- duplicate and stale-item rate;
- exclusion-reason stability;
- rendered/request digest consistency;
- provider count prediction error.

**Retrieval**

- recall@k, precision@k, MRR/nDCG where labels permit;
- exact identifier/path/error retrieval;
- temporal-valid-version accuracy;
- knowledge-update and abstention accuracy;
- wrong-scope or deleted-item return rate: required zero;
- fresh-tail recall while the index lags.

**Summarization/compaction**

- atomic fact, constraint, decision, unresolved-task, and evidence preservation;
- contradiction and unsupported-claim rate;
- correction/tombstone propagation;
- answer/task delta versus verbatim baseline;
- degradation after repeated compaction generations;
- recovery after provider-state loss.

**Durable Memory**

- useful-write precision and missed-write recall;
- user correction rate and time-to-correct;
- stale/superseded fact usage;
- unauthorized persistence and sensitive-data write rate;
- end-to-end task success gain versus no-Memory baseline.

### 15.3 Security and privacy metrics

- prompt-injection success and persistence across Sessions;
- cross-tenant/Workspace/Session/agent leakage: required zero;
- instruction-laundering rate from Memory/Artifacts to trusted slots: required zero;
- unauthorized Memory mutation/delete attempts denied;
- tombstone-to-query-invisibility latency;
- deleted-content resurrection after index rebuild, backup restore, summary reuse, cache reuse, and provider continuation: required zero locally;
- Artifact traversal/decompression/parser escapes;
- raw sensitive content incidents in logs/traces/manifests.

### 15.4 Efficiency metrics

- compile/retrieval/render latency distributions;
- request bytes and tokens by context section;
- prompt-cache eligible/write/read/uncached tokens and invalidation reasons;
- time to first provider output and total turn time;
- input, output, cache, summarization, embedding, and storage cost per successful task;
- amplification over 1, 10, 50, and 200 turns;
- RSS, SQLite/WAL/index/Artifact bytes, queue depth, and CPU;
- per-AgentRun context and cache footprint;
- compaction frequency and quality/cost trade-off.

### 15.5 Corpora

Use a layered corpus:

1. **Deterministic fixtures:** exact scope, correction, deletion, ordering, budget edges, duplicate sources, clock boundaries, and malformed data.
2. **Project trace corpus:** sanitized real coding/research tasks with paths, symbols, decisions, failures, tool output, branch results, and known correct outcomes.
3. **Adversarial corpus:** prompt injection, poisoned Memory proposals, cross-scope near-duplicates, secret-shaped content, archive/document attacks, and deletion resurrection.
4. **Long-session corpus:** hundreds of turns with updates, reversals, unresolved work, provider switches, child-agent forks, and repeated compaction.
5. **External research baselines:** LongMemEval for extraction/multi-session/temporal/update/abstention; LoCoMo for long conversational and temporal/causal behavior; RULER for configurable retrieval, multi-hop tracing, and aggregation; position sweeps informed by Lost in the Middle ([LongMemEval](https://proceedings.iclr.cc/paper_files/paper/2025/hash/d813d324dbf0598bbdc9c8e79740ed01-Abstract-Conference.html), [LoCoMo](https://aclanthology.org/2024.acl-long.747/), [RULER](https://github.com/NVIDIA/RULER), [Lost in the Middle](https://direct.mit.edu/tacl/article/doi/10.1162/tacl_a_00638/119630/Lost-in-the-Middle-How-Language-Models-Use-Long)).

RULER's authors explicitly say their synthetic tasks are not comprehensive and cannot replace realistic tasks; Lost in the Middle shows that relevant information position can materially change performance. **Fact**

**Recommendation:** external benchmark scores are regression signals, not ship criteria. The held-out project corpus and security invariants decide the harness design.

### 15.6 Baselines and ablations

For every feature, compare:

- recent verbatim history only;
- full context where it fits;
- lexical retrieval;
- lexical plus summaries;
- lexical plus dense retrieval;
- harness summaries versus provider-native compaction;
- single-agent context versus isolated subagents;
- caching off/on with identical semantics.

Hold Provider/model, total task budget, and tool access constant. Report confidence intervals and failures, not only averages.

## 16. Implementation phases and research-to-implementation gates

### Phase 0 — semantics, fixtures, and interface spike

Specify:

- canonical Event/Message content references and erasure behavior;
- Memory kinds, scope lattice, lifecycle, and capability effects;
- ContextLineage and ContextManifest schemas;
- logical context item ordering, exclusion reasons, and budget algebra;
- Provider capability snapshot and retention/deletion receipt vocabulary;
- Artifact descriptor and bounded access semantics.

Build only pure reducers, schema fixtures, and two materially different `ContextCompiler` interface prototypes. Compare a compiler that owns retrieval internally with one that accepts pre-retrieved candidates; prefer the design with the smaller caller interface and stronger replay locality.

**Exit gates:**

- replay and context manifests are deterministic under randomized fixture ordering;
- deletion can erase raw content without making Event replay ambiguous;
- no Provider, SQL, tokenizer, or vector type enters `arany-domain`;
- every field has an owner, bound, failure mode, and versioning story;
- security review covers persistent injection, scope leakage, and deletion resurrection;
- approved terms are added to `CONTEXT.md` and hard-to-reverse choices receive ADRs.

### Phase 1 — durable local context without autonomous long-term memory

Implement:

- SQLite journal/snapshots and separate retained `ContentObject`s;
- Artifact store with bounded streaming and descriptors;
- deterministic recent-history compiler and context explanation;
- one Provider adapter with capability snapshot, render/count, and cache usage receipt;
- explicit user-authored Workspace/Session Memory only;
- tombstone-first deletion and derived purge receipts;
- isolated ContextLineage for root plus up to three workers.

**Exit gates:**

- forced-kill recovery reconstructs the same working state and next manifest;
- mandatory context never silently truncates;
- user correction/deletion is invisible to subsequent compiles immediately;
- child contexts contain only their assignment capsule and authorized references;
- all queues/bytes/tokens/retries have tested overflow behavior;
- local context-preparation and recovery hypotheses are measured, not asserted.

### Phase 2 — lexical retrieval and inspectable summaries

Implement:

- FTS5 index through a transactional outbox;
- hard-scope filtered query and recent-tail merge;
- versioned summary schema and invalidation;
- Memory review UI/protocol projections;
- long-session checkpoint and summary compaction;
- injection/cross-scope/deletion evaluation corpus.

**Exit gates:**

- zero scope/deletion violations in adversarial fixtures and fuzz/property tests;
- lexical retrieval improves held-out task success over recent-history-only baseline;
- summary compaction remains within accepted fact/constraint/task preservation thresholds across repeated generations;
- index lag and rebuild do not hide corrections or resurrect tombstones;
- users can inspect, correct, export, and delete every active Memory record.

### Phase 3 — provider-native context optimization

Implement only for Providers whose adapters can prove semantics:

- stable-prefix cache placement and diagnostics;
- provider continuation state;
- provider-native compaction behind normalized capabilities;
- retention-profile compatibility checks and deletion receipts;
- provider-switch recovery from local canonical state.

**Exit gates:**

- cache optimization reduces representative latency/cost without changing selected logical items;
- cache misses and compaction never affect correctness or authority;
- loss of provider state recovers from local history within the recovery budget;
- active retention requirements are accurately represented and incompatible requests fail before data is sent;
- contract tests cover at least two Providers, proving the seam.

### Phase 4 — generated Memory and semantic retrieval, only if earned

Evaluate:

- model-proposed Memory with review workflows;
- embeddings plus exact vector baseline and optional ANN;
- reranking, query expansion, and hierarchical summaries;
- user-global Memory if explicitly demanded.

**Exit gates:**

- statistically meaningful end-to-end gain on the held-out project corpus;
- useful-write precision and correction burden meet product thresholds;
- no regression in scope, deletion, injection persistence, or abstention;
- latency, storage, embedding migration, and cost remain within declared budgets;
- lexical-only remains a supported fallback and derived semantic indexes are rebuildable.

### Phase 5 — multi-host scale, only when workload requires it

Consider PostgreSQL/pgvector or a dedicated retrieval service only when multiple authoritative writers, tenancy/replication/operations, or measured SQLite queue/index limits demand it. Preserve the Engine semantics, ContextManifest, tombstone guarantees, and conformance suite.

**Gate:** dataset size alone is not a migration reason. Use writer queue latency, recovery requirements, concurrent ownership, availability, and measured retrieval SLOs.

## 17. Closed version-1 defaults and implementation gates

The research baseline closes the product semantics now and leaves only workload-specific thresholds to implementation evidence:

1. **Erasable content:** Event envelopes, identifiers, reason codes, digests, and small non-sensitive status facts may be inline. Authored Message bodies, provider payloads, tool output, retrieved excerpts, and other sensitive or independently retained bytes use an erasable `ContentObject` or `Artifact`. The exact inline byte threshold is an implementation gate exercised by storage and deletion fixtures.
2. **Memory scopes:** version 1 supports Session and Workspace scopes. There is no implicit user-global Memory. Only the authenticated user or an explicitly granted `memory.write` effect may publish Workspace Memory.
3. **Explicit commitment:** ordinary conversation never creates durable Memory merely because the model says it will remember. A dedicated user command or UI action commits user-authored Memory; any model-proposed write remains pending review.
4. **Model proposals:** a model may propose a preference, project fact, decision, or reusable procedure with source evidence. Session-local proposals require an explicit configured review policy; Workspace proposals always require user approval in version 1. Generated summaries remain derived data rather than Memory.
5. **Deletion receipt:** deleting a Session removes its content, scoped Memory, Artifacts, and derived indexes. A separately scoped administrative receipt may retain only deletion operation ID, actor/principal ID, deleted scope ID, timestamps, schema version, outcome, and non-reversible aggregate counts—never deleted content, excerpts, embeddings, or content hashes usable to recover it.
6. **Retention profiles:** version 1 names `local-canonical`, `provider-transient`, and `provider-managed` behavior explicitly. The selected Provider capability snapshot must satisfy the active profile before content is sent. Persistent provider conversations, caches, or compaction are disabled when their retention/deletion contract is incompatible; the request fails before transmission if no compatible path exists.
7. **Security state:** open approvals, denials, grants, capabilities, budgets, blockers, and unresolved user decisions remain typed current state and cannot exist only inside a prose summary. A summary may link to them but does not replace them.
8. **Initial extraction formats:** UTF-8 text, Markdown, JSON, JSON Lines, unified diffs/patches, and bounded stdout/stderr receive structured or textual extraction. Other media is stored as an opaque Artifact with trusted metadata until a separately reviewed extractor exists.
9. **Token accounting:** the adapter's conservative local counter governs admission and reservation; provider-reported usage governs final provider billing when supplied. Both values and their provenance are recorded. A material disagreement becomes a compatibility signal, never a silent overwrite.
10. **Quality thresholds:** summary fidelity, generated-Memory precision, retrieval gain, and approximate-index acceptance thresholds are implementation gates set from a held-out project corpus. Until they pass, the corresponding feature stays disabled and the lexical/recent-history baseline remains supported.

These defaults are inputs to Phase 0 fixtures. Measurement selects thresholds and storage budgets without reopening the semantic distinctions above.

## 18. Anti-goals

- No single `memory` table containing transcript, summaries, embeddings, and cache state.
- No ever-growing prompt as canonical Session storage.
- No Provider conversation/compaction object as the only recoverable state.
- No arbitrary retrieved text in system/developer instructions or deterministic policy.
- No global vector search followed only by best-effort application filtering.
- No silent FIFO truncation, summary overwrite, or context mutation inside Provider adapters.
- No automatic cross-session Memory promotion from model output by default.
- No embeddings or vector database before a measured quality gate.
- No shared mutable context among orchestrator and workers.
- No user-facing claim that a summary is verbatim or that a relevance score is confidence.
- No claim of universal deletion when a Provider reports retained or unsupported state.
- No raw sensitive context in routine telemetry, cache keys, manifests, or error messages.
- No crate split solely because the architecture diagram names a module.

## 19. Final recommendation

Build context engineering as a deterministic, observable compilation system around an authoritative local journal—not as prompt concatenation and not as a vector database feature.

The design should make these statements true:

```text
History is replayable.
Working context is bounded and explainable.
Memory is scoped, attributed, correctable, and deletable.
Artifacts are immutable but access and retention are policy-controlled.
Summaries and indexes are disposable derived data.
Provider caches and compaction accelerate one branch but never own truth.
Each agent sees only its authorized assignment context.
Retrieval can improve relevance but cannot grant authority.
```

Rust is a strong implementation fit because the local work is dominated by typed state transitions, bounded buffers, streaming content, SQLite coordination, cancellation, and strict ownership of concurrent agent lineages. The important speed property is not “keep everything in RAM” or “use the largest context window.” It is that context preparation has explicit bounds, predictable latency, high cache locality where safe, and measurable quality—and that every optimization can be removed without losing the user's history or control.

## Primary-source bibliography

### Provider context, caching, compaction, and retention

- OpenAI: [Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching), [Compaction](https://developers.openai.com/api/docs/guides/compaction), [Conversation state](https://developers.openai.com/api/docs/guides/conversation-state), [Data controls](https://developers.openai.com/api/docs/guides/your-data), [Agents multi-agent](https://developers.openai.com/api/docs/guides/agents-api/multi-agent), [Agents observability](https://developers.openai.com/api/docs/guides/agents-api/observability), [Safety in building agents](https://developers.openai.com/api/docs/guides/agent-builder-safety), [Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents).
- Anthropic: [Context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows), [Context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing), [Prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), [Memory tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool), [API and data retention](https://platform.claude.com/docs/en/manage-claude/api-and-data-retention).
- Google: [Gemini context caching](https://ai.google.dev/gemini-api/docs/caching), [explicit context caching](https://ai.google.dev/gemini-api/docs/generate-content/caching), [Gemini API zero data retention](https://ai.google.dev/gemini-api/docs/zdr).

### Storage, retrieval, and artifacts

- SQLite: [FTS5 extension](https://www.sqlite.org/fts5.html), [WAL](https://www.sqlite.org/wal.html), [isolation](https://www.sqlite.org/isolation.html), [online backup API](https://www.sqlite.org/backup.html).
- pgvector project: [pgvector repository and search/filtering documentation](https://github.com/pgvector/pgvector).
- Open Container Initiative: [Content Descriptor specification](https://github.com/opencontainers/image-spec/blob/main/descriptor.md).

### Memory and long-context evaluation

- Wu et al., [LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory](https://proceedings.iclr.cc/paper_files/paper/2025/hash/d813d324dbf0598bbdc9c8e79740ed01-Abstract-Conference.html), ICLR 2025.
- Maharana et al., [Evaluating Very Long-Term Conversational Memory of LLM Agents (LoCoMo)](https://aclanthology.org/2024.acl-long.747/), ACL 2024.
- Hsieh et al., [RULER: What's the Real Context Size of Your Long-Context Language Models?](https://github.com/NVIDIA/RULER), COLM 2024 project and primary implementation.
- Liu et al., [Lost in the Middle: How Language Models Use Long Contexts](https://direct.mit.edu/tacl/article/doi/10.1162/tacl_a_00638/119630/Lost-in-the-Middle-How-Language-Models-Use-Long), TACL 2024.
