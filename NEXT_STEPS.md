# Arany next steps

This file contains unresolved defects, deferred functionality and verification
still required for the personal Linux beta and broader release. Contracts live
in [domain specs](./docs/specs/README.md); setup instructions live in
[the setup guide](./docs/setup.md) and [local tools](./docs/tools.md). Remove an
item when its completion condition is met; completed work belongs in Git/CI
history. Implementation status and verified support are separate claims.

## Next functional priority

Arany is a harness for multi-agent coding teams that can be trusted to work autonomously; its [non-negotiable guarantees](./AGENTS.md#non-negotiable-guarantees) bound every item below. Every item below either makes a non-negotiable guarantee cheaper, exposes it to more callers, or builds the team on top of it. Nothing below weakens a guarantee. Loop shape, wire formats, defaults, permission modes, hooks, agent definitions and embedding surfaces copy the named peer mechanism. The [initiative plan](./planning/agentic-teams/README.md) orders the team slices (C1-C3); Goal Foundation, A3 cache and A2 (message-native Provider request, #56) have landed.

### Pursue, in order

1. **Run probe P1** (owner, live; see [owner-run live probes](#message-native-provider-request-owner-run-live-probes)). Until P1 passes, the Anthropic native route stays "offline-tested, unvalidated live". Follow-up if P1 shows weak schema compliance: move `plan_contract` to its own `arany_plan` Tool so Tool schemas can be strict again.
2. **In-process read-only Tools.** Landed for List, Read and Search ([Tools spec](./docs/specs/tools.md#in-process-reads)): `in_process` attestations, 64 KiB Read page and Tool result, and the Read p95 <= 50 ms gate in the Tool journeys. Typed Git reads stay in the Guard because they spawn `git` (refactor Decision 4). Remaining: run the native Linux and macOS Guard lanes once on this change, since the native Tool journeys are not run locally.
3. **Engine hygiene for embedding.** One Engine per process (Provider swapped per thread); `Store::subscribe` ordered Event stream; `RunHandle { cancel, steer, approvals, events }`; `exec --output jsonl` and telemetry emitted from the stream. Copy Codex's core shape (submission queue in, ordered events out).
4. **Spec diet** (process, owner-approved). Remaining scope: collapse dated ledgers in [release review](./docs/security/beta-license-release-review.md) and [local Tool dependency review](./docs/security/local-tool-dependency-review.md), and the host-scoped note in [local tools](./docs/tools.md#historical-linux-seed-maintenance); the [architecture refactor](./planning/architecture-refactor/README.md) PR13 owns the `agents/` behavior migration. Preserve scenario-to-owner mappings and security findings. Applies to frozen areas too.
5. **Ledger diet.** Run-level active-time deadline plus one `active_ms` on `RunFinished` (replaces the per-action reserve/settle pairs); an `Action::max_events()` table replaces Event-tail literals; one `execute_step` shared by primary and children; per-kind payload versions so an optional field no longer invalidates every journal. The owner approved per-kind payload versions; they stay consistent with "one current format, no compatibility readers" because each kind has exactly one current version.
6. **Hook seam and permission modes.** Copy Claude Code: hook events `PreToolUse`/`PostToolUse`/`Stop`/`SubagentStop`/`PreCompact`, command hooks with JSON on stdin and exit-2-blocks, `permissionDecision` `allow|deny|ask`; permission modes `default|acceptEdits|plan|auto|dontAsk` and rule syntax `Bash(git diff *)`. Arany-specific: hooks only narrow, verdicts are journaled, `updatedInput` is accepted only when it narrows. Human approval and AI review become hook adapters.
7. **Sandbox escape hatch with approval.** Copy Claude Code's `dangerouslyDisableSandbox` flow with its permission prompt, and Codex's sandbox escalation. The Guard classifies a failure caused by its own enforcement (a Seatbelt or bwrap denial, blocked network, an unadmitted path) separately from a program failure, and shows that cause to the model and the user. The model may propose re-running that exact command outside the Guard. That is a new intent the user approves for that one command, with a risk warning; it is never an automatic retry, per the no-retry rule. An optional "always allow this exact command in this project" rule follows Claude Code permission rules; it is never a global bypass, and children inherit it only when granted explicitly. The approval and the unguarded effect are journaled. The Run and the effect are marked unguarded in the terminal and in Goal views. The journal, Uncertain and budget guarantees still apply. Spec owner: [Tools spec](./docs/specs/tools.md), when implemented.
8. **`arany serve --stdio`.** Chosen first by the north; ACP stays a later adapter (item 13). Copy Codex app-server: JSON-RPC 2.0 over NDJSON, `thread/start|resume|fork`, `turn/start|interrupt`, server-initiated approval requests, `thread/item/*` notifications, `arany generate-schema` (Codex `generate-ts`), an `initialize` capabilities array (Claude Code `system/init`). Arany-specific methods: `goal/pause|resume`, `turn/steer`, `receipt/get`, `events/subscribe { since }`. The attached TUI and `exec` become clients (OpenCode's TUI-as-client).
9. **Per-role Provider routing (plan D).** `Providers { primary, child, review, compaction }`; per-role provenance pinned in `RunStarted`. Copy OpenCode's per-agent `model` field and Claude Code's `--fallback-model` chain semantics (fallback only on call failure, never on Tool re-dispatch, per design-tree R3-7).
10. **Multi-call steps (Stage D)** after probe P2; copy Anthropic parallel `tool_use` with the journal's four-call cap.
11. **C2 edit-only workers.** Copy Claude Code agent files (`name`, `description`, `tools`, `disallowedTools`, `model`, `permissionMode`, `maxTurns`, `isolation: worktree`, foreground/background, resume by id) as the definition format; Arany-specific: leases, handover, integration order and conflicts are journaled Events; worker effects publish only through changesets with receipts.
12. **C3 command-capable views and the Supervisor as hook adapters.**
13. **ACP adapter** (`arany acp`) over the stdio protocol (Goose's `goose acp` shape) for Zed, JetBrains and Neovim; then the crate split `arany-engine` / `arany`.

Dogfooding starts after item 2; the owner leads it when that point is reached. Every later item is exercised through real use and supplies the Supervisor's calibration data.

Teams today are read-only researcher children (C1); writable teams arrive with item 11. Native build evidence and named-host measurements still precede Build-profile support claims (finite approved maxima: 4 GiB memory, 8 GiB requested cache, 1 GiB project storage, 256 tasks; effective cache capacity derives from StateRoot headroom and is not guaranteed). Remote MCP/OAuth, interactive/background jobs and other network grants need their own Policy/Guard/protocol evidence; retain the existing [Tool scope](./docs/specs/tools.md) until each slice lands.

- **Architecture refactor (parallel lane, Draft).** Fourteen behavior-preserving PRs that make the Engine, Tools and crate shape deep enough for roadmap items 2-8; [initiative plan](./planning/architecture-refactor/README.md). Caching never waits for it.

### Freeze until item 11 lands

CI stays green; no new features, spec sections or evidence gates for: terminal views and tmux evidence beyond dogfooding; macOS Guard features beyond parity probes; build cache; ChatGPT OAuth and subscription routes; A6 registry fetch; E4 checkpoints and rewind; the Supervisor as loop code; the Anthropic consumer-subscription gate; new typed Tools other than the in-process read set.

- **Deferred Anthropic consumer subscriptions.** Native Claude-plan access is missing and user-deferred behind the [route/authorization gate](./docs/research/subscription-provider-routes-2026-10.md#decision-boundary-for-beta-1). API-key billing is a separate route; another harness's implementation or credentials do not authorize Arany access.

### Copy or reuse (peer -> mechanism)

- Claude Code -> hook protocol, permission modes and rule syntax, subagent frontmatter and nesting/resume model, `--max-turns`/`--max-budget-usd` names, `system/init` capabilities array, cheap first-pass tool-output truncation before summarizing, `--autocompact` threshold setting.
- Codex CLI -> app-server method vocabulary and NDJSON framing, `generate-ts` schema export, sandbox preset names `read_only|workspace_write|full_access`, encrypted-reasoning replay (done), `agent-roles` naming for the roster.
- OpenCode -> TUI as a client of the server; per-agent `model`; session fork/abort/revert endpoints; OpenAPI at `/doc` when the HTTP stage comes.
- Goose -> `acp` subprocess adapter and `serve` loopback auth (`X-Secret-Key`, explicit allowed origins).
- OpenHands -> sandbox-on-demand workspace adapter shape for a future container-backed Guard; `RemoteConversation` as the model for a remote client.
- Aider -> repo map as a bounded, digest-pinned include (later).
- Cline/Roo -> shadow-git checkpoints when E4 is unfrozen.

## Architecture follow-ups

Found by the macOS architecture review after A5b; no behavior change is
claimed until each lands.

- **One Seatbelt profile builder.** `install`, `install_command` and `install_git` in `src/tools/guard/macos.rs` repeat rules, ancestors, grants, port detach, install and attestation; one private builder with a single install-and-attest tail keeps digest-relevant logic in one place.
- **One executable-identity table.** Platform root, shell and executable magic for trusted system executables are spread across `git.rs`, `config.rs`, `workspace.rs` and `macos.rs::system_file`; a single per-platform table makes a second Git root (for example Xcode.app) a one-row change.
- **Native Git profile probe hardening (needs a Mac).** Add a service-specific Mach-lookup denial with an unsandboxed success baseline to the Seatbelt Git probe. A direct lookup needs new test-only `unsafe` FFI, which requires the dedicated review in [AGENTS.md](./AGENTS.md#global-rules) rule 7.
- **Linux Git journey isolation assertions.** `typed_git_reads_run_in_the_guard_without_repository_code` should assert what the Linux contract guarantees for Git reads: the Workspace stays read-only (a write attempt from repository config changes nothing) and no host home or gitconfig is visible in the namespace. Restricting Linux Git reads to exec only the pinned `git` (as macOS does) is not in the Linux contract; it would be a separate enforcement and [Tools spec](./docs/specs/tools.md) decision.

## Terminal continuity evidence

- Exercise classic scrollback, narrow resize/reflow, temporary details, idle `/settings` chat-view transitions and a 200-line streamed response in a real macOS emulator and native Linux terminals, including under WSL2. Synthetic macOS PTYs prove controlled cursor ownership, transcript insertion and restoration; they do not emulate scrollback or prove other OS support.
- With explicit usage consent, repeat OpenAI, Anthropic, ChatGPT and exact-custom streaming conformance against actual selected accounts/endpoints. Offline fixtures prove protocol/lifecycle behavior, not live compatibility. Keep incomplete answers outside future context and confirm recovery after cancellation/crash.
- Manual lane: **macOS attached steering and pause** remains unverified; verify note delivery to synthesis, controls while a note waits and `/goal pause` at the next boundary in Classic and Fullscreen. Linux owners run in `Linux Tests (debug)`, `Linux Tests (release)`, `Native Linux Guard and tmux (x86_64)` and `Native Linux Guard and tmux (arm64)`.
- Check `--screen-reader`, `/transcript`, monochrome labels and managed following with real assistive technology. These usability claims remain unverified.

## Message-native Provider request: owner-run live probes

- **P1: Anthropic native Tools with structured Finish/Delegate.** With explicit usage consent, send one request on the selected model with the full Tool catalog (non-strict definitions, each with a required-but-nullable `plan_contract`), strict `output_config.format` for Finish/Delegate, `tool_choice: {"type":"auto","disable_parallel_tool_use":true}` and thinking on. Confirm the request is accepted, a `tool_use` stop and an `end_turn` JSON stop both occur, and a replayed thinking block is accepted on the next call. Measure how well the model follows the now non-strict Tool schemas, in particular the required-but-nullable fields `expected_digest`, `resource` and Git `revision`/`base`, and `plan_contract: null`. Confirm the three-breakpoint `cache_control` layout on active Tool-loop calls is accepted, with only a system breakpoint on no-Tool and compaction calls. Compare cache reads before and after the synthesis switch: `output_config.format` narrows from Finish|Delegate (Delegate alone for Team) to Finish and probably invalidates the messages cache. Also confirm a spent researcher accepts `tool_choice: {"type":"none"}` and returns strict Finish. This deliberate divergence from the prompt-only optimization avoids a further Tool response failing the whole Run as `InvalidResponse`, at the cost of the messages cache only; Tool and system caches survive. After a sufficiently large matching prefix is reused, inspect cache-read usage and the Goal report. A short prefix or an expired entry may yield no hit. Offline wire corpora prove only the encoding and accounting.
- **P2: Anthropic parallel Tool use.** Before enabling more than one Tool call per step, confirm with `disable_parallel_tool_use: false` that the model respects the four-call cap in practice; an over-cap response fails the Run without retry.
- **P3: ChatGPT/Codex `allowed_tools`.** Until this probe passes, the constant function list plus `allowed_tools` applies to the OpenAI API route only. ChatGPT retains the proven phase-dependent function list, Finish alone for a spent researcher, and `tool_choice: "required"`; it still sends the Session `prompt_cache_key` and accepts one cache miss at the synthesis switch. With explicit usage consent, confirm ChatGPT accepts `tool_choice: {"type":"allowed_tools",...}` for planning, synthesis and a spent researcher with the constant function list before enabling that shape. Confirm the Session UUIDv7 key is accepted and cache reads are reported when eligible. Offline tests do not establish live acceptance.

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
   Validate the chosen real model's strict Tool encoding before claiming support.
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
| Hosted CI and native Linux regression checks | `Linux Format and Clippy`, `Linux Tests (debug)`, `Linux Tests (release)`, `macOS tests (debug)` and `macOS tests (release)` must pass on the published head. Native consent, terminal, setup, Session picker, pinned-Skill and Tool owners include private-mount shared-source success, nested-mount refusal and no late propagation; command descendant termination before create/modify/delete publication; rejection, approval, cancellation and closed replay. `Native Linux Guard and tmux (x86_64)` and `Native Linux Guard and tmux (arm64)` (`linux_native_advanced=true`) separately own the shipped systemd Guard journey; both remain pending. [Workflow](./.github/workflows/ci.yml), [testing rules](./agents/testing.md) |
| macOS runtime mount topology | `macOS tests (debug)` owns mounted-image refusal and selected-runtime commands under Seatbelt. Manual lane: **macOS product Ready/GO mount injection** remains unverified; adapter topology and guarded unit commands do not prove lifetime mount isolation. Investigate the recurring busy-detach cleanup failure in `tests/session_run/tools_macos.rs::guarded_runtime_profile_checks_mount_topology_before_dispatch` natively, without retries. [Tools spec](./docs/specs/tools.md) |
| Native build profile coverage | Manual lane: **Linux and macOS offline builds** after A3/A4, on named hosts. Trace macOS `cc`/`xcrun` delegation to `/Library/Developer/CommandLineTools` and the selected SDK; grant only measured exact runtime roots. [Tools spec](./docs/specs/tools.md), [native evidence rules](./agents/references/testing-details.md#native-and-live-evidence) |
| macOS optimized product/account isolation | Manual lane: **macOS optimized account isolation**, on a disposable effective-user home, including full Engine/active-CLI, setup/refresh/logout and cross-user gates. Changing HOME, debug fixtures and override refusal do not prove account isolation. [Fixture rules](./agents/references/testing-details.md#host-and-account-isolation) |
| Native TLS to compiled endpoints | Manual lane: **compiled-endpoint trusted HTTPS**, with isolated resolution/routing and ordinary trust. Generated-anchor chain/name rejection does not prove a public trusted chain or production-endpoint success. [Security rules](./agents/security.md) |
| Native terminal faults and tmux | `Native Linux Guard and tmux (x86_64)` and `Native Linux Guard and tmux (arm64)` own composer/replay, active shelf and Classic/Fullscreen steering and pause; both remain pending. The Fullscreen parent cannot inspect cursor visibility in a dead pane. Manual lane: **native terminal faults and restoration**, including remaining tmux, acquisition/output/input-loss owners, optimized macOS, supported architectures and the exact distributed binary; real Fullscreen Ctrl+C/signal cursor restoration remains unverified. [Terminal fixture rules](./agents/references/testing-details.md#terminal-process-fixtures), [PTY finding](./docs/security/findings/terminal-pty-hangup-spin.md) |
| Native stores and clipboard | `macOS tests (debug)` owns isolated debug Keychain fixtures; `Linux Tests (debug)` and `Linux Tests (release)` own slow-authorization and uncertain-write supervision. Manual lane: **isolated Linux keyring lifecycle**, including shipped setup/reuse across StateRoots, concurrent replacement/holder death, locked stores, interrupted or OS-accepted stalled writes, exact UUID/Provider selection and cross-user exclusion; use synthetic slots and a private service, preserve secret-free channels and replacement locks through reaping, and never claim rollback after timeout. Human Keychain dialogs and general pasteboard remain checklist item 8; optimized macOS lifecycle remains separate. [Credentials](./docs/specs/credentials.md), [Terminal spec](./docs/specs/terminal.md#paste-and-attachments) |
| Readiness, load and observer faults | Manual lane: **native load and observer faults**; measure cancellation/cleanup and resource enforcement under declared load, including macOS trickling-Collector response deadlines and product traffic/shutdown. [Tool spec](./docs/specs/tools.md), [OTLP deadline finding](./docs/security/findings/otlp-http-response-deadline.md) |
| Performance | `Linux Tests (release)` with `history_benchmark=true` owns the history measurement. Its 32.76 MB text workload is about half the former 65.5 MB intent, not the same workload. Repeat the [hosted history sample](https://github.com/fpmirabile/arany/actions/runs/38820838049) on named hosts with sanitized kernel/CPU/load/pressure metadata; it is neither a performance budget nor cold-disk or other-host evidence. Measure strict replay, payload/SQLite bytes, Store load p50/p95/max, resume-to-ready, retained input/delegation and VmRSS/VmHWM. Keep exact 10,000-Event, 128 MiB HotSession, 256 MiB shared-Store and near-quota fallback gates distinct from this workload. Manual lane: **named-host performance**, including telemetry and macOS host-PID scanning at its nominal supervision interval before changing membership primitives or intervals. [History owner](./tests/session_run/active_terminal/tmux/history_latency.rs), [native measurement rules](./agents/references/testing-details.md#native-and-live-evidence) |
| Distribution and dependency policy | Manual lane: **release artifact and dependency policy**; keep the [release-policy workflow](./.github/workflows/release-policy.yml) passing for the release commit's `Cargo.lock`, run the exact four upstream-root MIT supplements and six nested license/attribution materials, and verify source/binary pairing, compiler/native provenance and MSRV. Reconcile the unreached standard-library attribution gate without weakening pins; the bundle pins `jsonschema` 0.58.6 supplements. [Release review](./docs/security/beta-license-release-review.md#current-result), [dependency inventory](./docs/security/beta-dependency-inventory.md) |
| Other platforms and enforcers | `Linux ARM64 Tests (debug)` and `Linux ARM64 Tests (release)` (`linux_arm64=true`) remain pending; dispatch against a ref whose workflow exists on the default branch and require native `aarch64` host/Rust-target assertions, namespaces and `--cfg arany_ci` workspace tests, without emulation or skips. Manual lane: **alternative Linux enforcement and WSL2**, including user namespaces, cgroup v2 and Guard enforcement. Native Windows is out of scope; actual platform evidence precedes support claims. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities), [native CI procedure](./agents/references/testing-details.md#local-verification-and-ci) |

These gates remain unverified until their named procedure completes. Specs and
existing tests describe what must hold; neither implemented code nor an old
passing run establishes broader support.
