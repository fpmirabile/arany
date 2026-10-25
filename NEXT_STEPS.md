# Arany next steps

This file lists unresolved defects, deferred functionality and code work still
needed for the personal Linux beta and broader release. Contracts live in
[domain specs](./docs/specs/README.md); setup instructions live in
[the setup guide](./docs/setup.md) and [local tools](./docs/tools.md). Remove an
item when its completion condition is met; completed work belongs in Git/CI
history. Implementation status and verified support are separate claims.

## Next functional priority

Arany is a harness for multi-agent coding teams that can be trusted to work autonomously; its [non-negotiable guarantees](./AGENTS.md#non-negotiable-guarantees) bound every item below. Every item below either makes a non-negotiable guarantee cheaper, exposes it to more callers, or builds the team on top of it. Nothing below weakens a guarantee. Loop shape, wire formats, defaults, permission modes, hooks, agent definitions and embedding surfaces copy the named peer mechanism. [Focused plans](./planning/README.md#active-plans) track in-flight work; current behavior is owned by the linked specs.

### Daily use first

The goal: one normal engineering day in this repository (explore, multi-file edits, tests, commit and PR) without falling back to Claude Code, then the owner's other repositories. These items come before the list below; none weakens a guarantee.

1. **Architecture protective batch 1.** A1, P3, E1a, E6, G1 and T1 ([architecture homogenization plan](./planning/architecture-homogenization/README.md#parallelism-map)). Each removes a duplicated owner before a feature edits that file.
2. **Architecture batch 2.** Complete B8, B6, E1b, S5a, R2a and R1a.
3. **Provider setup from the CLI.** Every Provider configurable from `arany-cli`, with no hand-edited `provider-profiles.json`, `provider-routing.json` or `provider-roster.json`: split `/setup` into a plan sign-in flow (ChatGPT) and an API-key flow (OpenAI, Anthropic and custom routes such as DeepSeek, Kimi and GLM). S1 in batch 3, S2 in batch 4, S3 in batch 5 ([plan](./planning/provider-setup-ui/README.md)).
4. **Primary host-command prefix rules, S2.** After E1c and B11, batch 4 ([plan](./planning/primary-host-commands/README.md)).
5. **Real-scale command snapshot, remaining PR3.** Raise decoded changesets to 32 KiB, after S5c in batch 4 ([plan](./planning/real-scale-command-snapshot/README.md)).
6. **Tool failure continuation, C2.** Delegated feedback after E7 in batch 4, in batch 5 ([plan](./planning/failure-feedback/README.md)).
7. **GitHub PR reads, S1.** Host PR listing after E6, E1b and B11, batch 5 ([plan](./planning/github-pr-reads/README.md)).
8. **`cargo test` here inside the Guard.** Toolchain and Cargo cache selection in folder trust, then measure `cargo test -p arany --lib`. Guarded Cargo Build commands currently run one job (`CARGO_BUILD_JOBS=1`); next is a Build profile on more cores: default 4 jobs, accepted range 1–16, resolved downward against controller CPU capacity and enforced as aggregate `cpu.max`. Socket-dependent library tests stay conditionally ignored in guarded Cargo builds, with named ignored counts. Remaining capability gap: a pinned, per-effect local test IPC capability with native Linux and macOS isolation, before those ignores are removed. See [Tools](./docs/specs/tools.md#persistent-workspace-build-cache).
9. **Development command environment** (security pass). Use all cores by default, plus opt-in network through a per-project domain allowlist behind a Guard proxy, like the Claude Code sandbox and Codex `network_access`.
10. **Git write workflow, macOS executor.** Typed Git writes and [human-approved host Git](./docs/specs/tools.md#human-approved-host-git) are landed. Remaining: launch the macOS Git executor through the existing unit supervisor under its own Seatbelt profile that denies Workspace `file-write*` and allows only private-view writes; close every mutation descriptor before it starts; exchange the view over bounded IPC; keep publication in the Guard worker. A single shared profile cannot both confine Git and publish ([Chromium's Seatbelt design](https://raw.githubusercontent.com/chromium/chromium/main/sandbox/mac/seatbelt_sandbox_design.md)).
11. **Tool ergonomics.** Typed delete and typed move, and parallel reads with the Multi-call steps item below.
12. **Anthropic window allowance.** Wire the Models API `max_input_tokens` into the window allowance; the evidence row has no slot for it today.
13. **Visible reliability.** Extend the ignored Linux latency corpus with guarded Edit and no-op command rows, beside the existing warm Read row ([corpus notes](./crates/arany-cli/tests/guard_runs/latency.md)). Close the intermittent macOS failures listed under [Beta 2 and broader release](#beta-2-and-broader-release).

Done when a normal engineering day in this repository needs no fallback to another harness.

### Pursue, in order

1. **Multi-call steps (Stage D).** Copy Anthropic parallel `tool_use` with the journal's four-call cap; an over-cap response fails the Run without retry.
2. **ChatGPT constant tool list.** Send a constant function list with `allowed_tools` mode `auto` across planning and synthesis, and `tool_choice: "none"` for a spent researcher, keyed by the Session UUIDv7 cache key. Today ChatGPT uses `tool_choice: "auto"` with phase-dependent definitions.
3. **Worker views and Supervisor.** The internal private Guard command target and pinned shared-read, discard-only cache stay gated until model-facing runtime admission exists; today's fresh per-command runtime is the current contract. Remaining: the 1.5 MiB Guard stack canary, and future C3 assignment-long Guard views with a read-only assignment-time cache and MCP invocation exclusion (admission gate remains). [Execution](./docs/specs/execution.md#worker-journal-protocol) and [Tools](./docs/specs/tools.md#private-worker-targets-and-handover-transfer) own the landed behavior. Supervisor signals stay shadow-only until calibrated thresholds are accepted ([Supervisor adapter](./docs/specs/tools.md#supervisor-adapter)).
4. **Provider roster: deferred D work.** Judge-only adapters need a researched typed-judge contract (vendor, verdict semantics, deadlines, usage, privacy). A durable per-agent pause, retry and switch lifecycle after exhausted routes needs accepted Events and user commands. Current fail-and-drain recovery with explicit new-Run resume stays authoritative ([Provider contract](./docs/specs/provider.md#named-roster-presets)).
5. **Conditional `arany-journal` crate extraction (deferred).** Revisit for an external SDK only after measured dependency, ownership or release pressure; first resolve `tools::Cancellation` ownership. The [serve contract](./docs/specs/serve.md) owns the existing binary-only schema and Server.

### Tool and Engine follow-ups

- **Hook cancellation reported as budget exhaustion.** [Hook dispatch](./crates/arany/src/engine/run_loop/step/hooks.rs) returns `EngineError::GoalBudgetExhausted` when the Run is cancelled during a hook, so the Goal reports an exhausted active-time budget instead of a cancellation. Report it as a cancellation.
- **Recorded native Guard storage.** Extend the [storage report](./docs/specs/terminal.md#storage-report) through the existing Linux seed and macOS image ownership records: backing-byte accounting, live/retained/unknown labels and the existing maintenance guidance. Cover mounted-image non-traversal, unrecorded prefixes, stale boot, replaced roots, busy or changing records and aggregate overflow. Do not introduce worktree management.
- **Full-output spill (owner decision).** Keep the [current excerpt-only contract](./docs/specs/tools.md#limits-and-failure-behavior) until dogfooding demonstrates a need for a private per-Run file that Read can page. Revisit only with a security pass for the new persistent format and read surface.
- **Conditional loop refinements.** Revisit RAII `ActiveSegment`, a typed `ChildQueue` state machine or a repeated-call stop only if dogfooding demonstrates remaining pain. First identify the failing lifecycle or repetition case; accept a contract before adding a stop beyond the existing proposal-refusal cap.
- **Deferred network grants.** Remote MCP/OAuth, interactive and background jobs, and other network grants need their own Policy, Guard and protocol designs.

### Terminal follow-ups

- **Plan usage shows unknown.** Observed 2026-10-23, ChatGPT plan on Konsole: after a completed Run, `/usage` shows the Run's tokens but reports plan usage as unknown, while Codex shows weekly and 5-hour use. The route parser reads `x-codex-*` headers and `rate_limits` events ([limits](./crates/arany/src/provider/openai/subscription/limits.rs)); find which observation is missing or rejected before changing the display.
- **API and custom-route rate limits.** Show exposed rate-limit facts for API-key OpenAI/Anthropic and admitted custom routes through the ChatGPT-window path; unsupported quotas show `unknown (not reported)`. Never translate vendor compatibility into a subscription allowance. See [reported usage](./docs/specs/terminal.md#reported-usage).
- **Approval choices and remember rules.** Observed 2026-10-23: the first `command_unguarded` prompt offers only Yes and No. Label the choices like Claude Code (Yes, Yes for this session, No), offer the session choice on the first eligible prompt, and show the sensitive-path warning first, in color. General command and MCP remember choices remain proposed under the [permission-rule contract](./docs/specs/tools.md#permission-rules); typed Git prefix rules shipped in #256, and primary host prefix rules are S2 of the [host-command plan](./planning/primary-host-commands/README.md).
- **Image attachment chips.** Pasted text chips are landed ([paste and attachments](./docs/specs/terminal.md#paste-and-attachments)). Remaining: clipboard and admitted-path PNG chips with `[Image #N]` selection, removal and renumbering, and restoration on conversation rewind. A dragged or pasted path resolves only inside the admitted Workspace root, read through a pinned no-follow snapshot ([D-24](./docs/research/next-step-decision-register.md#d-24--pasted-text-and-continuation-inputs)).
- **Copy from panels.** Observed 2026-10-23, Fullscreen on Konsole: text in `/usage`, `/settings` and other panels cannot be selected or copied. Make panel text selectable like History, as Claude Code's inline panels allow.
- **Short `/status`.** `/status` opens the `/usage` view. Make `/status` a one-screen summary like Codex's: model, tokens used, context left and plan limits remaining; keep the detail in `/usage`.
- **Drop the Mouse setting.** Mouse capture is a usability baseline, not a preference: remove the `/settings` Mouse row and its `wheel only` and `off` values, keeping `ARANY_DISABLE_MOUSE` as the unlisted escape hatch ([mouse contract](./docs/specs/terminal.md#selectors)).
- **One transcript key.** Ctrl+T and Ctrl+O open the same [transcript pager](./docs/specs/terminal.md#transcript-pager). Keep Claude Code's Ctrl+O and drop the Ctrl+T alias; Claude Code binds Ctrl+T to its task list.
- **Command row noise.** Observed 2026-10-23: Command rows print escaped `\u{000a}` instead of the script's first line, and every result appends zero counters (`0 oversized inputs, 0 discarded outputs`). Show the first line and only nonzero counters, like Claude Code's Bash rows.
- **Other terminal hosts (deferred).** Zellij and VS Code's xterm.js need Codex's insertion without partial scroll regions; `tmux -CC` needs forced Classic like Claude Code. Accept a contract and obtain native evidence before claiming support.

### Frozen areas

The owner lifted this freeze for the Worker views and Supervisor item, for the [typed Git writes](./docs/specs/tools.md#typed-git-writes) of Daily use first item 10 ([rationale](./docs/research/git-writes-2026-10.md)), and on 2026-10-22 for MultiEdit only in Daily use first item 11 ([MultiEdit contract](./docs/specs/tools.md#atomic-single-file-multiedit)). It stays in force for the rest of the list below until the owner lifts it.

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

## Account recovery and native Guard follow-ups

- **Affected 2026-10-08 macOS account fixture.** Reauthorize with a fresh `/setup` sign-in, or recover from an external backup of the lost registration/index metadata. The two validated 33-byte migration markers were backed up and removed under the account lock; Keychain and Session history were preserved. Markers and SQLite provenance cannot reconstruct the original metadata. Root relocation was removed on 2026-10-09; this does not restore that metadata.
- **Historical Linux seeds.** For an account with unrecorded `/tmp/arany-tools-*` roots or version-1 reservations, apply the [selected-object maintenance procedure](./docs/tools.md#historical-linux-seed-maintenance) before claiming cleanup. Its synthetic fixture never inventories an existing installation.

## Beta 2 and broader release

| Pending gate | Completion condition |
| --- | --- |
| Hosted CI | `Clippy`, `Linux Tests (debug)`, `Linux Tests (release)`, `macOS Tests (debug)`, `macOS Tests (release)`, `Native Linux Guard and tmux (x86_64)` and `(arm64)` (`linux_native_advanced=true`) must pass on the published head. Make the ignored Linux Guard test `retained_failed_unit_is_reset_before_capacity_is_reclaimed` pass: service names reset after successful and failed preparation, and refusal while unloading stays covered. [Workflow](./.github/workflows/ci.yml), [testing rules](./agents/testing.md) |
| macOS runtime mount topology | Busy-detach cleanup in `crates/arany-cli/tests/session_run/tools_macos.rs::guarded_runtime_profile_checks_mount_topology_before_dispatch` is unresolved. Diagnose it; repeated passing macOS CI is required before claiming the flake fixed. [Tool spec](./docs/specs/tools.md) |
| Native stores and clipboard | An intermittent `Locked` refusal of the authorized read after denial in `macOS Tests (debug)` needs diagnosis and repeated-CI stability. [Credentials](./docs/specs/credentials.md) |
| Readiness, load and observer faults | The intermittent macOS debug early stop in `guard_runs::permissions::permission_dispatch_modes_rules_command_mcp_and_edit` (`Guard execution/storage cleanup unconfirmed`) is unclassified. Use the named teardown stage with the permission-case context to separate connection, preparation, native-manager and cleanup failures before changing deadlines. Add focused tests for the trickling-Collector response-size refusal and the dropped incomplete-response future. [Tool spec](./docs/specs/tools.md), [whole-response deadline](./docs/specs/telemetry.md#export-bounds) |
| Distribution and dependency policy | [`deny.toml`](./deny.toml) ignores RUSTSEC-2026-0370/0371/0372 (unmaintained `zvariant`, `zbus_names` and `zvariant_derive`, reached through `keyring` and `secret-service` on `zbus 5`). Remove those ignores only when `Cargo.lock` no longer contains a `zbus` 5.x package. The [release-policy workflow](./.github/workflows/release-policy.yml) must pass for the release commit's `Cargo.lock`; complete the nested license and attribution materials in the [release review](./docs/security/beta-license-release-review.md#current-result). |
| Other platforms | `Linux ARM64 Tests (debug)` and `(release)` (`linux_arm64=true`) remain pending in CI. They need native `aarch64` hosts and Rust-target assertions, with `--cfg arany_ci` workspace tests, without emulation or skips. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities), [native CI procedure](./agents/references/testing-details.md#local-verification-and-ci) |

These gates remain open until their completion condition is met. Specs and existing tests describe what must hold; neither implemented code nor an old passing run establishes broader support.
