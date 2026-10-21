# Arany next steps

This file contains unresolved defects, deferred functionality and verification
still required for the personal Linux beta and broader release. Contracts live
in [domain specs](./docs/specs/README.md); setup instructions live in
[the setup guide](./docs/setup.md) and [local tools](./docs/tools.md). Remove an
item when its completion condition is met; completed work belongs in Git/CI
history. Implementation status and verified support are separate claims.

## Next functional priority

Arany is a harness for multi-agent coding teams that can be trusted to work autonomously; its [non-negotiable guarantees](./AGENTS.md#non-negotiable-guarantees) bound every item below. Every item below either makes a non-negotiable guarantee cheaper, exposes it to more callers, or builds the team on top of it. Nothing below weakens a guarantee. Loop shape, wire formats, defaults, permission modes, hooks, agent definitions and embedding surfaces copy the named peer mechanism. The [initiative plan](./planning/agentic-teams/README.md) defines the remaining C3 and related team scope; current behavior is owned by the linked specs.

### Daily use first

The goal: one normal engineering day in this repository (explore, multi-file edits, tests, commit and PR) without falling back to Claude Code, then the owner's other repositories. These items come before the list below; none weakens a guarantee.

1. **Tool protocol: live evidence.** Owner: run the [ChatGPT protocol checks](#tool-protocol).
2. **Tool output limits: live evidence.** Owner: run the [output-limit checks](#tool-output-limits) and resolve [catalog window metadata](#catalog-window-metadata).
3. **ChatGPT route limits: live evidence.** The [Run limits](./docs/specs/chatgpt.md#run-limits) have synthetic evidence only. Remaining (owner, live): an `xhigh` turn whose reasoning exceeds 4,096 tokens succeeds and `arany show` charges the full total; a turn running past 120 seconds completes; and the live stream reports `usage.output_tokens_details.reasoning_tokens` (absent details still count every token against the cap).
4. **Bounded Provider retries: live evidence.** The [retry contract](./docs/specs/provider.md#usage-provenance-and-reliability) has synthetic evidence only. Remaining (owner, live): against ChatGPT and each API-key route, confirm that a real 429 or 5xx shows `Reconnecting… n/3`, honors the server's `Retry-After` and charges every attempt in `arany show`; Codex's `Reconnecting... n/max` is the peer reference.
5. **Attached-turn defaults.** A Goal budget that fits ordinary turns, a [Write/Edit cap of at least 64 KiB](./planning/large-file-edits/README.md), large drafts as attachments, and nested plus user-global `AGENTS.md`.
6. **`cargo test` here inside the Guard.** Toolchain and Cargo cache selection in folder trust, and a Build profile on more cores; measure `cargo test -p arany --lib`.
7. **Real-scale command snapshot** (security pass). Respect `.gitignore`, skip files over 1 MiB instead of aborting, and raise the 2,048-file, 32 MiB and 8 KiB changeset caps.
8. **Development command environment** (security pass). Use all cores by default, plus opt-in network through a per-project domain allowlist behind a Guard proxy, like the Claude Code sandbox and Codex `network_access`.
9. **[Git write workflow](./planning/git-write-workflow/README.md).** Typed Guard operations (`commit`, `branch`, `switch`) on the neutralized view, plus approved prefix rules for `git push` and `gh pr`. Until then, exact-command "Always allow" rules.
10. **Tool ergonomics.** [Regex and glob Search beyond 64 rows](./planning/search-regex/README.md), a [Glob Tool](./planning/glob-tool/README.md), [larger Write/Edit](./planning/large-file-edits/README.md), MultiEdit, typed delete and move; parallel reads with Pursue item 10.
11. **Visible reliability.** Green macOS CI on `main`, a closed flake list, and Linux p95 for Read, Edit and a command end to end.
12. **Anthropic window allowance.** Wire the Models API `max_input_tokens` into the window allowance with the next native-evidence schema change; the evidence row has no slot for it today.
13. **Terminal: live evidence.** Owner: run the [terminal checks](#terminal); remaining feature work is in [Terminal follow-ups](#terminal-follow-ups).

Exit: a week of dogfooding with every fallback to another harness recorded here; done when a full day passes without one.

### Pursue, in order

1. **Run probe P1** (owner, live; see [owner-run live probes](#message-native-provider-request-owner-run-live-probes)). Until P1 passes, the Anthropic native route stays "offline-tested, unvalidated live".
2. **In-process read-only Tools: named-host performance.** Remaining: measure Read p95 <= 50 ms on named Linux hosts. The [repeated-Read corpus](./crates/arany/src/tools/tests.rs) reports warm-file dispatch samples in release with `--nocapture`; cold-disk and end-to-end Run latency remain unmeasured. No hosted test gates this latency.
3. Reserved (attached client integration is done).
4. **Spec diet** (process, owner-approved). Done: release and Tool review ledgers are collapsed and `agents/` holds working practice only. Keep new behavior facts in their single owning spec.
5. **Ledger diet.** Done: the [Goal contract](./docs/specs/goals.md#lifecycle-budgets-and-recovery) owns action headroom and crash ceilings.
6. Reserved (hook seam is done; the [Hook execution contract](./docs/specs/tools.md#hook-execution) owns it).
7. **Sandbox escape hatch with approval.** Remaining: owner-run live attached journey; published-head green macOS release CI for the existing unguarded owners; Linux private-state `engine_run` and `guard_runs` evidence outside the development sandbox; a rule remover in `/permissions` and `setrlimit` limits only if dogfooding needs them. Spec owner: [Tools spec](./docs/specs/tools.md#unguarded-re-run-after-a-guard-failure). The hook-origin interlock rule lives in the [Hook execution contract](./docs/specs/tools.md#hook-execution).
8. **`arany serve --stdio` follow-up.** The [serve contract](./docs/specs/serve.md) owns the implemented wire and loop; only the deferred extraction below remains.
   - **Conditional `arany-journal` crate extraction stays deferred.** Schema export and the Server remain binary-only; re-evaluate extraction for an external SDK when measured dependency, ownership or release pressure justifies it. Resolve `tools::Cancellation` runtime ownership before any dependency-free vocabulary split. Preserve the library dependency boundary.
9. **Provider roster: separately deferred D work.** Judge-only adapters and a durable per-agent pause/retry/switch lifecycle after exhausted routes need accepted contracts first; current fail-and-drain recovery with explicit new-Run resume stays authoritative ([Provider contract](./docs/specs/provider.md#named-roster-presets)). Unverified: macOS attached roster evidence (manifest and Ctrl+C cancellation) and macOS saved-account roster evidence, until the macOS roster owners pass on the published head; live account/Provider and human terminal support retain the user-owned checks below.
10. **Multi-call steps (Stage D)** after probe P2; copy Anthropic parallel `tool_use` with the journal's four-call cap.
11. Reserved (C2 edit-only workers are done; [Execution](./docs/specs/execution.md#worker-journal-protocol) and [Session](./docs/specs/session.md#worker-identity-and-explicit-reassignment) own the behavior).
12. **C3 command-capable views and the Supervisor as hook adapters.** Remaining: C3-3 (live serialized worker commands, delegated hooks and the in-view answer gate continuation loop). Keep the implemented internal private Guard command target and pinned shared-read, discard-only cache gated until model-facing runtime admission is implemented. The [Supervisor adapter](./docs/specs/tools.md#supervisor-adapter) owns the pure shadow evaluator and replay contract. Native Linux/macOS private-state Engine journeys, a native-provider product OTLP journey of shadow verdicts (exact custom profiles reject Tools) and the 1.5 MiB Guard stack canary need final-head CI evidence; the local sandbox cannot admit private State. Manual screen-reader inspection of shadow verdicts and native macOS terminal/OTLP journeys remain unverified. The `supervisor_corpus` show journey (privacy canaries, export stability/unlinkability and locked refusal) requires an outside-sandbox rerun: local private-state admission returns `StateNotPrivate`. Dogfooding calibration is owner-run: retain local corpus exports from at least 30 Sessions, label at least 200 evaluation points with at least 20 true positives per proposed signal, and sweep thresholds offline. Before enabling a signal, the owner must accept its thresholds and demonstrate precision at least 0.9 for pause/revoke or 0.75 for require-approval; otherwise it stays shadow. Corpus arithmetic and synthetic journeys do not establish that calibration or native support. Signal enforcement and overrides are a separate follow-up, gated on the accepted calibration criteria; shadow candidates grant no authority.

Dogfooding starts after item 2; the owner leads it when that point is reached. Every later item is exercised through real use and supplies the Supervisor's calibration data.

Teams support read-only researchers (C1) and Run-scoped edit-only workers (C2) with explicit fresh-assignment ID resume ([Session](./docs/specs/session.md#worker-identity-and-explicit-reassignment)). Native build evidence and named-host measurements still precede Build-profile support claims (finite approved maxima: 4 GiB memory, 8 GiB requested cache, 1 GiB project storage, 256 tasks; effective cache capacity derives from StateRoot headroom and is not guaranteed). Remote MCP/OAuth, interactive/background jobs and other network grants need their own Policy/Guard/protocol evidence; retain the existing [Tool scope](./docs/specs/tools.md) until each slice lands.

### Tool and Engine follow-ups

- **Full-output spill (owner decision).** Keep the [current excerpt-only contract](./docs/specs/tools.md#limits-and-failure-behavior) until dogfooding demonstrates a need for a private per-Run file that Read can page. Revisit only with a security pass for the new persistent format and read surface.
- **Original 45-line Read diagnosis (owner, optional).** If the dogfood Session journal survives, inspect its call and receipt with `arany show` to distinguish the model's byte limit from the fitted result budget. Line paging and fixed per-call budgets remove both causes; this is historical diagnosis, not a remaining paging defect.
- **Engine stop channels.** The [decision rule](./agents/arany.md#engine-loop-decisions) still has exceptions: `intent_observation` in `run_loop/step.rs` converts ActiveTime and Capacity back to errors, and `begin_worker_inner` and `complete_worker_inner` in `run_loop/worker.rs` use budget/limit errors for control flow. Carry closed stop/admission values through those helpers and their callers, preserving journal ordering and the existing stop/replay tables.
- **Conditional loop refinements.** Revisit RAII `ActiveSegment`, a typed `ChildQueue` state machine or a repeated-call stop only if dogfooding demonstrates remaining pain. First identify the failing lifecycle or repetition case; accept a contract before adding a stop beyond the existing proposal-refusal cap.

### Terminal follow-ups

- **Guarded remember rules.** Add "don't ask again" for commands and MCP alongside Daily use first item 9, under the [permission-rule contract](./docs/specs/tools.md#permission-rules). The current [approval choices](./docs/specs/terminal.md#interactive-commands-and-completion) remain authoritative until then.
- **Other terminal hosts (deferred).** Zellij and VS Code's xterm.js need Codex's insertion without partial scroll regions; `tmux -CC` needs forced Classic like Claude Code. Accept a contract and obtain native evidence before claiming support.

### Frozen areas

The owner lifted this freeze for Pursue item 12 (C3 views and the Supervisor in shadow mode), accepting its plan; it stays in force for the rest of the list below until the owner lifts it.

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

## Terminal continuity evidence

- On disposable projects, the owner runs native Linux/macOS capture and guarded rewind journeys (including APFS case aliases and the 1.5 MiB Guard stack canary), private-state Store/Engine and rewind crash-prefix gates, and `rewind_serve_guarded_corpus_restores_refuses_drift_keeps_overwrites_and_blocks_uncertain`. Local private-state failures require outside-sandbox verification. With explicit live usage consent, exercise `/rewind`, Esc Esc, code/conversation/both, Compare, both drift choices and partial/Uncertain receipts after real edits. Review those panels at narrow widths in Classic/Fullscreen on macOS and Linux, and with real screen readers. These native, live and assistive-technology claims remain unverified; compilation and synthetic frames establish none of them. Contracts: [Terminal](./docs/specs/terminal.md#rewind), [Serve](./docs/specs/serve.md#rewind), [Tools](./docs/specs/tools.md#checkpoints-and-rewind).
- With explicit usage consent, repeat OpenAI, Anthropic, ChatGPT and exact-custom streaming conformance against actual selected accounts/endpoints. Offline fixtures prove protocol/lifecycle behavior, not live compatibility. Keep incomplete answers outside future context and confirm recovery after cancellation/crash.
- Check `--screen-reader`, `/transcript`, monochrome labels and managed following with real assistive technology. These usability claims remain unverified.

## ChatGPT coding loop: owner-run live checks

Owner: use a ChatGPT-plan account with explicit usage consent, a new Session and attached chat in a trusted disposable project. Follow [Provider encoding](./docs/specs/provider.md#structured-outcomes-and-tool-encoding), [Tools](./docs/specs/tools.md#limits-and-failure-behavior) and [terminal output](./docs/specs/terminal.md#conversation-progress-and-history). Synthetic fixtures do not establish live or native support.

### Terminal

Owner: use macOS Terminal.app and iTerm2 and a native Linux terminal, locally, over SSH and inside tmux; include WSL2 separately. These emulator and clipboard checks remain unverified. Synthetic PTYs do not emulate native scrollback. Follow [terminal continuity](./docs/specs/terminal.md#terminal-continuity-and-provisional-text), [selectors](./docs/specs/terminal.md#selectors) and [clipboard compatibility](./docs/specs/terminal.md#paste-and-attachments).

1. Classic: wheel and Shift+PageUp scroll natively; drag and the terminal's copy key round-trip text; PageUp leaves Arany unchanged. Ctrl+T and Ctrl+O append details without an alternate screen, and the terminal's find locates older text.
2. Classic: idle, streaming, completion menus, approval, trust, setup, selectors, pickers and rewind take only their content height, up to the screen. Grow/shrink and narrow resize/reflow leave no stale rows, blank gap or duplicate history. A 200-line streamed answer stays readable. Idle `/settings` view changes preserve draft, caret and reading position.
3. Approval is inline and numbered: `1` approves a short action, a long action requires PgDn before Yes, and Escape denies. `/settings`, `/model`, `/resume`, `/help`, `/agents` and Ctrl+K appear inline; their linear/screen-reader forms work without mouse input. Real assistive-technology evidence stays in [Terminal continuity evidence](#terminal-continuity-evidence).
4. Fullscreen: wheel, drag, double-click and triple-click work; selection survives streaming and resize, and copied text joins wraps without headings or rails. Paste elsewhere and check the destination cue. Shift-drag (Option in iTerm2, Fn in Terminal.app) selects natively; enable emulator mouse reporting and tmux `mouse on` where needed.
5. Round-trip `/copy` and selection copies through local `wl-copy`/`xclip`, macOS `pbcopy`/`pbpaste` with non-ASCII text, tmux 3.2+ and SSH OSC 52. Confirm Linux serving clients exit when another application takes the selection. Check ignored OSC 52 in Terminal.app/VTE and native-selection recovery, iTerm2's clipboard permission and Homebrew tmux's `set-clipboard on` fallback; a send cue alone is not delivery evidence.
6. `ARANY_DISABLE_MOUSE=1`: native selection works and wheel input scrolls history with an empty composer; check Mouse `wheel only` and `off` too. Exit, suspend/resume, signals and view switches restore terminal modes.
7. Ctrl+O/Ctrl+T pager: `/`, `n`/`N`, movement and close keys work; `[` releases capture and the alternate screen, dumps bounded text to native/tmux scrollback and waits for Enter. A Run keeps streaming without redraw, Enter returns, and the dump survives exit.
8. An answer containing OSC 52 renders inert and leaves the clipboard unchanged before a copy gesture. Oversized or unavailable copies give the specified notice, keep selection and never retry automatically.

### Tool protocol

1. Send `hello!`: exactly one greeting answer, no Tool row or notice, and one Provider call in `arany show`.
2. Ask to write `test.txt` containing `hello`: one `⏺ Write(test.txt)` row with `⎿ applied`, one answer and matching file content.
3. Ask to write `../outside.txt`: a denial row; a follow-up asking why explains the denial.
4. Ask to write `a.txt`, then Ctrl+C before any Tool row: a follow-up recalls cancellation and no file was written.
5. `/ask-matt` + Enter starts immediately, shows `⏺ Skill(ask-matt)` and applies the admitted Skill.
6. `/ask-matt which skill fits a refactor plan?` applies that Skill to the question.
7. `/resume` after the greeting shows `Resumed: hello!`.
8. `/model` + Enter during a Run names the unavailable command and the cancellation alternative.
9. Force a proposal refusal with the [Engine refusal fixture](./crates/arany/tests/engine_run.rs): the development build's `development.log` and `arany show` retain the closed cause on the call.

### Tool output limits

In this repository, repeat a documentation task from a new Session using the [terminal contract](./docs/specs/terminal.md#terminal-continuity-and-provisional-text):

1. Grouped Command reads of the task's guidance files show a line count and truncation if needed, without a result-limit failure.
2. Read `agents/testing.md`: all current lines fit in one page, including after earlier Tool steps.
3. Read the absent `agents/skills/ponytail/SKILL.md`: the result says not found, and the agent locates `.agents/skills/ponytail/SKILL.md` without help.
4. No row says `cache not attempted`.
5. The agent proceeds through reading, editing and verification without giving up on a Tool result limit.
6. `arany show --output text` retains truncation totals and any `ToolResultsCleared`; a Run of at least 30 Tool calls completes without `ContextFull`.

### Catalog window metadata

Owner: confirm whether the authenticated ChatGPT `/v1/models` reply carries `context_window` for the selected account/model. Run `provider check chatgpt MODEL --effort LEVEL --accept-cost`, then a Run, and inspect `context_window_tokens` in `arany show`. An absent field must retain conservative bounds; a present admitted value must pin the window and derived allowance. Follow the [catalog contract](./docs/specs/chatgpt.md#catalog-checks-and-runs).

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
   private image-bearing JSONL and lossy pixel compaction. Copy transport and
   serving-client checks live under [Terminal](#terminal). Follow
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
| Hosted CI and native Linux regression checks | `Clippy`, `Linux Tests (debug)`, `Linux Tests (release)`, `macOS Tests (debug)` and `macOS Tests (release)` must pass on the published head. Native consent, terminal, setup, Session picker, pinned-Skill and Tool owners include private-mount shared-source success, nested-mount refusal and no late propagation; command descendant termination before create/modify/delete publication; rejection, approval, cancellation and closed replay. Include oversized/grouped Command output with normal changeset publication, the drain ceiling, lossy UTF-8, missing mutation parents and deterministic slash-Skill loading/digest refusal before egress. `Native Linux Guard and tmux (x86_64)` and `Native Linux Guard and tmux (arm64)` (`linux_native_advanced=true`) separately own the shipped systemd Guard journey; both remain pending. C2 parser/private-target execution, foreground/background cancellation, exact-ID recovery containment/disposal, no-host-execution and full channel/telemetry canaries on the final head remain unverified until these lanes run. Re-run the native Linux and macOS lanes after the workspace split, covering reservation overhead, admitted in-process reads, guarded mutations, same-binary Guard and credential-helper entry points, terminal journeys and closed replay; code motion and synthetic runs alone do not establish native parity. [Workflow](./.github/workflows/ci.yml), [testing rules](./agents/testing.md) |
| macOS timeout partial output | On native macOS, run `guard_runs::permissions::a_one_second_command_timeout_is_admitted_and_ends_as_a_limit` and the background-descendant timeout journey on the final head. Confirm bounded partial stdout/stderr, `limit_cause: timeout`, no changeset publication, confirmed teardown and identical reopened replay. Linux compilation does not establish launcher parity. |
| macOS runtime mount topology | `macOS Tests (debug)` owns mounted-image refusal and selected-runtime commands under Seatbelt. Manual lane: **macOS product Ready/GO mount injection** remains unverified; adapter topology and guarded unit commands do not prove lifetime mount isolation. Busy-detach cleanup in `crates/arany-cli/tests/session_run/tools_macos.rs::guarded_runtime_profile_checks_mount_topology_before_dispatch` remains unresolved. Identify the owner natively without retries; repeated passing macOS CI is required before claiming the flake fixed. [Tools spec](./docs/specs/tools.md) |
| Native build profile coverage | Manual lane: **Linux offline builds** after A3/A4, on named hosts. Prove a guarded offline Cargo build, execution of its artifact, cleanup and closed replay using only explicitly selected runtime profiles. The explicitly ignored macOS offline-build owner requires a passing published-head verdict from the named debug opt-in CI step; ordinary workspace tests do not run it. macOS build coverage on other architectures and the distributed binary remains unverified. [Tools spec](./docs/specs/tools.md), [native evidence rules](./agents/references/testing-details.md#native-and-live-evidence) |
| macOS optimized product/account isolation | Manual lane: **macOS optimized account isolation**, on a disposable effective-user home, including full Engine/active-CLI, setup/refresh/logout and cross-user gates, plus optimized macOS file/command/Skill/MCP Tool journeys after the workspace split. Changing HOME, debug fixtures and override refusal do not prove account isolation. [Fixture rules](./agents/references/testing-details.md#host-and-account-isolation) |
| Native TLS to compiled endpoints | Manual lane: **compiled-endpoint trusted HTTPS**, with isolated resolution/routing and ordinary trust. Generated-anchor chain/name rejection does not prove a public trusted chain or production-endpoint success. [Security rules](./agents/security.md) |
| Native terminal faults and tmux | `Native Linux Guard and tmux (x86_64)` and `Native Linux Guard and tmux (arm64)` own composer/replay, active shelf and Classic/Fullscreen steering and pause; both remain pending. The Fullscreen parent cannot inspect cursor visibility in a dead pane. Manual lane: **native terminal faults and restoration**, including remaining tmux, acquisition/output/input-loss owners, optimized macOS, supported architectures and the exact distributed binary; real Fullscreen Ctrl+C/signal cursor restoration remains unverified. [Terminal fixture rules](./agents/references/testing-details.md#terminal-process-fixtures), [PTY finding](./docs/security/findings/terminal-pty-hangup-spin.md) |
| Native stores and clipboard | `macOS Tests (debug)` owns isolated debug Keychain fixtures. An intermittent `Locked` refusal of the authorized read after denial needs diagnosis and repeated-CI stability; `Linux Tests (debug)` and `Linux Tests (release)` own slow-authorization and uncertain-write supervision. Manual lane: **isolated Linux keyring lifecycle**, including shipped setup/reuse across StateRoots, concurrent replacement/holder death, locked stores, interrupted or OS-accepted stalled writes, exact UUID/Provider selection and cross-user exclusion; use synthetic slots and a private service, preserve secret-free channels and replacement locks through reaping, and never claim rollback after timeout. Human Keychain dialogs and general pasteboard remain checklist item 8; optimized macOS lifecycle remains separate. [Credentials](./docs/specs/credentials.md), [Terminal spec](./docs/specs/terminal.md#paste-and-attachments) |
| Readiness, load and observer faults | Manual lane: **native load and observer faults**; measure cancellation/cleanup and resource enforcement under declared load, beyond the isolated macOS trickling-Collector and product traffic/shutdown fixtures. The trickling-Collector owner has an explicit local native pass but is ignored, without a CI lane; ordinary CI does not establish that check. The intermittent macOS debug early stop in `guard_runs::permissions::permission_dispatch_modes_rules_command_mcp_and_edit` remains unclassified. Its symptom is `Guard execution/storage cleanup unconfirmed`; macOS teardown shares one bounded deadline and names the failing stage, so use that stage with the permission-case context to distinguish connection, preparation, native-manager and cleanup failures before changing deadlines; passing native runs do not establish a fix. [Tool spec](./docs/specs/tools.md), [OTLP deadline finding](./docs/security/findings/otlp-http-response-deadline.md) |
| Performance | `Linux Tests (release)` with `history_benchmark=true` owns the history measurement. Its 32.76 MB text workload is about half the former 65.5 MB intent, not the same workload. Repeat the [hosted history sample](https://github.com/fpmirabile/arany/actions/runs/38820838049) on named hosts with sanitized kernel/CPU/load/pressure metadata; it is neither a performance budget nor cold-disk or other-host evidence. Measure strict replay, payload/SQLite bytes, Store load p50/p95/max, resume-to-ready, retained input/delegation and VmRSS/VmHWM. Keep exact 10,000-Event, 128 MiB HotSession, 256 MiB shared-Store and near-quota fallback gates distinct from this workload. Manual lane: **named-host performance**, including Linux telemetry, macOS telemetry under other declared loads, and macOS host-PID scanning at its nominal supervision interval before changing membership primitives or intervals. [History owner](./crates/arany-cli/tests/session_run/active_terminal/tmux/history_latency.rs), [native measurement rules](./agents/references/testing-details.md#native-and-live-evidence) |
| Distribution and dependency policy | Manual lane: **release artifact and dependency policy**; keep the [release-policy workflow](./.github/workflows/release-policy.yml) passing for the release commit's `Cargo.lock`. The fresh advisory fetch flags RUSTSEC-2026-0370/0371/0372 (unmaintained `zvariant`, `zbus_names`, `zvariant_derive`, moved into zbus 6) via `keyring` → `zbus-secret-service-keyring-store` → `secret-service 5.2.0` → `zbus 5`; the owner chose to wait, so add no ignore to `deny.toml`, and bump `keyring` or `secret-service` once they adopt zbus 6; run the exact four upstream-root MIT supplements and six nested license/attribution materials, and verify source/binary pairing, compiler/native provenance and MSRV. Reconcile the unreached standard-library attribution gate without weakening pins; the bundle pins `jsonschema` 0.58.6 supplements. [Release review](./docs/security/beta-license-release-review.md#current-result), [dependency inventory](./docs/security/beta-dependency-inventory.md) |
| Other platforms and enforcers | `Linux ARM64 Tests (debug)` and `Linux ARM64 Tests (release)` (`linux_arm64=true`) remain pending; dispatch against a ref whose workflow exists on the default branch and require native `aarch64` host/Rust-target assertions, namespaces and `--cfg arany_ci` workspace tests, without emulation or skips. Manual lane: **alternative Linux enforcement and WSL2**, including user namespaces, cgroup v2 and Guard enforcement. Native Windows is out of scope; actual platform evidence precedes support claims. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities), [native CI procedure](./agents/references/testing-details.md#local-verification-and-ci) |

These gates remain unverified until their named procedure completes. Specs and
existing tests describe what must hold; neither implemented code nor an old
passing run establishes broader support.

### ACP editor evidence

User-owned check: configure `arany acp` using the [setup guide](./docs/setup.md#use-an-acp-editor), then exercise a text turn, guarded edit with `--tools`, allow/reject and expiry/cancellation of a permission request, attached PNG/embedded text, Session load/resume and close/list, mode switches, bounded Goal plans, budget stop reasons, restart and unavailable credentials in Zed on Linux and macOS, a JetBrains ACP client, and Neovim CodeCompanion or avante. Verify stdout isolation and the final committed answer against `arany show`. These native editor workflows remain unverified; synthetic protocol tests do not establish compatibility.
