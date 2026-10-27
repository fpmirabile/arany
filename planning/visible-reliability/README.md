# Visible reliability

## Metadata

- Status: `Draft` — O1/O2 open; R1/R2 are recurrence checks scoped to VR3/VR4; VR1 and VR5 are implementable now; implementation has not started.
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
is owned by the [Tools contract](../../docs/specs/tools.md#linux-enforcement-and-compatibility). Measure the
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
| Shared fixture detach already retries within a bound, re-resolves the owned device and confirms absence after its final forced detach. Production images attach kernel-only. | [`test_support/process.rs:200`](../../crates/arany/src/test_support/process.rs#L200), [`macos/storage.rs:203`](../../crates/arany/src/tools/guard/macos/storage.rs#L203). Do not add a second retry loop or change attachment without a current reproduction; kernel-only attachment is a candidate mitigation, not a demonstrated repair. |
| Native cleanup already distinguishes storage checks and has one shared teardown deadline; a native disk command may outlast the remaining shared deadline. | [`tools.md:1115`](../../docs/specs/tools.md#L1115), [`macos/storage.rs:447`](../../crates/arany/src/tools/guard/macos/storage.rs#L447), [`macos/unit.rs:1690`](../../crates/arany/src/tools/guard/macos/unit.rs#L1690). An overrun is specified to remain Uncertain; increasing deadlines or accepting incomplete cleanup is not a fix. |
| Current failure capture can identify the failing Tool in optimized test builds. Debug stage files are snapshots, not a complete timeline. | [`test_support.rs:166`](../../crates/arany/src/test_support.rs#L166), [`guard_runs.rs:49`](../../crates/arany-cli/tests/guard_runs.rs#L49), [`permissions.rs:226`](../../crates/arany-cli/tests/guard_runs/permissions.rs#L226), [`macos/fixture.rs:1`](../../crates/arany/src/tools/guard/macos/fixture.rs#L1). Reuse intent-correlated stage/check evidence rather than infer causality from a final generic message. |
| Keychain denial is tested immediately before a helper-mediated authorized read of the same item. Several distinct native access errors map to Locked. | [`credentials/macos_tests.rs:185`](../../crates/arany-cli/src/accounts/credentials/macos_tests.rs#L185), [`keyring_helper/macos.rs:15`](../../crates/arany-cli/src/accounts/credentials/keyring_helper/macos.rs#L15), [`keyring_helper.rs:278`](../../crates/arany-cli/src/accounts/credentials/keyring_helper.rs#L278). Locked alone cannot identify actual keychain lock, item ACL refusal or store unavailability. |
| The native Keychain test pins a copied helper, disables user interaction, uses a private synthetic Keychain, and holds NATIVE_GATE. | [`credentials/macos_tests.rs:208`](../../crates/arany-cli/src/accounts/credentials/macos_tests.rs#L208), [`credentials.rs:1115`](../../crates/arany-cli/src/accounts/credentials.rs#L1115). Preserve these controls; no login Keychain, search-list edits or automatic unlock of user storage. |
| Clipboard has an independent process-group/cleanup owner. Its inclusion in the pending row is not evidence of a clipboard root cause. | [`terminal.md:347`](../../docs/specs/terminal.md#L347), [`terminal/clipboard.rs`](../../crates/arany-cli/src/terminal/clipboard.rs). Preserve late-result rejection and the restart-required latch after unknown cleanup. |
| Whole-response export timeout already wraps the Reqwest OTLP adapter; the existing ignored trickle gate tests progress, not streaming-size refusal or dropped-future cleanup separately. | [`telemetry/http.rs:44`](../../crates/arany/src/telemetry/http.rs#L44), [`telemetry/tests.rs:2058`](../../crates/arany/src/telemetry/tests.rs#L2058), [export bounds](../../docs/specs/telemetry.md#export-bounds). Add those two missing observations at this owner. |
| CI profile selection changed since the early debug failure: guard_runs now runs in the full release suite; debug selects unit/binary/session_run targets. | [`.github/actions/ci-tests/action.yml:59`](../../.github/actions/ci-tests/action.yml#L59). Historical debug logs must be read against their own revision; current release evidence must exercise the moved permissions owner. |

### CI history inspected and remaining diagnosis

Fable's validation reports a live `gh run view`/job-log scan of every failed
macOS job (all attempts) of `ci.yml` from 2026-10-06 through 2026-10-27, plus
rerun successes, PR #171's branch and the Focused CI lane: about 360 job logs.
That broader scan reports no busy-detach recurrence after `572b46b3` and no
permission-family recurrence after `8b3c653e` (#171); neither result proves a cause.

This revision independently checked run/attempt/job metadata through the GitHub
REST API, the cited source changes, and 219 retained aggregate macOS job logs
(110 debug) dated 2026-10-16–27. The available subset includes seven debug logs
at known descendants of `572b46b3`, with no busy-detach failure; eleven logs
(six debug) at known descendants of `8b3c653e` contain neither unconfirmed message.
No scanned log contains `Locked` or the native-store round-trip failure. The
subset is not the complete live-history scan: authenticated job-log downloads
were unavailable, and the additional family members below remain attributed to
Fable. Slice evidence must retain its exact inventory/count, attempts, checkout
SHA and job links. No raw native output or private paths belong in this plan.

| Failure family | Historical observation and attempt/job citation | Diagnosis and disposition |
| --- | --- | --- |
| Runtime mount busy-detach | [39052525247, attempt 1, debug job 117453946130](https://github.com/fpmirabile/arany/actions/runs/39052525247/attempts/1#job-117453946130), checkout `1cced52b`; [39069818405, attempt 1, debug job 117505186398](https://github.com/fpmirabile/arany/actions/runs/39069818405/attempts/1#job-117505186398), checkout `0c32ee4a`. Both retained logs show fixture detach exit 16, only `diskimage` holding the backing image in current-user `lsof`, unknown worker GO and completed supervisor detach. The first run ultimately succeeded on attempt 2. | Both precede the shared bounded helper, `572b46b3` (#122, 18 Oct). `diskimage` normally holds a userspace attachment's backing file; this alone does not identify the busy holder. No current-user mountpoint holder was listed; system-user holders remain unverified candidates. VR2 first reproduces on the current focused debug lane; absent recurrence, close with stability evidence and no source change. |
| Other topology assertions | [39166907834, attempt 1, debug job 117791757850](https://github.com/fpmirabile/arany/actions/runs/39166907834/attempts/1#job-117791757850), PR head `63b7c132`; [39163692991, attempt 1, debug job 117782166305](https://github.com/fpmirabile/arany/actions/runs/39163692991/attempts/1#job-117782166305), PR head `0dd6184c`. Fable identifies `tools_macos.rs:1115`, expected one call but observed two, in both; the first is independently confirmed in the retained log. | These are not detach failures and do not establish a surviving busy-detach defect. |
| Native stores and clipboard | Fable found no `Locked` refusal or `native_store_round_trips_an_isolated_synthetic_account` failure in its retained-history scan, including every PR #171 attempt and the focused lane; the independent subset also has none. | #171 introduced the pending row and pinned a private helper copy. Identity replacement is a hypothesis: interaction-required access with interaction disabled can yield `ERR_SEC_INTERACTION_NOT_ALLOWED` (-25308), which current code maps through `NoStorageAccess` → `EXIT_LOCKED` → `CredentialError::Locked`. That chain matches the symptom, but does not demonstrate its cause. R1/VR3 add closed-category diagnostics and native stability closure; no request for a nonexistent retained failing log or speculative repair tree. Clipboard keeps its separate owner. |
| Permission early-stop family | [39040295677, attempt 1, debug job 117418288627](https://github.com/fpmirabile/arany/actions/runs/39040295677/attempts/1#job-117418288627), checkout `08c9ae75`, has an unconsumed call without a cause. [39050001446, attempt 1, release job 117446474026](https://github.com/fpmirabile/arany/actions/runs/39050001446/attempts/1#job-117446474026), checkout `dd4da9d2`, shows both `default/allow/command` storage cleanup unconfirmed and `auto/mcp_list` execution unconfirmed in the children-inherit case. Both retained logs are independently confirmed. Fable's additional runs/cases are enumerated below. | Five tests across **eight distinct runs** on 17–18 Oct. The two closed stages are execution unconfirmed (`Guard stage=execution`) and storage cleanup unconfirmed. API head metadata/source history place all before #171; Fable reports neither message afterward. R2 first checks recurrence in full focused macOS release runs on one current head. A recurrence identifies the shared owner before any repair; no recurrence closes VR4 with evidence and row removal. |
| PR-branch resource-limit regression | [39802154785, attempt 1, release job 119906807260](https://github.com/fpmirabile/arany/actions/runs/39802154785/attempts/1#job-119906807260) is a `pull_request` run of `ac29130f`, “wait for primary approvals without a wall-clock expiry”; `916188a9` is its tested merge ref. Its retained log has 12 permissions/unguarded failures and `Limit "Goal active-time budget exhausted; no operation was dispatched" [Guard stage=resource limit]`. | A PR-branch regression, not a main flake. Retained only to distinguish Limit from unconfirmed cleanup; this failed job does not establish when it was fixed. The current fixture pauses time only inside approval wait and resumes before dispatch ([`permissions.rs:211`](../../crates/arany-cli/tests/guard_runs/permissions.rs#L211)). |

Fable's five-test early-stop inventory (all links select attempt 1; additional
job identities/statuses were checked through the API):

| Test, relative to `guard_runs` | Runs/jobs and closed observation |
| --- | --- |
| `permissions::permission_dispatch_modes_rules_command_mcp_and_edit` | 39040295677 and 39050001446 above; [39092399594 / release 117571730385](https://github.com/fpmirabile/arany/actions/runs/39092399594/attempts/1#job-117571730385). Unconsumed calls; the classified command case reports storage cleanup unconfirmed. |
| `permissions::permission_dispatch_children_inherit_parent_restrictions_and_review` | 39050001446 above (execution unconfirmed); [39080542506 / release 117536426585](https://github.com/fpmirabile/arany/actions/runs/39080542506/attempts/1#job-117536426585) (storage cleanup unconfirmed). |
| `permissions::guarded_command_over_its_timeout_with_a_background_child_is_a_limit_and_reaps_it` | [39100777287 / release 117596447425](https://github.com/fpmirabile/arany/actions/runs/39100777287/attempts/1#job-117596447425), [39103185039 / release 117603495406](https://github.com/fpmirabile/arany/actions/runs/39103185039/attempts/1#job-117603495406), [39106833027 / release 117614425534](https://github.com/fpmirabile/arany/actions/runs/39106833027/attempts/1#job-117614425534): execution unconfirmed, execution stage. |
| `permissions::hook_limits_fail_closed_and_timeouts_reap_descendants` | 39100777287 / release 117596447425 above; retain its own first stage/check if it recurs rather than infer it from the timeout case. |
| `unguarded::always_allow_records_the_exact_rule_for_this_project_only_and_applies_next_submission` (historical name) | [39087838535 / release 117558066050](https://github.com/fpmirabile/arany/actions/runs/39087838535/attempts/1#job-117558066050): storage cleanup unconfirmed. |

R1/R2 are scoped recurrence checks, not whole-plan Research blockers. A passing
repeat does not establish a historical cause; evidence-only closure records
stability without claiming a repair. Retain every attempt/failure, effective
nonzero selections and stage/check facts; missing stages remain unknown.

### Proven peers

Sources were read on 2026-10-27; immutable source links pin inspected code.

- [Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing#what-runs-outside-the-sandbox)
  separates built-in file Tools from sandboxed Bash. Its
  [monitoring attributes](https://code.claude.com/docs/en/monitoring-usage#span-attributes)
  separate permission waits from Tool execution. Keep Arany's existing execution
  sites and per-operation measurements. Claude's default unavailable-sandbox
  fallback is unsuitable for Arany; its strict settings demonstrate refusal is
  compatible with the peer UX.
- [Codex Tool timing](https://github.com/openai/codex/blob/b4ad33d1a3c10c9652dd57f7afd7a949f8f26895/codex-rs/otel/src/events/session_telemetry.rs#L1273)
  measures the awaited handler with `Instant` and records operation/success.
  Reuse that measurement shape, extending the endpoint through Arany's durable
  acknowledgement as already decided in [D-23](../../docs/research/next-step-decision-register.md#d-23--tool-latency-measures-durable-acknowledgement).
  No inspected peer supplies a transferable numeric latency objective.

### RPI gates

- **Research gate:** current code/spec and retained evidence establish the bounded
  work below; the full-history non-recurrence findings are attributed above.
  VR2 begins with current native evidence. R1/R2 govern only VR3/VR4 closure and
  any recurrence-driven repair; they do not block VR1, VR2 evidence or VR5 work.
- **Plan gate:** O1/O2 remain owner decisions for VR1 wording/measurement acceptance.
  VR1 and VR5 have concrete implementation scope now. Any source repair in VR2
  or VR4 requires a demonstrated current failure, owning function/callers,
  deterministic trigger and evidence that fails before the fix. Re-estimate that
  slice after diagnosis; no speculative repair, sleep, effect retry or changed
  acceptance threshold is authorized.
- **Implement gate:** deliver independent VR1–VR4 in any order; VR5 lands last
  because it closes the plan. Its adapter proof can be prepared now. Each PR names
  its exact ID; if code contradicts research, stop and revise the affected slice.

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
| Permission case completion and failure | Existing mode/rule/Tool matrix succeeds with exact permission source and replay; deterministic failures at connection, preparation, native manager and teardown retain their own stage. No dispatch before GO; unknown post-GO result stops continuation, publishes nothing and retains recovery records. Owner: `guard_runs/permissions.rs` fixture and its scenario children (`modes.rs`, `rules.rs`, `hooks.rs`, `host.rs`), the affected `unguarded.rs` caller, and existing Guard failure tables/native unit corpus. |
| Collector overflow / incomplete body / dropped future | A streaming response crossing the locked adapter's cap is refused; an incomplete response cannot hold the next export; dropping the bounded response future releases its owned read. A subsequent export works, shutdown stays bounded and canonical Run/output/replay are unchanged. Owners: `telemetry/http.rs`, existing telemetry corpus and `session_run/telemetry.rs`. |

## Chosen direction

Use the existing corpus, native process helpers, Keychain fixture, Guard lifecycle
owners and HTTP adapter. Ponytail selects reuse before new code: no new crate,
dependency, trait, daemon, benchmark subsystem or canonical Event. The only
behavioral variation is the existing Linux/macOS adapter pair. Existing fixture
gates may control a demonstrated race; VR5 gates only its local Collector socket,
never the HTTP adapter control flow.

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
- `Locked` collapses multiple native causes: expose only a closed native category
  through the bounded helper response, retaining the refusal. Helper identity is
  a hypothesis until a recurrence supplies evidence; stability is not cause proof.
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
branch/PR and one observable increment. VR1–VR4 are independent and may land in
any order or in parallel; VR5 lands last for the closing checklist.
Keep this list/index unchanged in ordinary slice PRs; merged PR `Slices` lines
are the progress record under [slice delivery](../PLANNING.md#slice-delivery).

| ID | Observable delivery | Estimate | Hard ceiling |
| --- | --- | --- | --- |
| VR1 | Audited guarded no-op Command measurement beside existing Read and Edit rows | +650 / -170 = **820** | **1,200** |
| VR2 | Current-head topology stability closure; attachment repair only on reproduced busy-detach | Evidence-only **≤120**; conditional repair +220 / -90 = **310** | **600** |
| VR3 | Closed-category Locked diagnostics and native selected-item stability closure | **≤200** | **400** |
| VR4 | Permission-family recurrence check and closure; shared-owner repair only if reproduced | Provisional until R2: +650 / -230 = **880**; evidence-only **≤150** | **1,400**, provisional until R2 |
| VR5 | Malformed/incomplete Collector responses release the observer and preserve canonical outcomes; close the plan | +440 / -560 = **1,000** | **1,500** |

VR5 includes deletion of this plan and its index row in its changed-line estimate.

### VR1 — Complete the Linux operation corpus

- Inventory: `crates/arany-cli/tests/guard_runs/latency.rs`, its `latency.md`,
  `crates/arany/src/test_support/latency.rs`, the existing latency scenario in
  `docs/specs/tools.md`, and this item's wording in `NEXT_STEPS.md`.
- Extend the current row table/helper with Command. Admit and hash a real native
  no-op executable on the selected host using ordinary executable pinning; record
  its canonical identity as a prerequisite and refuse absence. Update the corpus
  notes' owner procedure to require the selected revision's native namespace
  bootstrap and host authorization ([Ubuntu namespace prerequisite](../../docs/specs/tools.md#ubuntu-namespace-entry-diagnosis));
  stock-policy refusal is failed evidence, never a host/fake-Guard fallback.
  Read/Edit alone did not exercise that prerequisite. Use explicit argv,
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

### VR2 — Check current runtime topology stability before repair

- Inventory: the existing runtime-topology `NEXT_STEPS.md` row and retained native
  evidence first; `crates/arany-cli/tests/session_run/tools_macos.rs` and
  `crates/arany/src/test_support/process.rs` only for a reproduced current defect.
- Record the post-`572b46b3` history result and the exact number of macOS debug
  jobs inspected. Repeat the complete mounted-runtime refusal/replay → positive
  detach → clean selected-runtime success journey on the same current head via
  the existing Focused CI `macos-debug` lane. Name a finite N in the PR before
  running; retain all attempts and distinguish other topology assertions.
  This is diagnostic repetition, never retry-until-green.
- If busy-detach recurs with the current bounded helper, retain bounded
  `lsof`/`hdiutil info` red evidence. Kernel-only fixture attachment matching
  production is a candidate repair to verify, not an unconditional change.
  Keep the guard established immediately after attachment and release owned
  handles before detach; re-resolve image identity and freshly confirm absence.
  Never kill an ambient `diskimage` or act on a remembered device number.
- If it does not recur, ship no source change: record stability/run links and
  close the pending row without claiming a cause. Add a held-reference regression
  only if reproduction demonstrates false absence. Failure/unwind retains the
  image/root when absence cannot be proved. Existing product contracts suffice;
  no production detach retry or deadline change. A repair requires the focused
  native debug lane again before closure; full CI still belongs to E6.

### VR3 — Add closed-category diagnostics and close native-store stability

- Inventory: `crates/arany-cli/src/accounts/credentials/keyring_helper/macos.rs`,
  `credentials/keyring_helper.rs` and its Linux helper-protocol table,
  `credentials/macos_tests.rs`, the existing Credentials/Diagnostics scenario
  wording only if needed, and the native-stores `NEXT_STEPS.md` row.
- R1 starts from the retained-history absence above and #171's already-shipped
  pinned helper copy. Preserve that immutable identity through the exactly-one-test
  synthetic child. Identity replacement causing interaction-required access is
  the candidate explanation, not a demonstrated lock or ACL defect. Current
  recurrence diagnostics supply the missing native category.
- Carry a closed native category (constant name only, never platform text, paths,
  identities or contents) through the existing bounded helper response for
  `EXIT_LOCKED`. The caller currently discards helper stderr, so stderr-only
  diagnostics would not expose recurrence. Keep `CredentialError::Locked` and
  empty secret output; admit only the closed category at the reply decoder and
  reject malformed/unknown payloads. Add the Linux helper-protocol table row for
  category propagation/refusal and canaries; no public abstraction or fallback.
- Pin the synthetic Keychain's settings explicitly with `SecKeychain::set_settings`
  (no idle lock), while retaining actual lock refusal and explicit test unlock.
  Preserve denied actor → authorized read of the same unchanged item, stale
  identity, cancellation/reap and output canaries. Never remap Locked to success,
  retry credential reads, relax ACLs or automatically unlock user storage.
- Run the existing native parent repeatedly on one current head through Focused
  CI `macos-debug`, with finite N named in the PR and every attempt retained.
  Closed-category diagnostics plus stability close R1/the pending row within
  200 changed lines; a recurrence identifies its category and triggers a plan
  revision before any broader repair. Linux retains helper-protocol evidence.
  Clipboard remains separate; human Keychain/clipboard dialogs stay deferred.

### VR4 — Check the complete permission early-stop family before repair

- Inventory: `crates/arany-cli/tests/guard_runs/permissions.rs` (shared fixture),
  `permissions/modes.rs`, `permissions/rules.rs`, `permissions/hooks.rs`,
  `permissions/host.rs` and `unguarded.rs` as affected callers; existing Guard
  failure tables in `crates/arany/src/tools/guard.rs`, and only the demonstrated
  owner in `guard/macos/unit.rs`, `storage.rs` or `fixture.rs`. Update affected
  Tool/Diagnostics scenarios and the readiness/load `NEXT_STEPS.md` row as needed.
- **R2 begins with recurrence, not a presumed repair.** The five tests/eight runs
  above precede #171; Fable reports no later execution-unconfirmed or
  storage-cleanup-unconfirmed message. Select finite N full macOS release runs
  on the same current head via Focused CI `macos-release`, named in the PR before
  running. Retain every attempt and current intent-correlated stage/check capture.
  No recurrence closes VR4 as evidence plus row removal (≤150 changed lines),
  without source changes or a claim that the historical cause is known.
- **R2 result:** N=5 sequential Focused CI `macos-release` runs at `e6d33c30b954c4e819cb220945d35b5f52fdf7fb` all succeeded with nonzero test counts and no execution- or storage-cleanup-unconfirmed failure: [40514236959](https://github.com/fpmirabile/arany/actions/runs/40514236959), [40519063160](https://github.com/fpmirabile/arany/actions/runs/40519063160), [40534470573](https://github.com/fpmirabile/arany/actions/runs/40534470573), [40538385142](https://github.com/fpmirabile/arany/actions/runs/40538385142), [40544264386](https://github.com/fpmirabile/arany/actions/runs/40544264386). The family is closed without a cause claim.
- On recurrence, bind mode/rule/operation, intent identity, GO state and first
  closed stage/check before teardown overwrites a snapshot. Reuse the bounded
  release recorder/debug stages; distinguish execution unconfirmed
  (`Guard stage=execution`) from storage cleanup unconfirmed. Name the shared
  owner: native unit release, `storage.rs::cleanup_storage_until`, or the
  `permissions.rs` fixture, as demonstrated. Fix that owner and all affected
  callers rather than individual matrix cells. The 880/1,400 estimate/ceiling are
  provisional until R2; replace the repair inventory and re-estimate before repair.
- For a demonstrated lifecycle repair, extend existing fault tables only at its
  proven boundary: connection/preparation/native-manager refusal before GO proves
  no payload; post-GO process/report loss remains Uncertain, no integration,
  no next Provider call and non-executing closed replay. Successful release
  positively empties the coalition/removes the owned job, releases handles,
  confirms detach and settles the journal before continuation. Retained uncertain
  records remain recoverable under exact ownership; a new Run never retries an
  old intent. Evidence-only closure reuses these existing owners.
- Use channels/failpoints and paused time only outside native I/O. Never increase
  deadlines, insert sleeps, retry matrix cells, quarantine or weaken assertions.
  Full release CI exercises the family; focused runs diagnose and establish the
  scoped stability result, without replacing E6 or changing shared CI.

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
- The local Collector thread sends status/headers and part of the body, then
  holds the socket. After a virtual-time deadline drops the incomplete bounded
  future, observe server-side read EOF or a closed/reset connection (EPIPE on a
  write), proving the owned read was released. A second export to the same
  Collector must succeed. Do not mistake a server timeout for client disposal.
  Add no test gate, feature flag or `cfg(test)` control-flow branch in `http.rs`.
  If needed, extract one private async bounded-send function used by the existing
  runtime wrapper and test; that is the only permitted change in that file.
  No public client abstraction, dependency or alternate HTTP implementation.
  Use Collector-side synchronization and virtual time for logical deadlines,
  bounded real socket supervision, cleared ambient proxy configuration and
  nonblocking-to-blocking accepted-socket normalization on macOS. No sleeps.
- Through the existing product export journey, compare exact exit/stdout/stderr,
  closed Events and reduced view with the failed-Collector variant; drain exports
  through shutdown and check canaries. Retain the separately ignored native
  trickle gate as native timing evidence. Update the two existing acceptance rows
  to identify their new evidence owners; no runtime contract change is expected.
- Run the closing audit after all slices merge. Obtain three sequential complete
  `ci.yml` runs on the identical final head with both native macOS profiles.
  Use sequential `workflow_dispatch` runs at an unchanged ref, or rerun the full
  exact-head workflow; wait for completion before the next to avoid concurrency
  cancellation and verify each checkout/head. Retain all attempts and failures;
  focused passes alone do not close a flake.
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
  - [ ] E3 — closed-category diagnostics, helper-protocol refusal, native stability,
    denial/authorized read, real lock refusal and explicit recovery evidence.
  - [ ] E4 — full-family recurrence result; any demonstrated repair and classified
    lifecycle fault, no forbidden continuation/publication, replay and cleanup.
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
| R1 | Native-store recurrence/stability closure; no retained failing log exists in Fable's scan. | Preserve #171's pinned helper; helper-identity replacement is a hypothesis matching the existing error chain. Add closed-category `EXIT_LOCKED` diagnostics plus Linux protocol evidence, then repeated same-head macOS debug stability; retain all links. No remap, ACL change or auto-unlock. |
| R2 | Recurrence check for the five-test/eight-run family, with execution/storage cleanup as distinct closed stages; Fable reports none after #171. | Name finite N full focused macOS release runs on one current head using existing stage/check capture. No recurrence: evidence-only closure. Recurrence: identify first stage/check and shared native-release, storage or permissions-fixture owner before repair; re-estimate VR4. Never infer load/deadline causality. |

## Exit criteria

- **E1:** the ignored Linux release corpus contains audited single Read, concurrent
  Read, admitted Edit and guarded no-op Command rows; all valid samples join to
  durable history and fresh `show`. Failure/missing acknowledgement/overflow never
  produces a success verdict. Named-host claims require retained owner runs under
  [the corpus procedure](../../crates/arany-cli/tests/guard_runs/latency.md#owner-procedure).
- **E2:** current-head focused debug evidence closes VR2 without source changes
  if busy-detach does not recur; any repair has red evidence and native rerun.
  The journey refuses a mounted runtime before GO, positively proves fixture
  detach and succeeds on a new clean Run; exit or a missing path alone is no proof.
- **E3:** R1 has closed-category helper diagnostics, Linux protocol evidence and
  repeated same-head macOS debug stability. Unauthorized access refuses, authorized
  access to the unchanged item succeeds, actual lock refuses and explicit recovery
  works without leakage. Stability does not establish the helper-identity hypothesis.
- **E4:** R2 records the full-family same-head release recurrence result; no
  recurrence permits evidence-only VR4 closure. Any repair has a demonstrated
  cause/regression at its shared owner; pre/post-GO outcomes, storage/recovery and
  closed replay stay correct. Permission expectations remain exact on both OSes.
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
