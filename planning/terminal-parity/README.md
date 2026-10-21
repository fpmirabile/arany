# Terminal parity: scroll, copy and inline dialogs

## Metadata

- Status: `In progress`
- Owner: Fernando Mirabile
- Last updated: 2026-10-20

## Outcome and scope

The owner cannot use attached chat comfortably: scrolling does not work well, text cannot be copied reliably, and approvals, `/settings` and the other dialogs do not behave like Claude Code's inline dialogs. This plan makes both chat views behave like the reference peers:

- **Classic** works like Claude Code's classic renderer and Codex "Scrollback". It writes into native scrollback, never captures the mouse, and leaves scrolling, selection, find and copy to the emulator.
- **Fullscreen** works like Claude Code fullscreen and Codex "Fullscreen". It captures the mouse and owns selection and copy.

Dialogs become inline, numbered and Claude Code-style in both views, and each has a numbered linear equivalent. Classic is the default until Fullscreen selection and copy ship, and then Fullscreen becomes the default again, as Claude Code did.

The plan serves the harness's terminal presentation ([priority 1](../../AGENTS.md#product-and-implementation-priorities)). Every choice copies a peer ([priority 4](../../AGENTS.md#product-and-implementation-priorities)). A divergence remains only where one of the [guarantees](../../AGENTS.md#non-negotiable-guarantees) or a [global rule](../../AGENTS.md#global-rules) requires it, and [terminal.md](../../docs/specs/terminal.md) names it.

Out of scope:

- Folder consent (already full-area and inline) and the rewind panels.
- A `v`/`$EDITOR` transcript export. It would be a child process with the user's environment, and `arany show` already exports.
- Keyboard text selection (Shift+arrows). `/copy`, Classic and the transcript dump cover keyboard-only copying.
- Denial feedback to the model, such as Claude Code's "No, and tell Claude what to do differently". The [Tool contract](../../docs/specs/tools.md#folder-trust-and-approvals) says denial stops the Run.
- New hosts: Zellij, VS Code's xterm.js and `tmux -CC` (open question 4).

