# Terminal dogfood fixes: approval choices

## Metadata

- Status: `Draft`
- Owner: `fpmirabile`
- Last updated: `2026-10-27`

## Outcome and scope

The 2026-10-23 dogfood pass observed the approval dialog's choices and warning order. After this plan, approval choices read `Yes`, `Yes, for this session` and `No` (plus the existing remembered-rule choices), the session choice appears on the first eligible `command_unguarded` prompt in Request approvals, an Auto edits host prompt without it says where to get it, and the sensitive-path warning leads the dialog in color.

No authority, journal, Event or reducer behavior changes. Exclusions: general command and MCP remember choices stay proposed under the [permission-rule contract](../../docs/specs/tools.md#permission-rules); typed Git prefix rules are unchanged. The Command row first line and nonzero counters (NEXT_STEPS "Command row noise") are a fully specified fix delivered as a direct PR outside this plan, per [one feature per plan](../PLANNING.md#rules); it is listed in NEXT_STEPS as a direct follow-up.

## Research

Code (this worktree, 2026-10-27):

- Choice labels live in `ToolApprovalChoice::label` ([tool_rows.rs:704](../../crates/arany-cli/src/presentation/tool_rows.rs)): `Yes`, `Yes, and don't ask again for: file edits`, `Yes, and don't ask again for: this identical command in this live session (unguarded)`, `Yes, and allow … in this folder from the next Run`, `No`. The set is built by `tool_approval_choices` (same file, after `host_prefix_proposal`); the session choice (`ExactCommand`, reply `AllowForSession`) is added only when `UnguardedPrompt::remember` is true. Linear rendering is pinned in [linear.rs:838-850](../../crates/arany-cli/src/terminal/linear.rs); the reply parser `linear_approval_choice` accepts `yes`, `no`, numbers ([approval.rs:21](../../crates/arany-cli/src/cli/attached/run/approval.rs)).
- `remember` is Engine policy, not presentation: `decide_host_reuse` ([permission.rs:129](../../crates/arany/src/engine/run_loop/step/permission.rs)) returns `OnceOnly` unless the host command is settings-admitted, the policy mode is default/dontAsk, the live terminal mode is `Request`, the client negotiated session scope, no rule/hook restriction applies, the target is not a StateRoot/account-root path and an epoch exists; `OfferSession` further requires fewer than `MAX_HOST_SESSION_GRANTS` and mode default. The spec states the same eligibility: [Authority](../../docs/specs/tools.md#primary-host-commands) names the attached choice set and says session approval is offered only where reuse is eligible, and [Remembered host prefixes](../../docs/specs/tools.md#remembered-host-prefixes) says "Auto/acceptEdits, including live terminal modes, require a fresh human and ignore prefix/session allows". AGENTS.md: "auto and acceptEdits require a fresh human, never AI review".
- Cause of the observed prompt (Yes/No only): the attached terminal's default mode is Auto edits ([terminal.md, modes](../../docs/specs/terminal.md#interactive-commands-and-completion): Shift+Tab cycles Auto edits → Request approvals → Auto approval), and `facts.terminal_mode != Some(Request)` (`permission.rs:137`) forces `OnceOnly`; a second possible cause is unadmitted permission settings (`settings_admitted`). Both are specified behavior, not defects (Q1).
- Peer divergence (priority 4): Claude Code offers "don't ask again" in acceptEdits (its acceptEdits covers only file edits) and Codex offers its session option whenever the core lists `AcceptForSession`; Arany offers session scope only with live Request approvals because AGENTS.md and the Remembered host prefixes paragraph require a fresh human in auto and acceptEdits. That spec sentence is the record of the divergence.
- Command row: `process` in [summary.rs:62](../../crates/arany-cli/src/presentation/tool_rows/summary.rs) builds `Command({program} {args joined})`; for `sh -c "<script>"` the whole multi-line script is one argument, and the row is escaped and width-bounded by `safe_truncate` ([model.rs:298](../../crates/arany-cli/src/presentation/model.rs), `MAX_ROW_BYTES` 1024) so the newline prints as `\u{000a}`; `escape_terminal` ([presentation.rs:955](../../crates/arany-cli/src/presentation.rs)) escapes detail lines (`tool_rows/tests.rs:429`; `:836-842` are `safe_truncate` cases) and these tests must stay for non-Command text.
- Counters: `command_result_summary` ([tool_rows.rs:228-290](../../crates/arany-cli/src/presentation/tool_rows.rs)) already omits the whole group when all four counters are zero, but when any is nonzero it prints all four (`1 oversized input, 3 discarded outputs` with zeros included, pinned at `tool_rows/tests.rs:796-852`). The observed "0 oversized inputs, 0 discarded outputs" therefore comes from a nonzero `ignored_entries`/`pruned_directories` printing every field. `selection: null` prints ` · selection counts unavailable`.
- Detail and copy text (`tool_detail_parts`, tool_rows.rs:670) carries the full `Arguments:` JSON, so truncating the row loses no inspectable data.

Peers:

- Claude Code ([permissions](https://code.claude.com/docs/en/permissions), read 2026-10-27): the Bash prompt offers `Yes`, `Yes, and don't ask again for: <command prefix>`, `Yes, and switch to auto mode` and `No`; "allow … for the rest of the session" is the file-edit prompt's option; a prompt "sometimes offers only a one-time approval" when it cannot show everything a saved choice would allow. Its "don't ask again" for Bash is permanent per repository. `Yes, for this session` is not a Claude Code label: it follows Codex's session option and the owner's wording (D1). Arany's session scope is deliberately narrower (identical command, live epoch) per the guarantee.
- Claude Code Bash rows: closed source, so no row-renderer source exists to inspect; its public documentation does not specify the row format. Recorded exception (no implementation-time research): the first-line and nonzero-counter rules are owner-requested behavior taken from the NEXT_STEPS statement of the peer, not verified peer behavior.
- Codex CLI, verified from source at `openai/codex` main commit `aeb6b22c8bda5f4d4b6ab80ebd3ccdb3f47fb3ab` (read 2026-10-27): `codex-rs/tui/src/bottom_pane/approval_overlay.rs` labels command approvals `Yes, proceed`, ``Yes, and don't ask again for commands that start with `<prefix>` `` (omitted when the prefix contains a newline) and `Yes, and don't ask again for this command in this session` (lines ~862-896), (also omitted on `\r`) and `No, and tell Codex what to do differently`; the session choice is a separate decision offered only when the core lists it as available. Its exec history cell (`exec_cell/render.rs`) wraps the first line, then shows at most two continuation rows (`command_continuation_max_lines`) under a `  │ ` gutter and an ellipsis line naming the omitted count (the `  └ ` gutter is the output block). Codex confirms the session wording pattern; the owner chose `Yes, for this session` (D1) and a first-line-only row (D2), a deliberate simplification of Codex's first-line-plus-two shape.

Contract and evidence owners: [tools.md Authority](../../docs/specs/tools.md#primary-host-commands) and [Remembered host prefixes](../../docs/specs/tools.md#remembered-host-prefixes) (choices); [terminal.md](../../docs/specs/terminal.md) (color, `NO_COLOR` at line 37); tests in `presentation/tool_rows/tests.rs`, `terminal/linear.rs`, `cli/attached/run/tests.rs`, the host journey in `crates/arany-cli/tests/guard_runs/unguarded.rs` and the reuse table tests in `permission.rs`.

## Contract and scenarios

- Spec change: [tools.md Authority](../../docs/specs/tools.md#primary-host-commands) names the attached choices ("once, identical-command approval for this live Session, an eligible proposed host prefix, and deny") and gains the displayed labels; [terminal.md](../../docs/specs/terminal.md) gains the leading colored sensitive-path warning and the Auto edits session hint. Nothing else moves.
- Success: the first eligible host prompt in Request approvals shows three numbered choices `Yes` / `Yes, for this session` / `No`; an Auto edits host prompt without the session choice shows the dim hint `Shift+Tab to Request offers session approval for later eligible commands`; a sensitive path shows the warning above the program line in color (plain text with `NO_COLOR`).
- Rejection/recovery: choices not offered by policy stay absent (rule-based ask, hook restriction, StateRoot target, Auto edits, Auto approval, 128 grants reached, exec, and any Serve or ACP client without the negotiated session scope; a Serve client that negotiated [host session scope](../../docs/specs/serve.md) keeps its existing behavior unchanged, as does ACP per its spec); a linear `yes` reply stays once-only although the second label starts with "Yes" (typed-reply mapping unchanged).
- Evidence owners: linear/fullscreen choice rendering in `terminal/linear.rs` and the attached run tests; reuse eligibility in `permission.rs`; end to end in the host journey. Deferred: real-terminal color and macOS observations go to NEXT_STEPS dogfood.

## Chosen direction

- Simplest sufficient solution: presentation-only edits. Labels are changed in `ToolApprovalChoice::label` and its pinned tests; the hint is a dim line on an Auto edits host prompt lacking the session choice. No policy widening: the guarantee (`auto`/`acceptEdits` require a fresh human) stands.
- Linux/macOS: no OS difference; color follows the existing `NO_COLOR` and capability rules. macOS observations of the colored dialog join the existing dogfood checklist.
- Wording decision D1 (below) covers `Yes, for this session` versus Claude Code's `Yes, and don't ask again for …`; the label must say "session" for the epoch-limited scope, because "don't ask again" would suggest permanent persistence like Claude Code's Bash rule.

## Security and resource model

No trust boundary, authorization, journal or reducer change. Command text and `justification` remain model-controlled data and stay escaped on every path; the dialog continues to list the full escaped program and every argument. The session choice is offered by the same `decide_host_reuse` outcome as today; the hint is static text. A [security pass](../../agents/security.md#security-pass-triggers) is required in A1 (approval flow) and A2 (approval warning); evidence is the unchanged reuse table and that the warning comes from the same `sensitive` fact for color and plain output. The existing 128-grant cap is the only bound; no new buffers. The Command row rendering (first line, `safe_truncate`, control-heavy overflow test) belongs to the direct R1 PR's own security pass.

## Risks and mitigations

- Relabeling hides the scope difference from Claude Code's permanent rule → label says "this session"; spec states the epoch limit.
- Auto edits users never see the session choice → dim hint names Request approvals; widening would need an AGENTS.md amendment and is not planned.

## Vertical slices

1. **A1: Approval labels, session offer and Auto edits hint (~400 lines).** Security pass. Relabels `Yes`/`Yes, for this session`/`No` and the remembered-rule labels consistently; adds the dim hint `Shift+Tab to Request offers session approval for later eligible commands` on an Auto edits host prompt lacking the session choice; the journey asserts the three-choice prompt on the first eligible Request-approvals ask; tests keep the linear `yes` reply once-only; updates `linear.rs:838-850` and attached-run tests; tools.md labels and terminal.md hint. Depends on no other slice.
2. **A2: Leading colored sensitive-path warning (~250 lines).** Security pass. In `unguarded_approval_lines` places the warning first with the warning style, plain under `NO_COLOR`; linear mode already prints it as plain text, so only its order changes there; terminal.md; fullscreen and linear tests. Built on A1's final choice layout.
3. **C1: Closing checklist (~60 lines).** One line per slice ID naming its merged PR, one per exit criterion with its command/evidence; delete this plan and its `planning/README.md` row; in NEXT_STEPS remove only the completed observations (labels, first eligible session offer, sensitive-path warning) and the plan links, and keep the "Command row noise" direct follow-up unless its PR has merged, and keep the general command and MCP remember-choice proposal (and any unmet native/color evidence) as its own item before the plan is deleted.

## Open questions

None open. Owner decisions:

- **Q1.** Cause: `decide_host_reuse` (`permission.rs:137`) offers session scope only with live Request approvals, and the attached default is Auto edits; the spec and AGENTS.md require a fresh human there. Decision: no policy change, dim hint only. If the same prompt lacks the choice in Request approvals with admitted settings, fix it as a defect.
- **D1.** Label `Yes, for this session`.
- **D2.** Command row shows the first non-blank line plus `…` when more remain; no `#!` rule. Delivered by the direct R1 PR.
- **D3.** R1 (Command row first line and nonzero counters) is a direct PR outside this plan; this plan keeps A1/A2/C1.

## Exit criteria

- A1: host journey shows the three-choice prompt on the first eligible Request-approvals ask and the hint in Auto edits; reuse table tests prove no other mode, channel or restriction gains the choice; labels pinned in linear and attached-run tests.
- A2: sensitive-path warning is the first dialog line, colored, plain under `NO_COLOR`.
- Docs: tools.md and terminal.md updated; `typos`, `git diff --check` and link checks pass; native color/macOS observation handed to the NEXT_STEPS dogfood list; closing PR deletes this plan under the [lifecycle](../README.md#lifecycle).
