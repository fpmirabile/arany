# Arany next steps

This file contains unresolved defects, deferred functionality and verification
still required for the personal Linux beta and broader release. Contracts live
in [domain specs](./docs/specs/README.md); setup instructions live in
[the setup guide](./docs/setup.md) and [local tools](./docs/tools.md). Remove an
item when its completion condition is met; completed work belongs in Git/CI
history. Implementation status and verified support are separate claims.

## Next functional priority

Arany is a harness for multi-agent coding teams that can be trusted to work autonomously; its [non-negotiable guarantees](./AGENTS.md#non-negotiable-guarantees) bound every item below. Every item below either makes a non-negotiable guarantee cheaper, exposes it to more callers, or builds the team on top of it. Nothing below weakens a guarantee. Loop shape, wire formats, defaults, permission modes, hooks, agent definitions and embedding surfaces copy the named peer mechanism. The [initiative plan](./planning/agentic-teams/README.md) defines the remaining C3 and related team scope; current behavior is owned by the linked specs.

### Pursue, in order

1. **Run probe P1** (owner, live; see [owner-run live probes](#message-native-provider-request-owner-run-live-probes)). Until P1 passes, the Anthropic native route stays "offline-tested, unvalidated live".
2. **In-process read-only Tools: named-host performance.** Measure Read p95 <= 50 ms on named hosts; no hosted test gates this latency.
3. Reserved (attached client integration is done).
4. **Spec diet** (process, owner-approved). Done: release and Tool review ledgers are collapsed and `agents/` holds working practice only. Keep new behavior facts in their single owning spec.
5. **Ledger diet.** Done: the [Goal contract](./docs/specs/goals.md#lifecycle-budgets-and-recovery) owns action headroom and crash ceilings.
6. Reserved (hook seam is done; the [Hook execution contract](./docs/specs/tools.md#hook-execution) owns it).
7. **Sandbox escape hatch with approval.** Remaining: owner-run live attached journey and a macOS run of the unguarded tests; private-state `engine_run` and `guard_runs` evidence outside the development sandbox; a rule remover in `/permissions` and `setrlimit` limits only if dogfooding needs them. Spec owner: [Tools spec](./docs/specs/tools.md#unguarded-re-run-after-a-guard-failure). The hook-origin interlock rule lives in the [Hook execution contract](./docs/specs/tools.md#hook-execution).
8. **`arany serve --stdio` follow-up.** The [serve contract](./docs/specs/serve.md) owns the implemented wire and loop; only the deferred extraction below remains.
   - **Conditional `arany-journal` crate extraction stays deferred.** Schema export and the Server remain binary-only; re-evaluate extraction for an external SDK when measured dependency, ownership or release pressure justifies it. Resolve `tools::Cancellation` runtime ownership before any dependency-free vocabulary split. Preserve the library dependency boundary.
9. **Provider roster: separately deferred D work.** Judge-only adapters and a durable per-agent pause/retry/switch lifecycle after exhausted routes need accepted contracts first; current fail-and-drain recovery with explicit new-Run resume stays authoritative ([Provider contract](./docs/specs/provider.md#named-roster-presets)). Unverified: macOS attached roster evidence (manifest and Ctrl+C cancellation) and macOS saved-account roster evidence, because those roster tests are Linux-only; live account/Provider and human terminal support retain the user-owned checks below.
10. **Multi-call steps (Stage D)** after probe P2; copy Anthropic parallel `tool_use` with the journal's four-call cap.
11. Reserved (C2 edit-only workers are done; [Execution](./docs/specs/execution.md#worker-journal-protocol) and [Session](./docs/specs/session.md#worker-identity-and-explicit-reassignment) own the behavior).
12. **C3 command-capable views and the Supervisor as hook adapters.** Remaining: C3-3 (live serialized worker commands, delegated hooks and the in-view answer gate continuation loop). Keep the implemented internal private Guard command target and pinned shared-read, discard-only cache gated until model-facing runtime admission is implemented. The [Supervisor adapter](./docs/specs/tools.md#supervisor-adapter) owns the pure shadow evaluator and replay contract. Native Linux/macOS private-state Engine journeys, a native-provider product OTLP journey of shadow verdicts (exact custom profiles reject Tools) and the 1.5 MiB Guard stack canary need final-head CI evidence; the local sandbox cannot admit private State. Manual screen-reader inspection of shadow verdicts and native macOS terminal/OTLP journeys remain unverified. The `supervisor_corpus` show journey (privacy canaries, export stability/unlinkability and locked refusal) requires an outside-sandbox rerun: local private-state admission returns `StateNotPrivate`. Dogfooding calibration is owner-run: retain local corpus exports from at least 30 Sessions, label at least 200 evaluation points with at least 20 true positives per proposed signal, and sweep thresholds offline. Before enabling a signal, the owner must accept its thresholds and demonstrate precision at least 0.9 for pause/revoke or 0.75 for require-approval; otherwise it stays shadow. Corpus arithmetic and synthetic journeys do not establish that calibration or native support. Signal enforcement and overrides are a separate follow-up, gated on the accepted calibration criteria; shadow candidates grant no authority.

Dogfooding starts after item 2; the owner leads it when that point is reached. Every later item is exercised through real use and supplies the Supervisor's calibration data.

Teams support read-only researchers (C1) and Run-scoped edit-only workers (C2) with explicit fresh-assignment ID resume ([Session](./docs/specs/session.md#worker-identity-and-explicit-reassignment)). Native build evidence and named-host measurements still precede Build-profile support claims (finite approved maxima: 4 GiB memory, 8 GiB requested cache, 1 GiB project storage, 256 tasks; effective cache capacity derives from StateRoot headroom and is not guaranteed). Remote MCP/OAuth, interactive/background jobs and other network grants need their own Policy/Guard/protocol evidence; retain the existing [Tool scope](./docs/specs/tools.md) until each slice lands.

### Frozen areas

The owner lifted this freeze for item 12 (C3 views and the Supervisor in shadow mode), accepting its plan; it stays in force for the rest of the list below until the owner lifts it.

CI stays green; no new features, spec sections or evidence gates for: terminal views and tmux evidence beyond dogfooding; macOS Guard features beyond parity probes; build cache; ChatGPT OAuth and subscription routes; A6 registry fetch; the Anthropic consumer-subscription gate; new typed Tools other than the in-process read set.

- **Deferred Anthropic consumer subscriptions.** Native Claude-plan access is missing and user-deferred behind the [route/authorization gate](./docs/research/subscription-provider-routes-2026-10.md#decision-boundary-for-beta-1). API-key billing is a separate route; another harness's implementation or credentials do not authorize Arany access.

### Copy or reuse (peer -> mechanism)

- Claude Code -> hook protocol, permission modes and rule syntax, subagent frontmatter and nesting/resume model, `--max-turns`/`--max-budget-usd` names, `system/init` capabilities array, cheap first-pass tool-output truncation before summarizing, `--autocompact` threshold setting.
- Codex CLI -> app-server method vocabulary and NDJSON framing, `generate-ts` schema export, sandbox preset names `read_only|workspace_write|full_access`, encrypted-reasoning replay (done), `agent-roles` naming for the roster.
- OpenCode -> TUI as a client of the server; per-agent `model`; session fork/abort/revert endpoints; OpenAPI at `/doc` when the HTTP stage comes.
- Goose -> `acp` subprocess adapter and `serve` loopback auth (`X-Secret-Key`, explicit allowed origins).
- OpenHands -> sandbox-on-demand workspace adapter shape for a future container-backed Guard; `RemoteConversation` as the model for a remote client.
- Aider -> repo map as a bounded, digest-pinned include (later).
- Cline/Roo -> Restore files / conversation / both, with per-file Compare in the preview.

## Architecture follow-ups

Found by the macOS architecture review after A5b; no behavior change is
claimed until each lands. Keep the existing freeze for macOS
Guard internals until the owner lifts it.

- **Native Git profile probe hardening (needs a Mac).** Add a service-specific Mach-lookup denial with an unsandboxed success baseline to the Seatbelt Git probe. A direct lookup needs new test-only `unsafe` FFI, which requires the dedicated review in [AGENTS.md](./AGENTS.md#global-rules) rule 7.

## Terminal continuity evidence

- On disposable projects, the owner runs native Linux/macOS capture and guarded rewind journeys (including APFS case aliases and the 1.5 MiB Guard stack canary), private-state Store/Engine and rewind crash-prefix gates, and `rewind_serve_guarded_corpus_restores_refuses_drift_keeps_overwrites_and_blocks_uncertain`. Local private-state failures require outside-sandbox verification. With explicit live usage consent, exercise `/rewind`, Esc Esc, code/conversation/both, Compare, both drift choices and partial/Uncertain receipts after real edits. Review those panels at narrow widths in Classic/Fullscreen on macOS and Linux, and with real screen readers. These native, live and assistive-technology claims remain unverified; compilation and synthetic frames establish none of them. Contracts: [Terminal](./docs/specs/terminal.md#rewind), [Serve](./docs/specs/serve.md#rewind), [Tools](./docs/specs/tools.md#checkpoints-and-rewind).
- Exercise classic scrollback, narrow resize/reflow, temporary details, idle `/settings` chat-view transitions and a 200-line streamed response in a real macOS emulator and native Linux terminals, including under WSL2. Synthetic macOS PTYs prove controlled cursor ownership, transcript insertion and restoration; they do not emulate scrollback or prove other OS support.
- With explicit usage consent, repeat OpenAI, Anthropic, ChatGPT and exact-custom streaming conformance against actual selected accounts/endpoints. Offline fixtures prove protocol/lifecycle behavior, not live compatibility. Keep incomplete answers outside future context and confirm recovery after cancellation/crash.
- Manual lane: **macOS attached steering and pause** remains unverified; verify note delivery to synthesis, controls while a note waits and `/goal pause` at the next boundary in Classic and Fullscreen. Linux owners run in `Linux Tests (debug)`, `Linux Tests (release)`, `Native Linux Guard and tmux (x86_64)` and `Native Linux Guard and tmux (arm64)`.
- Check `--screen-reader`, `/transcript`, monochrome labels and managed following with real assistive technology. These usability claims remain unverified.

## Message-native Provider request: owner-run live probes

- **P1: Anthropic native Tools and text answers.** With explicit usage consent, send the full non-strict Tool catalog, optional `arany_plan` and phase-allowed Delegate, without `output_config.format` for Runs. Confirm auto selection with parallel Tools disabled, commentary followed by one Tool, an `end_turn` plain text answer, replayed thinking, and a spent researcher answering with Tool choice `none`. Measure required-nullable field compliance and cache reads across planning/synthesis with the existing breakpoint layout. Offline corpora prove only encoding and accounting.

- **P2: Anthropic parallel Tool use.** Before enabling more than one Tool call per step, confirm with `disable_parallel_tool_use: false` that the model respects the four-call cap in practice; an over-cap response fails the Run without retry.
- **P3: ChatGPT/Codex constant-list `allowed_tools` cache optimization.** ChatGPT already uses `tool_choice: "auto"` with phase-dependent definitions, independent of this probe. With explicit usage consent, confirm the constant function list with `allowed_tools` mode `auto` across planning and synthesis, and `tool_choice: "none"` for a spent researcher, before enabling that optimization. Confirm the Session UUIDv7 cache key and eligible cache-read usage. Offline tests do not establish live acceptance.

## Beta 1: final user-owned checks

Perform these after implementation handoff with deliberate consent for usage.
Missing real accounts, OS services, toolchains or another machine do not pause
unaffected implementation. Synthetic tests do not authorize live calls or access
to the user's credential store or clipboard.

1. **Broader ChatGPT workflows.** For other accounts/models, verify sign-in,
   visible model/effort, a direct task, follow-up, team and cancellation with
   closed Session inspection. Review the selected billing route and local-only
   output bound; cancellation does not guarantee remote work or usage stopped.
   Follow [setup](./docs/setup.md) and [execution](./docs/specs/execution.md).
2. **Credential lifecycle.** Verify setup, restart/reuse, replacement, reconnect,
   refresh and logout with the intended Linux backend, including interruption
   during submission/compaction admission. Check retained drafts, no pre-Run
   receipt/retry, uncertainty and pinned-backend refusal without silent downgrade.
   Private-file storage remains explicitly consented plaintext. The
   [credential contract](./docs/specs/credentials.md) owns expected outcomes.
3. **Trust and approvals.** Verify folder prompt/navigation/cancellation,
   remembered consent, file/folder mentions and all approval modes. Test exact
   Allow once/Deny and command review on a disposable project; highlighting alone
   executes nothing. Evaluate real-model Tool encoding and AI-review quality,
   including resumed/delegated edits. Follow [Tools](./docs/specs/tools.md).
4. **Terminal and accessibility.** Review long/multiline drafts, slash/Skill
   tabs, settings persistence, model/effort selection, history, narrow/colorless
   layouts, VS Code's first message under its sticky header, theme contrast and
   VoiceOver/Orca. During work, compaction and slow catalog refresh, edit, filter,
   change reasoning, cancel, resize/suspend and recover the same draft/caret.
   Verify no queued task or implicit retry. The [terminal contract](./docs/specs/terminal.md)
   owns the interaction scenarios.
5. **API-key and custom routes.** With separate usage authorization, verify
   OpenAI/Anthropic direct/team compatibility and explicit Anthropic workspace
   selection. A configured custom route requires exact endpoint/model conformance
   before Workspace disclosure. These checks can incur API charges and inherit
   no ChatGPT-plan compatibility claim. Follow [Provider spec](./docs/specs/provider.md).
6. **Clipboard and model vision.** Verify framed emulator paste and explicit
   text/PNG reads on the intended Linux terminal, retained editing, active-work
   behavior, modal discard and rejection without submission. Test native
   Wayland/X11 service interoperability and deliberately authorized image-only/
   text-plus-image Runs with closed replay. Confirm metadata-only human history,
   private image-bearing JSONL and lossy pixel compaction. Follow
   [Terminal spec](./docs/specs/terminal.md).
7. **Real coding workflow.** Review private grants on a disposable small project,
   then inspect/edit, run an offline check, load pinned Skill data and call local
   MCP. Inspect files and reopened observations; command changes publish only through
   a matching validated changeset receipt, MCP writes stay private, uncertainty
   stops continuation and no action retries itself. On Linux and macOS (Command Line Tools Git), also inspect typed Git status, diff and log on a repository with local changes.
   Validate the chosen real model's strict Tool encoding before claiming support. Include real-model agent-definition selection and explicit ID reassignment, confirming fresh authority/base/lease, source lineage and no historical effect re-execution.
8. **Native macOS stores and clipboard.** On a macOS account, verify setup,
   restart/reuse, replacement, refresh and logout through the login Keychain,
   including Allow/Always Allow/Deny and cancellation at the selected-item
   startup authorization dialog; Allow Once may prompt again because the OS owns
   per-item persistence. Read text and PNG from the
   general pasteboard, then confirm rejection of other images and oversized
   content without submission. Follow the [credential contract](./docs/specs/credentials.md)
   and [Terminal spec](./docs/specs/terminal.md).

## Beta 2 and broader release

| Pending gate | Completion condition and owner |
| --- | --- |
| Hosted CI and native Linux regression checks | `Clippy`, `Linux Tests (debug)`, `Linux Tests (release)`, `macOS Tests (debug)` and `macOS Tests (release)` must pass on the published head. Native consent, terminal, setup, Session picker, pinned-Skill and Tool owners include private-mount shared-source success, nested-mount refusal and no late propagation; command descendant termination before create/modify/delete publication; rejection, approval, cancellation and closed replay. `Native Linux Guard and tmux (x86_64)` and `Native Linux Guard and tmux (arm64)` (`linux_native_advanced=true`) separately own the shipped systemd Guard journey; both remain pending. C2 parser/private-target execution, foreground/background cancellation, exact-ID recovery containment/disposal, no-host-execution and full channel/telemetry canaries on the final head remain unverified until these lanes run. Re-run the native Linux and macOS lanes after the workspace split, covering reservation overhead, admitted in-process reads, guarded mutations, same-binary Guard and credential-helper entry points, terminal journeys and closed replay; code motion and synthetic runs alone do not establish native parity. [Workflow](./.github/workflows/ci.yml), [testing rules](./agents/testing.md) |
| macOS runtime mount topology | `macOS Tests (debug)` owns mounted-image refusal and selected-runtime commands under Seatbelt. Manual lane: **macOS product Ready/GO mount injection** remains unverified; adapter topology and guarded unit commands do not prove lifetime mount isolation. Busy-detach cleanup in `crates/arany-cli/tests/session_run/tools_macos.rs::guarded_runtime_profile_checks_mount_topology_before_dispatch` remains unresolved: failed detach now captures bounded, failure-only `lsof` diagnostics for the mount and backing image, with current-user visibility only; one best-effort forced cleanup runs after diagnostics are captured; it is not a retry of the test. Identify the owner natively without retries; repeated passing macOS CI is required before claiming the flake fixed. [Tools spec](./docs/specs/tools.md) |
| Native build profile coverage | Manual lane: **Linux and macOS offline builds** after A3/A4, on named hosts. Trace macOS `cc`/`xcrun` delegation to `/Library/Developer/CommandLineTools` and the selected SDK; grant only measured exact runtime roots. [Tools spec](./docs/specs/tools.md), [native evidence rules](./agents/references/testing-details.md#native-and-live-evidence) |
| macOS optimized product/account isolation | Manual lane: **macOS optimized account isolation**, on a disposable effective-user home, including full Engine/active-CLI, setup/refresh/logout and cross-user gates, plus optimized macOS file/command/Skill/MCP Tool journeys after the workspace split. Changing HOME, debug fixtures and override refusal do not prove account isolation. [Fixture rules](./agents/references/testing-details.md#host-and-account-isolation) |
| Native TLS to compiled endpoints | Manual lane: **compiled-endpoint trusted HTTPS**, with isolated resolution/routing and ordinary trust. Generated-anchor chain/name rejection does not prove a public trusted chain or production-endpoint success. [Security rules](./agents/security.md) |
| Native terminal faults and tmux | `Native Linux Guard and tmux (x86_64)` and `Native Linux Guard and tmux (arm64)` own composer/replay, active shelf and Classic/Fullscreen steering and pause; both remain pending. The Fullscreen parent cannot inspect cursor visibility in a dead pane. Manual lane: **native terminal faults and restoration**, including remaining tmux, acquisition/output/input-loss owners, optimized macOS, supported architectures and the exact distributed binary; real Fullscreen Ctrl+C/signal cursor restoration remains unverified. [Terminal fixture rules](./agents/references/testing-details.md#terminal-process-fixtures), [PTY finding](./docs/security/findings/terminal-pty-hangup-spin.md) |
| Native stores and clipboard | `macOS Tests (debug)` owns isolated debug Keychain fixtures; `Linux Tests (debug)` and `Linux Tests (release)` own slow-authorization and uncertain-write supervision. Manual lane: **isolated Linux keyring lifecycle**, including shipped setup/reuse across StateRoots, concurrent replacement/holder death, locked stores, interrupted or OS-accepted stalled writes, exact UUID/Provider selection and cross-user exclusion; use synthetic slots and a private service, preserve secret-free channels and replacement locks through reaping, and never claim rollback after timeout. Human Keychain dialogs and general pasteboard remain checklist item 8; optimized macOS lifecycle remains separate. [Credentials](./docs/specs/credentials.md), [Terminal spec](./docs/specs/terminal.md#paste-and-attachments) |
| Readiness, load and observer faults | Manual lane: **native load and observer faults**; measure cancellation/cleanup and resource enforcement under declared load, including macOS trickling-Collector response deadlines and product traffic/shutdown. The intermittent macOS debug early stop in `guard_runs::permissions::permission_dispatch_modes_rules_command_mcp_and_edit` remains unclassified. Its symptom is `Guard execution/storage cleanup unconfirmed`; macOS teardown shares one bounded deadline and names the failing stage, so use that stage with the permission-case context to distinguish connection, preparation, native-manager and cleanup failures before changing deadlines; passing native runs do not establish a fix. [Tool spec](./docs/specs/tools.md), [OTLP deadline finding](./docs/security/findings/otlp-http-response-deadline.md) |
| Performance | `Linux Tests (release)` with `history_benchmark=true` owns the history measurement. Its 32.76 MB text workload is about half the former 65.5 MB intent, not the same workload. Repeat the [hosted history sample](https://github.com/fpmirabile/arany/actions/runs/38820838049) on named hosts with sanitized kernel/CPU/load/pressure metadata; it is neither a performance budget nor cold-disk or other-host evidence. Measure strict replay, payload/SQLite bytes, Store load p50/p95/max, resume-to-ready, retained input/delegation and VmRSS/VmHWM. Keep exact 10,000-Event, 128 MiB HotSession, 256 MiB shared-Store and near-quota fallback gates distinct from this workload. Manual lane: **named-host performance**, including telemetry and macOS host-PID scanning at its nominal supervision interval before changing membership primitives or intervals. [History owner](./crates/arany-cli/tests/session_run/active_terminal/tmux/history_latency.rs), [native measurement rules](./agents/references/testing-details.md#native-and-live-evidence) |
| Distribution and dependency policy | Manual lane: **release artifact and dependency policy**; keep the [release-policy workflow](./.github/workflows/release-policy.yml) passing for the release commit's `Cargo.lock`. The fresh advisory fetch flags RUSTSEC-2026-0370/0371/0372 (unmaintained `zvariant`, `zbus_names`, `zvariant_derive`, moved into zbus 6) via `keyring` → `zbus-secret-service-keyring-store` → `secret-service 5.2.0` → `zbus 5`; the owner chose to wait, so add no ignore to `deny.toml`, and bump `keyring` or `secret-service` once they adopt zbus 6; run the exact four upstream-root MIT supplements and six nested license/attribution materials, and verify source/binary pairing, compiler/native provenance and MSRV. Reconcile the unreached standard-library attribution gate without weakening pins; the bundle pins `jsonschema` 0.58.6 supplements. [Release review](./docs/security/beta-license-release-review.md#current-result), [dependency inventory](./docs/security/beta-dependency-inventory.md) |
| Other platforms and enforcers | `Linux ARM64 Tests (debug)` and `Linux ARM64 Tests (release)` (`linux_arm64=true`) remain pending; dispatch against a ref whose workflow exists on the default branch and require native `aarch64` host/Rust-target assertions, namespaces and `--cfg arany_ci` workspace tests, without emulation or skips. Manual lane: **alternative Linux enforcement and WSL2**, including user namespaces, cgroup v2 and Guard enforcement. Native Windows is out of scope; actual platform evidence precedes support claims. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities), [native CI procedure](./agents/references/testing-details.md#local-verification-and-ci) |

These gates remain unverified until their named procedure completes. Specs and
existing tests describe what must hold; neither implemented code nor an old
passing run establishes broader support.

### ACP editor evidence

User-owned check: configure `arany acp` using the [setup guide](./docs/setup.md#use-an-acp-editor), then exercise a text turn, guarded edit with `--tools`, allow/reject and expiry/cancellation of a permission request, attached PNG/embedded text, Session load/resume and close/list, mode switches, bounded Goal plans, budget stop reasons, restart and unavailable credentials in Zed on Linux and macOS, a JetBrains ACP client, and Neovim CodeCompanion or avante. Verify stdout isolation and the final committed answer against `arany show`. These native editor workflows remain unverified; synthetic protocol tests do not establish compatibility.