The investigation (in Spanish, outside the repository) is `harness-team/logs/terminal-scroll-copy-dialogs.html`. It has the peer table with primary sources. [Corrections to the investigation](#corrections-to-the-investigation) lists what it got wrong.

## Reported failures and evidence

Verified on `main` at 87f018f0. Paths are under `crates/arany-cli/src/` unless stated.

| # | Owner report | Cause in code |
|---|---|---|
| F1 | The mouse wheel does nothing in Fullscreen. | Ordinary chat never captures the mouse. `MouseCapture` (`terminal.rs:305-331`) is acquired only for Session-picker rows (`terminal.rs:1730-1731`) and agent-inspector choices (`terminal.rs:2003-2004`). `History::handle` scrolls on `PointerScrollUp/Down` (`terminal/history.rs:543-569`), but that input exists only under capture. Nothing enables DECSET 1007, so the wheel reaches the alternate screen as nothing, or as arrows the composer consumes. |
| F2 | Text cannot be copied, especially in Fullscreen. | Fullscreen draws on the alternate screen (`terminal.rs:652-654`). Each redraw can clear a native selection, and the content leaves no scrollback on exit. There is no app selection: the decoder maps only `Moved`, `Down(Left)` and the wheel, with no drag or release (`terminal/input.rs:541-558`). `clipboard.rs` only reads, for paste (`wl-paste` `:226`, `xclip` `:253`, `osascript` `:331`). Ctrl+Shift+C is dropped (`terminal/input.rs:587-595`), yet the idle Ctrl+C hint says "Ctrl+Shift+C copies" (`cli/attached.rs:1237`, `terminal/linear.rs:775`). The spec forbids clipboard OSC (`docs/specs/terminal.md:52`). |
| F3 | In Classic, PageUp jumps to another screen. | `handle_transcript_input` turns Classic PageUp and Ctrl+F into the transcript panel (`terminal.rs:1002-1008`). `open_transcript_panel` borrows the alternate screen (`terminal.rs:971-986`, `AlternateScreen::acquire` at `:976`). |
| F4 | In Classic, the composer scrolls away and the fixed block wastes rows. | The Classic viewport is a fixed `Viewport::Inline(min(rows, 12))` (`terminal/view.rs:390`, `terminal.rs:2123`). The spec pins "at most 12 rows" (`terminal.md:58`) and accepts that the emulator can move the composer out of view (`terminal.md:60`). |
| F5 | Approvals are not inline like Claude Code's. | `draw_tool_approval` draws a centered boxed panel (`terminal.rs:541-616`, `boxed: true`) with vertical `Deny` / `Allow once` choices, Deny first. Replies map by index (`cli/attached/active.rs:49-55`). Enter accepts only after every page (`active.rs:590-627`). The linear prompt asks for literal words (`terminal/linear.rs:30-45`). Contracts: `terminal.md:141,143`, `docs/specs/tools.md:39,352`. |
| F6 | `/settings`, `/model`, resume and Ctrl+K are panels, not inline numbered dialogs. | Settings rows: `cli/attached/controls.rs:114-140`. Session picker: `terminal/view.rs:1107`. Ctrl+K: `:1423`. Help: `:1520`. Model catalog: `:1589`. Agent inspector: `:1778`. None of them numbers its rows; only linear `/model`, resume and `/agents` use numbers. |
| F7 | The earlier fix (#175) traded scrolling for copying. | Commit 892a467b removed capture from ordinary chat and pinned that with `crates/arany-cli/tests/session_run/chat_mouse.rs:88-89`. Its description says: "Trade-off: Fullscreen history loses mouse-wheel scrolling; PageUp/PageDown remain." |

### Corrections to the investigation

- Ctrl+K, `/help` and the pickers do not borrow the alternate screen in Classic. They draw inside the 12-row inline viewport (`terminal.rs:1801-1805` → `draw_frame`). The only Classic borrow is the transcript panel (`terminal.rs:971-986`).
- Ctrl+O is already bound to newline (`terminal/input.rs:579`, `terminal.md:56,99`), so the Claude Code pager key conflicts with an existing binding. [D4](#decisions) resolves it.
- crossterm's `EnableMouseCapture` turns on `?1003` (any-motion) and `?1015` as well as `?1000/?1002/?1006`. Peer-style capture needs explicit modes.
- Selection needs decoder work as well as capture: drag and release events are not decoded today (`terminal/input.rs:541-558`).
- Clipboard transports must be root-owned binaries in fixed directories (`terminal/clipboard.rs:386-401`), and macOS admits only `/usr/bin`. Homebrew `tmux` (`/opt/homebrew/bin`, `/usr/local/bin`) is therefore never admitted as a transport.
- By default, `wl-copy`, `xclip` and `xsel` fork a process that keeps serving the selection after the client returns. That conflicts with the clipboard group-termination rule (`terminal.md:231`); see [D9](#decisions) and open question 2.

## Decisions

Each row is **decided by the owner, with peer parity**, unless marked otherwise. The guarantee column names what still constrains the behavior. Peer links are at the end of this section.

| # | Decision | Peer source | Guarantee or divergence |
|---|---|---|---|
| D1 | Keep two chat views, Classic and Fullscreen, selected by the `/settings` Chat view row and `--classic`. Linear and screen-reader output is not a third view. | Claude Code has classic plus fullscreen (`/tui`, the `tui` setting) [cc-full]. Codex has Scrollback plus Fullscreen [cx-picker], [cx-config]. | None. |
| D2 | Classic never captures the mouse and never borrows the alternate screen. PageUp, PageDown and the wheel are the emulator's. Arany consumes no PageUp/PageDown in Classic, and Ctrl+F shows a notice naming the terminal's own find. | Claude Code classic leaves scrolling to the emulator [cc-full]. Codex Scrollback: "Native terminal copy, paste and scrollback" [cx-picker]. | None. |
| D3 | The Classic viewport is sized to its content (composer, shelf, status and any open dialog or menu) and anchored to the bottom row. It grows for a dialog up to the screen height and shrinks back afterwards. When the user scrolls back natively the composer still leaves view; typing returns to it through the emulator's scroll-on-input. | Codex sizes its inline viewport to the bottom pane and inserts history above it [cx-tui], [cx-insert]. Claude Code classic behaves the same way. | None. The emulator owns scroll position in both peers. |
| D4 | Ctrl+O and Ctrl+T open the detailed transcript. In Classic it is appended to native scrollback. In Fullscreen it opens a pager with less-style keys, and `[` writes it to native scrollback. Newline moves from Ctrl+O to Ctrl+J; decoded Shift+Enter keeps working. | Claude Code uses Ctrl+O for its transcript, `[` to write native scrollback, and Ctrl+J for newline [cc-full], [cc-keys]. Codex uses Ctrl+T for its transcript [cx-tui]. | [Global rule 4](../../AGENTS.md#global-rules): the dump keeps `/transcript`'s 16-Run / 64 KiB bound and a cue naming `arany show`, where Claude Code writes the whole conversation. |
| D5 | Tool approval, `/settings`, `/model`, `/resume`, Ctrl+K, `/help` and `/agents` render inline in the bottom area: below history in Fullscreen, and in the grown viewport in Classic. There is no centered box. The composer is hidden while a dialog owns input and comes back unchanged. Fixed short lists number their rows, and a digit selects and activates. Filterable lists (`/model`, `/resume`) keep typed text as their filter. Every dialog has a numbered linear form. | Claude Code's permission, `/config` and `/model` dialogs [cc-perm], [cc-keys]. Codex's bottom-pane approval overlay [cx-approval]. | None. |
| D6 | The approval list follows Claude Code: `1. Yes`, then `2. Yes, and don't ask again for: …` only where an existing contract supports remembering, then `No`. Escape means No, Ctrl+C still cancels the Run, and a digit chooses and confirms. A guarded approval opens with Yes focused. "Don't ask again" offers file edits (switching to Auto edits, as Shift+Tab does, and only when the Ask came from the Request approvals mode), and, for an ordinary unguarded re-run, the existing exact-command rule. | Claude Code's "Yes / Yes, and don't ask again for … / No", Esc = No, and "allow all edits during this session" [cc-perm]. Codex: "Yes, proceed" / "Yes, and don't ask again…" / No [cx-approval]. | Four divergences, each required by a guarantee. **(a)** Yes stays unavailable until every page has been shown. Per-effect isolation: the verdict binds the complete immutable intent. Safe AI cage: model text must not push the effect off the first page. **(b)** An unguarded re-run keeps No focused. It is the only exception to the Guard, and [tools.md](../../docs/specs/tools.md#unguarded-re-run-after-a-guard-failure) already names this. **(c)** Input that arrived before the dialog's first frame is discarded, not applied. Journal-first: a verdict must answer the shown intent, and an Enter meant as a steering note must not approve. **(d)** Linear mode takes a number or `yes`/`no`, and empty Enter means No, because linear output has no visible focus. This matches Claude Code's screen-reader "Select with numbers" [cc-a11y]. |
| D7 | Fullscreen captures the mouse by default with `?1000`, `?1002` and `?1006` (SGR), never `?1003` or `?1015`. The wheel scrolls history 3 rows. Selection works by click-drag, double-click (word) and triple-click (line). Copy-on-select is on by default. Ctrl+Shift+C, and Ctrl+C while a selection is active, also copy. A one-time hint names native selection: Shift-drag, Option-drag in iTerm2, Fn-drag in Terminal.app. | Claude Code fullscreen: its own selection, copy on release (`copyOnSelect`), Ctrl+Shift+C or Ctrl+C with a selection, and the Shift/Option/Fn hint [cc-full]. Codex: `text_selection`, `copy_on_select` and its copy keys [cx-copy]. | None. Cmd+C never reaches Arany, because Arany enables no keyboard-enhancement mode (`terminal.md:105`). |
| D8 | Add a `/settings` row "Mouse": `wheel and clicks` (default), `wheel only` or `off`. `ARANY_DISABLE_MOUSE=1` forces `off`. With capture off in Fullscreen, Arany enables DECSET 1007, and Up/Down scroll history only while the composer is empty and no menu, picker or draft recall is pending. | Claude Code's `CLAUDE_CODE_DISABLE_MOUSE` and `CLAUDE_CODE_DISABLE_MOUSE_CLICKS` [cc-env]. Codex enables alternate scroll (1007) when it is not capturing [cx-alt]. OpenCode's `tui.mouse` [oc-tui]. | None. |
| D9 | Clipboard writes happen only on a user gesture: selection release, a copy key, or `/copy`. They go to the first destination that works: a native tool when local (`pbcopy`; `wl-copy`; `xclip` or `xsel`), the tmux buffer inside tmux, and OSC 52 over SSH or when nothing else succeeded. The payload is at most 100,000 bytes, and a toast names the destination. `/copy` copies the latest committed answer. | Claude Code: native tool, then the tmux buffer, then OSC 52 as the SSH fallback [cc-full]. Codex: native, tmux, OSC 52 in SSH or as fallback with a 100,000-byte cap, and `/copy` [cx-copy]. | Safe AI cage: an explicit spec exception allows output-only, gesture-only OSC 52 ([security model](#security-and-resource-model)). |
| D10 | Classic becomes the default in slice 1. Fullscreen becomes the default again in slice 8, after selection and copy ship. Saved `view` values are honored either way. | Claude Code moved new users to fullscreen by default once it was ready [cc-full]. | None. |
| D11 | Screen-reader and linear output stays a separate accessibility setting and fallback (`TERM=dumb`, fewer than eight rows, unavailable geometry, an unanswered cursor query). It never uses Fullscreen. | Claude Code: "Screen reader mode is opt-in" and it always uses the classic renderer [cc-a11y]. Gemini's `ui.accessibility.screenReader` [gm-settings]. | Required by `terminal.md:30`. |

[cc-full]: https://code.claude.com/docs/en/fullscreen
[cc-keys]: https://code.claude.com/docs/en/interactive-mode
[cc-perm]: https://code.claude.com/docs/en/permissions
[cc-a11y]: https://code.claude.com/docs/en/accessibility
[cc-env]: https://code.claude.com/docs/en/env-vars
[cx-picker]: https://github.com/openai/codex/blob/main/codex-rs/tui/src/chatwidget/tui_mode_picker.rs
[cx-config]: https://github.com/openai/codex/blob/main/codex-rs/config/src/types.rs
[cx-tui]: https://github.com/openai/codex/blob/main/codex-rs/tui/src/tui.rs
[cx-insert]: https://github.com/openai/codex/blob/main/codex-rs/tui/src/insert_history.rs
[cx-alt]: https://github.com/openai/codex/blob/main/codex-rs/tui/src/tui/alternate_screen.rs
[cx-copy]: https://github.com/openai/codex/blob/main/codex-rs/tui/src/clipboard_copy.rs
[cx-approval]: https://github.com/openai/codex/blob/main/codex-rs/tui/src/bottom_pane/approval_overlay.rs
[oc-tui]: https://opencode.ai/docs/tui/
[gm-settings]: https://geminicli.com/docs/cli/settings/

## Contract and scenarios

[terminal.md](../../docs/specs/terminal.md) is the one home for every behavior here. Approval presentation now lives partly in [tools.md](../../docs/specs/tools.md#folder-trust-and-approvals) (line 39) and in [unguarded re-run](../../docs/specs/tools.md#unguarded-re-run-after-a-guard-failure) (line 352): the focused choice, choice labels, linear words and "boxed, paged panel". Slice 3 moves those presentation facts to terminal.md. tools.md keeps the authority semantics (one-use, expiry, which choices exist for each origin, page review) and links to terminal.md. Each slice edits only the sentences it changes. [Testing](../../agents/testing.md) decides what counts as evidence.

Scenarios. Each is owned by the slice that introduces it.

- **Classic native scroll (S1).** At startup Arany writes no `?1049h`, no mouse mode and no `?1007h`. PageUp leaves the frame byte-identical. Ctrl+T appends bounded detail rows inline with a limit cue.
- **Classic viewport (S2).** An empty composer takes only its own rows. A dialog grows the block, and closing it shrinks the block back without leaving stale rows. Small-screen insertion keeps the tmux single-row-region workaround (`terminal.rs:835-845`).
- **Approval (S3).**
  - On a one-page action, `1` approves and Escape denies.
  - On a multi-page action, `1` before the last page gives "Review the whole action first (PgDn)" and records no verdict.
  - An Enter typed before the dialog's first frame records no verdict.
  - Resize restarts review. Ctrl+C cancels the Run and dispatches nothing.
  - Linear mode accepts `1`, `yes` or `no`; empty Enter denies.
  - An unguarded re-run opens with No focused.
- **Selectors (S4).** Each dialog opens inline, Escape restores draft and caret, and linear mode shows numbered rows. Classic pickers emit no mouse mode.
- **Copy (S5).**
  - `/copy` and gestures reach the first working destination, and the toast names it.
  - Over the limit, the whole copy is refused with "Selection exceeds 100 KB; use `arany show`".
  - With no destination: "Copy unavailable: REASON". The selection is kept and nothing is retried.
  - A cleanup failure follows the existing unconfirmed-cleanup rule.
  - A model answer containing `ESC ] 52 ; c ; …` renders escaped, and the writer never runs during that Run.
- **Fullscreen capture (S6).**
  - Drag copies the selected text without rails or role headings, joining wrapped rows.
  - Selection anchors survive streaming, appended content and resize.
  - Every exit path disables `?1000/?1002/?1006` and `?1007`: exit, error, suspend, signal, unwind, setup, switch to Classic, and linear fallback.
  - `ARANY_DISABLE_MOUSE=1` enables no mouse mode and turns on 1007.
- **Pager (S7).** `/` find, `n`/`N`, `j`/`k`, `g`/`G`, Ctrl+U/Ctrl+D, PageUp/PageDown, and `q` or Escape all work. `[` leaves the alternate screen and releases capture, writes the bounded transcript, waits for Enter, then reacquires. The Run keeps going, and Ctrl+C still cancels.
- **Default (S8).** A fresh StateRoot starts in Fullscreen with capture. `--classic` and a saved `classic` start in Classic.

Evidence owners:

- Frame corpus: `terminal/view/tests.rs` and the inline cases in `terminal.rs` tests.
- Decoder table: `terminal/input.rs`. Linear corpus: `terminal/linear.rs`.
- Selection and pager corpus: `terminal/history.rs`. Transport tables: `terminal/clipboard.rs`.
- Linux PTY: `crates/arany-cli/tests/session_run/chat_mouse.rs`, `active_terminal.rs`, `session_picker.rs`, `tools.rs`.
- macOS PTY (debug lane only): `crates/arany-cli/tests/session_run/active_terminal_macos.rs`, `tools_macos.rs`.
- Native tmux lanes: `crates/arany-cli/tests/session_run/active_terminal/tmux.rs`.
- Deferred live and assistive-technology checks go to [NEXT_STEPS](../../NEXT_STEPS.md#terminal-continuity-evidence).

## Execution

There are eight slices, one PR each. Stacked PRs are fine ([Git](../../agents/git.md#pull-requests)). Order: Classic scroll and copy (1-2), inline dialogs (3-4), Fullscreen copy, capture and pager (5-7), then the default flip (8). Slices 3 and 4 depend on 2. Slice 6 depends on 5, slice 7 on 6, and slice 8 on 5-7. Every slice applies [Ponytail](../../.agents/skills/ponytail/SKILL.md), [rust-clean-code](../../agents/skills/rust-clean-code/SKILL.md), [clean-architecture-review](../../agents/skills/clean-architecture-review/SKILL.md) and the [terminal design skill](../../agents/skills/arany-terminal-design/SKILL.md), and follows the [PTY fixture rules](../../agents/references/testing-details.md#terminal-process-fixtures). Heavy native gates use `cfg_attr(not(arany_ci), ignore)` and run in CI.

Tests that assumed a Fullscreen default seed `ui-preferences.json` with `view: fullscreen` through one shared fixture helper in slice 1. No `--fullscreen` flag is added.

### Slice 1: Classic by default, native scroll and transcript dump

- **Scope.**
  - `TerminalMode`'s `#[default]` moves to `Classic` (`terminal.rs:62-68`). Saved version-3 `view` values are honored.
  - In Classic, PageUp, PageDown and Ctrl+F no longer open a panel (`terminal.rs:1002-1008`). PageUp/PageDown are ignored. Ctrl+F shows "Use your terminal's find; Ctrl+T writes details to scrollback".
  - Ctrl+T, Ctrl+O and `/transcript` in Classic append the bounded detail text (16 Runs, 64 KiB, limit cue) through `insert_native`. Delete the Classic branch of `open_transcript_panel` and the `parked_terminal` / alternate-screen borrow (`terminal.rs:971-986`).
  - Ctrl+O maps to Transcript and Ctrl+J to Newline (`terminal/input.rs:578-579`). Raw mode decodes LF separately from CR. Hints are updated.
  - The cold-projection cue names `arany show` instead of Ctrl+T as the complete history (`terminal.md:52`).
- **Specs (terminal.md).** Lines 15 and 56 (default), 52 (Classic borrow sentence removed, cue), 60 and 74 (Classic keys), 99 (newline key) and scenario rows 291 and 315.
- **Tests.**
  - Decoder table: Ctrl+O gives Transcript; Ctrl+J and Shift+Enter give Newline.
  - Linux PTY row in `active_terminal.rs`: default start writes no `?1049h` and no mouse mode, PageUp writes nothing, and Ctrl+T writes detail rows with no `?1049h`.
  - The same assertion as a row in `active_terminal_macos.rs`.
  - tmux Classic journeys stay green.
  - Linear: `/transcript` output is unchanged (existing corpus).
- **OS impact.** In Terminal.app, iTerm2 and Linux terminals, scrolling, selection, find and copy become the emulator's own. In tmux, scrollback is tmux copy-mode. Over SSH, nothing changes. Terminal.app and iTerm2 send Ctrl+J as LF; the decoder table pins this.

### Slice 2: content-sized Classic viewport

- **Scope.**
  - Replace the fixed 12 rows (`terminal/view.rs:390`, `terminal.rs:2123`) with the rows the bottom area needs, anchored to the screen's last row.
  - Grow for completion menus and dialogs up to the screen height, and shrink after.
  - Default to recreating the inline `Terminal` at the new height through the existing reacquire path, which costs one cursor query. Use an owned viewport area like Codex only if a PTY shows flicker or stale rows.
  - 64-row insert batches and the small-screen path are unchanged.
- **Specs (terminal.md).** Line 58 ("at most 12 rows" becomes "sized to content, at most the screen height") and line 60 (composer return through emulator scroll-on-input).
- **Tests.**
  - Inline frame cases at heights 8, 12, 24 and 50, plus grow and shrink.
  - Linux PTY: shrink leaves no stale rows, and cleanup uses the owned area.
  - macOS PTY row: the same.
  - tmux: `active_shelf_stays_above_retained_draft_in_tmux` and the pane-resize journey.
- **OS impact.** Terminal.app, iTerm2 and VTE scroll to the bottom on keystroke by default. In tmux, a key leaves copy-mode only per the user's tmux configuration. Both limits are documented, not worked around.

### Slice 3: inline numbered Tool approval

- **Scope.**
  - `draw_tool_approval` (`terminal.rs:541-616`) draws an inline block instead of `boxed: true`. It shows the operation title, the complete sanitized action (paged with "↓ N more lines · PgDn"), "Do you want to proceed?" and numbered rows under [D6](#decisions).
  - Replies map by choice identity, not index (`cli/attached/active.rs:49-55`).
  - Discard typeahead from before the first frame.
  - Shift+Tab is unchanged. "Don't ask again for: file edits" sets Auto edits through the existing cycle and records the verdict as one-use `AllowOnce`.
  - Linear: numbered rows and "Select 1-N, yes or no; empty Enter denies:" (`terminal/linear.rs:30-45`, `active.rs:57-65`).
  - The dialog draws only in the bottom area under trusted chrome, and digits act only while it has focus. History rows cannot draw there.
- **Specs.** terminal.md lines 141 and 143 hold the dialog. tools.md lines 39 and 352 keep authority and review rules and link to terminal.md for focus, labels and linear words.
- **Tests.**
  - Frame corpus at 120, 80, 50 and 16 columns: one-page, multi-page, unguarded and hostile action text with controls.
  - Linux `session_run/tools.rs`:
    - `1` approves and dispatches.
    - A pre-last-page digit gives no verdict.
    - Escape denies with no dispatch.
    - Typeahead Enter dispatches nothing.
    - Linear `1` and `no`.
    - Unguarded No focus and remember.
  - macOS: approval rows in `tools_macos.rs`.
- **Security pass.** Changed approval behavior ([triggers](../../agents/security.md#security-pass-triggers)); see the [security model](#security-and-resource-model).
- **OS impact.** Rendering only. Digits arrive identically on every target.

### Slice 4: inline selectors and help

- **Scope.**
  - `/settings`, `/model`, `/resume`, Ctrl+K, `/help` and `/agents` render as inline lists (`terminal/view.rs:1107,1423,1520,1589,1778`, `controls.rs:114-140`).
  - Ctrl+K numbers its four actions, and a digit activates.
  - Linear `/settings` changes from `key=value` to numbered rows with `NUMBER VALUE`, like `/model`'s `NUMBER [EFFORT|default]`. Linear Ctrl+K keeps the slash commands.
  - Classic drops transient picker capture (`terminal.rs:1730-1731,2003-2004`). Fullscreen keeps it until slice 6.
- **Specs (terminal.md).** Lines 133-137, 167, 169, 179, 187, 191 and 195, and scenario rows 301, 316, 317 and 319.
- **Tests.**
  - Frame corpus for each dialog at 80, 50 and 16 columns and 8 and 24 rows.
  - Linear corpus numbered forms, including `NUMBER VALUE` rejection.
  - Linux `session_picker.rs`: Classic emits no mouse mode, and the Fullscreen picker mouse still selects. `model_catalog.rs`, `slash_completion.rs`.
  - macOS: `native_resume_picker_restores_mouse_and_terminal_without_changing_history` is updated per mode.
- **OS impact.** None beyond rendering.

### Slice 5: clipboard writer and `/copy`

- **Scope.**
  - `terminal/clipboard.rs` gains a write path. A `CopyRequest` can only be built by the input layer from a gesture event or by `/copy` submission.
  - Destination order follows [D9](#decisions); each destination is tried at most once, with no retry.
  - Native clients reuse `installed()` admission, a cleared environment and the two-second deadline. `pbcopy` also gets `LANG=en_US.UTF-8`, because it decodes stdin by locale. A Linux serving client runs in its own new process group: Arany waits for its foreground exit and does not kill the serving descendant (open question 2).
  - tmux: `$TMUX`'s socket is validated like the Wayland socket (absolute, owned by the user, a socket), then `tmux load-buffer -w -` runs from an admitted binary.
  - OSC 52: `ESC ] 52 ; c ; BASE64` with a fixed terminator, written by the terminal owner on stderr and never in linear mode.
  - `/copy` joins the compiled registry and copies the latest committed answer's sanitized display text.
  - Nothing is journaled, and nothing enters Provider context or diagnostics content.
- **Specs (terminal.md).**
  - Line 52: replace "No mode enables … title/clipboard OSC" with the exception in the [security model](#security-and-resource-model).
  - Line 127: the registry count and row count.
  - Lines 231 and 235: write transports and the serving-client rule.
  - Line 70: clipboard cues.
  - Public limits: "Clipboard copy | 100,000 bytes; larger refuses whole".
- **Tests.**
  - `clipboard.rs` tables: destination selection (local, SSH, tmux, Wayland, X11, macOS), cleared environment, the 100,000-byte boundary, sanitization and base64 encoding.
  - Linux PTY row with `SSH_CONNECTION` set and no transports: `/copy` writes exactly one OSC 52 sequence on stderr. A scripted answer carrying OSC 52 renders escaped, and no `ESC]52` appears before the gesture (canary).
  - macOS `active_terminal_macos.rs` under `arany_ci`: a `pbcopy` round trip of non-ASCII text, compared through `pbpaste`.
  - Linear: `/copy` uses native or tmux only, and reports "OSC 52 unavailable in linear mode" otherwise.
- **Security pass.** A new child-process path, rendered output control and model-adjacent content ([triggers](../../agents/security.md#security-pass-triggers)).
- **OS impact.**
  - macOS: local `pbcopy` is admitted. Terminal.app does not support OSC 52, so over SSH it cannot copy and the notice suggests Fn-drag or Classic. iTerm2 blocks OSC 52 until "Applications in terminal may access clipboard" is enabled.
  - Linux: VTE terminals do not write OSC 52; kitty, foot, WezTerm, Ghostty and Alacritty do.
  - tmux: `load-buffer -w` needs tmux 3.2 or later. Homebrew tmux on macOS is not admitted, so inside tmux OSC 52 needs `set -g set-clipboard on`.
  - SSH: OSC 52 is the only route.

### Slice 6: Fullscreen capture, selection and copy

- **Scope.**
  - Global capture in Fullscreen writes `?1000h?1002h?1006h` directly, replacing crossterm's `EnableMouseCapture`.
  - The decoder adds `Down`, `Drag` and `Up` for the left button. Double and triple clicks are detected as the same cell within 500 ms.
  - Selection is anchored to History source rows and highlighted with inverse video, so it does not depend on color. Dragging at the edge scrolls one row per motion event.
  - Copy follows [D7](#decisions). With copy-on-select, the highlight clears once the copy succeeds, so the next Ctrl+C cancels as usual. Escape clears a selection.
  - The one-time hint is chosen from `TERM_PROGRAM`, with a generic list over SSH or tmux. Inside tmux it adds "wheel scrolling needs `set -g mouse on`".
  - `/settings` gains "Mouse" and "Copy on select" rows, and `ARANY_DISABLE_MOUSE` is read. 1007 is used when capture is off ([D8](#decisions)).
  - `ui-preferences.json` moves to version 4. Under [global rule 5](../../AGENTS.md#global-rules), a version-3 record gives the existing "unsupported settings; use /settings to replace them" (`controls.rs:316-320`).
  - Transient picker capture is removed, because capture is now global.
  - The idle Ctrl+C hint in Fullscreen with capture becomes "Select text with the mouse to copy; Shift-drag for terminal selection. Ctrl+C again exits" (`cli/attached.rs:1237`).
- **Specs (terminal.md).** Lines 52, 74, 121 ("unowned mouse events"), 133 (rows), 195 (capture rule rewritten), 245, 250 (Escape order) and 256 (restoration includes 1007), the limits row for preferences v4, and scenario rows 319, 325 and 327.
- **Tests.**
  - Decoder table: drag, release, double and triple click, and coordinate bounds.
  - History corpus: anchors survive append and resize; copied text excludes rails and joins wrapped rows; the 100,000-byte stop happens before allocation.
  - Frame corpus: the highlight with `--no-color`.
  - Rewrite the Linux `chat_mouse.rs`:
    - Fullscreen enables exactly 1000, 1002 and 1006, never 1003 or 1015, and disables them at every returned checkpoint.
    - Classic enables none.
    - `ARANY_DISABLE_MOUSE` enables none and turns 1007 on.
  - macOS: mode bytes and restoration on SIGTSTP and exit in `active_terminal_macos.rs`.
  - tmux: wheel scrolling in Fullscreen with `mouse on`.
- **Security pass.** Changed input decoding. Mouse bytes are bounded input from the user's terminal and grant nothing.
- **OS impact.**
  - Terminal.app: mouse reporting is on by default (View → Allow Mouse Reporting), and Fn-drag selects natively.
  - iTerm2: mouse reporting is a per-profile setting, and Option-drag selects natively.
  - Linux terminals: Shift-drag.
  - tmux: needs `set -g mouse on` for the wheel.
  - SSH: mouse bytes travel unchanged.

### Slice 7: Fullscreen transcript pager

- **Scope.**
  - Ctrl+O and Ctrl+T in Fullscreen open the existing detailed History as a modal pager with the keys in [S7](#contract-and-scenarios). Ctrl+F stays as an alias for `/`. Selection and copy work inside it.
  - `[` releases capture and the alternate screen and appends the bounded escaped transcript (16 Runs, 64 KiB, cue) to the primary screen. It then shows "Enter returns to Arany" and reacquires.
  - The dumped text stays in the shell's scrollback after exit.
- **Specs (terminal.md).** Lines 56, 66 and 74.
- **Tests.**
  - Pager key table in `history.rs`. Frame corpus.
  - Linux PTY: `[` emits `?1049l` and the mode disables, then bounded text, then `?1049h` and the modes after Enter, while a scripted Run keeps streaming.
  - macOS PTY row: the same.
  - Linear: `/transcript` is unchanged.
- **OS impact.** Terminal.app and iTerm2 keep the dump in scrollback. In tmux the dump enters pane history (copy-mode). Over SSH it works unchanged.

### Slice 8: Fullscreen by default

- **Scope.** `TerminalMode`'s `#[default]` moves back to `Fullscreen`, and the seeded fixtures drop the explicit view. The slice lands after slices 5-7 with green CI. Under [workflow 2](../../AGENTS.md#workflow), synthetic evidence is enough, though the owner may wait for the live checklist below.
- **Specs (terminal.md).** Lines 15 and 56.
- **Tests.** The startup PTY rows from slice 1 flip their expectation, on Linux and on macOS.
- **OS impact.** As in slice 6.

## Security and resource model

The [security pass](../../agents/security.md#security-pass-triggers) applies to slices 3, 5 and 6.

- **Assets.** The user's clipboard, the terminal's mode state and approval verdicts.
- **Trust boundaries.** Model, Tool, MCP, Skill, hook and repository text reaches the screen only through the existing escaping (`terminal.md:34`). User gestures from the sole raw reader are the only source of clipboard writes and approval choices.
- **Attacker-controlled inputs.**
  - Model or Tool text carrying OSC 52, other escape sequences, or text that imitates a dialog.
  - Hostile file names in actions.
  - Inbound OSC 52 replies, which stay discarded (`terminal/input.rs:996` test).
  - The environment variables used to choose a transport (`TMUX`, `WAYLAND_DISPLAY`, `DISPLAY`, `SSH_CONNECTION`, `SSH_TTY`, `TERM_PROGRAM`). They pick only a transport or hint text, after validation.
- **Authority source and immutable intent.** A copy is authorized only by a `CopyRequest` built from a raw-reader gesture or a submitted `/copy`. Its payload is a snapshot of already-escaped display text taken at gesture time. There is no API through which content, hooks, Skills, MCP or the Engine can request a copy. Approval verdicts still bind the immutable intent digest. The changes are presentation only: [D6](#decisions) (a) and (c), the trusted bottom-area chrome, and digits acting only while the dialog has focus.
- **Slice 3 security pass.** The Engine supplies only whether Request approvals alone caused a primary typed-edit Ask; rule, settings-mode, hook, child and AI-review fallback Asks cannot offer that mode shortcut. The presentation adapter maps fixed choice identities to existing replies; file edits still return AllowOnce. Intent normalization, grant admission, expiry, journal-before-dispatch, platform Guard enforcement and child restrictions keep their current owners. No new credential access, descendant, persistent format, egress, inference, cost or storage path is introduced; the existing finite action and input bounds apply. The pending dialog hides the composer, draws into a rectangle disjoint from history and discards pre-frame input without discarding cancellation. Failure stays denial, cancellation, expiry or no verdict while review is incomplete. This does not claim protection against a malicious terminal emulator or native macOS/assistive-technology evidence. Frame, reader, linear and native approval corpora own verification; test execution is CI-only for this slice.
- **Spec exception (replaces `terminal.md:52`'s clipboard sentence).** "OSC 52 is written only on an explicit user copy gesture (selection release, a copy key or `/copy`), never because of model, Tool, MCP, Skill, hook or repository content. It is output-only: Arany never emits the `?` query and discards inbound OSC 52. The payload is the user-selected display text, already escaped, with C0/C1 controls other than LF and TAB removed, base64-encoded under the fixed `c` selection, at most 100,000 bytes, and never written in linear mode. No mode enables focus reporting or title OSC." It names Claude Code and Codex as the peers.
- **Descendant authority.**
  - Copy clients get a cleared environment holding only the selected display or runtime variables (plus `LANG` for `pbcopy`). They get no credentials, network settings or home directory as configuration. They read only the payload on stdin.
  - A serving client receives nothing beyond the payload and exits when another owner takes the selection.
- **Sensitive data.** The copied text may include secrets the user chose to select. Diagnostics record only the destination and byte count, never content. Hidden key input never renders, so it cannot be selected.
- **Bounds.**
  - Copy payload: at most 100,000 bytes, refused whole above that.
  - Clipboard worker: one at a time for reads and writes, with a two-second deadline and a one-second cleanup (existing).
  - Destinations: at most three attempts, one each.
  - Selection materialization stops at the cap.
  - Mouse handling is per event with no timers. Edge scrolling is one row per event. The double-click window is 500 ms.
  - Transcript dump: 16 Runs and 64 KiB.
  - Hints: once per process.
  - Preferences: 1 KiB (unchanged).
- **Failure mode.** A failed copy keeps the selection, gives a notice and is never retried automatically. A copy over OSC 52 says "sent to terminal (OSC 52); it may be ignored", because Arany cannot confirm delivery (honest uncertainty). Capture and 1007 are released on every exit path before control returns.
- **Security non-claims.** Arany does not claim that the terminal honored OSC 52, that the clipboard stays private from other local applications, or that Shift-drag works in every emulator.

## Risks and mitigations

- **Capture brings back #175's complaint.** Mitigations:
  - Selection and copy ship in the same slice as capture, with the writer one slice earlier.
  - The Shift-drag hint.
  - Mouse `off` and `ARANY_DISABLE_MOUSE`.
  - Classic stays the default until slice 8.
- **Ctrl+O no longer inserts a newline.** Mitigations: the composer hint names Ctrl+J, and Shift+Enter works where it is decoded.
- **A reflexive Enter approves.** Mitigations: typeahead is discarded, Yes is gated on the last page, and an unguarded re-run keeps No focused.
- **Model text imitates a dialog.** Mitigations: dialogs draw only in the bottom area under trusted chrome, history is escaped and indented, and digits act only while a dialog has focus.
- **Ctrl+C with a selection copies instead of cancelling.** Mitigation: with copy-on-select on, the highlight clears once the copy succeeds; Escape also clears it.
- **1007 makes arrows ambiguous.** Mitigations: it applies only to an empty composer with nothing pending. Releasing it may turn off a terminal default (xterm `alternateScroll`); this is documented.
- **tmux differences** (versions before 3.2, `mouse off`, `set-clipboard`). Mitigations: the fixed destination order and the hints.
- **Grow/shrink flicker in Classic.** Mitigations: PTY evidence, and the owned-area fallback in slice 2.
- **Preference record v4.** The owner re-saves `/settings` once. A reverted binary refuses v4 with the same recovery notice, which is accepted pre-beta practice.
- **Rollback.** Revert the slice PR. No Event or journal format changes.

## Open questions

1. **Guarded "don't ask again" for commands and MCP.** Claude Code writes an allow rule. Arany has user-authored [permission rules](../../docs/specs/tools.md#permission-rules) in `permissions.json`, but no dialog writes them, and [next steps item 9](../../NEXT_STEPS.md#daily-use-first) plans exact-command rules. Recommendation: add the option with item 9 under tools.md. Until then the dialog offers it only for edits and unguarded exact rules.
2. **Linux serving clipboard clients.** `wl-copy`, `xclip` and `xsel` keep a process alive that serves the selection after Arany returns, and after Arany exits. This is the peer behavior. Recommendation: accept it as the one named exception to clipboard group termination (own process group, cleared environment, payload only). The alternative is OSC 52 only on Linux, which loses VTE terminals.
3. **Live view switch from `/settings`.** Codex asks for a restart and Claude Code relaunches. Arany already switches in place (`terminal.rs:1013-1029`) through the same reacquisition that suspension needs. Recommendation: keep it.
4. **Other hosts.** Zellij and VS Code's xterm.js need Codex's no-partial-region insertion, and `tmux -CC` needs forced Classic, as in Claude Code. Recommendation: out of scope, recorded in NEXT_STEPS.

## Exit criteria

- All eight slices are merged with green CI. Each scenario S1-S8 has its named owner, including the rewritten `chat_mouse.rs`.
- terminal.md describes the current behavior. tools.md approval lines link to it. NEXT_STEPS ["Terminal continuity evidence"](../../NEXT_STEPS.md#terminal-continuity-evidence) replaces its Classic scrollback item with the live checks below. Native macOS and assistive-technology claims stay deferred there.
- The owner runs this live checklist on macOS (Terminal.app and iTerm2) and on one Linux terminal, locally and over SSH, in a trusted project. Results go to NEXT_STEPS until stable pre-beta.
  1. Classic: the wheel and Shift+PageUp scroll natively, a drag plus the terminal's copy key copies, PageUp changes nothing, and the composer takes only its rows.
  2. Classic: Ctrl+T writes details into scrollback, and the terminal's find locates older text.
  3. An approval appears inline and numbered. `1` approves a short command. A long one needs PgDn before Yes. Escape denies.
  4. `/settings`, `/model`, `/resume` and Ctrl+K appear inline. With `--screen-reader`, each takes numbers.
  5. Fullscreen: the wheel scrolls, and a drag copies with a toast naming the destination; paste it elsewhere. Shift-drag (Option in iTerm2, Fn in Terminal.app) selects natively. Over SSH in an OSC 52 terminal, and in tmux with `mouse on`, copy works.
  6. `ARANY_DISABLE_MOUSE=1`: native selection works, and the wheel scrolls history.
  7. Ctrl+O pager: `/` finds, and `[` dumps to scrollback; Enter returns.
  8. An answer that contains an OSC 52 sequence renders inert and leaves the clipboard unchanged.
- Closeout follows the [planning lifecycle](../README.md#lifecycle): delete this plan when slice 8 lands.
