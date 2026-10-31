# Terminal: drafts, paste and attachments

## Draft editing and submission

Editing works on whole graphemes (combined emoji and accents count as one). Long drafts wrap into at most four visible rows around the caret, and the heading marks a clipped window.

| Input | Effect |
|---|---|
| Left/Right, Backspace/Delete | Whole-grapheme movement or deletion |
| Up/Down | Logical-line movement at a preserved grapheme column |
| Ctrl+Left/Right | Previous or next whitespace-delimited chunk; punctuation in paths and model IDs stays in the chunk |
| Alt+Left/Right | On an empty draft, switch [agent chats](./selectors.md#agent-chats); otherwise inert |
| Ctrl+W | Delete the same preceding chunk |
| Typed backslash immediately followed by Enter | Replace that backslash with a newline |
| Decoded Shift+Enter, selected Alt/Ctrl+Enter, or Ctrl+J | Insert a newline without submitting |
| Enter while idle | Submit through local validation, or complete an open slash-name menu |
| Enter during admission or catalog loading | Retain a task draft; never queue another Run |
| Enter during an admitted Run | Offer the draft as a steering note, or retain it as a task draft; never queue another Run |
| Enter during compaction | Retain all text, including slash commands; execute nothing |

Backslash then Enter is the portable newline. Only a backslash typed just before Enter counts: two backslashes then Enter keep one literal backslash and add a newline, and any other key, paste or caret move in between cancels it. Pasted or restored trailing backslashes stay literal. Dialogs, approvals, search and read-only child views never use it.

Visual modes negotiate the kitty keyboard protocol on each acquisition so modified Enter keys can be told apart; without it, legacy input and backslash then Enter remain. The keyboard mode is undone whenever Arany leaves the screen. No modifyOtherKeys mode or host terminal configuration is enabled automatically. The modified shortcut is recommended once it is known to work, with backslash then Enter still shown; newline settings report whether enhanced keys were negotiated. CSI-u and modifyOtherKeys chords work whenever received, and Konsole's default Shift+Enter (`ESC O M`) inserts a newline. Plain CR always means Enter, because an emulator or multiplexer can lose any modifier. The newline setting describes manual CSI-u mappings (CSI `13 ; 2 u` for Shift, `3` for Alt, `5` for Ctrl) and tmux's extended-key requirement.

Linear/screen-reader chat submits with Enter. A received line ending in backslash continues the draft with a newline instead. Ctrl+D after nonempty input continues without a newline; empty Ctrl+D exits on an empty draft and gives Enter and Ctrl+C guidance on a retained one. An oversized or invalid line is rejected whole without submitting a prefix. Linear entry and settings say `Modified newline shortcuts apply only to visual sessions; use backslash then Enter here`. Unframed terminal paste cannot be told from typing, so a pasted line ending in backslash continues and later lines can submit; guidance says so and recommends `/paste` for multiline text.

Local rejections (unknown slash syntax, invalid arguments, an oversized rename, a capacity rejection or an incomplete selection) keep text and caret for correction and do no credential, State, Workspace or Provider I/O. Once State was touched or an effect attempted, a failure does not restore the command, promise rollback or authorize resubmission.

During an admitted Run, valid slash controls give read-only inspection, a locked notice or `/goal pause`. Next-Run defaults and Session changes stay locked; `/exit` explains Ctrl+C cancellation first until the Run's outcome is committed.

Enter during an admitted Run offers the composer text as a steering note; with attached images it reports `Steering notes are text only; draft retained`. While waiting it shows `Steering note waits for the agent's next step; editing withdraws it`; once the Engine records it for the next primary call it shows `Steering note sent to the agent` and clears the composer if it still holds that text. Editing withdraws an untaken note with `Steering note withdrawn; draft retained`. When the Run's notes would exceed [8 KiB](../limits.md#max_steering_note_bytes) it reports `No room for more steering notes this Run`; a single note over [8 KiB](../limits.md#max_steering_note_bytes) reports `Steering note exceeds 8.0 KB; draft retained`; both keep the draft. Feedback never repeats note text. An undelivered note returns as a draft at Run end.

In linear mode, Enter hands the note off and clears the line; further lines while it waits merge into the same note after a blank line. `/status` and `/goal pause` run normally. A draft kept by a refusal (including images) is held for the Run, and further text stays in the composer with `Both drafts retained; wait for the Run to finish`. At Run end the held draft returns ahead of newer text if both fit; otherwise both survive in the [draft recovery slots](#paste-and-attachments): submit or clear the current draft, then use Up.

`/goal pause` asks the Run to pause at its next step under the Goal's [pause boundaries](../goals.md#steering-notes-and-pause): the notice reads `Pausing at the next step; Ctrl+C cancels now`, and the status row shows `Pausing at the next step... Ctrl+C cancels now` until the Run ends. A repeat reports it is already pausing; idle `/goal pause` reports that no Goal is running; `/goal pause` with other text during a Run is refused as a locked Goal change.

Compaction keeps draft, caret and history through success, failure, interruption, resize and suspension. Ctrl+K is locked; Ctrl+C interrupts compaction without clearing the draft. Automatic compaction follows a confirmed Run receipt with a visible warning. Interruption does not guarantee rollback of a queued append or remote usage.

Draft feedback counts graphemes; byte bounds still apply, so no fixed character maximum is promised. Capacity errors ask the user to shorten the input or paste a smaller section.

Input is never silently dropped: if the input backlog overflows, input closes with an explicit error. Malformed or unterminated mouse reports are discarded whole, so none of their bytes reach the draft, a dialog or an approval.

## Paste and attachments

Bracketed and clipboard pastes are bounded by the [public paste limits](./entry-and-modes.md#public-limits-and-compatibility). After line endings become LF, a paste of more than 1,000 characters becomes an atomic `[Pasted Content N chars]` chip; a shorter paste is ordinary typed text, joins command and mention discovery and counts against the [64 KiB](../limits.md#max_user_message_bytes) draft cap. Chips count only against the expanded-draft cap. Equal-size pastes are labeled ` #2`, ` #3` outside the bracket. There is no chip count limit. Text that merely looks like a chip label is ordinary text. Collapsing changes display only, not model input. Child-message pastes always stay editable text without chips, under the [child-message draft limit](./entry-and-modes.md#public-limits-and-compatibility).

Left/Right steps over a chip. At its start, Tab opens its full text read-only; Up/Down, PageUp/PageDown and Home/End scroll, and Escape or Enter closes. Delete/Backspace removes the whole chip. Linear mode prints the compact and expanded content.

Command and file completion are off while the draft holds a text chip, and a command mixed with chips is refused locally with the draft intact; slash, Skill, path and mention syntax inside chips is data. Mention scanning reads at most 64 KiB<!-- unrelated-bound: mention scanner input cap --> of editable text. Submission expands chips into the ordinary objective. `/goal continue` reuses the original Goal's recorded includes ([Goals](../goals.md#budget-continuation)). Accepted text survives reopen, fork and replay; unsubmitted text and chips live only in the process.

Cancelling admission, or a failure before acceptance, restores text, chips and images. If acceptance cannot be confirmed, a modal recovery dialog holds the submitted draft: Check history restores it on confirmed non-acceptance, drops it on confirmed acceptance and keeps holding it otherwise. Enter cannot resubmit while acceptance is unknown; Exit discards the draft and closes the Session. Nothing resubmits automatically. During a Run a large paste stays a draft and cannot bypass the [steering bound](../limits.md#max_steering_note_bytes). Drafts that cannot merge stay recoverable with Up after submitting or clearing the current one; ordinary recall keeps one draft. The active, held, recalled and pending slots each keep at most one message (256 KiB text and 768 KiB raw images total). Draft content never appears in notices, diagnostics or telemetry before submission.

In linear mode, an unexpected bracketed paste is rejected whole, including any Enter or slash inside it, and an unterminated one closes input without submitting anything.

<a id="framed-paste-deadline"></a>

In visual modes a bracketed paste inserts one block into the draft, also while work, compaction or catalog loading runs. Line endings become LF and tabs are kept. An oversized or invalid-UTF-8 paste is rejected whole. Paste never submits, completes a command or activates a selector; modal pickers ignore it. A paste left unterminated for ten seconds closes the terminal session. Secret and Workspace fields require explicit Enter.

Ctrl+V, Ctrl+Shift+V or `/paste` reads the OS clipboard once. Unsupported transport, a hostile payload or a failure gives local feedback and leaves the draft unchanged. Clipboard data is untrusted text until submitted and never acts as keys, file authority or a Run. Enter while loading retains rather than submits. Opening a picker or find, suspension or losing the terminal discards a late result; Ctrl+C still cancels a Run. Linear mode shows a complete `Draft (not sent)` preview.

Clipboard tools come only from fixed system locations, never from `PATH`, a remote host, an arbitrary command or Provider content: Wayland or local X11 clients on Linux; on macOS `osascript` for paste (PNG preferred over text; other image formats refused) and `pbcopy` for copy; and tmux for copy. They run with a cleared environment, without credentials or network settings. Reads and copies have a two-second deadline. A copy client on Linux (`wl-copy` or `xclip`) keeps serving the selection in the background, even after Arany exits. One clipboard operation runs at a time; another is refused until it finishes. If a clipboard process cannot be confirmed stopped, the result is `clipboard cleanup unconfirmed`, never a payload, and later reads report it until restart. Nothing retries automatically.

The composer shows attachments as `[Image #N]` chips. Clipboard PNGs and pasted `.png` paths share the admission limits; paths follow the [PNG path bound](./entry-and-modes.md#pasted-png-path-limit). Text containing `://` or control characters stays text even when it ends in `.png`; URLs are never fetched. A pasted path is read only if it is a regular file inside the Workspace on the same filesystem, reached without symlinks or mount crossings, and the Workspace is still the one admitted; otherwise the draft is unchanged. A path that does not exist stays pasted text. At most four PNGs totaling 192 KiB are admitted, also with empty text. Animated PNGs and unknown critical chunks are refused, and an accepted image is not proof the model can read it. Admitted bytes are sent unchanged; filenames, URLs and terminal image escapes are never admitted. Overflow rejects a whole image, never a truncated one. Custom routes stay text-only.

When chips do not fit the header, it shows the selected chip or the image count. Left/Right selects image chips at the text boundary; Delete/Backspace removes the selected image and the rest renumber. Human history shows image metadata only; canonical history, forks and JSONL keep the bytes. Compaction keeps image metadata and the assistant's earlier interpretation, not pixels.

<a id="image-hover-preview"></a>

Hovering an image chip in Fullscreen with mouse capture, outside tmux, shows a bordered preview balloon above the composer, keeping the image's aspect ratio, when at least four rows fit. It uses the Kitty graphics protocol, Sixel, or the iTerm2 protocol (`TERM_PROGRAM` `iTerm.app` or `WezTerm`, or `LC_TERMINAL` `iTerm2`), whichever the terminal supports; Kitty and iTerm2 decode the PNG themselves, outside Arany's security boundary, and Sixel is decoded locally within the [preview limits](./entry-and-modes.md#public-limits-and-compatibility). Detection never changes terminal or tmux configuration. A decode error or overflow just shows no preview; the attachment is unaffected and nothing is logged or sent. Leaving the chip, resize, opening any panel, mode switch, suspension and exit remove the preview. Classic, linear, screen-reader, disabled-mouse and tmux sessions draw no preview.

Idle `/copy` copies the latest answer's source Markdown; it is locked during a Run. A Fullscreen selection copies the Markdown source behind the selected text, so selecting a whole answer returns its original Markdown; a double click on plain words copies just the word. Classic emulator selection copies the rendered text.

A copy goes to the first destination that works, each tried at most once:

1. A native client when not over SSH (no `SSH_CONNECTION`, `SSH_CLIENT` or `SSH_TTY`): `wl-copy` (Wayland) or `xclip` (X11) on Linux, `pbcopy` on macOS. `xsel` is not used.
2. Inside tmux, `tmux load-buffer` (tmux 3.2 or later), only through a private socket owned by the user.
3. OSC 52.

Homebrew tmux on macOS is never used. Copied text is never journaled, sent to a Provider or logged. OSC 52 depends on the emulator: Terminal.app and VTE ignore it; iTerm2 requires "Applications in terminal may access clipboard"; Kitty, foot, WezTerm, Ghostty and Alacritty support it. Over SSH, Terminal.app users need Fn-drag or Classic; inside Homebrew tmux on macOS, OSC 52 needs `set -g set-clipboard on`.
