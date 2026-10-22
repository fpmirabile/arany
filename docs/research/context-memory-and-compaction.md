# Context, Memory and compaction: distinctions and future gates

**Research baseline:** 2026-09-28.

[Session spec](../specs/session.md) owns current context compilation,
compaction and lineage; [architecture](../architecture/system-overview.md) owns
placement. Cross-Session Memory, general Artifact storage, autonomous retrieval
and opaque Provider continuation are expansion options, not an implementation
checklist or authority granted by this report.

## Keep four kinds of state separate

| Kind | Purpose | Authority and lifetime |
|---|---|---|
| Canonical history | Explain accepted work and recover Sessions | Durable Events; Provider state cannot replace it |
| Working state | Current Run, budgets and unresolved operations | Reduction of canonical facts, with explicitly owned live operations |
| Working context | Material selected for one inference | Bounded derived input; inclusion does not grant authority |
| Durable Memory | Knowledge deliberately reused across work | Would require its own scope, provenance, review and retention lifecycle |

A summary is a representation, not a Memory commitment. A model saying it will
remember something does not create a durable authorized record. Likewise,
content-addressed storage detects identity or corruption; its digest does not
grant permission to retrieve the content.

## Why compile context locally

A deterministic compiler can explain what history was selected, what was omitted
and how the request fits the admitted budget. The selected Provider receives
Arany's semantic request rather than becoming the sole owner of resumable state.
Branch context and child assignments remain scoped instead of merging every
agent's private scratch data into a shared transcript.

Context priority is not authority priority. A recent Tool result, retrieved
excerpt or child summary stays untrusted data even if it is highly relevant.
Approvals, capabilities, budgets and unresolved decisions need typed ownership;
a prose summary cannot be their only surviving representation.

## Different ways to reduce input

| Operation | Meaning | Consequence |
|---|---|---|
| Omission | Do not select an item for this call | Original remains available |
| Truncation | Select a bounded excerpt | Must expose that it is incomplete |
| Summarization | Create a derived account of sources | Needs coverage and invalidation rules |
| Provider compaction | Carry vendor-specific condensed or opaque state | Depends on that adapter's compatibility and retention guarantees |

These operations have different loss and provenance behavior. A summary should
retain source coverage and unresolved constraints; exact identifiers and security
decisions should remain recoverable from originals. Cancellation or a stale
source snapshot must not publish a half-valid context revision. The current
contract and acceptance owners are in [Session spec](../specs/session.md).

Hierarchical summaries add error propagation and invalidation work. Introduce
levels only if a held-out task corpus shows they improve useful context under
the actual budget. A larger context window or a cache hit is not itself evidence
of better answers.

## Caching remains a Provider optimization

The original investigation compared prefix caching and vendor-native compaction.
Cache keys, minimums, TTLs, billing and opaque state differ by Provider. Those
wire details belong in adapters, not a universal Memory abstraction.

Stable prefixes can reduce repeated work, but correctness must survive a cache
miss. Changed instructions, revoked capabilities or corrected/deleted content
cannot remain eligible merely to preserve reuse. Compare total cost and latency
with a smaller uncached request, not only the hit ratio. Provider-managed state
also needs an explicit retention/deletion assessment before it is admitted.

## Before adding durable Memory

A future Memory feature needs an accepted contract for explicit commitment,
visibility, correction, expiry and deletion. Model proposals should remain data
pending the authorized publication decision. Scope cannot expand because a
classifier reports confidence or two Workspaces have similar embeddings.

Provenance must distinguish user-authored, model-proposed, imported and derived
content. Corrections supersede eligible versions rather than silently averaging
conflicting claims. Expiry stops use; it is not proof of physical erasure. Users
need to inspect what can influence later work independently of an activity log.
Persistent prompt injection, hidden personalization and cross-scope disclosure
are meaningful failure paths for that proposal.

## Retrieval and deletion must agree

Start with a demonstrated retrieval gap and a lexical baseline. Dense retrieval
is justified only by held-out end-to-end gains within latency, storage, cost and
privacy budgets. Similarity scores measure relevance, not truth or authorization.

Authorization must constrain the candidate corpus before ranking; returned hits
still need current validity checks. Post-filtering a global search can disclose
existence through counts, scores or timing. Lagging indexes cannot return stale
corrections or resurrect tombstoned content.

A future deletion contract must cover source content, summaries, indexes, caches,
queued extraction/indexing and restored backups. Immediate query denial and later
physical purge are different states. Provider cleanup may be unavailable or
partial; a receipt cannot promise deletion everywhere. Any retained administrative
receipt must exclude the deleted content and recoverable derivatives.

General Artifact storage would also need bounded extraction, media handling and
retention. Adding content objects alone does not authorize parsers or replace
the [current image contract](../specs/terminal.md). Proposed implementations
belong in [planning](../../planning/README.md), not Rust type sketches here.

## Evidence before expansion

