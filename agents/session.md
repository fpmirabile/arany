# Sessions and collaboration

Load when changing Session lifecycle, resume/fork/rename, compaction, context reconstruction, collaboration policy, agent topology, queued input, or per-Run budgets.

The evidence and rationale live in `docs/research/beta-sessions-teams-terminal-providers-and-license.md` and `docs/research/context-memory-and-compaction.md`. Keep this file as the working contract.

## Domain boundaries

- A Session is one durable ordered conversation across process exits. A Run is one bounded execution caused by one accepted user Message. An AgentRun is one primary or child execution inside that Run.
- Resume preserves Session identity. Fork creates a new Session from an exact committed Run boundary and records immutable source Session, Run, sequence, and prefix digest.
- Historical agents remain inspectable after resume; no in-memory child is fabricated or restarted. Retrying interrupted work creates a new Run ID.
- Provider-hosted conversation IDs, prompt-cache keys, and opaque compaction items are accelerators, never canonical Session history.

## Collaboration policy

- Pin exactly one `single`, `auto(max_active_children = N)`, or `team(max_active_children = N)` policy before each Run. Session defaults may change only for the next Run.
- The primary AgentRun owns the final answer and an ordered `0..N` collection of direct read-only children. Children cannot spawn nested teams in beta.
- `N` is generic but never unbounded. Child admission is the minimum of the Session policy, process safety ceiling, remaining call/token/byte/time budgets, and Provider concurrency limit.
- Default `auto` to at most three active children. `single` admits none. `team` requests team-oriented decomposition but still cannot exceed effective capacity.
- Every child has a causal assignment, stable identity, isolated context, and persisted terminal disposition. Cancellation reaches every admitted child.

## Session commands

- Bare `arany` creates a new Session. `--continue` resumes the latest admitted-Workspace Session; `--resume` resumes an exact Session or opens the picker; `--fork` creates a new Session at a committed boundary.
- Interactive Session commands are `/sessions`, `/new` with `/clear` as its familiar alias, `/resume`, `/fork`, `/rename`, and `/compact`.
- Do not implicitly resume based only on the current directory. A draft or active Run must be handled explicitly before switching Sessions.
- Provider, model, profile, permission, and collaboration defaults may change between Runs, never during one. `RunStarted` persists the resolved immutable values.
- `exec` creates one Run and exits by default. Appending to an existing Session requires an explicit Session ID.

## Context and compaction

- Context compilation is deterministic over committed local Messages, Events, instruction snapshots, exact budgets, and the selected Provider capability snapshot.
- Compaction never deletes or replaces canonical history. It creates a digest-bound derived snapshot covering an exact Session prefix with author/provider/model/schema provenance.
- `/compact` runs only while idle and exposes its Provider usage. Automatic compaction may run only at a committed Run boundary after a visible threshold warning.
- A snapshot mismatch, fork outside its covered prefix, incompatible Provider item, deleted source, or unknown compiler version invalidates the snapshot and fails closed.
- Cross-Session Memory remains a different product. Session history does not silently become durable Memory.

## Persistence and proof

- Every canonical Event carries `session_id`; Run and AgentRun scopes are optional where the Event is Session-only. Session lists may use rebuildable indexes, never a second source of truth.
- Persist Session creation/default/rename/fork/compaction facts plus existing Run and AgentRun lifecycle facts. Do not persist terminal focus, panel state, hover, picker selection, or native scroll position.
- One evidence-dense scenario must prove multiple Runs, process exit, resume, provider change between Runs, bounded `N`, fork prefix identity, compaction failure safety, and deterministic replay. Boundary tables own `single`, `auto`, capacity, cancellation, and invalid snapshot cases.
