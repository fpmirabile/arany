# Arany next steps

This file contains unresolved defects, deferred functionality and verification
still required for the personal Linux beta and broader release. Contracts live
in [domain specs](./docs/specs/README.md); setup instructions live in
[the setup guide](./docs/setup.md) and [local tools](./docs/tools.md). Remove an
item when its completion condition is met; completed work belongs in Git/CI
history. Implementation status and verified support are separate claims.

## Next functional priority

Arany is a harness for multi-agent coding teams that can be trusted to work autonomously; its [non-negotiable guarantees](./AGENTS.md#non-negotiable-guarantees) bound every item below. Every item below either makes a non-negotiable guarantee cheaper, exposes it to more callers, or builds the team on top of it. Nothing below weakens a guarantee. Loop shape, wire formats, defaults, permission modes, hooks, agent definitions and embedding surfaces copy the named peer mechanism. [Focused plans](./planning/README.md#active-plans) track proposed team features; current behavior is owned by the linked specs.

### Daily use first

The goal: one normal engineering day in this repository (explore, multi-file edits, tests, commit and PR) without falling back to Claude Code, then the owner's other repositories. These items come before the list below; none weakens a guarantee.

1. **Tool protocol: live evidence.** Owner: run the [ChatGPT protocol checks](#tool-protocol).
2. **Tool output limits: live evidence.** Owner: run the [output-limit checks](#tool-output-limits) and resolve [catalog window metadata](#catalog-window-metadata).
3. **ChatGPT route limits: live evidence.** The [Run limits](./docs/specs/chatgpt.md#run-limits) have synthetic evidence only. Remaining (owner, live): an `xhigh` turn whose reasoning exceeds 4,096 tokens succeeds and `arany show` charges the full total; a turn running past 120 seconds completes; and the live stream reports `usage.output_tokens_details.reasoning_tokens` (absent details still count every token against the cap).
4. **Bounded Provider retries: live evidence.** The [retry contract](./docs/specs/provider.md#usage-provenance-and-reliability) has synthetic evidence only. Remaining (owner, live): against ChatGPT and each API-key route, confirm that a real 429 or 5xx shows `Reconnecting… n/3`, honors the server's `Retry-After` and charges every attempt in `arany show`; Codex's `Reconnecting... n/max` is the peer reference.
5. **Large draft attachments.** Large drafts as attachments; shared [Goal defaults](./docs/specs/goals.md#lifecycle-budgets-and-recovery) apply across clients.
6. **`cargo test` here inside the Guard.** Toolchain and Cargo cache selection in folder trust, and a Build profile on more cores; measure `cargo test -p arany --lib`. Follow the [implementation plan](./planning/guarded-cargo-test/README.md). **Remaining capability gap: local test IPC.** Socket-dependent library tests are conditionally ignored only in guarded Cargo builds, with named ignored counts; ordinary CI/local runs outside the Guard retain them. Item 6 may close after the guarded path and its measurement succeed, moving this gap to a separate Daily use first item. Owner/test-security: deliver a pinned, per-effect local test IPC capability with native Linux/macOS isolation and denial evidence before removing these ignores. Outbound domain-proxy access in item 8 does not close this gap.
7. **Real-scale command snapshot** (security pass; [plan](./planning/real-scale-command-snapshot/README.md), owner gate passed). Respect `.gitignore`, skip ordinary inputs over 1 MiB instead of aborting, reject capture for oversized publishable outputs or enlarged baseline files, raise the snapshot limits to 16,384 entries and 128 MiB, and raise the decoded changeset limit to 32 KiB.
8. **Development command environment** (security pass). Use all cores by default, plus opt-in network through a per-project domain allowlist behind a Guard proxy, like the Claude Code sandbox and Codex `network_access`.
9. **Git write workflow: remaining owner/native work.** S1–S3 landed; [typed Git writes](./docs/specs/tools.md#typed-git-writes) and [human-approved host Git](./docs/specs/tools.md#human-approved-host-git) own the behavior. Owner: complete the [native credential/gh and live host checks](#beta-2-and-broader-release). Owner, native macOS: launch the Git executor through the existing unit supervisor under its own Seatbelt profile that denies Workspace `file-write*` and allows only private-view writes, close every mutation descriptor before it starts, exchange the view over bounded IPC and keep publication in the Guard worker, since one shared profile cannot both confine Git and publish ([Chromium's Seatbelt design](https://raw.githubusercontent.com/chromium/chromium/main/sandbox/mac/seatbelt_sandbox_design.md)); then enable it with native hostile-configuration, lock and recovery evidence.
10. **Tool ergonomics.** Typed delete and move; parallel reads with Pursue item 10. Remaining (owner, live): a 64 KiB [Write and Edit](./docs/specs/tools.md#limits-and-failure-behavior) completes on each Provider route; it has synthetic evidence only.
11. **Visible reliability.** Green macOS CI on `main`, a closed flake list, and Linux p95 for Read, Edit and a command end to end. Remaining owner-run latency evidence:
    - **Linux named-host latency: unverified.** Owner: repository owner, native `asustuf`, idle on stable power; kernel `7.2.10-zen1-1-zen`, the home filesystem on local ext4 NVMe (WD_BLACK SN770). Use the [ignored release corpus and retained-report procedure](./crates/arany-cli/tests/guard_runs/latency.md) for three separately reported invocations. Slice 1 supplies the warm Read row; guarded Edit and no-op command remain in [slice 2](./planning/linux-tool-latency/README.md#2-pr-complete-the-same-report-with-guarded-edit-and-command-rows). Accepted provisional p95 objectives: Read 100 ms, Edit 250 ms, no-op command 500 ms. Preserve misses and diagnosis ownership, with no CI timing gate or automatic objective increase. Keep [Pursue item 2](#pursue-in-order)'s dispatch evidence separate. Cold disk, long history, large snapshots and concurrent load remain unverified.
    - **macOS named-host latency: unverified.** Owner: repository owner on a named native macOS host, after the guarded rows exist; record native preparation/teardown, pinned binary and workload, three separate reports and closed replay using the same endpoint methodology. Linux results and objectives do not establish a macOS pass.
12. **Anthropic window allowance.** Wire the Models API `max_input_tokens` into the window allowance with the next native-evidence schema change; the evidence row has no slot for it today.
13. **Terminal: live evidence.** Owner: run the [terminal checks](#terminal); remaining feature work is in [Terminal follow-ups](#terminal-follow-ups).
14. **[Anthropic-compatible custom routes](./docs/specs/provider.md#custom-destinations-and-routing): live evidence.** Owner, live: run `arany provider check custom:NAME` for each exact DeepSeek, Kimi and GLM profile with the owner's own keys outside the repo. The [Provider contract](./docs/specs/provider.md#custom-destinations-and-routing) owns each route's availability and the evidence it needs; failed routes stay unavailable. Verify the plain-text compaction request on the exact route, then direct/child use, `/compact`, the next Run and reopened `show`; schema support is no longer an admission prerequisite.

**Read discovery:** implement [gitignore-aware typed discovery and file suggestions](./planning/gitignore-read-discovery/README.md) after snapshot PR1; preserve protected-name and Read-permission exclusions.

Exit: a week of dogfooding with every fallback to another harness recorded here; done when a full day passes without one.

### Pursue, in order

1. **Run probe P1** (owner, live; see [owner-run live probes](#message-native-provider-request-owner-run-live-probes)). Until P1 passes, the Anthropic native route stays "offline-tested, unvalidated live".
2. **In-process read-only Tools: named-host performance.** Remaining: measure Read p95 <= 50 ms on named Linux hosts. The [repeated-Read corpus](./crates/arany/src/tools/tests.rs) reports warm-file dispatch samples in release with `--nocapture`; cold-disk and end-to-end Run latency remain unmeasured. No hosted test gates this latency.
7. **Primary host commands: native and live evidence.** Remaining: owner-run attached once/session approval, explicit privilege-command handoff, host inspection with partial output, Linux/macOS executable identity and process-group teardown evidence, and final-head CI for the [host journey](./crates/arany-cli/tests/guard_runs/unguarded.rs). Synthetic compile checks do not establish native support. [Tools spec](./docs/specs/tools.md#primary-host-commands).
8. **Conditional `arany-journal` crate extraction (deferred).** Revisit for an external SDK only after measured dependency, ownership or release pressure; first resolve `tools::Cancellation` ownership. The [serve contract](./docs/specs/serve.md) owns the existing binary-only schema and Server.
9. **Provider roster: separately deferred D work.** Judge-only adapters and a durable per-agent pause/retry/switch lifecycle after exhausted routes need accepted contracts first; current fail-and-drain recovery with explicit new-Run resume stays authoritative ([Provider contract](./docs/specs/provider.md#named-roster-presets)). Unverified: macOS attached roster evidence (manifest and Ctrl+C cancellation) and macOS saved-account roster evidence, until the macOS roster owners pass on the published head; live account/Provider and human terminal support retain the user-owned checks below.
10. **Multi-call steps (Stage D)** after probe P2; copy Anthropic parallel `tool_use` with the journal's four-call cap.
12. **Worker native evidence and Supervisor calibration.** Future C3 scope is retained in the [persistent assignment-view plan](./planning/worker-assignment-views/README.md). Keep the implemented internal private Guard command target and pinned shared-read, discard-only cache gated until model-facing runtime admission is implemented. This gate applies to the proposed persistent-view lifetime; today's fresh per-command runtime remains the current contract. Worker commands, delegated hooks and the in-view Finish gate landed in #169; [Execution](./docs/specs/execution.md#worker-journal-protocol) and [Tools](./docs/specs/tools.md#private-worker-targets-and-handover-transfer) own the behavior. Remaining: final-head native Linux/macOS private-state Engine journeys, native-provider OTLP shadow-verdict journey and the 1.5 MiB Guard stack canary. Rerun `supervisor_corpus` outside the development sandbox (`StateNotPrivate` locally); manual screen-reader shadow-verdict inspection and native macOS terminal/OTLP journeys remain unverified. Owner: export the [Supervisor corpus](./docs/specs/tools.md#supervisor-adapter) from at least 30 Sessions, label at least 200 evaluation points with at least 20 true positives per proposed signal, and sweep thresholds offline. Accept thresholds with precision at least 0.9 for pause/revoke or 0.75 for require-approval before [signal enforcement](./planning/agentic-teams/README.md); otherwise remain shadow. Synthetic arithmetic and old runs establish neither calibration nor native support.

Dogfooding starts after item 2; the owner leads it when that point is reached. Every later item is exercised through real use and supplies the Supervisor's calibration data.

Native build evidence and named-host measurements still precede [Build-profile support](./docs/specs/tools.md). Remote MCP/OAuth, interactive/background jobs and other network grants need their own Policy/Guard/protocol evidence.

### Tool and Engine follow-ups

- **Full-output spill (owner decision).** Keep the [current excerpt-only contract](./docs/specs/tools.md#limits-and-failure-behavior) until dogfooding demonstrates a need for a private per-Run file that Read can page. Revisit only with a security pass for the new persistent format and read surface.
- **Engine stop channels.** The [decision rule](./agents/arany.md#engine-loop-decisions) still has exceptions: `intent_observation` in `run_loop/step.rs` converts ActiveTime and Capacity back to errors, and `begin_worker_inner` and `complete_worker_inner` in `run_loop/worker.rs` use budget/limit errors for control flow. Carry closed stop/admission values through those helpers and their callers, preserving journal ordering and the existing stop/replay tables.
- **Conditional loop refinements.** Revisit RAII `ActiveSegment`, a typed `ChildQueue` state machine or a repeated-call stop only if dogfooding demonstrates remaining pain. First identify the failing lifecycle or repetition case; accept a contract before adding a stop beyond the existing proposal-refusal cap.

### Terminal follow-ups

- **Guarded remember rules.** General command and MCP remember choices remain proposed under the [permission-rule contract](./docs/specs/tools.md#permission-rules). Typed Git prefix rules shipped in #256; primary host prefix rules remain S2 of the [host-command plan](./planning/primary-host-commands/README.md).
- **Other terminal hosts (deferred).** Zellij and VS Code's xterm.js need Codex's insertion without partial scroll regions; `tmux -CC` needs forced Classic like Claude Code. Accept a contract and obtain native evidence before claiming support.

### Frozen areas

The owner lifted this freeze for Pursue item 12 (C3 views and the Supervisor in shadow mode), accepting its plan, and for the [typed Git writes](./docs/specs/tools.md#typed-git-writes) of Daily use first item 9 ([rationale](./docs/research/git-writes-2026-10.md)), and on 2026-10-22 for MultiEdit only in Daily use first item 10 ([MultiEdit contract](./docs/specs/tools.md#atomic-single-file-multiedit)); it stays in force for the rest of the list below until the owner lifts it.

CI stays green; no new features, spec sections or evidence gates for: terminal views and tmux evidence beyond dogfooding; macOS Guard features beyond parity probes; build cache; ChatGPT OAuth and subscription routes; A6 registry fetch; the Anthropic consumer-subscription gate; new typed Tools other than the in-process read set and the explicitly approved exceptions above. Typed delete, typed move and parallel reads retain their existing scope and gates.

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

- Owner, native Linux/macOS and live attached clients: exercise long primary Tool approval waits, late allow, Escape/Ctrl+C, pause, disconnect, stale pins and saturated output through terminal, serve and ACP. Run the [approval scenario owners](./planning/approvals-wait/README.md#contract-and-scenarios) outside private-State sandbox restrictions, including hook Ask, unguarded proposals, child timer withdrawal and finite post-approval Guard admission. Synthetic evidence and source audits do not establish native or live support.

- **Budget feedback (owner, native Linux/macOS).** Exercise finite call, token, active-time and Run limits after known Tool failures across attached, `exec`/`show`, serve and ACP; compare live status with reopened `GoalFinished.budget_refusal`, including preserved answers and no extra Provider call. Repeat with live Provider routes only with explicit usage consent. Native and live observations remain unverified; the [Goal contract](./docs/specs/goals.md#budget-refusal-feedback) owns the expected behavior.
- On disposable projects, the owner runs native Linux/macOS capture and guarded rewind journeys (including APFS case aliases and the 1.5 MiB Guard stack canary), private-state Store/Engine and rewind crash-prefix gates, and `rewind_serve_guarded_corpus_restores_refuses_drift_keeps_overwrites_and_blocks_uncertain`. Local private-state failures require outside-sandbox verification. With explicit live usage consent, exercise `/rewind`, Esc Esc, code/conversation/both, Compare, both drift choices and partial/Uncertain receipts after real edits. Review those panels at narrow widths in Classic/Fullscreen on macOS and Linux, and with real screen readers. These native, live and assistive-technology claims remain unverified; compilation and synthetic frames establish none of them. Contracts: [Terminal](./docs/specs/terminal.md#rewind), [Serve](./docs/specs/serve.md#rewind), [Tools](./docs/specs/tools.md#checkpoints-and-rewind).
- With explicit usage consent, repeat OpenAI, Anthropic, ChatGPT and exact-custom streaming conformance against actual selected accounts/endpoints. On the official ChatGPT direct-use route, check whether passive window headers/events actually appear; absence must remain unknown. Reopen `/usage` and inspect weekly/reported 5-hour observations, stale/reset/unknown labels, and draft restoration at 80, 50, 40 and 16 columns in Classic/Fullscreen on Linux/macOS and with real screen readers. Run the Store plan-observation crash gate on native Linux. These live, native and usability checks remain unverified. Offline fixtures prove protocol/lifecycle behavior, not live compatibility. Keep incomplete answers outside future context and confirm recovery after cancellation/crash.
- Check `--screen-reader`, `/transcript`, monochrome labels and managed following with real assistive technology. These usability claims remain unverified.
- On native Terminal.app, iTerm2, a Linux VTE or xterm emulator and tmux, record the bytes Option or Alt+Left/Right send (for example `ESC [1;3D` or `ESC b`), then confirm they switch [agent chats](./docs/specs/terminal.md#agent-chats) only on an empty composer while a draft keeps them, and review the read-only view, its live `New activity from child N:` lines, `/transcript agents` and the [`/agents` picker](./docs/specs/terminal.md#agent-picker)'s search, idle group and `o` choice with real screen readers. Synthetic decoder rows and the Linux PTY journey establish none of these claims.

Child messaging needs native macOS Option-key/PTY and human assistive-technology evidence: open a running child, offer literal text, observe waiting then committed recording, return with the primary draft intact, and verify final-call non-delivery retains the child draft without another call. The [terminal](./docs/specs/terminal.md#agent-chats) and [Goal](./docs/specs/goals.md#steering-notes-and-pause) contracts own the expected behavior.

## Storage report native and live evidence

Before claiming native storage-report support, run `arany storage` and idle
`/storage` with synthetic private Session/account roots on Linux and macOS; check
allocated blocks versus sparse logical length, private ACL/foreign-owner refusal,
mount exclusion (including Linux same-device bind mounts), concurrent cache
activity and directory replacement, cancellation without mutation, and pager
navigation at narrow widths in Classic, Fullscreen and a real screen reader.
On macOS verify metadata-only `O_EVTONLY` descriptors with ACL admission under a
physical `/private/tmp` fixture. Record native/live results separately from
synthetic CI evidence. Guard backing/image inventory remains the next
[storage slice](./planning/arany-storage/README.md), not current coverage.

## Account recovery and native Guard follow-ups

- **MultiEdit native and live evidence.** Owner: run the MultiEdit Guard permission/cancellation/crash cases, checkpoint/rewind corpus, private-worker handover journey and Linux/macOS Tool journeys outside the sandbox on each native platform. With explicit usage consent, exercise ordered and dependent replacements, replace-all and a 64 KiB batch on each admitted Tool-capable Provider route; confirm late failure leaves the file unchanged and a lost receipt never retries. Synthetic corpora and the macOS source audit do not establish native or live support. [Tools](./docs/specs/tools.md#atomic-single-file-multiedit) owns the behavior and evidence owners. The existing 64 KiB Write/Edit live checks remain outstanding.

- **Affected 2026-10-08 macOS account fixture.** Reauthorize with fresh `/setup`
  sign-in, or recover from an external backup of the lost registration/index
  metadata. The two validated 33-byte migration markers were backed up and
  removed under the account lock; Keychain and Session history were preserved.
  Markers and SQLite provenance cannot reconstruct the original metadata.
  Root relocation was removed on 2026-10-09; this does not restore that metadata.
- **macOS metadata confinement.** Run the file-profile and process-unit owners
  against unselected metadata, ancestor/control-root directory listing and
  admitted command/MCP startup. Source changes alone do not establish native
  regression or crash-cleanup evidence.
- **Live ChatGPT identity rejection.** Owner: retry sign-in and, if rejection
  persists, record only the closed identity failure category. The corrected
  synthetic singleton-audience case does not diagnose the prior real token;
  see the [source review](./docs/security/beta-chatgpt-identity-source-review.md).
- **Historical Linux seeds.** For an account with unrecorded `/tmp/arany-tools-*`
  roots or version-1 reservations, apply the [selected-object maintenance
  procedure](./docs/tools.md#historical-linux-seed-maintenance) before claiming
  cleanup. Its synthetic fixture never inventories an existing installation.

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
8. Verify typed backslash-Enter, distinct Shift+Enter and saved Alt/Ctrl+Enter in Classic, Fullscreen and editable child drafts, locally and through tmux/SSH. Check fresh-session fallback hints, modifier loss warnings, canonical continuation and framed-paste draining; include VoiceOver/Orca. Keyboard negotiation remains [slice 2](./planning/composer-newline-keys/README.md#vertical-slices), with no native support claim from synthetic coverage.
9. An answer containing OSC 52 renders inert and leaves the clipboard unchanged before a copy gesture. Oversized or unavailable copies give the specified notice, keep selection and never retry automatically.

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
   stops continuation and no action retries itself. On Linux and macOS (Command Line Tools Git), also inspect typed Git status, diff, log and branch discovery on a disposable repository with packed and loose local/remote-tracking refs, divergent tips, a stale upstream and a symbolic remote alias. Owner: fpmirabile; on each native host compare branch OIDs and counts with your own `git for-each-ref` and `git rev-list --left-right --count`, confirm unavailable counts stay null and a large-history cap breach returns null ahead/behind with `counts_withheld:true` and `counts_capped:true` under the [branch comparison bound](./docs/specs/tools.md#branch-discovery), confirm completed rows on deadlines, cancel during enrichment, and verify closed-store `show` after changing the refs/config; no fetch or native/live support is claimed by the synthetic corpus. Then, on Linux, commit selected paths, create a branch and switch to it, and confirm with your own `git log` and `git status`.
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
| ChatGPT check journal (C2) | Selected ChatGPT `provider check` still dispatches unjournaled catalog/probe calls. Complete [C2](./planning/provider-check-journal/README.md#remaining-vertical-slice), including account-index migration and uncertainty across JSON/SQLite publication, before setup UI S1. Custom/native C1 does not close this gap. |
| Standalone Provider check native/live evidence | Owner-run paid custom/native checks on isolated Linux and macOS StateRoots must confirm receipts, selected-source revalidation, lease/ACL refusal, process death and recovery without repeated requests. Published-head CI owns synthetic check/migration/crash fixtures; these do not establish live compatibility or native parity. [Provider](./docs/specs/provider.md#commands), [Execution](./docs/specs/execution.md#standalone-provider-checks), [fixture procedures](./agents/references/testing-details.md#native-and-live-evidence) |
| macOS runtime mount topology | `macOS Tests (debug)` owns mounted-image refusal and selected-runtime commands under Seatbelt. Manual lane: **macOS product Ready/GO mount injection** remains unverified; adapter topology and guarded unit commands do not prove lifetime mount isolation. Busy-detach cleanup in `crates/arany-cli/tests/session_run/tools_macos.rs::guarded_runtime_profile_checks_mount_topology_before_dispatch` remains unresolved. Identify the owner natively without retries; repeated passing macOS CI is required before claiming the flake fixed. [Tools spec](./docs/specs/tools.md) |
| Typed host Git native and live evidence | Owner-run Linux/macOS lane: with disposable remote repositories and existing credential-helper/ssh-agent and gh accounts, approve one push and a separately pinned PR; inspect remote state, closed replay, host labels in terminal/exec/show/Goal report, cancellation and group teardown. Check PTY once/deny and screen-reader output. Real network writes, credentials/keychains and native parity remain unverified by local synthetic peers. [Host contract](./docs/specs/tools.md#human-approved-host-git) |
| Native build profile coverage | Owner-run named-host lane: after A3/A4 on Linux, run the explicit Linux `guarded_offline_cargo_tests_use_selected_runtime_and_registry` and macOS `guarded_offline_rust_build_uses_selected_runtime_and_sdk` gates with `ARANY_TEST_RUST_SYSROOT` naming one physical versioned installed toolchain. The fixture owns its copied runtime, synthetic registry and account roots. Verify artifact execution and cleanup, nonzero tests, named IPC ignores, flag-sensitive rebuild, warm reuse, failures, cancellation and closed replay. Then measure exact `cargo test -p arany --lib` through production consent/Engine/Guard under slice 1's one-job Build environment; record the first limiting phase, actual resources and cache outcome before slice 2. No native or performance evidence is claimed by compilation. The macOS measurement remains deferred because its private volumes use `/private/tmp` and the owner's build-output placement restriction permits no exception. The explicitly ignored macOS offline-build owner requires a passing published-head verdict from the named debug opt-in CI step; ordinary workspace tests do not run it. macOS build coverage on other architectures and the distributed binary remains unverified. [Plan and measurement procedure](./planning/guarded-cargo-test/README.md), [Tools spec](./docs/specs/tools.md), [native evidence rules](./agents/references/testing-details.md#native-and-live-evidence) |
| macOS optimized product/account isolation | Manual lane: **macOS optimized account isolation**, on a disposable effective-user home, including full Engine/active-CLI, setup/refresh/logout and cross-user gates, plus optimized macOS file/command/Skill/MCP Tool journeys after the workspace split. Changing HOME, debug fixtures and override refusal do not prove account isolation. [Fixture rules](./agents/references/testing-details.md#host-and-account-isolation) |
| Native TLS to compiled endpoints | Manual lane: **compiled-endpoint trusted HTTPS**, with isolated resolution/routing and ordinary trust. Generated-anchor chain/name rejection does not prove a public trusted chain or production-endpoint success. On both macOS release architectures, test valid, invalid-chain and wrong-name peers, incomplete-chain issuer fetches and revocation listeners with cold/warm caches; distinguish Arany payload destinations from auxiliary OS certificate traffic and exclude Workspace/credential canaries from those listeners. [Security rules](./agents/security.md) |
| Native terminal faults and tmux | `Native Linux Guard and tmux (x86_64)` and `Native Linux Guard and tmux (arm64)` own composer/replay, active shelf and Classic/Fullscreen steering and pause; both remain pending. The Fullscreen parent cannot inspect cursor visibility in a dead pane. Manual lane: **native terminal faults and restoration**, including remaining tmux, PTY hangup and cursor-acquisition faults, output/input loss, optimized macOS, supported architectures and the exact distributed binary; real Fullscreen Ctrl+C/signal cursor restoration remains unverified. [Terminal fixture rules](./agents/references/testing-details.md#terminal-process-fixtures), [input and restoration contract](./docs/specs/terminal.md#public-limits-and-compatibility) |
| Native stores and clipboard | `macOS Tests (debug)` owns isolated debug Keychain fixtures. An intermittent `Locked` refusal of the authorized read after denial needs diagnosis and repeated-CI stability; `Linux Tests (debug)` and `Linux Tests (release)` own slow-authorization and uncertain-write supervision. Manual lane: **isolated Linux keyring lifecycle**, including shipped setup/reuse across StateRoots, concurrent replacement/holder death, locked stores, interrupted or OS-accepted stalled writes, exact UUID/Provider selection and cross-user exclusion; use synthetic slots and a private service, preserve secret-free channels and replacement locks through reaping, and never claim rollback after timeout. Human Keychain dialogs and general pasteboard remain checklist item 8; optimized macOS lifecycle remains separate. [Credentials](./docs/specs/credentials.md), [Terminal spec](./docs/specs/terminal.md#paste-and-attachments) |
| Readiness, load and observer faults | Manual lane: **native load and observer faults**; measure cancellation/cleanup and resource enforcement under declared load, beyond the isolated macOS trickling-Collector and product traffic/shutdown fixtures. The trickling-Collector owner has an explicit local native pass but is ignored, without a CI lane; ordinary CI does not establish that check. It checks forward progress, not the exact export deadline, response-size refusal or dropping the incomplete response future before the next batch; focused verification of those bounds remains open. The intermittent macOS debug early stop in `guard_runs::permissions::permission_dispatch_modes_rules_command_mcp_and_edit` remains unclassified. Its symptom is `Guard execution/storage cleanup unconfirmed`; macOS teardown shares one bounded deadline and names the failing stage, so use that stage with the permission-case context to distinguish connection, preparation, native-manager and cleanup failures before changing deadlines; passing native runs do not establish a fix. [Tool spec](./docs/specs/tools.md), [whole-response deadline](./docs/specs/telemetry.md#export-bounds) |
| Performance | `Linux Tests (release)` with `history_benchmark=true` owns the history measurement. Its 32.76 MB text workload is about half the former 65.5 MB intent, not the same workload. Repeat the [hosted history sample](https://github.com/fpmirabile/arany/actions/runs/38820838049) on named hosts with sanitized kernel/CPU/load/pressure metadata; it is neither a performance budget nor cold-disk or other-host evidence. Measure strict replay, payload/SQLite bytes, Store load p50/p95/max, resume-to-ready, retained input/delegation and VmRSS/VmHWM. Keep exact 10,000-Event, 128 MiB HotSession, 256 MiB shared-Store and near-quota fallback gates distinct from this workload. Manual lane: **named-host performance**, including Linux telemetry, macOS telemetry under other declared loads, and macOS host-PID scanning at its nominal supervision interval before changing membership primitives or intervals. [History owner](./crates/arany-cli/tests/session_run/active_terminal/tmux/history_latency.rs), [native measurement rules](./agents/references/testing-details.md#native-and-live-evidence) |
| Distribution and dependency policy | Manual lane: **release artifact and dependency policy**; keep the [release-policy workflow](./.github/workflows/release-policy.yml) passing for the release commit's `Cargo.lock`. The fresh advisory fetch flags RUSTSEC-2026-0370/0371/0372 (unmaintained `zvariant`, `zbus_names`, `zvariant_derive`, moved into zbus 6) via `keyring` → `zbus-secret-service-keyring-store` → `secret-service 5.2.0` → `zbus 5`; the owner chose to wait, so add no ignore to `deny.toml`, and bump `keyring` or `secret-service` once they adopt zbus 6; run the exact four upstream-root MIT supplements and six nested license/attribution materials, and verify source/binary pairing, compiler/native provenance and MSRV. Reconcile the unreached standard-library attribution gate without weakening pins; the bundle pins `jsonschema` 0.58.6 supplements. [Release review](./docs/security/beta-license-release-review.md#current-result), [dependency inventory](./docs/security/beta-dependency-inventory.md) |
| Other platforms and enforcers | `Linux ARM64 Tests (debug)` and `Linux ARM64 Tests (release)` (`linux_arm64=true`) remain pending; dispatch against a ref whose workflow exists on the default branch and require native `aarch64` host/Rust-target assertions, namespaces and `--cfg arany_ci` workspace tests, without emulation or skips. Manual lane: **alternative Linux enforcement and WSL2**, including user namespaces, cgroup v2 and Guard enforcement. Native Windows is out of scope; actual platform evidence precedes support claims. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities), [native CI procedure](./agents/references/testing-details.md#local-verification-and-ci) |

These gates remain unverified until their named procedure completes. Specs and
existing tests describe what must hold; neither implemented code nor an old
passing run establishes broader support.

### ACP editor evidence

User-owned check: configure `arany acp` using the [setup guide](./docs/setup.md#use-an-acp-editor), then exercise a text turn, guarded edit with `--tools`, allow/reject and expiry/cancellation of a permission request, attached PNG/embedded text, Session load/resume and close/list, mode switches, bounded Goal plans, budget stop reasons, restart and unavailable credentials in Zed on Linux and macOS, a JetBrains ACP client, and Neovim CodeCompanion or avante. Verify stdout isolation and the final committed answer against `arany show`. These native editor workflows remain unverified; synthetic protocol tests do not establish compatibility.
