# Terminal and output contract

## Scope and related contracts

This is the agreed contract for attached chat, local interactive controls, presentation, accessibility and deterministic process channels. It describes required behavior; it does not certify live accounts, every terminal or native platform support. [Next steps](../../NEXT_STEPS.md) owns open defects and unperformed checks.

[Session rules](../../agents/session.md) retain lifecycle and compaction semantics; [Provider](../../agents/provider.md), [credentials](../../agents/credentials.md) and [ChatGPT rules](../../agents/chatgpt.md) retain account, model, consent and billing admission. [Clipboard](../../agents/clipboard.md) and [image rules](../../agents/image.md) retain transport and image admission. [Terminal rules](../../agents/terminal.md) own editing constraints and implementation invariants, and [architecture](../architecture/system-overview.md#8-terminal-presentation) owns physical responsibilities. These unmigrated contracts remain authoritative within their scopes.

## Entry, modes and channels

| Entry | Observable contract |
|---|---|
| `arany [PROMPT]` | Requires terminal stdin and stderr. Without a prompt, opens a durable Session composer; a prompt starts the first Run only after admission and leaves chat open. The inline interface draws on stderr in the primary screen. Confirmed results go to stdout after terminal restoration. |
| `--continue`, `--resume`, `--fork` | Explicit Session entry. Workspace or TTY matching alone never resumes work. Session identity and committed-boundary semantics belong to the Session contract. |
| `--setup` | Opens setup before creating a new Session. Cancellation before account save creates no Session. Idle `/setup` instead operates inside the current Session. |
| `exec --output text` | Noninteractive; initializes no terminal modes. Stdout contains only the sanitized, indented committed answer; stderr contains the stable Session/Run/status receipt. A failed Run exits unsuccessfully without an invented answer. |
| `exec --output jsonl` | Stdout contains the new Session fact, if any, and only this Run's committed Events. Successful JSONL leaves stderr empty. |
| `show --output text\|jsonl SESSION_OR_RUN_ID` | Deterministic read-only replay without terminal modes. Session selection exposes its direct canonical Events and fork reference; Run selection exposes only that Run after strict parent validation. Invalid or conflicting ownership fails without output. |

The command chooses the contract. An ineligible bare invocation fails before Engine entry and directs the caller to `exec`. Headless objective prefixes such as `/`, `@` and `!` are literal data, not interactive commands.

`--screen-reader` or `ARANY_SCREEN_READER=1` selects labeled append-only output with canonical input, without raw mode, cursor rewriting, boxes or animation. `TERM=dumb`, fewer than eight rows or unavailable geometry chooses that same fallback before acquisition. A presentation remains fixed during its ownership; resizing does not oscillate between modes.

`--no-color` and `NO_COLOR` remove color, including after reacquisition. Text labels, selection markers, focus and bracketed hints retain meaning. Message bodies are unstyled; `You:` and `Arany:` remain distinguishable. Inline role headings use compact `› You:` and `● Arany:` markers and emphasis. A two-cell rail groups nonempty user-body rows; assistant bodies retain plain indentation. The terminal background remains user-owned, and only trusted markers/headings receive styling outside explicit find matches. Existing spacing separates messages without additional indexed rows. When at least five history rows fit, inline chat reserves one blank screen row above the transcript so a single-row terminal sticky command header cannot cover the first speaker label; the spacer is presentation-only and reduces visible history capacity, never indexed message rows. With four or fewer available history rows, prioritize content over the spacer. Restored inline user previews use the same compact `› You:` label. Attached committed answers use `Arany · Answer:`; headless `exec` retains `Answer:`. Linear mode keeps plain role labels without decorative rules, including when a fast completion prints the objective only after restoration. Linear and deterministic output is unstyled and contains no CSI/OSC or carriage-return rewriting.

## Conversation, progress and history

The inline primary screen contains committed transcript, conditional agent activity, a bottom composer and status. It uses no alternate screen, focus reporting or title/clipboard OSC. Empty chat shows a short width-aware welcome and `/help`; without a selected Provider, welcome, input hint and status point to `/setup`. Hints never become draft bytes.

History exposes only committed objectives and terminal answers, with bounded local notices interleaved. Notices never enter canonical Events, Provider context, replay or `show`; they survive redraw/reacquisition and clear on Session change. Important errors have a textual heading, independently of color. Confirmed failed/cancelled Runs show safe compiled guidance without invented answers, raw error bodies or automatic retries. Retained-draft cues remain visible separately from history notices. A notice uses status fallback if no transcript row fits.

PageUp/PageDown navigate; Ctrl+F opens local literal find, with grapheme Backspace and visible query/Enter/Escape hints; Ctrl+L returns to live. New content and resize preserve an older source-text reading anchor and expose a textual new-content cue. Selector return and reacquisition preserve the reading position. Emulator scrollback is not the complete Session history; `arany show` is the canonical export path.

| Work state | Activity |
|---|---|
| Idle | No empty agent dashboard |
| Selected-Provider admission | Local preparing row and next-Run model; no prior Run/context presented as current |
| Single active primary | One primary row |
| Team or attention | At most three stable primary-first/spawn-ordered agent rows, with `+N more` for overflow |

Preparing is transient admission feedback, not an Event or proof of inference. Linear mode announces it once without per-character output or an input prompt. Current committed progress replaces it. Every agent state/wait has a textual label, and completed/failed children remain represented while their Run is active. Progress is driven by committed snapshots, not token streams or reasoning; coalescing display updates does not discard Events. Redraw occurs only for committed updates, user actions or resize; there are no spinners, animations, idle refreshes or continuously ticking clocks.

Status prioritizes interaction state/model, adds Provider from 50 columns, and shows a recognizable conversation title when space permits. Terminal outcomes return to ready for another message; detailed last-task outcomes remain available through `/status` and history. Request-size metrics belong in explicitly requested `/status` details, with an explanation of the local content limit, rather than appearing and disappearing in the ordinary status row. Older-history navigation may take priority at narrower widths; actionable notices can occupy the row. Active `/status` explains its rounded local request-size percentage, counts agents with a singular/plural label and points to `/agents` for details; it includes chat, instructions, files and images. It measures the pinned Arany content budget, including encoded images; it is not a Provider context-window percentage or an estimate for a new draft. Model tokens are shown only from recorded Provider usage, with missing values labeled unknown. Agent details identify input/output tokens for their last recorded call. Clipped printable ASCII model IDs retain both ends. The composer, `/status`, resume feedback, human export headings and discovery use the same generated title and explicit-title precedence; canonical title Events remain unchanged. Resume feedback names the conversation rather than its UUID.

## Draft editing and submission

Inline edits operate on whole graphemes and keep the cursor at a grapheme boundary. Insertion, deletion and wrapping preserve accepted bytes except explicit editing and paste newline normalization. Long drafts wrap visually into at most four rows around a visible caret; the heading identifies a clipped visual-row window. Growth clips activity before input/status.

| Input | Effect |
|---|---|
| Left/Right, Backspace/Delete | Whole-grapheme movement/deletion |
| Up/Down | Explicit logical-line movement at a preserved grapheme column |
| Ctrl+Left/Right | Previous/next Unicode-whitespace-delimited chunk; punctuation in paths and model IDs remains part of a chunk |
| Ctrl+W | Delete the same preceding chunk range |
| Ctrl+O or decoded Shift+Enter | Insert newline without submission |
| Enter while idle | Submit through local validation, or complete an open slash-name menu |
| Enter during admission/Run/catalog loading | Retain a task draft; never queue another Run |
| Enter during compaction | Retain all text, including slash commands; execute no inspection or command |

Shift+Enter works only when the emulator distinguishes it; Arany enables no keyboard-enhancement mode. Ctrl+O remains the explicit inline alternative. Linear input uses Enter to submit and Ctrl+D after nonempty input to continue a logical draft. On an empty draft Ctrl+D exits; on an already retained draft it gives Enter/Ctrl+C guidance. Overflow or invalid physical input rejects that segment without automatically submitting a prefix or erasing earlier accepted text.

Before consuming an idle inline draft, unknown slash syntax, invalid local arguments, an oversized rename or incomplete objective selection must leave text/caret correctable without credential, State, Workspace or Provider I/O. Rename uses the Session title contract. Linear submitted text starts a fresh physical line after rejection; image attachments retain their own rules. Failure after State access or an attempted effect does not restore a consumed command, promise rollback or authorize automatic resubmission.

During an admitted Run, valid slash controls provide read-only inspection or a locked notice. Next-Run defaults and Session mutations stay locked; `/quit` explains Ctrl+C cancellation first. Parser-rejected inline commands remain correctable. A task draft requires a later explicit idle Enter after work ends.

Manual/automatic compaction preserves draft/caret/history through success, failure, interruption, resize and suspension. Ctrl+K is locked and Ctrl+C interrupts maintenance without clearing the draft. Completion reloads canonical facts and returns to explicit idle submission. Automatic maintenance follows a confirmed Run receipt and a visible warning; its status/usage excludes model-authored summary text. An interruption does not guarantee rollback of a queued append or remote usage.

Draft feedback counts user-perceived characters (graphemes), including accepted whitespace. Combined emoji and combining accents count as one character. Storage/physical-line admission retains its existing UTF-8 bounds, so the UI does not promise a fixed maximum number of Unicode characters. Capacity errors ask the user to shorten input or paste a smaller section. Human attachment sizes use rounded decimal KB; canonical data and machine exports retain exact bytes.

## Interactive commands and completion

The [compiled registry](../../src/terminal/commands.rs) owns exact command names, aliases, argument syntax and active availability. Commands are trusted local controls; Workspace, Provider, Skill or persisted text cannot register or redefine them. Only an exact lowercase name after a single column-zero slash is command syntax. `//text` submits `/text`; unknown commands produce local suggestions without Provider input. No-argument controls accept trailing whitespace and reject extra arguments in every Run state.

Inline ambiguous slash-name completion shows at most three focused rows at the draft-end caret. Up/Down focuses, Tab/Enter fills the name without execution, Escape closes without editing, and a later Enter uses normal admission. Exact names, arguments, multiline or mid-draft editing take precedence. Outside that menu, Tab completes a unique registry name/value or admitted cached model candidate; ambiguous arguments leave the draft unchanged. Hints distinguish Tab completion from Enter submission and disappear away from the draft-end caret. Placeholders never enter draft, submission or Events.

Completion performs no catalog/credential I/O on a keystroke. Provider/account changes invalidate catalog and effort candidates; same-source redraw or model-only change retains them. Reviewed native levels, explicit unknown-native/ChatGPT effort and declared custom levels follow Provider admission; displayed availability is not compatibility evidence. Linear input announces literal arguments and inline-only Tab completion.

Inline `/help` opens a scrollable registry-backed panel: arrows, PageUp/PageDown and Home/End navigate; Enter/Escape restore composer/history. It stays usable during Run progress while Ctrl+C keeps its cancellation meaning. Linear help appends the full registry, schemas, aliases and availability without modal focus or a truncated notice. Help performs no State, credential, catalog or Provider I/O.

`/clear` aliases `/new` and does not delete history. New-conversation selection and committed-boundary switching belong to the Session contract. `/compact` creates derived context. `/permissions` reports requested or pinned effective authority, including the explicitly enabled Tool path; it never grants approval or widens authority. This beta has no approval UI or unsandboxed fallback. `/effort` and `/models` are retired; `/model` owns both choices. `/models` rejects locally with a `/model` suggestion and cannot submit an objective.

## Selectors

Idle Ctrl+K offers Models, Agents, Resume and Setup without consuming the draft. With no Provider, Setup initially has focus. Up/Down selects; Enter activates; Escape/Ctrl+C restores draft/caret. All four actions fit at 16×8. Session switching requires an empty draft, including no outstanding images. Opening the selector performs no catalog or credential I/O. During a Run it is locked; linear mode uses slash commands.

Bare `/model` opens an idle bounded catalog panel above the retained composer. Up/Down focuses models; Left/Right changes that row's tentative effort; filtering preserves per-row choices. Enter saves model and effort atomically; Escape changes neither defaults nor draft/caret/history. An empty filtered result cannot activate anything. The panel overlays lower transcript without moving the history anchor. Its local ASCII filter never enters Session/context/scrollback. Below 16 columns or eight rows only resize/close is admitted. Linear mode uses numbered pages and `NUMBER [EFFORT|default]`.

Metadata identifies reviewed, listed or exact choices. Unknown native models require explicit effort and remain unproven until an actual strict response succeeds; ChatGPT choices also require explicit levels, and custom tuples use their declaration. Changed typed native/ChatGPT models start at low; the same model retains effort. Provider changes clear model/effort. Selection itself loads no token and starts no inference; Run admission revalidates account/consent and custom evidence.

Cached native/ChatGPT choices open immediately while one cancellable refresh remains input-owned. Refresh failure keeps old rows visibly stale; inline replacement preserves filter/model identity/admissible effort. Linear numbers remain the immutable opened snapshot, so refresh cannot retarget an edited choice. Completion/cancellation during catalog loading must preserve accepted draft text without automatic submission; arbitrary queued typeahead is not guaranteed. Custom declarations use their own local loading path.

Saved-account model/effort choices and acquired catalog IDs survive process restart. A new bare Session restores the last acknowledged tuple for the same Provider/account UUID and seeds its cached rows without credential loading or network work. Explicit flags and resumed Session defaults take precedence; a replaced or different account inherits neither tuple nor catalog. `/model` refreshes restored rows through the same input-owned flow. Remembering failure is visible without undoing a committed Session selection. The cache is bounded to eight sources, 4,096 IDs per source and 1 MiB in aggregate, evicting the oldest unselected source first. A separate optional Provider/account pointer remembers explicit setup/model and resume choices; asynchronous catalog refresh never changes it. Old token-free preference documents without that pointer remain readable. Stored availability never authorizes an account, model, effort, custom destination or inference.

When that account has no saved preference yet, startup may copy only the matching model/effort from the latest valid Session in the same Workspace. It creates a new Session with empty conversation history; a mismatched account or unavailable prior selection is not inherited.

Idle selection rejected because another Session operation holds its lock returns labeled chat feedback without saving or retrying the attempted defaults. A typed unavailable ChatGPT account or invalid custom model/effort choice likewise preserves Provider/model/effort/account selection and starts no sign-in, fallback or inference. Catalog failure changes no candidate defaults, while account work already published is not rolled back. State/replay/terminal-owner failures retain their fatal boundary under CLI rules.

`/agents` inspects primary-first agents from the 16 most recent Runs, newest first, with bounded inert identity/state/objective/summary/result previews. Its argument form changes only next-Run collaboration. Inline arrows select, PageUp/PageDown scroll, Home/End jump and Enter/Escape close. Linear mode uses numbered choices, `n`/`p`, `q` and labeled invalid-choice feedback. If work ends while linear inspection is open, incomplete picker input is discarded before chat resumes. `show` exposes complete older history.

`/resume` opens the Workspace conversation picker; `/resume SESSION_ID` retains exact selection. `/sessions` is retired and suggests `/resume` locally. Rows prioritize recognizable titles rather than IDs. Generic titles use the first successfully completed primary AI summary, bounded to 128 UTF-8 bytes and normalized whitespace; until then, use the first objective or `Empty conversation`. Non-placeholder creation titles and `/rename` win. This projection makes no extra Provider call and does not rewrite Events or Provider context. Fork discovery derives inherited title/model only through its recorded prefix.

Focused details show creation, last committed activity in UTC, access route and model. Opening a conversation without a committed change does not fabricate activity. At narrow widths dates shorten to calendar dates; keyboard and mouse preserve selection and only list rows activate. Linear choices announce full dates and configuration without UUID labels.

Credentials and explicit saved-source preference belong to the OS user. A configured conversation retains its selection. With no selection, resume recovers its latest pinned Run tuple, otherwise the current conversation selection or saved user preference; account-only defaults can recover the same account's remembered tuple. Persist inheritance before switching, preserving collaboration policy. Preview the applicable default for empty rows. Missing/replaced pinned credentials never select another account or billing route; Run admission still validates current credentials/consent. Empty/unavailable lists are distinct from terminal-owner failures. In chat an empty list gives ordinary guidance and an unavailable list gives error feedback; before Session entry either fails. Rendering/input/cleanup errors propagate to restoration and exit.

Mouse reporting is disabled in ordinary chat/history. It is transiently enabled only for visible Session-picker or agent-inspector rows; off-list input is inert and every action has a keyboard equivalent. Close, suspension, cancellation, failure and loss of ownership disable it before control returns.

## Setup and account choices

A new bare start with no account still opens chat and names `/setup`; an unconfigured task starts no Run. A saved explicit access/model preference is reused on bare startup, retaining its Provider/account identity. Disconnected ChatGPT access is not restored as connected. Without a remembered selection, mixed saved API/ChatGPT access requires an explicit billing-route choice, with Cancel initially focused. Empty Enter cancels and creates no Session; invalid chosen access never falls back to another billing route.

Idle `/setup` preserves the caller's draft and existing defaults on pre-save cancellation. Native and ChatGPT setup return from selected-account catalog discovery with a low-effort model, without a model/effort/cost questionnaire or synthetic inference. The Provider/account contracts own preferred-model selection and admission. Catalog visibility alone does not prove compatibility.

Saved ChatGPT choices are Use saved, Reconnect and Connect new. Multiple saved accounts show short IDs and the current marker. Cancelling the account picker changes neither selection nor Session; reusing the current account avoids browser sign-in. An explicit switch, new connection or reconnect can retain account-only defaults if later catalog work fails/cancels; this is not rollback of account publication. Reconnect repeats backend-specific consent and clears old diagnostic evidence. Failed catalog discovery exposes a retry notice without retrying itself.

On Linux, replacing the executable during an open conversation must not prevent the same running image from launching its credential helper or masquerade as a credential-store failure. Credential-store failures explain that the desktop password store could not be accessed and direct the user to unlock it and retry. Missing saved sign-in directs to Reconnect; generic authorization failure gives setup/retry guidance without asserting its cause. Reconnect preserves a pinned store; when unavailable, guidance identifies Connect new's separately consented private-file option. That choice explains that sign-in credentials will be stored without encryption, keeps Cancel focused, and opens no browser before explicit choice and risk consent.

Non-secret inline setup uses centered borderless focused rows with arrows, Tab, Enter and Escape; typed characters do not select rows. Linear choices accept visible names, not numeric codes, and announce the option empty Enter will choose. Reopening a choice announces it again, while identical redraws remain quiet. Labels and actions remain readable at 16 columns without color.

API-key input disables echo and exposes only a character count, never key content. Printable ASCII admission makes its 512-character limit exact. Non-secret input retains echo. ChatGPT consent exposes the complete backend-specific warning before Back/Accept; Back starts focused and empty Enter declines. Resize/resume restarts review and resets focus. Insufficient inline geometry rejects before sign-in. Private-file storage has a separate plaintext warning and explicit selection, with Cancel initially focused; it never silently replaces a pinned keyring.

Rejected linear setup/key characters form a correction-only suffix: Backspace removes those characters before earlier valid input, and later characters do not become choice/key bytes until correction. Anthropic's optional Console workspace ID is visible text with local validation, not a secret counter or ambient scope. Invalid inline text remains correctable; linear rejection restores echo and starts a new physical line. Cancellation preserves the established setup boundary. The OS store can display its own unlock prompt; Arany makes no OS-store encryption guarantee.

## Paste and attachments

Framed Unix inline text paste atomically inserts one bounded valid block into the editable draft, including during admission/work/maintenance/catalog waits. CR/CRLF becomes LF; tabs remain bytes with four-space visual expansion. Overflow/invalid UTF-8 rejects the entire record. Paste never submits, completes a command or activates a selector; modal pickers ignore it. Unterminated framing fails terminal ownership closed after its deadline. Secret and workspace fields retain separate bounds and explicit Enter. Linear canonical input does not claim safe unframed emulator multiline paste.

Explicit Ctrl+V, decoded Ctrl+Shift+V or `/paste` requests one bounded OS clipboard read. Unsupported transport, hostile payload or process failure gives local feedback without altering the draft. Enter while loading retains rather than submits. Picker/find entry, release, suspension and Session ownership changes discard late results. During a Run Ctrl+C still cancels the Run. Linear mode exposes a complete inert `Draft (not sent)` preview, including during work, and renews its idle prompt after success/failure; busy work suppresses that prompt.

Linux supports admitted text/PNG through standard Wayland/local X11 clients; compiled routing is not proof of native desktop interoperability. The image contract admits at most four PNGs totaling 192 KiB raw bytes, including image-only submission. Human history shows metadata; private canonical history/forks and explicit JSONL export retain bytes. Compaction uses metadata and prior assistant interpretation, not covered pixels. Custom routes remain text-only. Native service success, real vision and macOS clipboard support require separate evidence.

## Interruption, suspension and restoration

| State/action | Required outcome |
|---|---|
| Idle Ctrl+C with a draft | Clear the draft |
| Idle Ctrl+C on empty input | Explain emulator Ctrl+Shift+C copying; a second press within two seconds exits |
| Ctrl+C during selected-Provider admission | Return to chat with the new draft retained and no Run receipt/retry; delegated account work may still complete |
| First Ctrl+C during a Run | Request graceful whole-Run cancellation |
| Second Ctrl+C during cancellation | Bounded local shutdown; report only the strongest durably confirmed terminal state |
| Escape | Close the focused panel first; it does not cancel work |
| Unix SIGTSTP / inline Ctrl+Z | Release input/modes before stopping; SIGCONT reacquires the chosen presentation and preserves draft/caret/history and pinned work |
| Unix SIGTERM/SIGHUP | Exit through the input-owning state; an active Run requests cancellation before bounded outcome confirmation |

`q`, Ctrl+D and slash controls do not cancel active work. The cancellation/shutdown grace is at most three seconds; expiration must not fabricate a cancelled receipt or answer. Replay can instead expose an interrupted prefix. An interrupted compaction makes no Run-cancellation or remote-billing-stop promise.

Restore raw mode, cursor, wrapping, echo and every enabled mode on normal exit, error, cancellation, signal, render failure, partial acquisition, unwind and suspension. Receipt/answer output follows restoration and read-only confirmation. Renderer failure after Run admission may expose only committed sanitized linear facts or a typed presentation failure. Secret bytes remain absent from drawing, notices, Events and output throughout cleanup.

## Public limits and compatibility

| Surface | Limit and overflow behavior |
|---|---|
| Logical text draft / framed paste | 8 KiB UTF-8; overflowing insertion rejects without changing accepted text |
| Canonical physical line | Linux 4,094 bytes; macOS implementation 1,023 bytes; rejection adds no segment bytes |
| Inline wrapped draft | At most four visible editable rows; clipping preserves bytes/caret |
| Model filter | At most 64 ASCII characters; local-only |
| History find | At most 64 UTF-8 bytes; local-only |
| Hidden native API key | At most 512 printable ASCII characters; secret content never reaches presentation |
| Slash-name suggestions | At most three focused rows |
| Agent inspection | 16 most recent Runs; bounded previews, complete export through `show` |
| Inline model selection | Below 16×8 only resize/close; initial unsafe/short geometry selects linear presentation |
| Unterminated inline record | Ten-second deadline, then terminal ownership fails closed |

Width layouts distinguish at least 80 columns, 50–79 and narrower terminals. At 20–39 columns picker hints retain editing/navigation/Enter/Escape; at 16–19 they preserve distinguishing identity and essential actions. These are intended contracts, not proof for every native state or theme. Full native macOS, real assistive technology, arbitrary terminal fault timings and canonical typeahead/echo interleaving remain unverified. No chain-of-thought, raw prompt, token stream, reasoning item or raw Tool/error payload is a presentation source.

## Acceptance scenarios and evidence owners

Each row is a required scenario and names its existing owner, not a claim that all listed gates have passed. [Testing rules](../../agents/testing.md) determine evidence admission; dated outcomes live in [the beta plan](../../planning/arany-beta/README.md), and user-owned checks in [next steps](../../NEXT_STEPS.md#beta-1-final-user-owned-checks).

| Scenario | Given / when / expected outcome | Existing owner |
|---|---|---|
| Headless channels | A committed Run is rendered/exported; exact stdout/stderr and JSONL agree with closed replay, without terminal acquisition. | [Session product journey](../../tests/session_run.rs), [custom route](../../tests/session_run/custom.rs), [pure output](../../src/presentation.rs) |
| Unconfigured chat | No account is selected; opening chat and editing works, submitting gives setup guidance and creates no Run. | [Setup product corpus](../../tests/session_run/setup.rs) |
| Local correction | Unknown command, invalid local argument, incomplete selection or capacity rejection leaves inline text/caret correctable without egress; linear rejection cannot join the next command. | [Registry](../../src/terminal/commands.rs), [composer](../../src/terminal/composer.rs), [slash product corpus](../../tests/session_run/slash_completion.rs) |
| Safe completion/help | Highlighting/Tab/first Enter completes without execution; later Enter uses admission; help is complete and Provider-free. | [Registry](../../src/terminal/commands.rs), [slash product corpus](../../tests/session_run/slash_completion.rs) |
| Atomic model selection | A draft/history position exists; Escape preserves it and defaults, Enter saves model/effort together; failed refresh retains cached choices. | [Catalog product corpus](../../tests/session_run/model_catalog.rs), [synthetic ChatGPT journey](../../tests/session_run/setup/offline_https/chatgpt/checked_turn.rs), [frame corpus](../../src/terminal/view/tests.rs) |
| Remembered model selection | A saved account has an acknowledged tuple and acquired catalog; a fresh process starts an empty Session with that same tuple/cache, while a different UUID, explicit flags or resumed defaults cannot inherit or be overwritten by it. Malformed/over-limit preferences reject without authority. | [Saved-account product journey](../../tests/session_run/setup/offline_https/chatgpt.rs), [preference admission corpus](../../src/cli/attached/models.rs), [private file admission](../../src/store/state.rs) |
| Unicode editing | Long grapheme/word edits and wrapping retain correct bytes/caret; newline keys do not submit. | [Composer](../../src/terminal/composer.rs), [tmux product corpus](../../tests/session_run/active_terminal/tmux.rs) |
| Human measurements | Draft feedback counts graphemes, hidden keys count admitted ASCII characters, and human image metadata uses KB. Explicit status details explain rounded local request capacity without stale usage; agent details show only the last recorded call's known or unknown input/output tokens, readable at 40 columns. | [Linear owner](../../src/terminal/linear.rs), [frame corpus](../../src/terminal/view/tests.rs), [semantic status](../../src/presentation/model.rs), [agent details](../../src/presentation/agents.rs), [active terminal](../../tests/session_run/active_terminal.rs) |
| Busy draft | During Run/admission/compaction/catalog waits, edits survive Enter, interruption and restoration without a queued objective. | [Active terminal](../../tests/session_run/active_terminal.rs), [compaction](../../tests/session_run/auto_compaction.rs), [setup](../../tests/session_run/setup.rs) |
| Current activity | Before admission display preparing; after commit display current primary/team facts, never prior activity or token streams. | [Active tmux](../../tests/session_run/active_terminal/tmux.rs), [frame corpus](../../src/terminal/view/tests.rs) |
| Older history | While reading/finding older rows, append/resize/selector return/suspend preserves the source anchor and draft. | [History](../../src/terminal/history.rs), [active tmux](../../tests/session_run/active_terminal/tmux.rs), [latency gate](../../tests/session_run/active_terminal/tmux/history_latency.rs) |
| Resume discovery and inheritance | `/resume` shows recognizable AI/manual titles, UTC creation/activity and route/model; resume preserves configured selections and fills absent defaults before switching, without inference. Status/resume/human export share the same name, terminal outcomes return to ready and ordinary status omits request metrics. Explicit placeholder-looking renames win; failed/cancelled responses do not supply AI titles; stale catalog completion cannot change the global source. | [Session journey](../../tests/engine_run.rs), [discovery](../../tests/session_run.rs), [startup owner](../../src/cli/attached.rs), [preference corpus](../../src/cli/attached/models.rs), [isolated account journey](../../tests/session_run/setup/offline_https/chatgpt.rs), [picker/frame owners](../../tests/session_run/session_picker.rs) |
| Picker ownership | Keyboard and transient mouse select only local visible rows; close/error/signal restores modes; incomplete linear inspector input cannot become chat. | [Session picker](../../tests/session_run/session_picker.rs), [agent inspector](../../tests/session_run/agent_inspector.rs) |
| Explicit consent | Empty Enter declines mixed billing/private storage/ChatGPT consent; warning review and hidden-key echo survive cancellation/suspend safely. | [Setup product corpus](../../tests/session_run/setup.rs) |
| Paste remains a draft | A framed block or isolated clipboard response arrives during work; whole-record admission retains it without submission and discards late modal results. | [Raw reader](../../src/terminal/input/raw.rs), [clipboard owner](../../src/terminal/clipboard.rs), [active terminal](../../tests/session_run/active_terminal.rs) |
| Cleanup and cancellation | Ctrl+C, signals, hangup, output failure, acquisition/unwind fault or Linux executable replacement occurs; restore terminal before a confirmed receipt and preserve honest replay. | [Active terminal](../../tests/session_run/active_terminal.rs), [acquisition](../../tests/session_run/active_terminal/acquisition.rs), [panic](../../tests/session_run/active_terminal/panic.rs), [credential admission/replacement](../../tests/session_run/setup.rs), [picker termination](../../tests/session_run/session_picker/termination.rs) |
| Accessible presentation | Render linear/no-color/narrow states; the first speaker remains below a blank top row during work and after the first answer, without shifting the bottom composer; role markers, user rails and message spacing remain readable, bodies stay unstyled/inert, and exact linear output contains no rewriting controls. | [Linear owner](../../src/terminal/linear.rs), [frame corpus](../../src/terminal/view/tests.rs), [manual handoff](../../NEXT_STEPS.md#beta-1-final-user-owned-checks) |

Native PTYs prove terminal/process boundaries; TestBackend proves frames, not real assistive technology. Live Provider/vision, native clipboard/keyring and Orca/VoiceOver checks retain their separate owners. An ignored test is not passing evidence, and a dead stderr sink cannot prove delivery of a mouse-disable sequence.
