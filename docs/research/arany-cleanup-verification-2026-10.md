# Duplication and recovery cleanup verification

Status: selected local implementation and consolidation complete on 2026-10-06; release and intermittent terminal-loss evidence remain open. This record covers the validated duplication/recovery review, applied findings and subsequent Ponytail decisions. Product behavior belongs to the domain specs, physical ownership to the architecture, editing rules to `agents/`, and remaining user/platform/release checks to [next steps](../../NEXT_STEPS.md).

## Applied responsibilities

| Selected scope | Result and authoritative owner |
| --- | --- |
| Private State reads (D-S2) | Provider profiles use the existing bounded, no-follow private-record reader while retaining required-file errors. [StateRoot](../../src/store/state.rs) owns admission. |
| Bounded CLI lists (D-P9) | Model preferences, accounts, identity keys and catalog lists reuse the existing visitor inside the CLI. [CLI](../../src/cli.rs) owns this mechanism; library catalog and image byte-aware visitors keep their separate contracts. |
| Local repeated sequences (D-P8, D-S1, D-E1, D-C1, D-X2, D-X5) | Store request/reply, subscription result acceptance, idle rename/defaults lifecycle and Workspace/state overlap admission share private helpers. Close/join, locks, operation validation and failure ordering remain explicit. Existing PTY settings and buffer-row helpers are reused. |
| Provider identity (D-P5 / A-5) | Live selection and persisted Run validation share a pure borrowed value validator. Credential, evidence and Run-specific limits remain separate admission steps; [Provider](../../src/provider.rs) remains the only substitutable Engine behavior seam. |
| Native semantic protocol (D-P1 / A-6) | Outcome/summary schemas, semantic projections and common instructions have one private Provider owner. OpenAI framing, Anthropic framing, images, usage and strict decoding remain adapter-owned. Bounded JSON construction reuses the existing duplicate-key/depth/node parser. |
| HTTP protection (D-P2 / D-P3 / R-4) | Private client defaults and bounded identity-body reads are shared within each crate. Endpoint pinning, numeric-loopback HTTP, media/status checks, JWKS acceptance, secret scans, deadlines and error categories stay route-owned; SSE and OTLP retain different mechanics. |
| Local recovery (S-1, S-2, narrowed S-3, S-4, C-6) | Initial trust Escape carries ephemeral read-only access; missing/unsafe/merged-overflow inputs are admitted before draft consumption or Session materialization; partial bounded suggestions survive the traversal cap; real discovery errors remain visible; linear rejection asks for a new line and clears interruption state. [Terminal spec](../specs/terminal.md) owns behavior. |
| Unsaved title precedence (narrowed C-4) | Explicit placeholder-looking titles materialize through the existing canonical rename semantics. The reducer/listing share the placeholder predicate; no Event migration was introduced. |
| Current Tool observation encoding (A-2 / C-2) | The native OpenAI function route carries each current action/result once, with paired call IDs. Historical context remains separate. [Tool spec](../specs/tools.md) and [Provider rules](../../agents/provider.md) own that distinction. Synthetic request size decreases by 309–408 bytes for the six one-observation cases; no remote caching or billing claim follows. |
| Native quick-action picker (D-U3) | Ratatui List/ListState own row rendering/highlighting for the simple pilot. Existing action, scroll, geometry and close policies remain terminal-owned. Frames and actual draft/caret return are preserved. |
| Human output and categories (U-2, A-7, D-U6, R-3, U-14) | Collaboration/status labels use intentional human wording. Active children do not imply dispatch. Fork help exposes its optional ID. Callers select transient/history feedback explicitly, independent of wording. Guard bootstrap failures retain their original closed stage through receipt/diagnostic construction. |
| Remaining local reuse (D-S3/S4, D-E2/E3/L1/T1, D-U4/U8, D-X1, C-7, DOC-3) | Optional account-root admission and identical account-index selection, placeholder titles, identical filesystem filters and existing fixtures are reused. Unix-specific Skill corpus code is gated without claiming Windows runtime behavior. Architecture/glossary distinguish one primary loop, one-call children, total child budget versus Provider concurrency, and width-dependent terminal projection. |
| Published Crossterm | The user-selected direction uses unmodified registry 0.29.0 and its locked checksum. Only that source/checksum identity changed among 458 packages; versions and selected dependency identities did not change. Source and bundle scripts use ordinary registry source/license paths. [Terminal-loss finding](../security/findings/terminal-pty-hangup-spin.md) and [release review](../security/beta-license-release-review.md) own platform/artifact evidence. |

