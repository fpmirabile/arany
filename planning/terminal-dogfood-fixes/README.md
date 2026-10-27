# Terminal dogfood fixes: approval choices and command rows

## Metadata

- Status: `Draft`
- Owner: `fpmirabile`
- Last updated: `2026-10-27`

## Outcome and scope

Two observations from the 2026-10-23 dogfood pass share one surface, the attached terminal's Tool presentation, and one acceptance test (a dogfood Run that asks to run a command): the approval dialog and the Command result row. After this plan:

- Approval choices read `Yes`, `Yes, for this session` and `No` (plus the existing remembered-rule choices), the session choice appears on the first eligible `command_unguarded` prompt, and the sensitive-path warning leads the dialog in color.
- A Command row shows the script's first line instead of an escaped `\u{000a}` run, and a result row shows only nonzero selection counters.

No authority, journal, Event or reducer behavior changes. Exclusions: general command and MCP remember choices stay proposed under the [permission-rule contract](../../docs/specs/tools.md#permission-rules); typed Git prefix rules are unchanged. Two observations in one plan departs from [one feature per plan](../PLANNING.md#rules); D3 asks the owner to confirm, and the two slice families are independent so either can be split out unchanged.

## Research

Code (this worktree, 2026-10-27):

- Choice labels live in `ToolApprovalChoice::label` ([tool_rows.rs:704](../../crates/arany-cli/src/presentation/tool_rows.rs)): `Yes`, `Yes, and don't ask again for: file edits`, `Yes, and don't ask again for: this identical command in this live session (unguarded)`, `Yes, and allow … in this folder from the next Run`, `No`. The set is built by `tool_approval_choices` (same file, after `host_prefix_proposal`); the session choice (`ExactCommand`, reply `AllowForSession`) is added only when `UnguardedPrompt::remember` is true. Linear rendering is pinned in [linear.rs:838-850](../../crates/arany-cli/src/terminal/linear.rs); the reply parser `linear_approval_choice` accepts `yes`, `no`, numbers ([approval.rs:17](../../crates/arany-cli/src/cli/attached/run/approval.rs)).
- `remember` is Engine policy, not presentation: `decide_host_reuse` ([permission.rs:129](../../crates/arany/src/engine/run_loop/step/permission.rs)) returns `OnceOnly` unless the host command is settings-admitted, the policy mode is default/dontAsk, the live terminal mode is `Request`, the client negotiated session scope, no rule/hook restriction applies, the target is not a StateRoot/account-root path and an epoch exists; `OfferSession` further requires fewer than `MAX_HOST_SESSION_GRANTS` and mode default. The spec states the same eligibility ([tools.md, Authority](../../docs/specs/tools.md#primary-host-commands): "Session approval is offered only where reuse is eligible … `auto` and `acceptEdits` always need a fresh human").
- Consequence for the observed prompt (Yes/No only): the attached terminal's default mode is Auto edits ([terminal.md, modes](../../docs/specs/terminal.md#entry-modes-and-channels): Shift+Tab cycles Auto edits → Request approvals → Auto approval), and `facts.terminal_mode != Some(Request)` forces `OnceOnly`. Whether Auto edits, a missing epoch or settings admission caused the 2026-10-23 prompt is not established from the note; slice A1 begins with a reproducing test that names the cause (see Q1).
- Command row: `process` in [summary.rs:62](../../crates/arany-cli/src/presentation/tool_rows/summary.rs) builds `Command({program} {args joined})`; for `sh -c "<script>"` the whole multi-line script is one argument, and the row path escapes control characters (`escape_status_text`, [presentation.rs:955](../../crates/arany-cli/src/presentation.rs)) so the newline prints as `\u{000a}`. Tests pin the escape for other rows (`tool_rows/tests.rs:429,839`) and must stay for non-Command text.
- Counters: `command_result_summary` ([tool_rows.rs:228-290](../../crates/arany-cli/src/presentation/tool_rows.rs)) already omits the whole group when all four counters are zero, but when any is nonzero it prints all four (`1 oversized input, 3 discarded outputs` with zeros included, pinned at `tool_rows/tests.rs:796-852`). The observed "0 oversized inputs, 0 discarded outputs" therefore comes from a nonzero `ignored_entries`/`pruned_directories` printing every field. `selection: null` prints ` · selection counts unavailable`.
- Detail and copy text (`tool_detail_parts`, tool_rows.rs:670) carries the full `Arguments:` JSON, so truncating the row loses no inspectable data.

Peers:

- Claude Code ([permissions](https://code.claude.com/docs/en/permissions), read 2026-10-27): the Bash prompt shows the model's description, the command, "This command requires approval", then `Yes`, `Yes, and don't ask again for: <command prefix>` (and for edits "allow … for the rest of the session"), `No`; a prompt "sometimes offers only a one-time approval" when it cannot show everything a saved choice would allow. Its "don't ask again" for Bash is permanent per repository, for file edits until session end. Arany's session scope is deliberately narrower (identical command, live epoch) per the guarantee; only the wording and placement follow the peer.
- Claude Code Bash rows: closed source, so no row-renderer source exists to inspect; its public documentation does not specify the row format. Recorded exception (no implementation-time research): the first-line and nonzero-counter rules are owner-requested behavior taken from the NEXT_STEPS statement of the peer, not verified peer behavior.
- Codex CLI, verified from source at `openai/codex` main commit `aeb6b22c8bda5f4d4b6ab80ebd3ccdb3f47fb3ab` (read 2026-10-27): `codex-rs/tui/src/bottom_pane/approval_overlay.rs` labels command approvals `Yes, proceed`, ``Yes, and don't ask again for commands that start with `<prefix>` `` (omitted when the prefix contains a newline) and `Yes, and don't ask again for this command in this session` (lines ~862-896), and `No, and tell Codex what to do differently`; the session choice is a separate decision offered only when the core lists it as available. Its exec history cell (`exec_cell/render.rs`) wraps the command onto continuation lines under a dim `└` gutter rather than printing an escaped newline run; it documents no first-line rule. Codex confirms the session wording pattern; the owner-requested `Yes, for this session` label is kept (D1).

Contract and evidence owners: [tools.md Authority](../../docs/specs/tools.md#primary-host-commands) and [Remembered host prefixes](../../docs/specs/tools.md#remembered-host-prefixes) (choices); [terminal.md](../../docs/specs/terminal.md) (rows, color, `NO_COLOR` at line 37); tests in `presentation/tool_rows/tests.rs`, `terminal/linear.rs`, `cli/attached/run/tests.rs`, the host journey in `crates/arany-cli/tests/guard_runs/unguarded.rs` and the reuse table tests in `permission.rs`.

## Contract and scenarios

- Spec change: [tools.md Authority](../../docs/specs/tools.md#primary-host-commands) names the attached choices ("once, identical-command approval for this live Session, an eligible proposed host prefix, and deny") and gains the displayed labels; [terminal.md](../../docs/specs/terminal.md) gains the Command row first-line and nonzero-counter rule and the leading colored sensitive-path warning. Nothing else moves.
- Success: a guarded `sh -c "make\ntest"` row prints `Command(sh -c make …)` with an ellipsis marker and no `\u{000a}`; a result with only `ignored_entries: 7` prints `7 ignored entries`; the first eligible host prompt in Request approvals shows three numbered choices `Yes` / `Yes, for this session` / `No`; a sensitive path shows the warning above the program line in color (plain text with `NO_COLOR`).
- Rejection/recovery: choices not offered by policy stay absent (rule-based ask, hook restriction, StateRoot target, Auto edits, Auto approval, 128 grants reached, exec, and any Serve or ACP client without the negotiated session scope; a Serve client that negotiated [host session scope](../../docs/specs/serve.md) keeps its existing behavior unchanged, as does ACP per its spec); the model-supplied first line is escaped (a lone `\r` or ESC in it is shown escaped, never executed); `selection: null` still says counts are unavailable; detail view and copy keep the full escaped script.
- Evidence owners: row tests in `tool_rows/tests.rs`; linear/fullscreen choice rendering in `terminal/linear.rs` and the attached run tests; reuse eligibility in `permission.rs`; end to end in the host journey. Deferred: real-terminal color and macOS observations go to NEXT_STEPS dogfood.

## Chosen direction

- Simplest sufficient solution: presentation-only edits. Labels are changed in `ToolApprovalChoice::label` and its pinned tests; the counter fix filters `command_result_summary` to nonzero fields; the first line is taken from the unescaped argument before escaping, with one named width constant. The only Engine question is Q1: if the cause is that Auto edits forbids session reuse, the guarantee (`auto`/`acceptEdits` require a fresh human) stands and the fix is a prompt-time hint, not a policy widening.
- Linux/macOS: no OS difference; color follows the existing `NO_COLOR` and capability rules. macOS observations of the colored dialog join the existing dogfood checklist.
- Wording decision D1 (below) covers `Yes, for this session` versus Claude Code's `Yes, and don't ask again for …`; the label must say "session" for the epoch-limited scope, because "don't ask again" would suggest permanent persistence like Claude Code's Bash rule.

## Security and resource model

No trust boundary, authorization, journal or reducer change. Command text, the first line and `justification` remain model-controlled data and stay escaped on every path; the first line is extracted raw and passed through the existing `safe_truncate` ([model.rs:298](../../crates/arany-cli/src/presentation/model.rs)), which escapes per grapheme and enforces both the cell width and `MAX_ROW_BYTES`, so escape expansion cannot overflow the row; control-heavy overflow input (ESC, NUL, bidi, long runs) is a required test. Showing only the first line must not hide authority-relevant content in the approval dialog: the dialog continues to list the full escaped program and every argument; only the history row is shortened. The session choice is offered by the same `decide_host_reuse` outcome as today. A [security pass](../../agents/security.md#security-pass-triggers) is required in R1 (untrusted Command text rendering), A1 (approval flow) and A2 (approval warning); evidence is the unchanged reuse table, a test that a `\n`-carrying first line cannot inject a row, and that the warning comes from the same `sensitive` fact for color and plain output. Bounds: first-line width and the existing 128-grant cap are named constants; no new buffers.

## Risks and mitigations

- Relabeling hides the scope difference from Claude Code's permanent rule → label says "this session"; spec states the epoch limit.
- First-line display hides a malicious second line → approval dialog and detail view keep the complete escaped argument; the row is a summary only.
- Q1 turns out to need policy change → stop and update this plan (RPI) rather than widening host reuse.

## Vertical slices

1. **R1: Command row first line and nonzero counters (~250 lines).** Security pass; raw first-line extraction then `safe_truncate`, with a control-heavy overflow test. `summary.rs` `process` shows the first non-empty script line with an ellipsis marker when more lines exist; `command_result_summary` prints only nonzero counters; updates tests at `tool_rows/tests.rs:796-852` and adds multi-line and ESC cases; terminal.md row rule. Linear and fullscreen rows share the code, so one path.
2. **A1: Approval labels and first-eligible session offer (~400 lines).** Security pass. Starts with the failing journey that reproduces Q1; relabels `Yes`/`Yes, for this session`/`No` and the remembered-rule labels consistently; if Q1's cause is a presentation gap, fixes it here; updates `linear.rs:838-850` and attached-run tests; tools.md labels. Security pass. Depends on no other slice.
3. **A2: Leading colored sensitive-path warning (~250 lines).** Security pass. In `unguarded_approval_lines` places the warning first with the warning style, plain under `NO_COLOR`; terminal.md; fullscreen and linear tests. Built on A1's final choice layout.
4. **C1: Closing checklist (~60 lines).** One line per slice ID naming its merged PR, one per exit criterion with its command/evidence; delete this plan and its `planning/README.md` row; in NEXT_STEPS remove only the completed observations (labels, first eligible session offer, sensitive-path warning, row first line and counters) and the plan links, and keep the general command and MCP remember-choice proposal (and any unmet native/color evidence) as its own item before the plan is deleted.

## Open questions

- **Q1 (owner: owner; recommend: confirm by test).** Why did the first prompt lack the session choice? Code says Auto edits (the default) forces once-only. Recommendation: keep the guarantee, reproduce in Request approvals; if reproduced there, fix as a defect; if only Auto edits, show a dim hint `Switch to Request approvals (Shift+Tab) to allow for this session` instead of changing policy.
- **D1 (owner).** Label wording. Recommend `Yes, for this session` (matches the request and the epoch scope) over `Yes, and don't ask again for this command this session`.
- **D2 (owner).** First line for `sh -c` scripts: recommend skipping blank lines and a leading `#!` line, and showing `…` when more remain.
- **D3 (owner).** Keep both observations in one plan (recommended; separate slice families) or split into two plans.

## Exit criteria

- R1: row tests show first line and nonzero counters; no `\u{000a}` in Command rows (grep of the row tests); detail/copy keep the full script.
- A1: host journey shows the three-choice prompt on the first eligible Request-approvals ask; reuse table tests prove no other mode, channel or restriction gains the choice; labels pinned in linear and attached-run tests.
- A2: sensitive-path warning is the first dialog line, colored, plain under `NO_COLOR`.
- Docs: tools.md and terminal.md updated; `typos`, `git diff --check` and link checks pass; native color/macOS observation handed to the NEXT_STEPS dogfood list; closing PR deletes this plan under the [lifecycle](../README.md#lifecycle).
