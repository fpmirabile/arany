# Agent definition loader security review

Scope: the [definition contract](../specs/execution.md#agent-definitions) and
[parser admission](../specs/tools.md#agent-definition-parser-admission), with
editor execution unavailable. Release blockers are any authority expansion,
pre-journal parsing or Provider disclosure, host YAML execution, unbounded input,
replay execution, or uncertain outcome treated as success.

| Boundary | Authority and attack prevented |
|---|---|
| Project/private files → immutable snapshots | The admitted Workspace read roots and private StateRoot fix the only sources. Directory capabilities, ownership, no-follow, same-device, single-link and replacement checks prevent path/target substitution and protected-file disclosure. No model path or Git configuration participates. |
| Snapshot → native parser | A committed internal intent binds the index/raw digest. Canonical bounded bytes cross the private task channel; Ready validates those bytes before one-use GO. YAML runs in the native Guard. Linux copies no selected command/Skill resources for parsing; macOS grants no Workspace subtree to the parser. The definition supplies no credentials, network grant or executable configuration. |
| Normalization → Provider | The whole bounded registry validates before the first call. Private precedence cannot hide malformed documents. Exact child-model checks prevent route or credential selection. Only selected bodies enter labelled untrusted user data. Catalog generation and deterministic Engine/reducer checks independently narrow read-only child authority and inference turns. |
| Definition mode → approval | Deny/Ask precedence retains parent and terminal restrictions; a static definition cannot approve itself. Replay requires the corresponding verdict and a human/review source for an approved definition Ask. |
| Journal → recovery/context | Ordered parser receipts precede Provider reservations. Missing, failed, duplicate, stale or malformed preparation authorizes nothing. Replay executes no parser; historical preparation bodies stay out of model history and compaction. A new Run re-admits inputs and authority. |

The Engine alone owns scheduling, journal writes, Goal accounting and the
normalized registry. Native adapters transport immutable task data and enforce
resource/process boundaries; they select no product policy. No crate, daemon,
plugin seam, interpreter or worker model process is introduced.

Definitions and their bodies are sensitive, untrusted journal data, with the
existing unencrypted retention policy. Existing input/output, catalog, Event,
Store, task, concurrency and action-time bounds apply before their respective
allocations or dispatch; overflow refuses the whole action/registry. There is no
same-effect retry or fallback to host parsing. Cancellation and unknown outcomes
retain the existing Tool settlement and cleanup rules.

Evidence owners are the parser/snapshot corpus, strict reducer initialization
corpus, Engine unsafe-source corpus, and Guard definition journeys linked by the
Tool contract. Native Linux/macOS execution, cancellation during parsing, live
model selection and full channel/telemetry canary evidence remain named in
[NEXT_STEPS](../../NEXT_STEPS.md); passing pure tests or compilation does not
establish those claims.