The changed coding projection increments the ChatGPT account-admission fingerprint to v11. Data-free read-only model/native/custom probe formats are unchanged, so their check versions are not invalidated. Saved consent, backend authority, account UUIDs, Event schemas and historical replay remain unchanged.

## Conditional work not selected

- Broader List migrations would retain or reintroduce filter, mouse-row and completion geometry. The pilot removes concrete row-rendering work; adapters for every other picker would not establish a smaller contract.
- Draft-edit dispatch sets differ at Tab/Escape and by input mode. Consent, approval and setup pages differ in grapheme/ASCII wrapping, minimum geometry, last-page actions, reset and rejection. No universal modal/editor/page abstraction is justified.
- Native saved-account defaults, nullable preference sources and resumed Run defaults have different meaning, especially current policy versus pinned historical policy. Their short conversions remain explicit. Linear Session number/UUID grammar and model-number-plus-effort grammar remain distinct; selector page/exit and dedupe ownership are retained.
- No generic resource registry, ProviderKind migration, HTTP service trait, runtime framework, Guard substitute, module/crate split or testing framework was introduced. These suggestions need an actual second adapter or measured ownership/dependency reason.
- No scheduling/lifetime optimization was selected from these local measurements. A future offload needs its own bounded work, cancellation/join and real terminal-boundary evidence; process-lived Engine/Store or pooling needs a demonstrated workload gain and security/lifetime review.

## Regression evidence

Existing owners were extended, rather than adding per-helper suites:

- The partial-discovery case fails with `ToolError::Limit` under the old behavior, and now retains 64 sorted admitted candidates from 8,193 entries.
- The unsaved-title lifecycle case fails with lost explicit provenance under the old materialization and now preserves both `New Session` and `Forked Session` through canonical replay.
- The isolated consent PTY fails when initial Escape again discards the loaded access. The fixed path reaches ordinary request admission without another trust panel, persists no trust, and admits no Run when its synthetic selected credential is unavailable. This is local recovery evidence, not a paid successful turn.
- The existing quick-action/draft PTY rejects missing includes, outside-pointing symlinks and 16 flag includes plus another mention before Session Events, retains the caret and permits insertion at that caret. Disabling local preflight makes the rejection lose its recovery suffix and consume/materialize the draft, and the corpus fails. The linear owner checks exact fresh-line feedback for invalid and missing mentions followed by an independent command.
- The existing history corpus admits wording formerly classified as transient under an explicit history category and excludes changed error-shaped wording under an explicit transient category. The Guard minimum-budget corpus checks six typed bootstrap stages through exact uncertainty receipts.
- The actual shipped native Tool journey exercises both native adapters, ordinary TLS, the separate Guard, continuation, strict outcomes and closed journal replay. The networkless ChatGPT checked-turn journey exercises saved account admission, catalog/diagnostic paths and direct/team replay.

## Local owner measurements (R-1, A-4, A-8 pooling)

Optimized Rust 1.99.0 on Linux x86-64, before selecting any scheduling/lifetime optimization. Temporary probes called the current owners directly and were removed before closeout; they created only synthetic private filesystem/SQLite and numeric-loopback HTTP data. Two warmups were excluded; Tool/snapshot paths have 20 retained samples and Store/HTTP paths 30. Two probes ran concurrently with each other, without a Cargo build; these distributions are workload-specific diagnostics, not latency promises.

| Protected boundary | p50 (ms) | p95 (ms) | maximum (ms) |
| --- | ---: | ---: | ---: |
| Tool admission: one shell command, one MCP program, one pinned Skill | 1.586 | 1.873 | 1.907 |
| Runtime assets without Workspace copy | 2.003 | 2.157 | 2.288 |
| Command/MCP snapshot: 31 one-MiB Workspace files plus those assets | 23.529 | 27.037 | 27.472 |
| Snapshot drop/cleanup | 3.162 | 3.746 | 4.201 |
| Cold read-only Store open | 0.123 | 0.176 | 0.183 |
| Cold 100-Event view read/reduction | 0.095 | 0.151 | 0.152 |
| Store close and owner-thread join | 0.031 | 0.043 | 0.050 |
| Secure HTTP client construction | 6.812 | 7.644 | 7.684 |
| Fresh numeric-loopback HTTP connection through response headers | 0.326 | 0.384 | 0.396 |

The connection sample includes scheduling, request serialization and a bounded immediate peer response; it isolates neither TLS nor DNS and is not evidence about production Provider latency. Store pages and native executable assets were warm in the OS cache; cold Store means a new connection/owner, not dropped kernel caches. A preceding Tool-only sample measured p95 1.725/2.089/24.384/3.284 ms for admission/assets/snapshot/cleanup respectively. At surrounding snapshots, load averages ranged 3.74–8.01 (one minute), CPU pressure `some avg10` 0.11–4.30 and `full avg10` zero. No before/after optimization comparison is claimed because none was applied.

