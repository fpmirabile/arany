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

One Cargo package and one long-lived process keep internal interfaces private.
A current-thread async coordinator, a dedicated SQLite owner and bounded helper
lifecycles separate waiting from blocking work. Extra crates or a daemon need a
real dependency, release, privilege, ownership or Client boundary. See
[runtime rationale](./rust-foundation-and-engine-contract.md) and
[physical architecture](../architecture/system-overview.md).

### D-02 — Durable Session model

Arany owns conversation history across Provider changes and process exit.
Resume continues that identity; fork creates a new lineage; compaction is
rebuildable context. This avoids depending on opaque Provider continuation for
recovery. [Session rules](../../agents/session.md) own lifecycle semantics;
[context rationale](./context-memory-and-compaction.md) explains the distinction.

### D-03 — Generic bounded agent protocol

One accountable primary and direct read-only children provide useful fan-out
without recursive scheduling, leases or ownership transfer. Ordered assignments
make synthesis independent of completion order. Single-agent work avoids
unnecessary coordination and cost. [Execution](../specs/execution.md) owns the
protocol and bounds; [supervision rationale](./multi-agent-loop-and-user-feedback.md)
owns alternatives and expansion conditions.

### D-04 — Provider seam and profiles

Provider is the substitutable Engine behavior seam because multiple real
adapters exist. Semantic types prevent vendor schemas from defining policy.
Exact custom profiles require data-free conformance: resemblance to an API does
not prove output, privacy or routing guarantees. [Provider rules](../../agents/provider.md)
own admission and current capability requirements.

### D-05 — Event contract and replay

One SQLite journal owns canonical facts; Session and Run views are reductions.
This keeps durable acknowledgement, causal history and replay consistent without
multiple mutable representations of the same state. Rollback journaling fits
the single-owner workload. [Store rules](../../agents/store.md) own durability
and format checks; [architecture](../architecture/system-overview.md) owns schema.

### D-06 — Identity and time

Typed identities, journal sequence, display timestamps and monotonic deadlines
solve different problems. Keeping them distinct avoids ordering by wall time
or treating a display identifier as authority. Concrete standard/library types
suffice; no Clock or ID-generator seam is earned by the current flow.

### D-07 — Workspace and instruction inputs

Pinned handles and immutable bounded snapshots tie disclosure to admitted
objects. Project text cannot select credentials or grant capabilities. Exact
root instructions avoid implicit discovery and expansion. [Session input rules](../../agents/session.md)
and [security rules](../../agents/security.md) own the admission contract.

### D-08 — Resource bounds

Reserve aggregate resources before admitting work so concurrency and model
proposals cannot multiply the per-call allowance. Limits change for demonstrated
needs, not speculative configurability. [Execution](../specs/execution.md),
[Tools](../specs/tools.md) and their module owners define current bounds and
overflow; this register keeps no parallel constant table.

### D-09 — CLI and Session commands

Explicit attached and machine modes make interpretation predictable for humans
and scripts. A compiled local registry prevents project/model text from becoming
command authority. [Terminal](../specs/terminal.md) and [CLI rules](../../agents/cli.md)
own current entry modes, channels and syntax.

### D-10 — Terminal contract

Primary-screen history and compact conditional activity keep ordinary chat
readable while allowing team inspection. Complete keyboard access and transient
picker mouse reporting avoid imposing a permanent control room or pointer
requirement. [The terminal spec](../specs/terminal.md) owns interaction and
restoration. Source precedents reviewed on 2026-09-29:
[linear accessibility](https://code.claude.com/docs/en/accessibility),
[fullscreen and scrollback](https://code.claude.com/docs/en/fullscreen), and
[noninteractive output](https://developers.openai.com/codex/noninteractive).
They inform the choice; they do not prove Arany's accessibility.

### D-11 — Configuration, credentials, and custom endpoints

Accounts belong to the current OS user across Sessions and StateRoots; a
Session override must not create another billing identity. Trusted selection
precedes secret access and project input. OS stores have distinct availability
and confidentiality limits, so fallback must be explicit. [Credentials](../specs/credentials.md)
owns scope, storage and recovery; [Provider rules](../../agents/provider.md)
own destination admission.

### D-12 — Cancellation, errors, and retries

Local cancellation preserves an honest durable prefix; it cannot undo remote
usage or an already dispatched effect. Automatic retries and fallback would add
billable attempts and recovery ambiguity. [Execution](../specs/execution.md),
[Tools](../specs/tools.md) and [Provider rules](../../agents/provider.md) own the
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
fails. [Telemetry rules](../../agents/telemetry.md) own enforcement;
[OTLP rationale](./otlp-observability.md) explains the transport and SDK choices.

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
[Guard rules](../../agents/guard.md).

Source syntax reviewed on 2026-10-12: Apple's shipped
`/System/Library/Sandbox/Profiles/application.sb` and
`com.apple.storereceiptinstaller.sb` in the same directory (macOS 26.6.2,
build 25G83), and the
[WebKit WebProcess profile](https://raw.githubusercontent.com/WebKit/WebKit/main/Source/WebKit/WebProcess/com.apple.WebProcess.sb.in)
compose literal filters with `require-any`. The WebKit link tracks mutable
`main`. These examples support union composition, not a stable public DSL,
compiler performance or the timeout's cause. Native fixtures own enforcement
evidence.

Fixture capture on 2026-10-13 attributed the stopped-command timeout: under
host memory pressure the over-limit `receipt-loss` payload stayed below the
512 MiB resident-size threshold, so supervision never stopped it and the case
reached its 30-second deadline in `controller_receipt`. Supervision now bounds
`ri_phys_footprint`, which counts compressed memory whether held in RAM or
swapped out. The same
capture showed the Security signature scan costing about 1.3 seconds of every
command-capable Run, and up to 5 seconds under parallel load; stamp-bound
in-process verdict reuse removes the repeated scan without caching authority
across processes. Earlier `mcp-version` timeouts were captured with most time
before Ready, where that scan ran; after both changes the case completed in
about 4 seconds across three `arany_ci` suites and twelve journeys run four at a
time. A recurrence is a new defect; capture its stages through the
[fixture](../../agents/guard-fixture.md) before changing budgets.

### D-16 — Evidence-dense testing

Exercise real Engine, filesystem and SQLite behavior with only Provider scripted.
Reuse a scenario or corpus when it proves the same claim; add another layer only
for distinct process, native or durability evidence. This catches integrated
failures without maximizing test count. [Testing rules](../../agents/testing.md)
own procedures; [testing rationale](./testing-strategy-for-rust-cli-harness.md)
owns the trade-offs.

### D-17 — Product identity, platforms, and license

The product and executable are Arany and `arany`. Linux-first delivery does not
change the OS parity goal; native evidence determines support claims. Apache-2.0
plus NOTICE supplies standard attribution and patent terms without a custom
license. It does not require private users or hosted services to display credit.
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
billing fallback. [ChatGPT rules](../../agents/chatgpt.md) own account-bound
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
another secret-adjacent preference record. [The credential contract](../specs/credentials.md)
owns the selection and validation behavior; [the scoped finding](../security/findings/ambiguous-saved-access-selection.md)
retains the failure context.

## 3. Expansion decisions

Reconsider a boundary only for its named constraint. Subject reports retain
conditions for a second Client, recursive supervision, cross-Session Memory,
retrieval, Artifacts and storage optimizations. [The research index](./README.md)
locates them. An extension requires a concrete accepted contract and plan; this
register does not maintain a second deferred-feature or execution checklist.
