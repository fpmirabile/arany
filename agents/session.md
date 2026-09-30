# Sessions and collaboration

Load when changing Session lifecycle, resume/fork/rename, compaction, context reconstruction, collaboration policy, agent topology, queued input, or per-Run budgets.

The evidence and rationale live in `docs/research/beta-sessions-teams-terminal-providers-and-license.md` and `docs/research/context-memory-and-compaction.md`. Keep this file as the working contract.

## Domain boundaries

- A Session is one durable ordered conversation across process exits. A Run is one bounded execution caused by one accepted user Message. An AgentRun is one primary or child execution inside that Run.
- Resume preserves Session identity. Fork creates a new Session from an exact committed Run boundary and records immutable source Session, Run, sequence, and prefix digest.
- A fork's first Event is `SessionForked`; replay resolves at most eight lineage links and 10,000 total Events, verifies each canonical Event-prefix digest, and includes only inherited committed Runs through the recorded boundary. A missing or changed source fails replay before context compilation.
- Historical agents remain inspectable after resume; no in-memory child is fabricated or restarted. Retrying interrupted work creates a new Run ID.
- A pre-`RunStarted` interrupted objective has no pinned Workspace identity. Later completed Runs may be forked or compacted in their admitted Workspace; compaction carries the old objective as unanswered data, while a fork inherits only committed terminal Runs.
- A new Session's versioned start Event pins its admitted Workspace identity even before the first Run. Legacy v1 empty Sessions remain unbound; exact-ID resume may admit them, but latest-Workspace selection must exclude them until a Run pins identity. Every later Run and fork must agree with the pinned identity.
- `--continue` selects the matching Session with the greatest committed Event sequence, then revalidates it under the exact-ID resume path. Discovery reads canonical Events, never directory names or a mutable index; malformed history fails rather than choosing an older Session.
- The Session picker lists only canonical heads bound to the admitted Workspace, newest committed activity first. Its titles and selection are presentation data; choosing a row still runs exact-ID resume and fails without switching if replay or Workspace revalidation fails.
- Provider-hosted conversation IDs, prompt-cache keys, and opaque compaction items are accelerators, never canonical Session history.

## Collaboration policy

- Pin exactly one `single`, `auto(max_active_children = N)`, or `team(max_active_children = N)` policy before each Run. `SessionDefaultChanged` records a complete validated next-Run selection only while idle; every Run still re-admits its selected Provider before Workspace input.
- The primary AgentRun owns the final answer and an ordered `0..N` collection of direct read-only children. Children cannot spawn nested teams in beta.
- `N` is generic but never unbounded. Child admission is the minimum of the Session policy, process safety ceiling, remaining call/token/byte/time budgets, and Provider concurrency limit.
- Default `auto` to at most three active children. `single` admits none. `team` requests team-oriented decomposition but still cannot exceed effective capacity.
- Every child has a causal assignment, stable identity, isolated context, and persisted terminal disposition. Cancellation reaches every admitted child.

## Session commands

- Bare `arany` creates a new Session. `--continue` resumes the latest admitted-Workspace Session; `--resume` resumes an exact Session or opens the picker; `--fork` creates a new Session at a committed boundary.
- Interactive Session commands are `/sessions`, `/new` with `/clear` as its familiar alias, `/resume`, `/fork`, `/rename`, and `/compact`.
- Do not implicitly resume based only on the current directory. A draft or active Run must be handled explicitly before switching Sessions.
- Provider, model, effort, profile, permission, and collaboration defaults may change between Runs, never during one. `RunStarted` persists the resolved immutable values; legacy records without effort remain readable.
- A saved native API account is selected by a UUID in Session defaults, never by replayed credential bytes. The CLI resolves that UUID and exact Provider against the current protected account before Run or catalog access; replacing the account makes an older Session's saved-key selection unavailable rather than silently switching billing identity. `RunStarted` pins the selected saved-account UUID as optional provenance; replay requires a native Provider and UUIDv7 when it is present. Legacy Session and Run records without it remain readable and never authorize a credential lookup.
- `exec` creates one Run and exits by default. Appending to an existing Session requires an explicit Session ID.

## Context and compaction

- Context compilation is deterministic over committed local Messages, Events, instruction snapshots, exact budgets, and the selected Provider capability snapshot.
- Compaction never deletes or replaces canonical history. It creates a digest-bound derived snapshot covering an exact Session prefix with author/provider/model/schema provenance.
- Manual Engine compaction summarizes accepted Run outcomes after the latest valid summary, including unanswered failed/cancelled/interrupted objectives as data. Its `ContextCompacted` Event records provider-neutral model-authored text or a typed failure; replay checks source/content digests and version before any later Run can select it. A failed attempt preserves the last good summary.
- `/compact` runs only while idle and exposes its Provider usage. New `RunStarted` facts record selected context-content bytes, compactable Session-history bytes, and the policy-adjusted budget; legacy records without that footprint remain readable. After a successful attached Run reaches 80% of that budget with at least 10% compactable history, or 24 of 32 selectable history Runs, a visible warning precedes one automatic compaction attempt for that exact committed boundary. Immutable Workspace input alone never triggers a useless compaction call. Failure is recorded but not automatically retried at the same boundary; manual `/compact` remains available. `exec` makes no hidden post-Run Provider call.
- A snapshot mismatch, fork outside its covered prefix, incompatible Provider item, deleted source, or unknown compiler version invalidates the snapshot and fails closed.
- Cross-Session Memory remains a different product. Session history does not silently become durable Memory.

## Persistence and proof

- Every canonical Event carries `session_id`; Run and AgentRun scopes are optional where the Event is Session-only. Session lists may use rebuildable indexes, never a second source of truth.
- The reducer's incremental committed-Event transition is shared with live Run observation. Closed-history replay alone labels an unfinished final Run `Interrupted`; an in-process active projection must not apply that end-of-history rule until the Run actually ends or the process exits.
- Persist Session creation/default/rename/fork/compaction facts plus existing Run and AgentRun lifecycle facts. Do not persist terminal focus, panel state, hover, picker selection, or native scroll position.
- One evidence-dense scenario must prove multiple Runs, process exit, resume, provider change between Runs, bounded `N`, fork prefix identity, compaction failure safety, and deterministic replay. Boundary tables own `single`, `auto`, capacity, cancellation, and invalid snapshot cases.
