# Arany decision register

This register owns accepted architectural choices and their reasons. Current
behavior belongs to [domain specs](../specs/README.md) and the unmigrated owners
in [the documentation map](../README.md). [Architecture](../architecture/system-overview.md)
owns physical implementation; [next steps](../../NEXT_STEPS.md) owns open work.
A decision is not evidence that its implementation or release gate passed.

Keep a decision here while its reason affects future changes. Replace superseded
choices rather than appending amendments, copied contracts or verification logs.

## 1. Why SQLite remains canonical state

The detailed storage comparison and reconsideration gates live in
[persistence rationale](./agent-state-persistence.md). D-05 records the accepted
architectural consequence.

## 2. Settled decisions

### D-01 — Runtime and physical ownership

One library crate and one CLI crate in a long-lived process keep harness
interfaces separate from presentation.
A current-thread async coordinator, a dedicated SQLite owner and bounded helper
lifecycles separate waiting from blocking work. Extra crates or a daemon need a
real dependency, release, privilege, ownership or Client boundary. See
[physical architecture](../architecture/system-overview.md#2-runtime-architecture).
A dedicated SQLite owner avoids a connection pool and keeps enduring blocking
work out of Tokio's generic pool, whose started tasks cannot simply be aborted.
Source baseline: 2026-09-29, [Tokio blocking-task lifetime](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html);
this explains ownership, not measured scheduling performance.

### D-02 — Durable Session model

Arany owns conversation history across Provider changes and process exit.
Resume continues that identity; fork creates a new lineage; compaction is
rebuildable context. This avoids depending on opaque Provider continuation for
recovery. [Session spec](../specs/session.md) owns lifecycle semantics;
[context rationale](./context-memory-and-compaction.md) explains the distinction.

### D-03 — Generic bounded agent protocol

One accountable primary and bounded direct children provide useful fan-out
without recursive scheduling or final-answer ownership transfer. Ordered assignments
make synthesis independent of completion order. Single-agent work avoids
unnecessary coordination and cost. [Execution](../specs/execution.md) owns the
protocol and bounds. An Assignment DAG requires demonstrated dependencies or
ownership transfer, including attempts, leases, fencing and stale-result
rejection; more agents alone do not justify it. [Architecture](../architecture/system-overview.md#14-explicitly-deferred)
owns expansion triggers.

### D-04 — Provider seam and profiles

Provider is the substitutable Engine behavior seam because multiple real
adapters exist. Semantic types prevent vendor schemas from defining policy.
Exact custom profiles require data-free conformance: resemblance to an API does
not prove output, privacy or routing guarantees. [Provider spec](../specs/provider.md)
owns admission and current capability requirements.

The 2026-09-29 [OpenRouter routing review](https://openrouter.ai/docs/guides/routing/provider-selection)
and [response-cache review](https://openrouter.ai/docs/guides/features/response-caching)
separated executing upstream, parameter support, cache provenance and privacy.
A broker model slug cannot establish all four; that dated comparison is not
current route admission or a live conformance result.

### D-05 — Event contract and replay

One SQLite journal owns canonical facts; Session and Run views are reductions.
This keeps durable acknowledgement, causal history and replay consistent without
multiple mutable representations of the same state. Rollback journaling fits
the single-owner workload. [Execution spec](../specs/execution.md#admission-persistence-and-recovery) owns durability
and format checks; [architecture](../architecture/system-overview.md) owns schema.

### D-06 — Identity and time

Typed identities, journal sequence, display timestamps and monotonic deadlines
solve different problems. Keeping them distinct avoids ordering by wall time
or treating a display identifier as authority. Concrete standard/library types
suffice; no Clock or ID-generator seam is earned by the current flow.

### D-07 — Workspace and instruction inputs

Pinned handles and immutable bounded snapshots tie disclosure to admitted
objects. Project text cannot select credentials or grant capabilities. Bounded, handle-relative
instruction discovery keeps scope within the admitted Workspace. [Session spec](../specs/session.md)
and [security rules](../../agents/security.md) own the admission contract.
The instruction-organization source review (2026-10-12) found mixed evidence:
[Gloaguen et al., v3](https://arxiv.org/abs/2602.11988v3) and
[Lulla et al., v2](https://arxiv.org/abs/2601.20404v2) used different tasks and
reported different cost/runtime effects. Neither establishes an optimal file
layout or measured benefit for Arany; link checks establish consistency only.

### D-08 — Resource bounds

Reserve aggregate resources before admitting work so concurrency and model
proposals cannot multiply the per-call allowance. Limits change for demonstrated
needs, not speculative configurability. [Execution](../specs/execution.md),
[Tools](../specs/tools.md) and their module owners define current bounds and
overflow; this register keeps no parallel constant table.

### D-09 — CLI and Session commands

Explicit attached and machine modes make interpretation predictable for humans
and scripts. A compiled local registry prevents project/model text from becoming
command authority. [Terminal spec](../specs/terminal.md)
owns current entry modes, channels and syntax.

### D-10 — Terminal contract

Navigable history and compact conditional activity keep ordinary chat
readable while allowing team inspection. Complete keyboard access and transient
picker mouse reporting avoid imposing a permanent control room or pointer
requirement. [The terminal spec](../specs/terminal.md) owns interaction and
restoration. Source precedents reviewed on 2026-09-29:
[linear accessibility](https://code.claude.com/docs/en/accessibility),
[fullscreen and scrollback](https://code.claude.com/docs/en/fullscreen), and
[noninteractive output](https://developers.openai.com/codex/noninteractive).
They inform the choice; they do not prove Arany's accessibility.

Agent-chat sources checked 2026-10-21: [Codex navigation](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/multi_agents.rs)
provides empty-composer Alt+Left/Right and stable traversal; its
[parent-owned child views](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/session_lifecycle.rs)
are view-only. [Claude Code's agent panel](https://code.claude.com/docs/en/agent-teams#start-your-first-agent-team)
documents transcript messaging and collapsing excess idle rows. These are
separate precedents: native Option-key delivery, accessibility and scale are not
proved by source inspection. [Terminal](../specs/terminal.md#agent-chats) and
[Goals](../specs/goals.md#steering-notes-and-pause) own Arany's current interaction.

### D-11 — Configuration, credentials, and custom endpoints

Accounts belong to the current OS user across Sessions and StateRoots; a
Session override must not create another billing identity. Trusted selection
precedes secret access and project input. OS stores have distinct availability
and confidentiality limits, so fallback must be explicit. [Credentials](../specs/credentials.md)
owns scope, storage and recovery; [Provider spec](../specs/provider.md)
owns destination admission.

The 2026-10-06 [Secret Service prompt review](https://specifications.freedesktop.org/secret-service/latest/prompts.html)
found that disconnect invalidates prompts but races completion. Killing a
credential helper therefore bounds waiting without proving rollback; an
uncertain native write cannot justify silently saving a second copy to a file.
This is protocol rationale, not a diagnosis of a particular native stall.

### D-12 — Cancellation, errors, and retries

Local cancellation preserves an honest durable prefix; it cannot undo remote
usage or an already dispatched effect. Automatic retries and fallback would add
billable attempts and recovery ambiguity. [Execution](../specs/execution.md),
[Tools](../specs/tools.md) and [Provider spec](../specs/provider.md) own the
applicable outcomes and retry policy.

The [transactional-outbox comparison](https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html)
(reviewed 2026-09-28) explains the dual-write problem: a local commit and an
external effect are not one atomic transaction. A crash after the effect can
leave uncertainty. This rationale does not introduce an outbox or retry worker.

### D-13 — Dependency boundary

Use reviewed libraries for actual capabilities without adding pools, generic
configuration, dynamic plugins or forwarding traits. A dependency's default
retry, routing or logging policy cannot become harness policy implicitly.
[Cargo inputs](../../Cargo.toml) and the [dependency inventory](../security/beta-dependency-inventory.md)
own the current set and review scope.

### D-14 — OTLP is optional lossy output

Traces describe timing and causality; they are not durable history. A local
Collector keeps remote authentication and routing outside the harness. Bounded,
content-free export protects Run correctness and privacy even when delivery
fails. [Telemetry spec](../specs/telemetry.md) owns enforcement;
HTTP/protobuf reuses the HTTP stack without a second gRPC stack. The SDK batch
worker and synchronous shutdown need an owned reactor and lifecycle outside the
current-thread Engine coordinator, so shutdown cannot starve its own requests.
Source baseline: 2026-09-29, [SDK 0.33.0 batch processor](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/trace/struct.BatchSpanProcessor.html)
and [OTLP transport](https://opentelemetry.io/docs/specs/otlp/). That source review
is not a current dependency version or delivery claim.

### D-15 — Security boundary

Establish authority before consuming untrusted input. Model proposals require
deterministic Policy and separately enforced effects; instructions alone cannot
provide containment. [Security rules](../../agents/security.md) and
[the Tool contract](../specs/tools.md) own current requirements. Incident evidence
remains in [security research](./harness-security-lessons-and-controls.md).

The macOS executable catalogue uses one union of exact literal paths to avoid
repeating the same operation rule for each admitted program. This keeps the
intended allow set narrow; directory-wide grants would broaden it. Fresh file
revalidation and independent denies remain governed by
[Tool spec](../specs/tools.md).

Source syntax reviewed on 2026-10-12: Apple's shipped
`/System/Library/Sandbox/Profiles/application.sb` and
`com.apple.storereceiptinstaller.sb` in the same directory (macOS 26.6.2,
build 25G83), and the
[WebKit WebProcess profile](https://raw.githubusercontent.com/WebKit/WebKit/main/Source/WebKit/WebProcess/com.apple.WebProcess.sb.in)
compose literal filters with `require-any`. The WebKit link tracks mutable
`main`. These examples support union composition, not a stable public DSL,
compiler performance or the timeout's cause. Native fixtures own enforcement
evidence.

The macOS resource supervisor uses physical footprint because resident size
alone can miss compressed or swapped memory. Signature verdict reuse is bound
to executable stamps within one process; it does not cache authority across
processes. The [Tool contract](../specs/tools.md) owns enforcement and bounds.
A recurrence of the macOS stopped-command timeout is a new defect: capture its
stages through the fixture before changing budgets.

### D-16 — Evidence-dense testing

Exercise real Engine, filesystem and SQLite behavior with only Provider scripted.
Reuse a scenario or corpus when it proves the same claim; add another layer only
for distinct process, native or durability evidence. This catches integrated
failures without maximizing test count. [Testing rules](../../agents/testing.md)
own procedures; [testing rationale](./testing-strategy-for-rust-cli-harness.md)
owns the trade-offs.

### D-17 — Product identity, platforms, and license

The product and executable are Arany and `arany`. Arany targets Linux and
macOS; native Windows is out of scope and Windows users run the Linux build
under WSL2 (decided 2026-10-13: one Unix enforcement family keeps the Guard
and its evidence tractable). Native evidence determines support claims,
including for WSL2. Apache-2.0
plus NOTICE supplies standard attribution and patent terms without a custom
license. An MIT alternative would let recipients bypass Apache-specific NOTICE
and changed-file obligations. It does not require private users or hosted
services to display credit.
[LICENSE](../../LICENSE), [NOTICE](../../NOTICE) and [project priorities](../../AGENTS.md)
own the current obligations.

The [Apache license](https://www.apache.org/licenses/LICENSE-2.0) and
[official FAQ](https://www.apache.org/foundation/license-faq.html), reviewed on
2026-09-29, retain the source basis for this choice.

### D-18 — Explicitly accepted ChatGPT-plan route is beta scope

The user accepted a distinct consented plan route with a weaker remote cost
guarantee: local stream/time limits cannot enforce a remote output-token or
plan-usage cap. The benefit is eligible plan access; the cost is that explicitly
accepted limitation. It does not weaken API-key/custom guarantees or authorize
billing fallback. [ChatGPT spec](../specs/chatgpt.md) owns account-bound
consent and transport; the [subscription source review](./subscription-provider-routes-2026-10.md)
retains the dated provider-policy evidence.

### D-19 — Personal beta prioritizes Engine flows and user-owned live testing

Prioritize a usable local harness and genuine account-backed work over packaging
or further latency tuning. Keep automated security/correctness checks; separately
authorized paid calls and human UX checks remain user-owned evidence, not silent
release clearance. Linux-first milestone ordering does not prevent native macOS
work or imply parity. Anthropic subscription integration remains conditional on
provider authorization or an admitted published route; API-key billing is a
separate choice. [Next steps](../../NEXT_STEPS.md) owns current delivery and gaps.

### D-20 — Mixed saved access is chosen on every new start

A repeated explicit billing choice avoids inferring intent from timestamps,
Session history or whichever credential is found first. Checked record presence
is a routing hint, not authorization. This costs one interaction and avoids
another secret-adjacent preference record. [The credential contract](../specs/credentials.md#backend-selection-and-startup)
owns the selection and validation behavior.

### D-21 — Checkpoint storage

Claude Code supplies the Run/prompt checkpoint shape and retention precedent;
Cline/Roo supply the intended restore choices and compare surface. Arany uses
content-addressed private storage instead of shadow Git because it already has
exact validated changesets, and its Git operations require a neutralized private
view. [The Tool contract](../specs/tools.md#checkpoints-and-rewind) owns capture
behavior, retention and storage limits.

### D-22 — Pre-beta history below the migration floor

The owner accepted archiving pre-floor history during pre-beta on 2026-10-21.
Some past format changes introduced facts that cannot be reconstructed honestly;
preserving that history intact in an archive avoids fabricating replay facts.
[The execution contract](../specs/execution.md#journal-migration) owns the floor
and migration behavior. The [migration owner](../../crates/arany/src/store/migrate.rs)
implements registered upgrades and pre-floor archival; this source observation
is not native crash-recovery or release evidence.

### D-23 — Pasted text and continuation inputs

Owner decision, 2026-10-22: follow Codex's scalar threshold, duplicate labels and inline expansion, with process-local unsubmitted chips and canonical submitted text. Arany retains its smaller objective and journal bounds; collapse cannot save input tokens. The pinned [Codex composer](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/tui/src/bottom_pane/chat_composer/paste_input.rs#L152-L193) supplies the presentation precedent. In-process inert inspection avoids introducing a separately admitted external editor.

Like Claude Code and Codex replaying history without re-expanding old message text, Goal continuation and ordinary resume reuse admitted inputs. Parsing stored objectives would turn literal pasted `@` text into new file disclosure. The journal previously stored only include digests, which cannot recover paths, so a minimal nullable admitted-path list and registered migration are required. Unknown legacy paths remain unknown; guessing from text would violate honest uncertainty and per-effect isolation. [Goals](../specs/goals.md#budget-continuation), [Terminal](../specs/terminal.md#paste-and-attachments) and [Execution](../specs/execution.md#journal-migration) own behavior and limits.

### Rejected layering

Retained from the 2026-10-04 architecture review (its report is folded into this register):

- Keep profile admission before Workspace access and strict replay validation:
  they defend different boundaries. Revisit only demonstrated invariant drift.
- Keep concrete Store errors and one-library semantic type references. Useful
  SQLite causes do not require a backend-neutral interface unless a caller needs
  backend-neutral recovery or another implementation exists.
- Defer a broader typed-notice interface until another demonstrated feedback
  lifetime mismatch or an accepted presentation redesign requires it.
- Reject traits per dependency, crates per architecture circle, runtime plugins
  and file-length splits. Concrete substitution, isolation, release or ownership
  pressure must justify a new boundary.

## 3. Expansion decisions

Reconsider a boundary only for its named constraint. Subject reports retain
conditions for a second Client, recursive supervision, cross-Session Memory,
retrieval, Artifacts and storage optimizations. [The research index](./README.md)
locates them. An extension requires a concrete accepted contract and plan; this
register does not maintain a second deferred-feature or execution checklist.
