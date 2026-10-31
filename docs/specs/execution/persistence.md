# Execution: incompatible state and persistence bounds

### Incompatible history

Pre-beta, stored formats are never migrated: only the current journal schema runs, and nothing rewrites stored history. History is archived when its schema differs (older or newer), it holds an unknown or removed Event kind or variant, or any row is replay-invalid. Then `events.sqlite3` and `checkpoints.sqlite` with their `-journal`, `-wal` and `-shm` sidecars move intact, bytes untouched, into a private `archive/history-<UTC timestamp>/` directory and a fresh journal opens. The move is crash-safe: an interrupted archive resumes, never moves a newer file, and on any mismatch leaves everything in place with a typed archive conflict; a completed archive holds its `manifest.json`. Accounts, settings, folder trust and other StateRoot records stay; nothing is deleted. Attached chat names the archive in its [startup notice](../terminal/setup-and-accounts.md#setup-and-account-choices); headless commands print one `warning: …; started a new history` line on stderr. Damaged SQLite, a busy store, full storage and unsafe or inaccessible files are never archived: attached chat offers its typed recovery choice, with an explicit reset for damaged history, and headless commands exit 1 with a typed `error: …` that, for damaged history, says to run `arany` to reset it.

The same gate covers `checkpoints.sqlite`: a readable store at another schema version is archived with the history; a damaged, busy or extra-object store never is ([checkpoint store contract](../tools/changesets-and-checkpoints.md#checkpoints-and-rewind)).

### Incompatible records

Private StateRoot JSON records carry no version; SQLite stores keep their `user_version`. A record is one of two classes:

- **Tolerant** preference and cache records, `ui-preferences.json` and `model-preferences.json`: unknown fields are ignored, missing ones defaulted and invalid entries or values dropped; an unreadable record reads as defaults. They are never archived, never refuse startup or a Run, and a rewrite may drop fields this binary does not know.
- **Strict** authority records, everything else (`tools.json`, `permissions.json`, `workspace-permissions.json`, `unguarded-commands.json`, `command-environment.json`, `command-tools.json`, `mcp-oauth.json`, `provider-profiles.json`, `provider-roster.json`, `provider-routing.json`, `account-credentials.json`, `chatgpt-accounts.json`, `chatgpt-registration.json`): unknown or missing required fields, wrong types and failed validation refuse the record.

Before any record is used, attached chat, `exec`, `serve` and `acp` check every strict record in the selected State directory and the account root. A refused record, from another binary or damaged, moves intact into one private `archive/records-<UTC timestamp>/` directory (with a `-N` suffix if an interrupted attempt left that name), and its owner fails closed as if absent: the folder asks for trust again, accounts sign in again, rules and routes are re-created. Attached chat adds the archived names and folder to its startup notice; headless commands print one `warning: …` line on stderr. Unsafe, oversized or unreadable files are never archived or altered; their owner reports them. OS keyring entries are never moved: their owner refuses them, and the ChatGPT and native-account setup resets still cover keyring entries and records that disagree. A crash leaves each record in place or archived, never both.

A change of meaning renames the field. Tolerant records only add optional fields with defaults; any strict shape change makes another binary archive the record. Guard [capacity](../tools/native-guard.md#shared-native-capacity-and-settings) and unit-recovery records are strict but shared live between processes, so they are never archived: a refused shape fails closed as protection unavailable.

### Workspace rewind Events

`WorkspaceRewindStarted` and `WorkspaceRewindFinished` are Session-scoped facts, each within the [generated Event payload limit](../limits.md#max_event_bytes) including JSON escaping. Started records a UUIDv7 rewind ID, a Run or rewind checkpoint with its sequence, the head sequence and canonical prefix digest, the effective-plan SHA-256, file count and restored bytes, sorted disjoint kept and overwrite selections, Workspace identity and Tool policy and enforcement digests. Overwrite selections carry the observed digest, byte length and executable bit, or explicit null for absence. Kept plus overwritten paths are capped at 64, each at most 512 bytes<!-- unrelated-bound: rewind selection path bytes -->. Neither selections nor stored bytes grant permission.

Finished immediately follows its own Started with the rewind ID, a closed `restored`, `conflict`, `denied`, `failed`, `cancelled` or `uncertain` outcome, required nullable attestation and checkpoint, and sorted conflict paths (the first 32 plus the exact total, at most 2,048). Restored, and an attested Uncertain outcome, require an attestation with plan, Workspace, enforcement and execution receipt digests, and carry checkpoint capture status for overwritten content: captured or skipped when any before state exists, otherwise not_needed. Known refusal or cancellation before publication carries neither capture nor receipt. An unconfirmed outcome without a receipt stays [Uncertain](./events-and-outcomes.md#effect-outcomes) and does not assert whether publication began. The [Session contract](../session.md#rewind-planning-and-replay) owns planning, sealing and history projection; [Goals](../goals.md#rewind-and-goal-recovery) owns recovery and evidence invalidation.

### Persistence bounds

| Bound | Limit | Overflow |
|---|---|---|
| Session lock files | [generated lock-file quota](../limits.md#max_session_lock_files), including files stranded by process death; scans stop after it plus [generated State-file overhead](../limits.md#max_state_overhead_files) | quota failure before Workspace input or Provider disclosure |
| Workspace writer lock files | [generated writer-lock quota](../limits.md#max_workspace_lock_files), shared across StateRoots, owned by [Goals](../goals.md#lifecycle-budgets-and-recovery) | new writers refused before disclosure or effects |
| Direct-Session projection cache | [generated hot Session cache bound](../limits.md#max_hot_session_payload_bytes) of serialized Event payload | not kept; the next append replays strictly |
| Supervisor verdicts | [generated verdict size](../limits.md#max_verdict_bytes); at most [the generated per-Run verdict count](../limits.md#max_verdicts), the last marked `capped: true` | oversized payload rejected; evaluation continues without further verdicts |
| Event payload, default | [generated Event payload bound](../limits.md#max_event_bytes) | append refused |
| `ContextCompacted` and `GoalContextCompacted` | [generated compaction Event bound](../limits.md#max_compaction_event_bytes), which fits a worst-case escaped summary at the [generated summary bound](../limits.md#max_compaction_summary_bytes) | append refused |
| `MessageAccepted` | [generated message payload bound](../limits.md#max_message_accepted_bytes), with inline PNG bytes (an empty list for text) | append refused |
| `GoalStarted` | [generated Goal start bound](../limits.md#max_goal_started_bytes) | pre-admission `GoalInputTooLarge` ("objective plus includes too large"); no Event or Provider call |
| `GoalFinished` | [generated Goal finish bound](../limits.md#max_goal_finished_bytes) | append refused |
| `ToolProposed`, `ToolStarted` | [generated Tool start bound](../limits.md#max_tool_started_bytes): the complete intent, including Write, Edit or MultiEdit arguments at the [Tool argument ceiling](../tools/limits-and-failures.md#limits-and-failure-behavior) | append refused |
| `HookFinished` | [generated Tool observation bound](../limits.md#max_tool_observation_bytes) | append refused |
| `ToolFinished` | [generated Tool finish bound](../limits.md#max_tool_finished_bytes), including checkpoint metadata. Optional `spill: {ref, bytes, truncated}` binds the intent digest and [retained-output bounds](../tools/limits-and-failures.md#limits-and-failure-behavior); only metadata is journaled and replay never opens a spill | append refused |
| Steering note per Run | [generated steering note bound](../limits.md#max_steering_note_bytes), including history labels | offer refused; the caller keeps its text |
| Requested Tools per model step | 0, or 1 to the [generated per-step Tool-call limit](../limits.md#max_step_tool_calls) for a `ToolRequested` disposition | replay refused |
| Session discovery | At most [the generated Session count](../limits.md#max_discovered_sessions) heads; a listing page scans at most [the generated Event count](../limits.md#max_list_scan_events) | partial page with a continuation cursor; exact-ID resume fallback |
| Events per Session lineage | [generated Session Event limit](../limits.md#max_session_events), including inherited history | append refused; replay fails closed with a Store error |
| `provider-profiles.json` and `tools.json` | 64 KiB<!-- unrelated-bound: private Provider and Tool records --> each | rejected; never parsed as trusted configuration |

Every action reserves Event capacity for its records and observations before it starts ([admission](./admission-and-recovery.md#admission-persistence-and-recovery)).

Canonical state lives in a private directory outside the Workspace, resolved before repository input, with verified owner and permissions or ACL. SQLite integrity is not tamper evidence, and the database is not encrypted at rest. Persisted objectives, summaries, results and errors are untrusted typed data.

Goal admission checks the serialized objective plus admitted includes against the `GoalStarted` ceiling, so heavy JSON escaping can refuse individually bounded input before Goal acceptance, disclosure, usage or a partial fork target; earlier history and summary stay readable. Byte preflight is conservative, not a lease against independent writers or disk faults.