Whole-process and terminal gates, consolidated checks and source/candidate outcomes follow. Passing local checks does not establish native macOS, live paid Provider, real keyring/browser, human assistive-technology or redistribution approval.


## Whole-process and terminal measurements

Named host `asustuf`, Linux x86-64 with 16 available hardware threads, optimized Rust 1.99.0. Existing release owners ran with their unchanged thresholds. Surrounding load snapshots were 5.47–6.75 (one minute), CPU pressure `some avg10` 0.61–2.58 and `full avg10` zero; no fresh source build ran during these measurements. The show measurement overlapped the end of the functional Session suite; the subsequent startup, team and terminal measurements ran sequentially.

| Boundary | Samples | p50 (ms) | p95 (ms) | Maximum (ms) | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| `show`, one Event | 1,000 | 2.184 | 2.635 | 3.264 | Diagnostic baseline |
| `show`, 100 Events | 1,000 | 2.261 | 2.718 | 3.732 | Passed 25 ms p95 |
| Process spawn through Provider accept, two 32-KiB files | 100 | 12.026 | 13.557 | 14.443 | Passed 50 ms p95 |
| Two-child whole-process Run | 100 | 18.891 | 22.785 | 23.321 | Passed 100 ms p95 |
| Active keyboard with 1,600 Runs / 65,528,000 transcript bytes | 10 | 4.038 | Not reported | 5.069 | Existing native tmux owner passed |

The near-cap terminal owner also measured 510.960 ms admission, cancellation/replay and exact terminal restoration. The startup owner initially failed its outdated raw-file-only assertion: current include projections already contain the escaped path, digest and untrusted-data label. Its expected request now checks that complete existing projection and exact file bytes. This repairs the measurement fixture; it changes neither product encoding nor a latency threshold. Event-creation timings remain diagnostics rather than substitutes for the monotonic process-to-accept gate. These synthetic timings establish no production network/Provider latency or universal responsiveness promise.

## Consolidated verification

Locked offline all-target suites pass in both development and optimized profiles, including strict replay and the affected default PTY journeys. Strict all-target Clippy passes in both profiles; formatting, shell syntax and ShellCheck pass. Cached offline `cargo deny --locked --offline check` passes advisories, bans, licenses and sources with permitted duplicate-version warnings; no fresh advisory fetch is inferred. Changed Markdown links/anchors pass review.

The explicitly activated optimized native Tool continuation, actual Guard failure corpus, synthetic checked ChatGPT direct/team journey, folder-consent journey and Linux TLS chain/name owner each report one passing parent test. The affected Tool/ChatGPT/trust owners also passed in development. Default ignored helpers/platform/live lanes remain unperformed unless explicitly listed.

One optimized concurrent Session suite observed an active lost-PTY socket-closure timeout. The isolated case, finite twelve-case/four-concurrent series, terminal group and later complete suite passed without reproducing it. The existing owner remains enabled with its original deadline and improved bounded diagnostics. These later passing samples do not establish a cause or clear the availability finding; [the terminal-loss record](../security/findings/terminal-pty-hangup-spin.md) and next steps retain it. No upstream-fix, native macOS, real credential service, paid inference, compiler-provenance or redistribution approval is claimed.

The fresh source archive selects published Crossterm and both macOS TLS-verifier graphs, preserves the exact manifest/lockfile and passes a new offline compile. Its 156 files include 154 byte-identical current inputs and two Cargo-generated metadata files; there is no vendor tree. The bundle builds from that exact extraction and passes its native dependency/link checks, then refuses the changed root-license exception set. Its current 305-archive release union needs review of four additional root-text exceptions and two new nested filename matches; the later standard-library pin also differs from the installed compiler and was not reached. [The release review](../security/beta-license-release-review.md#current-result) records the exact source hash, refusal and remaining provenance checks. No finished candidate exists from this attempt.

Final responsibility/security review preserves the SQLite owner and commit/close ordering, Run-boundary revalidation, endpoint pinning, streaming body/list bounds, duplicate-key rejection, credential/backend publication, exact Guard enforcement and joined terminal ownership. Local input preflight shares the existing reader and grants no disclosure or Tool authority. Only the agreed recovery, copy and current-observation behavior changes required spec updates; mechanical extractions leave their existing contracts adequate. Durable decisions and open checks were routed to those owners, and the completed execution plan was removed under the planning lifecycle. No Git operation, paid request, real-account/clipboard inspection, new dependency, CI change or threshold increase was performed.