Measure summary fidelity, source coverage, contradiction handling, cross-scope
leakage, stale-result rejection and deletion propagation before admitting a new
Memory/retrieval path. Long-context and Memory benchmarks can supply cases but
cannot replace project-specific adversarial and task evaluations.
[Testing rules](../../agents/testing.md) own deterministic evidence;
[evaluation research](./testing-strategy-for-rust-cli-harness.md#behavioral-evaluation) covers model quality;
[next steps](../../NEXT_STEPS.md) owns current gaps.

## Long-lived canonical history

**Question:** Does Arany need persisted replay snapshots, another history index,
or an archive/new-Session policy before reaching its current admission bounds?
**Source checked:** 2026-10-14. This is an evaluation; it authorizes no runtime
change.

**Conclusion:** measure append, cold replay and terminal history separately
before adding another index or persisted snapshot. [Execution](../specs/execution.md#admission-persistence-and-recovery)
owns Store limits and hot-cache behavior; [Session](../specs/session.md) owns
lineage and [Terminal](../specs/terminal.md) owns history projection. A per-Session
lookup must preserve global journal validation. SQLite's connection-local
[`data_version`](https://www.sqlite.org/pragma.html#pragma_data_version) is an
invalidation hint, not a durable revision. [Next steps](../../NEXT_STEPS.md#beta-2-and-broader-release)
owns the remaining quota-scale measurements; existing smaller workloads do not
establish performance at every admission boundary.

### Proposed policy, not accepted behavior

Keep the current fail-closed limits. Do not automatically prune Events or
archive individual Sessions: a fork's source Events and prefix digest remain
necessary to replay it, while per-Session deletion cannot reclaim the one
shared database safely. The `--state-dir` option
([setup guide](../setup.md#storage-and-recovery)) can select an independent
StateRoot for future work, but that only starts a separate journal. The old
root must be retained to inspect or resume its Sessions; a Session in the new
root cannot fork a source stored only in the old root. Moving the complete
closed StateRoot to user-managed archive storage and selecting a new root would
reduce active root growth only after the user deliberately makes that change.
It does not erase the archived bytes. Each root has separate `tools.json`
configuration; an explicitly configured grant stays with its root and is not
copied to a new root ([Tools contract](../specs/tools.md#private-grants)).

The proposed pre-quota choice is therefore split: a fresh ordinary Session
avoids inheriting a source lineage when Event-count pressure is high; shared
byte pressure requires a separate StateRoot, since starting a Session inside
the full root cannot make `StorageFull` recoverable. Selecting another
`--state-dir` changes Session-root configuration, including its `tools.json`,
but it does not change the fixed OS-user account root. Remembered trust for the
same folder and its generated default Tool grant, saved accounts and saved
account/model preferences remain available across StateRoots; changing roots is
not an implicit trust revocation or account-isolation boundary. The user can
revisit folder trust through `/permissions`. The [Workspace trust loader](../../crates/arany/src/tools/workspace.rs)
and [Tools contract](../specs/tools.md#folder-trust-and-approvals) own folder
trust; the [account scope contract](../specs/credentials.md#account-scope-and-identity)
owns saved account and preference scope. The UI thresholds and recovery wording
remain unaccepted until measured and specified.

If an archive/switch workflow is later accepted, archive the complete
StateRoot and keep it independently readable; do not delete canonical Events
or promise cross-root lineage. Prefer quiescing all Store workers and writers,
waiting for in-flight appends, releasing active Session and Workspace claims,
and closing all Store handles before making a checked, user-managed whole-root
copy. Any switch requires that quiesced state and a checked acknowledged-commit
cutoff: the archive must include every Event acknowledged by the old Store
through that cutoff and preserve all fork references. If copying while the
source remains writable, reconcile the final incremental prefix and global
sequence through the cutoff after quiescence, then reopen and strictly validate
the archived journal and fork lineages before switching. SQLite's Online
Backup API can make a consistent snapshot, but snapshot validity alone does not
prove it contains every commit acknowledged by the old root through cutover;
validating a snapshot cannot recover a later acknowledged Event omitted from it.
[SQLite Online Backup API](https://www.sqlite.org/backup.html) Any early warning threshold,
archive lifecycle or user-facing recovery choice needs a separate accepted
contract and measured trigger. No new threshold, snapshot schema, migration,
automatic archive, or cutover behavior is proposed as current behavior.

## Primary sources

- OpenAI: [prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching), [compaction](https://developers.openai.com/api/docs/guides/compaction), [conversation state](https://developers.openai.com/api/docs/guides/conversation-state), [data controls](https://developers.openai.com/api/docs/guides/your-data)
- Anthropic: [context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows), [prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), [Memory tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool), [data retention](https://platform.claude.com/docs/en/manage-claude/api-and-data-retention)
- SQLite: [`data_version`](https://www.sqlite.org/pragma.html#pragma_data_version), [transactions](https://www.sqlite.org/lang_transaction.html), [Online Backup API](https://www.sqlite.org/backup.html), [FTS5](https://www.sqlite.org/fts5.html)
- [pgvector filtering and search](https://github.com/pgvector/pgvector), [OCI content descriptors](https://github.com/opencontainers/image-spec/blob/main/descriptor.md)
- [LongMemEval](https://proceedings.iclr.cc/paper_files/paper/2025/hash/d813d324dbf0598bbdc9c8e79740ed01-Abstract-Conference.html), [LoCoMo](https://aclanthology.org/2024.acl-long.747/), [RULER](https://github.com/NVIDIA/RULER), [Lost in the Middle](https://direct.mit.edu/tacl/article/doi/10.1162/tacl_a_00638/119630/Lost-in-the-Middle-How-Language-Models-Use-Long)
