# Terminal: entry, modes and restoration

## Entry, modes and channels

| Entry | Observable contract |
|---|---|
| `arany [PROMPT]` | Needs terminal stdin and stderr. Without a prompt the composer stays local until the first submission; a prompt starts the first Run after admission and leaves chat open. Fullscreen (default) draws on stderr in the alternate screen; Classic uses the primary screen. Redirected output follows [terminal continuity](./conversation.md#terminal-continuity-and-provisional-text). |
| `--continue`, `--resume`, `--fork` | Explicit Session entry; Workspace or TTY matching never resumes work. A missing conversation, or a fork source without a completed Run, fails before the prompt naming `arany --resume` or `/resume` and `--state-dir`. |
| `--profile PRESET` | Explicit private roster selection for attached mode or exec, supplying the primary tuple. Supplied Provider/model/effort fields must match; a present role-routing record conflicts. No preset is read by default. |
| `--setup` | Resolves folder access before setup. Cancellation before account save creates no Session. Idle `/setup` works inside the current conversation. |
| `exec --output text` | Noninteractive. Without `--provider` and `--profile` it uses the saved attached-mode selection, or names the missing `--provider`/`--model` or `/model` choice; a missing `--effort` uses the effort saved for that Provider and model. Stdout holds only the sanitized committed answer; stderr the Session/Run/status receipt. A failed Run exits unsuccessfully without an invented answer. A hook-stopped Run reports `Status: stopped_by_hook` and `error: Run stopped by hook[: REASON]`. An interrupted or hook-stopped Run whose Goal stays open adds `Goal paused; resume with arany exec --session-id SESSION --resume-goal GOAL` to stderr and exits unsuccessfully. An autonomous Goal stopping after a committed Run keeps that answer on stdout, adds `error: Goal stopped after a committed Run: REASON` to stderr and exits unsuccessfully. |
| `exec --output jsonl` | Stdout holds the latest Run's committed Events, its Goal's lifecycle and final report, and any new Session fact ([Goal selection](../goals.md#current-format-replay-and-exec)). Stderr stays empty on success, except that roster mode first writes its route manifest there; an interrupted Run adds the same pause line. |
| `acp` | [ACP spec](../acp.md) owns the editor subprocess, capabilities and core text turns. |
| `serve --stdio`, `generate-schema [--out DIR]` | [Serve spec](../serve.md) owns protocol framing, schema export and the server surface. |
| `storage [--workspace PATH] [--state-dir PATH]` | Local text inventory without a Provider, account or Session; see [Storage report](./commands.md#storage-report). |
| `show --output text\|jsonl SESSION_OR_RUN_ID` | Deterministic read-only replay: opens only existing State, reads no credential and renders valid history byte-identically. A Session ID exposes its direct canonical Events and fork reference; a Run ID only that Run. Invalid or conflicting ownership fails without output. If strict replay rejects the Session with `InvalidHistory` or `InvalidSnapshot`, it scans the journal within [the generated Event scan limit](../limits.md#max_list_scan_events) and prints the valid prefix, the rejecting sequence and cause, a lenient tail with a fixed marker for undecodable payloads and a notice that the projection is not resumable, then exits unsuccessfully; JSONL puts a `notice` object first. Exceeded bounds and Store errors fail without output. |
| `show --output supervisor-corpus SESSION_ID` | Session calibration JSONL on stdout; the [Supervisor adapter](../tools/hooks.md#supervisor-adapter) owns format and limits. Refusal exits unsuccessfully with empty stdout and a fixed diagnostic on stderr. |

Headless `exec` output projects committed history only; Server notifications never become output lines. `exec --tools` follows the [host policy](../tools/host-commands.md#primary-host-commands): admitted host prefixes may run in default/dontAsk, every unresolved host ask refuses at once, and it cannot create rules. Text, JSONL, show and Goal receipts keep the unguarded and non-rewindable effect markers.

An ineligible bare invocation fails before work and directs the caller to `exec`. Headless objective prefixes such as `/`, `@` and `!` are literal data, except that `exec --tools` and `--prompt` treat `/name [task]` as an invocation when `name` is an admitted Skill and not a compiled command (`serve` has no slash form). Diagnostics never fall back to stdout, and a missing stderr never changes the exit class.

`--screen-reader` or `ARANY_SCREEN_READER=1` selects labeled append-only linear output with canonical input, without cursor rewriting, boxes or animation. `TERM=dumb`, fewer than eight rows or unknown geometry selects the same fallback. Resizing never switches presentation; only an idle `/settings` chat-view change switches between Classic and Fullscreen.

`--no-color` and `NO_COLOR` remove color; labels, selection markers, focus and bracketed hints keep their meaning. Choices are vertical rows, never horizontal button groups. Message bodies are unstyled except recognized leading slash tokens and assistant [Markdown](./conversation.md#conversation-progress-and-history). Role headings are `› You:` and `● Arany:`, with a rail beside user text; attached committed answers use `Arany · Answer:` and headless `exec` `Answer:`. Linear mode keeps plain role labels. The terminal background stays user-owned.

Human output escapes or rejects terminal control sequences (ANSI/ECMA-48, OSC, C0/C1, carriage return, backspace, bidirectional controls) and forged line prefixes. JSONL comes only from a serializer, one object per line. Linear and deterministic output is unstyled and never rewrites lines.

Roster selection displays the complete escaped [manifest](../provider/selection.md#named-roster-presets), with native API / ChatGPT plan / custom unknown-pricing labels and every fallback, before credential access or Workspace loading: on stderr when headless, and as a notice for each attached submission. The flag is the selection; account/plan consent is still required. `/new` and `/clear` keep it. Conflicting `/model` or `/setup` defaults cannot substitute a roster route; restore the preset tuple or start without `--profile`. Roster-Goal resume requires the matching selector ([Sessions](../session.md#roster-recovery-and-compaction)).

## Human-readable measures

Human-facing sizes show exact bytes below 1 KB (`500 B`), otherwise one decimal KB/MB/GB/TB with a 1024 base (`38.3 KB`). Draft and text counts use characters or lines where those are the measure. JSON/JSON-RPC (`serve`, ACP), JSONL including `exec`, journals/Events, schema fixtures and diagnostics keep exact machine units (integer bytes and milliseconds).

Durations use `h`, `min` and `s`, omitting zero parts except `0 s`, with millisecond precision: `0.001 s`, `1.5 s`, `2 min`, `1 h 30 min`. Limits are never rounded away. Large counts use digit grouping (`12,340 lines`) or are marked approximate.

## Interruption, suspension and restoration

Primary Tool approval prompts never time out while the terminal and Run are live, including during suspension or in tmux/screen. No records a human decline and stops the Run; Escape and Ctrl+C cancel the whole Run. EOF, SIGHUP/shutdown and lost terminal I/O withdraw the request without a verdict and restore the composer. A child approval stays pending through idle waits, then withdraws when shared delegation or Goal active time runs out, naming that cause apart from human denial. Completed effects remain.

| State/action | Required outcome |
|---|---|
| Ctrl+C on folder consent | Exit and restore the terminal without selecting read-only or saving trust |
| Idle Ctrl+C with a draft | Clear the draft |
| Idle Ctrl+C on empty input | Explain copying (in Fullscreen with mouse selection, `Select text with the mouse to copy; GESTURE for terminal selection` with the emulator's selection gesture; otherwise the emulator's Ctrl+Shift+C); a second press within two seconds exits |
| Ctrl+C during selected-Provider admission | Return to chat with the draft retained and no Run receipt or retry; delegated account work may still complete |
| `/goal pause` during a Run | Pause at the next step; the in-flight call completes and a dispatched action is not aborted; Ctrl+C afterwards cancels now |
| First Ctrl+C during a Run | Request graceful whole-Run cancellation, even while a Fullscreen selection is shown |
| Second Ctrl+C during cancellation | Bounded local shutdown; report only the strongest durably confirmed state |
| Escape | Closes the focused panel first; otherwise cancels a pending clipboard read, then clears a Fullscreen selection, then closes the completion menu. With nothing focused during a Run, cancels the Run like Ctrl+C: the Goal stays paused and resumable and the draft is kept; a second Escape during cancellation is ignored. Linear input has no Escape; Ctrl+C cancels there |
| Unix SIGTSTP / inline Ctrl+Z | Restore the terminal before stopping, never stop if restoration fails; SIGCONT resumes the chosen presentation with draft, caret and history |
| Unix SIGTERM/SIGHUP | Exit; an active Run requests cancellation before bounded outcome confirmation |

`q`, Ctrl+D, `/goal pause` and other slash controls do not cancel work. An empty-draft Ctrl+D or `/exit` after the Run's outcome is shown exits once the Run returns; `/exit` also abandons a held linear draft or undelivered steering note. Cancellation and shutdown grace is at most three seconds, and expiry never fabricates a cancelled receipt or answer. Interrupting admission or compaction is not Run cancellation; interrupted compaction promises no remote billing stop.

Every terminal mode Arany enabled (raw mode, cursor, wrapping, echo, mouse reporting, alternate scroll) is restored on exit, error, cancellation, signal, render failure, partial acquisition, panic and suspension; Fullscreen leaves the alternate screen. Receipts and answers print after restoration. A renderer failure after Run admission shows only committed sanitized facts or a typed presentation failure. Secrets never reach the screen, notices, Events or output during cleanup. If the cursor-position query gets no reply, Classic falls back to linear input before work.

## Public limits and compatibility

| Surface | Limit and overflow behavior |
|---|---|
| Child message drafts | At most eight targets × 8 KiB<!-- unrelated-bound: child message draft cap --> (64 KiB<!-- unrelated-bound: aggregate child draft retention --> aggregate); nonempty drafts are kept and another target is refused when full; the [shared per-Run note quota](../goals.md#steering-notes-and-pause) includes waiting offers |
| Primary text draft | [64 KiB](../limits.md#max_user_message_bytes) UTF-8 expanded objective across typed, startup, restored and pasted text; an overflowing insertion is rejected whole, and restored slash text stays inert |
| Canonical physical line | Linux 4,094 bytes; macOS 1,023 bytes |
| History find | At most 64 UTF-8 bytes; local-only |
| Rendered history row | 4,096 cells |
| Local notices | 32 of 8 KiB<!-- unrelated-bound: local notice text cap -->; oldest dropped, long text cut with an ellipsis |
| Clipboard copy | 100,000 bytes; larger refuses whole |
| <a id="pasted-png-path-limit"></a>Pasted PNG path | At most 512 UTF-8 bytes after trimming whitespace and quotes; longer text stays ordinary text, even if it ends in `.png` |
| Image hover preview | A 32×10 inner-cell balloon, within the [cell pixel limit](../limits.md#max_cell_pixels) and the [preview pixel limit](../limits.md#preview_max_pixels); at most 256 KiB of graphics output per hover |
| Display and execution preferences | Tolerant 1 KiB<!-- unrelated-bound: UI preferences record cap --> private `ui-preferences.json` of changed keys; unknown or invalid content keeps the defaults ([record rules](../execution/persistence.md#incompatible-records)) until `/settings` saves. `exec` rejects unsafe records before dispatch; unsafe or unavailable files also block attached submission and saving |
| Agent inspection | 16 most recent Runs × nine agents (144 rows); complete export through `show`; search of at most 64 ASCII characters over labels and objective previews; at most 10 rows in view; three idle rows before the collapsed group; linear choices of at most 16 ASCII bytes, or `/` and 64 search characters |
| Agent chat | The current or last Run's agents; one transcript of at most 64 KiB<!-- unrelated-bound: selected agent transcript buffer --> and 4,096 rows<!-- unrelated-bound: selected agent transcript row cap -->, ending with `Earlier activity omitted; arany show prints the whole conversation` when records are left out; a lag or gap reloads the view instead of dropping history; linear choices of at most 16 ASCII bytes |

Keyboard text selection (Shift+arrows) and external-editor transcript export are unsupported; use `/copy`, Classic native selection, the transcript dump or `arany show`. Releasing alternate scroll can also turn off an emulator default such as xterm's `alternateScroll`.

Markdown, HTML, SVG, files, links and model output are inert and never reach privileged APIs; browser Clients stay [deferred](../../architecture/system-overview.md#14-explicitly-deferred).

Layouts distinguish at least 80 columns, 50–79 and narrower; at 20–39 columns picker hints keep editing, navigation, Enter and Escape, and at 16–19 identity and essential actions. Chain-of-thought, raw prompts, reasoning items, raw token streams and raw upstream errors never appear in the conversation; only an action approval shows the bounded sanitized action for a decision.
