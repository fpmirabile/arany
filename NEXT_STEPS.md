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
2. **Prompt caching (Stage C).** Copy Anthropic's three-breakpoint `cache_control` plan and OpenAI's `prompt_cache_key` (Codex CLI relies on implicit Responses caching plus encrypted-reasoning replay, already done in A2). Replace the `tool_choice: none` flip for finish-only researchers with a prompt line so the prefix stays byte-stable. Evidence: cached input tokens in the Goal report.
3. **In-process read-only Tools.** Copy the peer split (Claude Code and Codex sandbox commands, not reads): List, Read, Search and typed Git reads run in the Engine under the admitted `cap-std` capability and the same bounds; receipts record `InProcess`; the Guard stays mandatory for every mutation, command, Skill script and MCP call. Read page and Tool result rise to 64 KiB. New gate: Read p95 <= 50 ms in the Tool journeys.
4. **Engine hygiene for embedding.** One Engine per process (Provider swapped per thread); `Store::subscribe` ordered Event stream; `RunHandle { cancel, steer, approvals, events }`; `exec --output jsonl` and telemetry emitted from the stream. Copy Codex's core shape (submission queue in, ordered events out).
5. **Spec diet** (process, owner-approved). Collapse per-slice evidence tables into test names and move native-gate ledgers into CI job names. Applies to frozen areas too.
6. **Ledger diet.** Run-level active-time deadline plus one `active_ms` on `RunFinished` (replaces the per-action reserve/settle pairs); an `Action::max_events()` table replaces Event-tail literals; one `execute_step` shared by primary and children; per-kind payload versions so an optional field no longer invalidates every journal. The owner approved per-kind payload versions; they stay consistent with "one current format, no compatibility readers" because each kind has exactly one current version.
7. **Hook seam and permission modes.** Copy Claude Code: hook events `PreToolUse`/`PostToolUse`/`Stop`/`SubagentStop`/`PreCompact`, command hooks with JSON on stdin and exit-2-blocks, `permissionDecision` `allow|deny|ask`; permission modes `default|acceptEdits|plan|auto|dontAsk` and rule syntax `Bash(git diff *)`. Arany-specific: hooks only narrow, verdicts are journaled, `updatedInput` is accepted only when it narrows. Human approval and AI review become hook adapters.
8. **Sandbox escape hatch with approval.** Copy Claude Code's `dangerouslyDisableSandbox` flow with its permission prompt, and Codex's sandbox escalation. The Guard classifies a failure caused by its own enforcement (a Seatbelt or bwrap denial, blocked network, an unadmitted path) separately from a program failure, and shows that cause to the model and the user. The model may propose re-running that exact command outside the Guard. That is a new intent the user approves for that one command, with a risk warning; it is never an automatic retry, per the no-retry rule. An optional "always allow this exact command in this project" rule follows Claude Code permission rules; it is never a global bypass, and children inherit it only when granted explicitly. The approval and the unguarded effect are journaled. The Run and the effect are marked unguarded in the terminal and in Goal views. The journal, Uncertain and budget guarantees still apply. Spec owner: [Tools spec](./docs/specs/tools.md), when implemented.
9. **`arany serve --stdio`.** Chosen first by the north; ACP stays a later adapter (item 14). Copy Codex app-server: JSON-RPC 2.0 over NDJSON, `thread/start|resume|fork`, `turn/start|interrupt`, server-initiated approval requests, `thread/item/*` notifications, `arany generate-schema` (Codex `generate-ts`), an `initialize` capabilities array (Claude Code `system/init`). Arany-specific methods: `goal/pause|resume`, `turn/steer`, `receipt/get`, `events/subscribe { since }`. The attached TUI and `exec` become clients (OpenCode's TUI-as-client).
10. **Per-role Provider routing (plan D).** `Providers { primary, child, review, compaction }`; per-role provenance pinned in `RunStarted`. Copy OpenCode's per-agent `model` field and Claude Code's `--fallback-model` chain semantics (fallback only on call failure, never on Tool re-dispatch, per design-tree R3-7).
11. **Multi-call steps (Stage D)** after probe P2; copy Anthropic parallel `tool_use` with the journal's four-call cap.
12. **C2 edit-only workers.** Copy Claude Code agent files (`name`, `description`, `tools`, `disallowedTools`, `model`, `permissionMode`, `maxTurns`, `isolation: worktree`, foreground/background, resume by id) as the definition format; Arany-specific: leases, handover, integration order and conflicts are journaled Events; worker effects publish only through changesets with receipts.
13. **C3 command-capable views and the Supervisor as hook adapters.**
14. **ACP adapter** (`arany acp`) over the stdio protocol (Goose's `goose acp` shape) for Zed, JetBrains and Neovim; then the crate split `arany-engine` / `arany`.

Dogfooding starts after item 3; the owner leads it when that point is reached. Every later item is exercised through real use and supplies the Supervisor's calibration data.

Teams today are read-only researcher children (C1); writable teams arrive with item 12. Native build evidence and named-host measurements still precede Build-profile support claims (finite approved maxima: 4 GiB memory, 8 GiB requested cache, 1 GiB project storage, 256 tasks; effective cache capacity derives from StateRoot headroom and is not guaranteed). Remote MCP/OAuth, interactive/background jobs and other network grants need their own Policy/Guard/protocol evidence; retain the existing [Tool scope](./docs/specs/tools.md) until each slice lands.

### Freeze until item 12 lands

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
- **macOS Tool evidence in release builds.** `tools_macos` compiles only with `debug_assertions`, so the macOS release job has no Guard, Seatbelt or Git journeys. Split it into the account-root journeys and the parts that need only a temporary StateRoot so release can carry the profile probes.
- **Researcher children with Tools.** They still have no native macOS owner.
- **Native Git profile probe hardening (needs a Mac).** Extend the Seatbelt Git probe to deny exec of a sibling Command Line Tools binary and of the `/usr/bin/git` shim; deny writes outside the view and allow them inside; deny reading `~/.gitconfig`; and exercise a Mach-lookup denial. Current probes cover only the pinned Git exec and the workspace read.
- **Git probe without Command Line Tools.** `native_git_profile_runs_pinned_git_and_denies_host_access` runs in the default local lane and panics on a Mac without Command Line Tools; decide between an `arany_ci` gate and an explicit unavailable assertion.

## Terminal continuity evidence

- Exercise classic scrollback, narrow resize/reflow, temporary details, idle `/settings` chat-view transitions and a 200-line streamed response in a real macOS emulator and native Linux terminals, including under WSL2. Synthetic macOS PTYs prove controlled cursor ownership, transcript insertion and restoration; they do not emulate scrollback or prove other OS support.
- With explicit usage consent, repeat OpenAI, Anthropic, ChatGPT and exact-custom streaming conformance against actual selected accounts/endpoints. Offline fixtures prove protocol/lifecycle behavior, not live compatibility. Keep incomplete answers outside future context and confirm recovery after cancellation/crash.
- Native Linux linear-mode PTY journeys ([steering](tests/session_run/active_terminal/steering.rs)) show a steering note reaching synthesis, controls running while a note waits, and `/goal pause` pausing at the next boundary. Native Linux tmux journeys ([steering and pause](tests/session_run/active_terminal/tmux.rs), `classic_steering_note_and_pause_in_tmux` and `fullscreen_steering_note_and_pause_in_tmux`) cover the same behavior in a real pane for Classic and Fullscreen. macOS attached journeys remain unverified; their evidence is the Engine corpus, reducer tests and terminal unit tests.
- Check `--screen-reader`, `/transcript`, monochrome labels and managed following with real assistive technology. These usability claims remain unverified.

## Message-native Provider request: owner-run live probes

- **P1: Anthropic native Tools with structured Finish/Delegate.** With explicit usage consent, send one request on the selected model with the full Tool catalog (non-strict definitions, each with a required-but-nullable `plan_contract`), strict `output_config.format` for Finish/Delegate, `tool_choice: {"type":"auto","disable_parallel_tool_use":true}` and thinking on. Confirm the request is accepted, a `tool_use` stop and an `end_turn` JSON stop both occur, and a replayed thinking block is accepted on the next call. Measure how well the model follows the now non-strict Tool schemas, in particular the required-but-nullable fields `expected_digest`, `resource` and Git `revision`/`base`, and `plan_contract: null`. No `cache_control` is sent before Stage C, so a cache read is not expected. Offline wire corpora prove only the encoding.
- **P2: Anthropic parallel Tool use.** Before enabling more than one Tool call per step, confirm with `disable_parallel_tool_use: false` that the model respects the four-call cap in practice; an over-cap response fails the Run without retry.
- **P3: ChatGPT/Codex `allowed_tools`.** Confirm the ChatGPT route accepts `tool_choice: {"type":"allowed_tools",...}` for a zero-budget researcher. Until it passes, that route lists only `arany_finish` for such a call.
- **Stage C: constant OpenAI function list.** The Finish/Delegate functions follow the phase, so the OpenAI `tools` array changes at the synthesis switch. Keep one constant function list and restrict it by phase with `allowed_tools` when prompt caching lands.

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
| Hosted CI and native Linux regression checks | Publish when authorized, verify the pull-request jobs on a hosted runner (format and Clippy, Linux and macOS debug/release tests with `--cfg arany_ci`), and run the native consent, terminal, setup, Session picker, pinned-Skill and Tool owners, including the production private-mount bootstrap fixture (shared-source success, pre-existing nested-mount refusal and no late propagation), Linux command descendant termination before changeset capture and create/modify/delete publication, rejection, approval and cancellation with closed replay. The full systemd Guard product journey is separate from the topology fixture and is activated by a default-false `linux_native_advanced` manual lane. The product Guard and tmux outcomes remain pending until the exact parents pass on native x86_64 and ARM64. Native Linux command changeset execution remains unverified on the current macOS host. [Workflow](./.github/workflows/ci.yml), [testing rules](./agents/testing.md) |
| macOS runtime mount topology | Run the native mounted-image refusal and selected-runtime command under Seatbelt on hosted macOS CI; the focused `getfsstat` topology fixture and existing guarded unit command cover adapter behavior, but a full product-level injected-mount Ready/GO journey remains unverified. Do not claim a lifetime mount-isolation guarantee. Known flake: the `hdiutil detach` busy failure in `guarded_runtime_profile_checks_mount_topology_before_dispatch` (fixture cleanup in `tests/session_run/tools_macos.rs`, near the final `hdiutil detach` assertion) recurs on hosted macOS; investigate natively on a Mac, no retries. [Tools spec](./docs/specs/tools.md) |
| Native build profile coverage | After A3/A4, run real offline Arany builds on named Linux and macOS hosts. On macOS specifically trace `cc`/`xcrun` delegation to `/Library/Developer/CommandLineTools` and the selected SDK; grant only measured, exact runtime roots if the actual build demonstrates that need. [Tools spec](./docs/specs/tools.md), [native evidence rules](./agents/references/testing-details.md#native-and-live-evidence) |
| macOS typed Git reads | The Command Line Tools Git profile probe (`native_git_profile_runs_pinned_git_and_denies_host_access`) and the macOS Git journey (`guarded_native_git_reads_survive_closed_replay`) passed in the hosted macOS debug and release jobs on head b83ac83 ([run 38929214556](https://github.com/fpmirabile/arany/actions/runs/38929214556)) and on a local arm64 host. The macOS journey file compiles only with `debug_assertions`, so release-profile Git evidence is unverified. Hosts with only Xcode.app or the `/usr/bin/git` xcrun shim and no Command Line Tools report Git unavailable. Git outside the platform root (Nix, Homebrew, Guix) stays unavailable by design. [Tools spec](./docs/specs/tools.md#typed-git-reads) |
| macOS optimized product/account isolation | On a disposable effective-user home, run the full optimized Engine/active-CLI and account setup/refresh/logout/cross-user gates. Changing HOME does not isolate account paths; debug fixtures and override refusal do not prove this gate. [Fixture rules](./agents/references/testing-details.md#host-and-account-isolation) |
| Native TLS to compiled endpoints | Provide isolated resolution/routing and ordinary trust for a compiled-endpoint HTTPS journey. Generated-anchor chain/name rejection alone does not prove a public trusted chain or production-endpoint success. [Security rules](./agents/security.md) |
| Native terminal faults and tmux | The opt-in Linux release lane activates the composer/replay, active-shelf, Classic steering and pause, and Fullscreen steering and pause tmux parents on x86_64 and ARM64; keep all four outcomes pending until the hosted run passes. The Fullscreen parent does not check cursor visibility after exit (tmux reports no cursor state for a dead pane). Fullscreen normal and unwind exits are covered by the byte-stream check in `terminal_readers_preserve_shared_output_and_setup_redraw`; a Ctrl+C or signal exit from a real Fullscreen Run is still unverified for cursor visibility. Run the remaining native tmux and acquisition/output/input-loss owners, including optimized macOS and supported architectures, and verify restoration on the exact distributed binary. [Terminal fixture rules](./agents/references/testing-details.md#terminal-process-fixtures), [PTY finding](./docs/security/findings/terminal-pty-hangup-spin.md) |
| Native stores and clipboard | Finish isolated Linux keyring-backed product setup and native store lifecycle gates; human Keychain dialog and general-pasteboard checks remain in checklist item 8. [Credentials](./docs/specs/credentials.md), [Terminal spec](./docs/specs/terminal.md#paste-and-attachments) |
| Readiness, load and observer faults | Measure cancellation/cleanup and resource enforcement under declared load. [Tool spec](./docs/specs/tools.md) |
| Performance | The opt-in ignored Linux release history fixture encodes 800 Runs across 50 complete Goal/Run/Provider lifecycles (8,901 Events, leaving 1,099 below the 10,000 lineage cap) and asserts strict replay, serialized payload and SQLite file bytes. It reports 100 fresh read-only Store `load_view` samples (nearest-rank p50/p95/max), one process resume-to-ready interval, retained input/delegation latency and product VmRSS/VmHWM. The text workload is 32,764,000 objective/answer bytes, about half the former 65.5 MB intent; it is not the same workload. After correcting its stale status predicate and Provider reply, [hosted run 38820838049](https://github.com/fpmirabile/arany/actions/runs/38820838049) on `ubuntu-24.04` passed: resume-to-ready 1,338 ms, Store replay p50/p95/max 167/168/177 ms, delegation admission 1.06 s, key p50/max 4.2/4.4 ms, idle RSS/HWM 207/256 MB and final RSS/HWM 422/574 MB. That is one point-in-time GitHub-hosted sample, not a performance budget or other-host evidence; repeat it on named hosts with sanitized kernel/CPU/load/pressure metadata; fresh Store instances do not clear the OS page cache, so these samples do not establish cold-disk replay. This fixture does not test the exact 10,000-Event boundary, the 128 MiB HotSession boundary, the 256 MiB shared-Store quota, or near-quota fallback; keep those gates distinct. Also measure telemetry on named hosts and the bounded macOS host-PID scan at its nominal supervision interval before choosing another membership primitive or interval. [History gate](./tests/session_run/active_terminal/tmux/history_latency.rs), [native measurement rules](./agents/references/testing-details.md#native-and-live-evidence) |
| Distribution and dependency policy | Keep the release-policy workflow passing for the release commit's `Cargo.lock`; run the updated bundle with its exact four upstream-root MIT supplements and six nested license/attribution materials, then verify source/binary pairing, compiler/native provenance and MSRV. Reconcile the unreached standard-library attribution gate without weakening pins. The bundle pins the `jsonschema` 0.58.6 supplements. [Release review](./docs/security/beta-license-release-review.md#current-result), [dependency inventory](./docs/security/beta-dependency-inventory.md) |
| Other platforms and enforcers | Dispatch the existing [CI workflow](./.github/workflows/ci.yml) with `linux_arm64=true` against the ref containing this change; the workflow file itself must exist on the default branch. Require both native `ubuntu-24.04-arm` debug and release jobs to pass their `aarch64` host/Rust-target assertions, Linux namespace prerequisites and `--cfg arany_ci` workspace tests; keep this evidence pending until the hosted run completes green. Do not use emulation or skip gates. Separately verify alternative Linux-native enforcement and the Linux build under WSL2 (user namespaces, cgroup v2 and Guard enforcement), and require actual platform behavior evidence before support claims. Native Windows is out of scope. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities), [native CI procedure](./agents/references/testing-details.md#local-verification-and-ci) |

These gates remain unverified until their named procedure completes. Specs and
existing tests describe what must hold; neither implemented code nor an old
passing run establishes broader support.
