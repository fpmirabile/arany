# Visible reliability

## Metadata

- Status: `Draft` — Research and Plan gates remain open as listed below; implementation has not started.
- Owner: Arany owner; the team lead owns CI evidence collection and slice delivery.
- Last updated: `2026-10-27`
- Research baseline: `20b5dd02580959e89caeed02225d820db2a9c51f`.

## Outcome and scope

Make daily Tool completion trustworthy and its local overhead inspectable: extend
the existing offline Linux latency evidence and close the three macOS reliability
gaps named in [Daily use first, item 6](../../NEXT_STEPS.md#daily-use-first).
The observable result is a valid per-operation report plus stable native journeys
that distinguish refusal, confirmed completion and unknown effects correctly.
The Engine still owns continuation and committed outcomes; native adapters own
process/storage teardown; accounts own selected-item access; telemetry remains a
lossy observer.

This is reliability work on existing paths, not a new Guard, reporting command,
credential backend or macOS feature. The related Linux overlay implementation
remains with its [own plan](../command-workspace-overlay/README.md). Measure the
implementation actually present at the selected revision, including its overlay
and cache profile; do not restore a retired copy path for comparison.

## Research

### Current contracts, code and evidence owners

Local citations below name the baseline file and starting line. The editing rules
are [planning](../PLANNING.md), [security](../../agents/security.md),
[testing](../../agents/testing.md), [native fixtures](../../agents/references/testing-details.md)
and the [spec workflow](../../docs/specs/README.md). Apply Ponytail,
[Rust Clean Code](../../agents/skills/rust-clean-code/SKILL.md) and
[Clean Architecture Review](../../agents/skills/clean-architecture-review/SKILL.md)
to each implementation and review.

| Finding | Source and implication |
| --- | --- |
| The brief's warm Read-only description is stale: the corpus already has single Read, four concurrent Reads and single Edit. | [`latency.rs:9`](../../crates/arany-cli/tests/guard_runs/latency.rs#L9), [`latency.md:3`](../../crates/arany-cli/tests/guard_runs/latency.md#L3). Preserve all three and add only the missing Command row. |
| Edit is an admitted in-process publication on both OSes, with a digest precondition, FileReceipt and checkpoint. It is not a Guard process. | [`tools.md:944`](../../docs/specs/tools.md#in-process-file-operations), [`latency.rs:195`](../../crates/arany-cli/tests/guard_runs/latency.rs#L195). Calling it guarded must not introduce another execution site or fake Guard attestation; O1 needs owner disposition. |
| Command is already a recognized recorder operation, but the fixture and helper reject it. | [`latency.rs:74`](../../crates/arany-cli/tests/guard_runs/latency.rs#L74), [`latency.rs:390`](../../crates/arany-cli/tests/guard_runs/latency.rs#L390), [`test_support/latency.rs:320`](../../crates/arany/src/test_support/latency.rs#L320). Extend the existing fixture and audits, without another timing framework. |
| Timing starts at complete typed Provider receipt and ends after matching ToolFinished append acknowledgement. Journal wall time is a different measurement. | [`execution.md:17`](../../docs/specs/execution.md#L17), [`latency.md:10`](../../crates/arany-cli/tests/guard_runs/latency.md#L10), [`latency.rs:597`](../../crates/arany-cli/tests/guard_runs/latency.rs#L597). Keep the held/lost acknowledgement owner and scoped replay joins. |
| Row count affects evidence allocation; single-call objective selection currently treats every operation alike. | [`latency.rs:475`](../../crates/arany-cli/tests/guard_runs/latency.rs#L475), [`latency.rs:572`](../../crates/arany-cli/tests/guard_runs/latency.rs#L572), [`test_support/latency.rs:10`](../../crates/arany/src/test_support/latency.rs#L10). Derive the four-row allocation and retained-attempt ceiling; select objectives by operation, not merely call count. |
| Runtime topology refusal and fixture-image detachment are separate observations. | [`tools_macos.rs:1488`](../../crates/arany-cli/tests/session_run/tools_macos.rs#L1488) attaches a userspace-backed sparse image; [`tools_macos.rs:1661`](../../crates/arany-cli/tests/session_run/tools_macos.rs#L1661) proves pre-GO refusal/replay before cleanup at line 1683. A fixture detach failure cannot be repaired by accepting the mounted runtime. |
| Shared fixture detach already retries within a bound, re-resolves the owned device and confirms absence after its final forced detach. Production images attach kernel-only. | [`test_support/process.rs:200`](../../crates/arany/src/test_support/process.rs#L200), [`macos/storage.rs:203`](../../crates/arany/src/tools/guard/macos/storage.rs#L203). Do not add a second retry loop. Align fixture attachment with the existing production mechanism, then verify the actual failure. |
| Native cleanup already distinguishes storage checks and has one shared teardown deadline; a native disk command may outlast the remaining shared deadline. | [`tools.md:1115`](../../docs/specs/tools.md#L1115), [`macos/storage.rs:447`](../../crates/arany/src/tools/guard/macos/storage.rs#L447), [`macos/unit.rs:1690`](../../crates/arany/src/tools/guard/macos/unit.rs#L1690). An overrun is specified to remain Uncertain; increasing deadlines or accepting incomplete cleanup is not a fix. |
| Current failure capture can identify the failing Tool in optimized test builds. Debug stage files are snapshots, not a complete timeline. | [`test_support.rs:166`](../../crates/arany/src/test_support.rs#L166), [`guard_runs.rs:49`](../../crates/arany-cli/tests/guard_runs.rs#L49), [`permissions.rs:226`](../../crates/arany-cli/tests/guard_runs/permissions.rs#L226), [`macos/fixture.rs:1`](../../crates/arany/src/tools/guard/macos/fixture.rs#L1). Reuse intent-correlated stage/check evidence rather than infer causality from a final generic message. |
| Keychain denial is tested immediately before a helper-mediated authorized read of the same item. Several distinct native access errors map to Locked. | [`credentials/macos_tests.rs:185`](../../crates/arany-cli/src/accounts/credentials/macos_tests.rs#L185), [`keyring_helper/macos.rs:15`](../../crates/arany-cli/src/accounts/credentials/keyring_helper/macos.rs#L15), [`keyring_helper.rs:278`](../../crates/arany-cli/src/accounts/credentials/keyring_helper.rs#L278). Locked alone cannot identify actual keychain lock, item ACL refusal or store unavailability. |
| The native Keychain test pins a copied helper, disables user interaction, uses a private synthetic Keychain, and holds NATIVE_GATE. | [`credentials/macos_tests.rs:208`](../../crates/arany-cli/src/accounts/credentials/macos_tests.rs#L208), [`credentials.rs:1115`](../../crates/arany-cli/src/accounts/credentials.rs#L1115). Preserve these controls; no login Keychain, search-list edits or automatic unlock of user storage. |
| Clipboard has an independent process-group/cleanup owner. Its inclusion in the pending row is not evidence of a clipboard root cause. | [`terminal.md:347`](../../docs/specs/terminal.md#L347), [`terminal/clipboard.rs`](../../crates/arany-cli/src/terminal/clipboard.rs). Preserve late-result rejection and the restart-required latch after unknown cleanup. |
| Whole-response export timeout already wraps the Reqwest OTLP adapter; the existing ignored trickle gate tests progress, not streaming-size refusal or dropped-future cleanup separately. | [`telemetry/http.rs:44`](../../crates/arany/src/telemetry/http.rs#L44), [`telemetry/tests.rs:2058`](../../crates/arany/src/telemetry/tests.rs#L2058), [export bounds](../../docs/specs/telemetry.md#export-bounds). Add those two missing observations at this owner. |
| CI profile selection changed since the early debug failure: guard_runs now runs in the full release suite; debug selects unit/binary/session_run targets. | [`.github/actions/ci-tests/action.yml:59`](../../.github/actions/ci-tests/action.yml#L59). Historical debug logs must be read against their own revision; current release evidence must exercise the moved permissions owner. |

### CI history inspected and remaining diagnosis

This planning task ran no `gh` command. It inspected previously downloaded
`gh run` ZIP logs in the read-only CLI cache and saved CI logs in the team log
directory. The run links below are portable citations; aggregate log entry names
and line numbers identify the inspected evidence. They are historical failures,
not verification of the research baseline. No raw native output, user identities,
temporary paths or secret material need copying into the repository.

| Failure family | Verified historical observation | Diagnosis and disposition |
| --- | --- | --- |
| Runtime mount busy-detach | [Run 39052525247](https://github.com/fpmirabile/arany/actions/runs/39052525247), checkout `1cced52beb7743cdee9716c994963aeeacf298c2`, `0_macOS Tests (debug).txt:1081`; [run 39069818405](https://github.com/fpmirabile/arany/actions/runs/39069818405), checkout `0c32ee4a19f49b15150d0f210b234cf3879796c8`, same entry at line 1098. Both fail at fixture `hdiutil detach`, exit 16, with `lsof` identifying `diskimage` holding the synthetic backing image. Product trace has no worker GO, and supervisor detach/controller storage completed. | Confirmed fixture cleanup failure after the topology rejection; the logs do not establish a product mount-check failure. The current shared detach helper already adds bounded recovery. VR2 aligns the remaining userspace attachment with kernel-only production attachment and requires a held-image/positive-absence regression plus native stability evidence. Do not claim that source alignment alone closes the historical flake. |
| Native stores and clipboard | The exact authorized-read-after-denial Locked failure was not present in the inspected cache or saved CI logs. The pending item is the only report available here; current code identifies its boundary but not its cause. | **Research gate open, R1.** Lead supplies the failing run/job/attempt, checkout SHA and bounded failure block. Preserve the refusal and successful authorized-read controls. VR3's repair cannot be finalized from the generic Locked category. Clipboard stays a separately verified contract, not a guessed explanation. |
| Permission readiness/load/cleanup early stop | [Run 39040295677](https://github.com/fpmirabile/arany/actions/runs/39040295677), checkout `08c9ae7572025875e03685cce091278ee32d38f2`, `1_macOS Tests (debug).txt:989`, stops with an unconsumed scripted call but no cause. [Run 39050001446](https://github.com/fpmirabile/arany/actions/runs/39050001446), checkout `dd4da9d27ec4aa6334e5baa40fdc0f987685273f`, `2_macOS Tests (release).txt:997`, identifies `default/allow/command`, Failed Run, Uncertain Tool, absent attestation and unconfirmed storage cleanup. | Continuation correctly stops after an unknown effect. These older logs do not distinguish listing, detachment, remaining mounts, process stop or an enclosing completion timeout. Current code has finer stage/check capture. **Research gate open, R2:** obtain intent-correlated current failure evidence before choosing the VR4 repair. |
| Nearby permission failure with a different cause | [Run 39802154785](https://github.com/fpmirabile/arany/actions/runs/39802154785), checkout `916188a93e12974eae06f7ead128fc38cfa230f0`, `5_macOS Tests (release).txt:1868`, identifies `default/ask/command`, non-dispatched Limit, resource-limit stage and active-time exhaustion. | Keep this separate from unconfirmed cleanup. The current permission fixture advances paused time only inside the approval wait and resumes before dispatch ([`permissions.rs:211`](../../crates/arany-cli/tests/guard_runs/permissions.rs#L211)); do not reintroduce virtual-time advancement around native I/O or explain every early stop as host load. |

R1/R2 collection belongs to Research, before implementation, rather than a
research-only PR masquerading as a vertical slice. The lead uses the preferred
GitHub wrapper to list the exact workflow/job/attempt and obtain the corresponding
`gh run` logs, records the tested checkout SHA and failing case, and compares the
failure with intervening source changes. Read existing debug diagnostics and the
bounded test-support stage/check first. Missing stages remain unknown. If the
native failure cannot be reproduced, retain the gap and this Draft status; a
passing rerun does not establish a cause.

### Proven peers

Sources were read on 2026-10-27; immutable source links pin inspected code.

- [Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing#what-runs-outside-the-sandbox)
  separates built-in file Tools from sandboxed Bash. Its
  [monitoring attributes](https://code.claude.com/docs/en/monitoring-usage#span-attributes)
  separate permission waits from Tool execution. Keep Arany's existing execution
  sites and per-operation measurements. Claude's default unavailable-sandbox
  fallback is unsuitable for Arany; its strict settings demonstrate refusal is
  compatible with the peer UX.
- [Anthropic's macOS sandbox wrapper](https://github.com/anthropics/sandbox-runtime/blob/3a2954dc8ab938877fe4e279f04dfe4dd18ab40d/src/sandbox/macos-sandbox-utils.ts#L1596)
  constructs a Seatbelt `sandbox-exec` command. This inspected path supplies no
  disk-image cleanup algorithm to copy. Arany's bounded storage and positive
  cleanup requirements justify its additional lifecycle work.
- [Codex Tool timing](https://github.com/openai/codex/blob/b4ad33d1a3c10c9652dd57f7afd7a949f8f26895/codex-rs/otel/src/events/session_telemetry.rs#L1273)
  measures the awaited handler with `Instant` and records operation/success.
  Reuse that measurement shape, extending the endpoint through Arany's durable
  acknowledgement as already decided in [D-23](../../docs/research/next-step-decision-register.md#d-23--tool-latency-measures-durable-acknowledgement).
  No inspected peer supplies a transferable numeric latency objective.
- [Codex exec lifecycle](https://github.com/openai/codex/blob/cba716c0f1caa4f2528dedbb1075b50126709d7e/codex-rs/core/src/exec.rs#L1034)
  distinguishes cancellation/timeout, terminates the child group and bounds pipe
  draining ([same source, drain owner](https://github.com/openai/codex/blob/cba716c0f1caa4f2528dedbb1075b50126709d7e/codex-rs/core/src/exec.rs#L84)).
  Copy explicit lifecycle ownership and bounded waits. Group termination alone
  cannot replace Arany's native coalition/storage proof or settle an unknown
  effect as success. No inspected peer source diagnoses Arany's Keychain flake.

### RPI gates

- **Research gate:** current code/spec and peers are verified above. R1 and R2
  remain material missing evidence; historical busy-detach has a confirmed failure
  boundary, but native confirmation of the proposed attachment repair remains a
  required experiment. Obtain red-capable exact native reproductions and minimize
  them; then replace provisional repair choices with observed causes. Do not use
  sleeps, random ordering, effect retries or changed acceptance thresholds to
  manufacture a passing result.
- **Plan gate:** O1/O2 need owner disposition and R1/R2 must resolve. Every repair
  must name a demonstrated failure, the owning function/callers, a deterministic
  trigger, and evidence that fails before the fix. Re-estimate the affected slice
  if those findings change its inventory. No slice is authorized to invent a
  repair while these gates are open.
- **Implement gate:** only after both gates pass, land the slices below in order,
  one PR per exact ID. If code contradicts research, stop and revise this plan.

## Contract and scenarios

Existing normative owners remain [Tools](../../docs/specs/tools.md#macos-enforcement-and-compatibility),
[file publication](../../docs/specs/tools.md#in-process-file-operations),
[Execution](../../docs/specs/execution.md#committed-event-stream),
[Credentials](../../docs/specs/credentials.md#selected-item-authorization),
[clipboard](../../docs/specs/terminal.md#paste-and-attachments) and
[Telemetry](../../docs/specs/telemetry.md#export-bounds).
These fixes restore those contracts. In the slice that extends the latency
corpus, update its existing Tool scenario row and corpus notes to name Read,
Edit and Command; add no new behavioral section. Change another owning sentence
or scenario row only if the approved repair exposes a missing recovery contract.

| Scenario | Required observation and authoritative evidence owner |
| --- | --- |
| Warm Read / Edit / no-op Command | Existing Read and Edit observations retain their execution site; Command runs the real native Guard and returns Succeeded with its receipt, successful exit, empty output/changeset, unchanged Workspace and no publication checkpoint. Close Engine, reopen SQLite, compare strict replay and fresh `show` JSONL. Owner: `guard_runs/latency.rs` and its corpus notes. |
| Latency failure or lost acknowledgement | Denial, stale digest, pre-GO refusal, failed command, cancellation, unknown post-GO cleanup, lost append acknowledgement or capture overflow invalidates the row; no success percentile or replacement attempt. Existing acknowledgement/failure scenarios own this, extended by a small operation table. |
| Mounted runtime, then explicit new clean attempt | Nested mount rejects before GO with no payload marker; closed replay retains failure. Fixture cleanup positively proves owned image and mount absence. A separately requested new Run after removal succeeds under the selected runtime; this is not an automatic retry. Owner: `session_run/tools_macos.rs::native_runtime_mount_journey_helper`. |
| Native store deny and authorized read | Unauthorized test actor cannot access/delete the item; the admitted helper can read the unchanged selected identity afterward. Actual lock still refuses; explicit synthetic unlock recovers; malformed/stale identities and cancellation stay refused. No helper/Provider fallback or secret output. Owner: `credentials/macos_tests.rs` through the existing native parent. |
| Permission case completion and failure | Existing mode/rule/Tool matrix succeeds with exact permission source and replay; deterministic failures at connection, preparation, native manager and teardown retain their own stage. No dispatch before GO; unknown post-GO result stops continuation, publishes nothing and retains recovery records. Owner: `guard_runs/permissions/modes.rs`, shared fixture and existing Guard failure tables/native unit corpus. |
| Collector overflow / incomplete body / dropped future | A streaming response crossing the locked adapter's cap is refused; an incomplete response cannot hold the next export; dropping the bounded response future releases its owned read. A subsequent export works, shutdown stays bounded and canonical Run/output/replay are unchanged. Owners: `telemetry/http.rs`, existing telemetry corpus and `session_run/telemetry.rs`. |

## Chosen direction

Use the existing corpus, native process helpers, Keychain fixture, Guard lifecycle
owners and HTTP adapter. Ponytail selects reuse before new code: no new crate,
dependency, trait, daemon, benchmark subsystem or canonical Event. The only
behavioral variation is the existing Linux/macOS adapter pair. Private test gates
may control the demonstrated race; they are not production configuration.

Linux gains a fourth ignored release measurement row. macOS gains repairs and
native evidence in its existing enforcement/account lanes; no Linux timing result
establishes macOS latency. Clipboard and OTLP invariants remain common, with real
native service evidence tracked separately. Linux overlays keep their existing
profile, while macOS keeps its copy/image profile.

Module ownership and existing process boundaries remain as described by the
[architecture](../../docs/architecture/system-overview.md#2-runtime-architecture).
Engine/Journal own ordering, Goal budgets and accepted continuation; Guard owns
READY/GO, identities, deadlines, coalition/job/storage state and cleanup; account
helpers own secret access and redacted errors. Only closed stage/check facts cross
back to diagnostics. No native error text or platform type enters Engine policy.
Existing directory capabilities and receipts remain the publication boundary.

## Security and resource model

Security-pass triggers are native process/storage lifecycle, credential-loading
behavior, temporary files and observer I/O. Each slice PR supplies security Notes
against the [security pass](../../agents/security.md#security-pass-triggers):

- **Assets and authority:** Workspace files, private journal/checkpoints, account
  metadata/items, native unit records and the caller's desktop. Authority remains
  the pinned typed grant and human/user-owned policy. Normalize and commit intent
  and verdict before dispatch; verify effective enforcement before GO. Neither
  fixture metadata nor a CI log, repository, model or observer grants authority.
- **Untrusted inputs and descendants:** hostile paths, mount replacement, reused
  disk numbers/PIDs, malformed native listings, forged helper output, clipboard
  bytes and arbitrary Collector responses. Preserve no-follow identity checks,
  boot/incarnation binding and least-privilege environment/descriptor sets.
  Children remain within parent grants and receive no host authority.
- **Secrets and startup:** synthetic Keychain only; pin the admitted helper before
  item creation, folder consent before selected-item authorization and credential
  use only at the account edge. Never read live account storage, loosen item ACLs,
  unlock a user store, alter the search list or silently select private files.
  Diagnostic additions use closed categories only; canaries cover all outputs.
- **Bounds and overflow:** retain finite Goal, action, native cleanup, process,
  capture, result and aggregate-storage limits at their owners. VR1 derives
  recorder/evidence capacity from four finite rows without increasing aggregate
  retention; its admission failure stops measurement. VR2/VR4 use existing shared
  cleanup budgets and ownership revalidation, with no new effect retry. VR5 keeps
  request/response/queue/shutdown bounds and disabled exporter retries.
- **Failure containment and non-claims:** unknown post-GO completion remains
  Uncertain, stops continuation and never publishes or retries. Before-GO refusal
  proves no payload, not successful execution. Failed cleanup retains owned
  records and capacity; no recursive deletion through mounted/unknown paths and
  no global image/process sweep. Observers cannot delay Journal appends, supply
  approvals or alter outcomes. Host commands never become a Guard fallback.
  Private files do not protect against the same user/root, and the journal is
  not tamper evidence.

## Risks and mitigations

- Old CI symptoms may already have a partial repair: compare the exact failing
  revision with current owners, demonstrate the surviving gap, and avoid duplicate
  retry or instrumentation machinery.
- `Locked` collapses multiple native causes: obtain R1 and distinguish them in a
  private native fixture before deciding between fixture and product repair.
- Timing can hide missing guarantees: audit receipts, unchanged bytes and closed
  replay before accepting any sample; never trade SQLite durability for a target.
- A full native run can expose unrelated failures: consolidate failures on that
  exact head, keep their run links, and obtain owner disposition under
  [failure discipline](../../agents/testing.md#failure-and-maintenance-discipline).
- Existing large test parents must not grow indiscriminately: put new focused
  native rows beside their child owner under [corpus placement](../../agents/testing.md#corpus-placement).
  No unrelated corpus split is bundled into a reliability slice.

## Vertical slices

Every estimate is **added + deleted lines**, including tests and same-PR docs;
there are no move/deletion exclusions or ceiling exceptions. Each ID is one
branch/PR and one observable increment. Order: VR1 → VR2 → VR3 → VR4 → VR5.
Keep this list/index unchanged in ordinary slice PRs; merged PR `Slices` lines
are the progress record under [slice delivery](../PLANNING.md#slice-delivery).

| ID | Observable delivery | Estimate | Hard ceiling |
| --- | --- | --- | --- |
| VR1 | Audited guarded no-op Command measurement beside existing Read and Edit rows | +650 / -170 = **820** | **1,200** |
| VR2 | Mounted-runtime refusal followed by positively cleaned fixture and clean selected-runtime success | +220 / -90 = **310** | **600** |
| VR3 | Native unauthorized denial followed by reliable authorized selected-item access | +330 / -110 = **440** | **800** |
| VR4 | Permission commands complete under native lifecycle races or stop at the correct proven failure boundary | +650 / -230 = **880** | **1,400** |
| VR5 | Malformed/incomplete Collector responses release the observer and preserve canonical outcomes; close the plan | +440 / -560 = **1,000** | **1,500** |

VR5 includes deletion of this plan and its index row in its changed-line estimate.

### VR1 — Complete the Linux operation corpus

- Inventory: `crates/arany-cli/tests/guard_runs/latency.rs`, its `latency.md`,
  `crates/arany/src/test_support/latency.rs`, the existing latency scenario in
  `docs/specs/tools.md`, and this item's wording in `NEXT_STEPS.md`.
- Extend the current row table/helper with Command. Admit and hash a real native
  no-op executable on the selected host using ordinary executable pinning; record
  its canonical identity as a prerequisite and refuse absence. Use explicit argv,
  no interpreter, no network, no MCP and no approval wait, with a matching admitted
  allow rule. Do not use `CommandUnguarded` or a fake Guard. Planning is already
  optional ([Goals](../../docs/specs/goals.md#initial-plan-contract-and-success-receipt));
  keep two scripted calls and ordinary Goal budgets.
- Cross the real Provider receipt → permission/intent commits → native Guard
  preparation/GO/execution/cleanup → ToolFinished acknowledgement → closed replay
  and fresh `show` path. Assert the Guard receipt, zero exit, empty stdout/stderr
  and changeset, unchanged file digest, and no integration/publication checkpoint.
  The timed interval includes native preparation/capture/cleanup and durable
  bookkeeping; exclude fixture/admission startup, inference and inspection as now.
- Keep the existing warm-ups, measured count, attempt order, percentile rule,
  current-thread runtime, local-disk/private-account controls and total retention
  cap. Derive the fourth row's allocation and recorder ceiling once. Report
  partial/failed evidence without a valid success distribution; no replacement
  samples, outlier deletion or deadline changes. Keep existing Read/Edit objectives
  operation-specific; Command objective follows O2.
- Extend the existing acknowledgement/failure table for Edit and Command success,
  denial, missing acknowledgement and post-GO unknown outcome as appropriate;
  reuse native failure owners instead of making the ignored timing benchmark a
  deterministic CI gate. CI verifies recorder semantics on Linux/macOS. The owner
  runs the ignored corpus on the named Linux host under its documented procedure,
  retaining each invocation's reports and journals; no native numbers are claimed
  by this planning PR.

### VR2 — Settle runtime topology fixture ownership

- Inventory: `crates/arany-cli/tests/session_run/tools_macos.rs`,
  `crates/arany/src/test_support/process.rs` only if a demonstrated shared-helper
  defect remains, its focused child evidence if needed, and the existing
  macOS runtime-topology `NEXT_STEPS.md` row.
- Use kernel-only attachment for the synthetic topology image as production
  storage already does. Keep the image guard established immediately after
  attachment and release owned handles before explicit detach. Reuse the bounded
  image-to-device lookup/confirmation helper; never kill an ambient `diskimage`
  process or select a device by a remembered number.
- Preserve the complete mounted-runtime refusal/replay and subsequent clean
  selected-runtime success journey. Add a controlled held-reference case at the
  fixture owner: cleanup must not claim absence while the OS still lists the
  image or mount; after explicit release, require fresh inventory and mount
  absence before root deletion. A held descriptor alone is not proof that forced
  detachment must fail. Failure/unwind retains the image/root if absence cannot
  be proved.
- Gate the repair with the historical failure boundary and native macOS debug
  runs. Existing production topology/cleanup contracts are sufficient; do not
  change them or add production detach retries for this fixture defect. Record
  stability before removing its pending row.

### VR3 — Repair the selected-item denial/read boundary

- Inventory: `crates/arany-cli/src/accounts/credentials/macos_tests.rs`,
  `credentials/keyring_helper/macos.rs` and `credentials/keyring_helper.rs` only
  when R1 proves a production classification/ownership defect, plus the existing
  Credentials scenario and native-stores `NEXT_STEPS.md` row as needed.
- **Repair selection is gated by R1.** Pin the historical failing stage and
  native error category without secret/platform-text logging. Reproduce the
  unauthorized actor → admitted helper sequence in an exactly-one-test child
  with the same immutable helper identity and synthetic item.
- If R1 proves fixture lifetime contamination, isolate the negative actor's
  native handles/process and require its completion before the authorized helper;
  keep the same item/identity and verify unchanged contents. If it proves the
  Keychain really locked, establish why the fixture's lock policy/lifetime changed
  and repair that synthetic lifecycle. If it proves a helper/adapter defect, fix
  the single shared owner and exercise every affected read/authorize caller.
  Replace this decision tree with the demonstrated repair before Ready.
- Preserve actual locked/denied refusal, stale identity, explicit synthetic
  unlock, cancellation/reap and output canaries. Never translate Locked into
  success, retry credential reads, relax ACLs or automatically unlock storage.
  Run the existing native parent in macOS debug and retain Linux helper-protocol
  coverage. Clipboard regressions use their existing owner only if R1 connects
  them; human clipboard/Keychain dialog checks stay deferred native evidence.

### VR4 — Close the permission-case native lifecycle failure

- Inventory: `crates/arany-cli/tests/guard_runs/permissions.rs`,
  `permissions/modes.rs`, existing Guard failure tables in
  `crates/arany/src/tools/guard.rs`, and the demonstrated owner in
  `guard/macos/unit.rs`, `storage.rs` or `fixture.rs`; update only the affected
  Tool/Diagnostics scenario and readiness/load `NEXT_STEPS.md` row if necessary.
- **Repair selection is gated by R2.** Bind mode/rule/operation, intent identity,
  GO state and first failed native stage/check before teardown overwrites a
  snapshot. The existing bounded release recorder and debug stages are the first
  probe. Fix the shared lifecycle owner and all its callers, not each permission
  matrix cell. Replace this provisional inventory with the diagnosed function
  and repair before Ready; split by failure risk if the ceiling would be exceeded.
- Exercise explicit gates for connection refusal, preparation failure,
  native-manager refusal, post-GO process/report loss and the proven storage
  check. A pre-GO failure must prove no payload; post-GO loss remains Uncertain,
  no integration, no further Provider call and non-executing closed replay.
  Successful release must positively empty the coalition/remove the owned job,
  release storage handles, confirm detach and settle the journal before
  continuation. Retained uncertain records remain recoverable under exact
  ownership; a fresh explicit Run never retries the old intent.
- Test schedules use channels/failpoints and paused time only outside native
  I/O. Do not increase product deadlines, insert sleeps, retry matrix cases,
  quarantine the case or weaken expected permission/attestation assertions.
  Run the full matrix through current macOS release CI; use a focused debug
  invocation diagnostically if R2 requires it, without changing shared CI.

### VR5 — Prove observer response disposal and close delivery

- Inventory: `crates/arany/src/telemetry/http.rs`, a focused `http/tests.rs` if
  necessary, existing `telemetry/tests.rs` and
  `crates/arany-cli/tests/session_run/telemetry.rs`, the existing Telemetry
  acceptance rows, and closing plan/index/`NEXT_STEPS.md` link changes.
- Reuse the locked HTTP client's response cap and complete-response deadline.
  Through a data-free local Collector, test streaming cap-minus-one/exact-cap/
  cap-plus-one responses without a trustworthy Content-Length, and a bounded
  response that never completes. Require typed oversize refusal independently
  of timeout so an early deadline cannot pass the size test.
- Add a gate after headers/first body bytes and drop the incomplete response
  future at that gate; prove owned read disposal and a successful next export.
  If access to the inner bounded future is needed, extract one private async
  function used by the existing runtime wrapper and the test; no new public
  client abstraction, dependency or alternate HTTP implementation. Use explicit
  gates and virtual time for logical deadlines, bounded real socket supervision,
  cleared ambient proxy configuration and nonblocking-to-blocking accepted-socket
  normalization on macOS. No sleep-driven ordering.
- Through the existing product export journey, compare exact exit/stdout/stderr,
  closed Events and reduced view with the failed-Collector variant; drain exports
  through shutdown and check canaries. Retain the separately ignored native
  trickle gate as native timing evidence. Update the two existing acceptance rows
  to identify their new evidence owners; no runtime contract change is expected.
- Run the closing audit after all slices merge. Obtain three sequential complete
  `ci.yml` runs on the identical final head with both native macOS profiles,
  retaining all attempts and failures; focused passes alone do not close a flake.
  Record effective nonzero test counts/profile selections. No workflow or shared
  runner change is included; request owner scope if a new lane is necessary.
- The closing PR uses this checklist, one completed line per ID and criterion:

  - [ ] VR1 — merged PR URL; actual added/deleted count within ceiling.
  - [ ] VR2 — merged PR URL; actual added/deleted count within ceiling.
  - [ ] VR3 — merged PR URL; actual added/deleted count within ceiling.
  - [ ] VR4 — merged PR URL; actual added/deleted count within ceiling.
  - [ ] VR5 — closing PR URL; actual added/deleted count within ceiling.
  - [ ] E1 — corpus command, exact nonzero selection and audited retained reports;
    accepted host/objective disposition and every target miss.
  - [ ] E2 — topology refusal, positive detach and clean success evidence/run links.
  - [ ] E3 — denial/authorized read, real lock refusal and explicit recovery evidence.
  - [ ] E4 — each classified lifecycle fault, no forbidden continuation/publication,
    replay and positive cleanup observations.
  - [ ] E5 — oversize and incomplete/dropped-body cases, next export and unchanged
    product observations; Linux/macOS commands/results.
  - [ ] E6 — formatting, Clippy, full exact-head CI run URLs/attempts and stability
    results, documentation/link/spelling checks and security review disposition.
  - [ ] E7 — remaining native/live/manual claims moved or retained once in
    `NEXT_STEPS.md`; inbound links updated, index row and plan removed under the
    planning lifecycle, or an unmet required criterion keeps the plan open.

## Open questions

| ID | Owner decision or missing evidence | Recommended answer |
| --- | --- | --- |
| O1 | Arany owner: does “guarded Edit” mean the current admitted typed Edit, or a new process execution site? | Keep the existing in-process Edit row with its real FileReceipt/checkpoint; rename the backlog wording to “admitted Edit and guarded no-op Command.” This matches both the current contract and peer separation of file Tools from commands. A separate Guard Edit path requires a different, explicitly authorized contract. |
| O2 | Arany owner: select the quiet native Linux host and the no-op Command objective. | Preserve the existing Read/Edit objectives; publish Command p50/p95/max as a separate baseline with a null objective until the owner accepts a host-specific number. Do not inherit the single-call objective accidentally or infer a peer SLO. Retain all three corpus invocations per the existing owner procedure. |
| R1 | Lead: exact failing native-store CI job/attempt/log and observed native category after denial. | Supply the archived evidence and a native controlled reproduction; repair the fixture unless evidence identifies a shared product-owner defect. No blanket error remapping or auto-unlock. |
| R2 | Lead: current permission-case failure's first closed stage/check and native reproduction. | Use existing diagnostics/intent recorder on the exact head, classify connection/preparation/manager/process/storage independently, then fix the demonstrated shared owner. Do not decide that load or a short deadline is the cause without evidence. |

## Exit criteria

- **E1:** the ignored Linux release corpus contains audited single Read, concurrent
  Read, admitted Edit and guarded no-op Command rows; all valid samples join to
  durable history and fresh `show`. Failure/missing acknowledgement/overflow never
  produces a success verdict. Named-host claims require retained owner runs under
  [the corpus procedure](../../crates/arany-cli/tests/guard_runs/latency.md#owner-procedure).
- **E2:** the topology journey refuses a mounted runtime before GO, proves fixture
  detachment and succeeds on a new clean Run. No cleanup result is inferred from
  a subprocess exit or missing path alone.
- **E3:** R1 has a demonstrated cause and deterministic regression: unauthorized
  access refuses, authorized access to the unchanged item succeeds afterward,
  actual lock still refuses and explicit recovery works without credential leakage.
- **E4:** R2 has a demonstrated cause/regression at its owner; meaningful pre/post-GO
  faults retain correct outcomes, storage/recovery ownership and closed replay.
  Permission matrix expectations remain exact on Linux and macOS.
- **E5:** response-size refusal and incomplete-future disposal are distinct proven
  observations through the actual OTLP adapter; subsequent export/shutdown proceed
  and product outcomes/channels/replay remain unchanged.
- **E6:** implementation PRs run formatting and relevant Clippy through the allowed
  worktree/team build path, with tests in CI under the team standing rule. Full
  exact-head CI and the VR5 native stability observation are green. This planning
  task runs no Cargo, commits, pushes or GitHub CLI; its checks are
  `~/.cargo/bin/typos`, `git diff --check` and a local relative-link/anchor scan.
- **E7:** behavior/scenario coverage has one owning spec and each regression one
  evidence owner. Native human dialogs, clipboard desktop interoperability and
  unrun named-host measurements remain explicit in `NEXT_STEPS.md` and never count
  as completed proof. Follow the final-slice checklist and
  [planning lifecycle](../README.md#lifecycle); unresolved required fixes keep
  this plan open rather than disappearing with its deletion.
