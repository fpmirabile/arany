# Beta terminal interface and multi-agent feedback

> **Human-review amendment — 2026-09-29:** [Beta Sessions, teams, terminal, providers, and license](./beta-sessions-teams-terminal-providers-and-license.md) supersedes the always-visible three-row control-room shape and the no-mouse rule. Native scrollback remains canonical, but the default bottom region is now composer + compact status row + conditional activity shelf. Single-agent work gets no empty team chrome; `/agents` opens full details for ordered bounded `0..N` children. Keyboard is complete and mouse reporting may activate transiently only inside an open picker/panel, with restoration on every exit path.

> **Interactive-command amendment — 2026-09-29:** [Interactive CLI conventions and command surface](./interactive-cli-conventions-and-command-surface.md) supersedes this report's earlier attached-human spelling and “no prompt editor” boundary. The scrollback-first inline control room, `exec`/`show` separation, accessibility, terminal lifecycle, module shape, and G0–G7 remain. Bare `arany [MESSAGE]` owns a durable multi-Run Session, bounded composer, and closed local slash registry; each accepted Message creates one bounded Run.

**Status:** recommendation for the beta contract; it supersedes the earlier append-only/no-TUI assumption only after the implementation gates in this report pass

**Retrieved:** 2026-09-29

**Scope:** one Rust package, one process, Linux and macOS, durable Sessions, one primary plus bounded direct children, SQLite Events as canonical state, CLI only

**Evidence policy:** product claims below use official documentation or official source repositories. “Fact” reports that evidence, “Inference” derives a consequence for Arany, “Recommendation” proposes a product decision, and “Implementation gate” states evidence required before the decision becomes shippable.

## Executive decision

**Recommendation.** The beta should not be a basic stdout-only experience for an attached human. It should expose two deliberately different commands over the same committed Events and replayed `RunView`:

1. Bare `arany [OBJECTIVE]` is the attached-human command. In an eligible terminal, an event-driven, **scrollback-first inline dashboard** on stderr shows the team, selected-agent detail, a short causal timeline, attention, provider/model attribution, and usage provenance. Committed semantic transitions are also inserted into native scrollback. The root's final result remains stdout. Screen-reader mode replaces the dashboard with a linear attached-terminal presentation.
2. `arany exec` is the non-interactive command for a pipe, CI job, log capture, or a human who wants stable text. It emits deterministic append-only human text with no terminal control sequences, or one compact JSONL record per stdout line.

This is still CLI-only. It introduces neither a web client, a daemon, a protocol, nor a second source of truth. The terminal is a projection of `RunView`, and terminal failure must not mutate Engine state or invent progress.

**Recommendation.** Use Ratatui with Crossterm for the attached-terminal projection. Default to Ratatui's inline viewport and reject an alternate-screen implementation for the beta. Ratatui explicitly supports fullscreen, inline, and fixed viewports, while its inline viewport stays in normal output flow and leaves output above the viewport in terminal history ([Ratatui `Terminal`](https://docs.rs/ratatui/latest/ratatui/struct.Terminal.html), [Ratatui `Viewport`](https://docs.rs/ratatui/latest/ratatui/enum.Viewport.html)). An alternate-screen client becomes justified only after Arany can supply its own transcript navigation, search, copy/export, screen-reader equivalent, and restoration evidence.

**Recommendation.** Add a bounded durable Session composer, not a general-purpose editor. Bare `arany [MESSAGE]` needs a live monitor and control surface that remains familiar without cloning another harness wholesale. The bottom viewport reads navigation and cancellation keys while the Engine performs one bounded Run at a time.

**Recommendation.** The differentiator should be trustworthy agent feedback, not decorative animation: every displayed lifecycle fact has a durable sequence; every assignment and handback is causally linked; every child has an explicit terminal disposition; cost and context values state their provenance or say `unavailable`; and safe summaries explain observable work without exposing hidden chain-of-thought.

**Recommendation.** Do not auto-select between interactive and automation contracts. Bare `arany` verifies that stdin and stderr are usable terminals before the Engine starts; otherwise it exits with an instruction to use `arany exec`. `exec` never initializes terminal state, even when launched from a TTY. This makes command choice—not ambient CI variables or TTY topology—the stable contract.

## What the reviewed products actually do

### Codex

**Fact.** Codex separates its interactive TUI from `codex exec`. Non-interactive execution writes progress to stderr and the final message to stdout; `--json` produces JSONL lifecycle and item events, and the last message can be written separately ([Codex non-interactive mode](https://developers.openai.com/codex/noninteractive)).

**Fact.** The official Rust source uses Ratatui and Crossterm and treats terminal ownership as a subsystem rather than a few print calls. It contains alternate-screen and inline behavior, raw mode, bracketed paste, focus and mouse modes, cursor control, restoration, suspend/resume handling, transcript reflow, screen-reader detection, motion policy, status rendering, token usage, and agent/thread selection ([Codex TUI terminal owner](https://github.com/openai/codex/blob/main/codex-rs/tui/src/tui.rs), [Codex TUI modules and options](https://github.com/openai/codex/blob/main/codex-rs/tui/src/lib.rs), [Codex job control](https://github.com/openai/codex/blob/main/codex-rs/tui/src/tui/job_control.rs)). Codex exposes `--no-alt-screen` and a configuration policy for always, never, or automatic alternate-screen use. Its restore guard runs on normal drop and emits a recovery instruction if restoration fails ([Codex TUI library](https://github.com/openai/codex/blob/main/codex-rs/tui/src/lib.rs)).

**Fact.** Codex does not treat resize as merely “draw again.” Its source-backed transcript reflow debounces resize and repairs layout again after streaming finishes ([Codex transcript reflow](https://github.com/openai/codex/blob/main/codex-rs/tui/src/transcript_reflow.rs)). Its screen-reader probe is bounded and feeds motion behavior; current source checks macOS VoiceOver and Linux AT-SPI/Orca signals, but that probe alone is not evidence of a fully linear accessible interface ([Codex screen-reader detection](https://github.com/openai/codex/blob/main/codex-rs/tui/src/screen_reader.rs), [Codex system-motion policy](https://github.com/openai/codex/blob/main/codex-rs/tui/src/system_motion.rs)).

**Fact.** Codex's application-event layer has explicit events for loading agent threads, opening an agent picker, selecting a thread, and directing operations to a selected thread. This keeps interactive application state outside core execution messages ([Codex application events](https://github.com/openai/codex/blob/main/codex-rs/tui/src/app_event.rs)).

**Inference.** Codex validates Rust, Ratatui, Crossterm, inline/fullscreen policy, and an interactive/non-interactive split. It also demonstrates that alternate screen, streaming reflow, suspend/resume, and terminal restoration are real product work. Arany should adopt the split and lifecycle discipline, adapt agent selection to its much smaller fixed team, and avoid importing Codex's chat composer and transcript complexity.

### Claude Code

**Fact.** Claude Code's interactive keyboard contract distinguishes stopping the current response from exiting. `Esc` stops a response or tool while preserving completed work; `Ctrl+C` interrupts an operation and has different idle behavior; `Ctrl+D` exits with confirmation; `Ctrl+L` redraws a damaged screen; `Ctrl+O` opens a transcript viewer; `Ctrl+T` shows tasks; and Unix `Ctrl+Z` suspends the process ([Claude Code interactive mode](https://code.claude.com/docs/en/interactive-mode)).

**Fact.** Claude Code offers a research-preview fullscreen mode using the alternate screen. It gains a fixed input area and visible-only rendering, but native scrollback is unavailable; it therefore supplies in-app scrolling, search, and transcript export back to native scrollback. Screen-reader mode stays on the classic renderer, and fullscreen can fall back after startup failures ([Claude Code fullscreen](https://code.claude.com/docs/en/fullscreen)).

**Fact.** Claude Code documents an explicit screen-reader mode with flat, labeled, linear text, no boxes, no color-only cues, no redraw-based spinner, sentence-form tables, and native terminal scrollback. It also documents reduced-motion handling, accessible themes, and an optional terminal bell ([Claude Code accessibility](https://code.claude.com/docs/en/accessibility)).

**Fact.** Claude Code agent teams expose a lead plus separate-context teammates, a team panel, selected-teammate inspection and direct messages, task states and dependencies, interruption controls, and in-process or split-pane display. The documentation also records current limitations: task status can lag, shutdown can be slow, resumed sessions do not restore teammates, and nested teams are unsupported ([Claude Code agent teams](https://code.claude.com/docs/en/agent-teams)).

**Fact.** Claude Code's status-line inputs support model, working directory, context percentage, cost, and duration ([Claude Code status line](https://code.claude.com/docs/en/statusline)). Its headless mode is a separate `-p` contract and documents signal/exit behavior rather than pretending an interactive transcript is a stable automation format ([Claude Code headless mode](https://code.claude.com/docs/en/headless)).

**Inference.** Claude Code supplies the strongest documented accessibility and team-navigation precedent. Arany should adopt linear screen-reader output, explicit stop semantics, redraw/help keys, stable team rows, and selected-agent detail. It should reject alternate screen until it has the transcript facilities Claude Code had to build to compensate for lost native scrollback. Claude Code's product documentation is authoritative for behavior; its public repository does not expose a comparable implementation architecture for the terminal client, so this report does not infer internal module boundaries from it.

### OpenCode

**Fact.** OpenCode makes the product split explicit: `opencode` starts the TUI, while `opencode run` is the programmatic path and can emit JSON. Model selection uses a provider/model identity ([OpenCode CLI](https://opencode.ai/docs/cli/)). Its TUI exposes sessions, models, details, and command-driven navigation ([OpenCode TUI](https://opencode.ai/docs/tui/)).

**Fact.** OpenCode models subagent work as child sessions and provides parent, first-child, and sibling navigation through configurable keybindings ([OpenCode agents](https://opencode.ai/docs/agents), [OpenCode keybindings](https://opencode.ai/docs/keybinds)). Current `run` source explicitly distinguishes non-interactive operation, an interactive local mode backed by an in-process server, and interactive attachment; it also owns JSON output, resume, and fork choices outside the TUI ([OpenCode `run` source](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/cli/cmd/run.ts)).

**Fact.** OpenCode's official extraction specification places its TUI in a standalone package using OpenTUI/Solid and a generated SDK boundary. The specification requires renderer cleanup on normal exit, interruption, startup failure, and destruction; keeps named non-TUI commands from initializing the TUI; and calls for package tests, snapshots, and tmux smoke tests ([OpenCode TUI package specification](https://github.com/anomalyco/opencode/blob/dev/specs/tui-package.md)). The repository describes core/server and SolidJS/OpenTUI as separate implementation areas ([OpenCode contributing guide](https://github.com/anomalyco/opencode/blob/dev/CONTRIBUTING.md)). OpenTUI itself uses a Zig native core with TypeScript bindings and React/Solid renderers ([OpenTUI repository](https://github.com/anomalyco/opentui)).

**Fact.** OpenCode documents animation and terminal-theme settings, but its public documentation does not promise a linear screen-reader mode comparable to Claude Code's or Gemini CLI's. An official repository issue from a screen-reader user records problems with animation, emoji, Unicode presentation, and interactive navigation; it is evidence of a reported gap, not a product guarantee ([OpenCode accessibility issue](https://github.com/anomalyco/opencode/issues/8565)).

**Inference.** OpenCode validates child-session navigation and strict separation of named automation commands from TUI initialization. Its server/SDK/TUI and native OpenTUI topology is excessive for Arany's one-process beta. Arany should adapt the parent/child navigation model inside one in-process presentation module and make accessibility a release gate rather than a theme setting.

### Gemini CLI

**Fact.** Gemini CLI separates headless mode from the interactive UI. Headless operation activates through `-p` or non-TTY use and supports text, JSON, and streaming JSON with usage, latency, error, and exit information ([Gemini CLI headless mode](https://geminicli.com/docs/cli/headless/)).

**Fact.** Gemini CLI defaults `useAlternateBuffer` to false. Its settings expose alternate-buffer choice, screen-reader mode, model/context footer visibility, context percentage, spinner behavior, and incremental rendering. Incremental rendering is tied to alternate-screen use because it reduces flicker and artifacts there ([Gemini CLI settings](https://geminicli.com/docs/cli/settings/), [Gemini CLI configuration](https://geminicli.com/docs/reference/configuration/)).

**Fact.** Its documented keyboard contract includes task display, detail display, error detail, copy support in alternate-screen mode, mouse control, redraw, suspension, and escape behavior ([Gemini CLI keyboard shortcuts](https://geminicli.com/docs/reference/keyboard-shortcuts/)). Its todo UI distinguishes pending, in-progress, completed, cancelled, and blocked work and can expand above the input area ([Gemini CLI todos](https://geminicli.com/docs/tools/todos/)). Gemini subagents run with focused context and toolsets and return results to the main agent ([Gemini CLI subagents](https://geminicli.com/docs/core/subagents/)).

**Fact.** Official source dynamically imports the heavy interactive UI only when it is needed and routes non-interactive operation through a separate path ([Gemini CLI entry point](https://github.com/google-gemini/gemini-cli/blob/main/packages/cli/src/gemini.tsx)). The interactive application uses React/Ink containers and contexts for UI state and terminal behavior, separate from core request/tool handling ([Gemini CLI application container](https://github.com/google-gemini/gemini-cli/blob/main/packages/cli/src/ui/AppContainer.tsx), [Gemini CLI core architecture](https://geminicli.com/docs/core/)).

**Inference.** Gemini CLI validates default inline behavior, explicit accessible presentation, dynamic avoidance of TUI startup in headless mode, and semantic task states. Arany should adopt those boundaries and use a smaller Rust-native implementation.

### goose

**Fact.** goose demonstrates the other viable terminal style: an enhanced line-oriented REPL rather than a full-screen cell renderer. Current CLI dependencies include `rustyline`, `cliclack`, `console`, `indicatif`, and `anstream`, not Ratatui/Crossterm ([goose CLI dependencies](https://github.com/aaif-goose/goose/blob/main/crates/goose-cli/Cargo.toml)).

**Fact.** The official CLI source separates interactive `session` from headless `run`. `run` supports text, JSON, and stream-JSON formats, quiet and interactive switches, resume/no-session choices, and provider/model overrides; session commands include list, export, and import ([goose CLI source](https://github.com/aaif-goose/goose/blob/main/crates/goose-cli/src/cli.rs), [goose CLI command guide](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/goose-cli-commands.md)).

**Fact.** goose's subagent documentation shows inline subagent identity and parallel-task status/summary behavior, including the possibility that a failed subagent returns no output ([goose subagents](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/context-engineering/subagents.mdx)).

**Inference.** goose proves that polished CLI feedback does not require an alternate-screen TUI. Arany should combine goose's native-scrollback friendliness with a small inline Ratatui viewport because three simultaneously meaningful agents need a stable team overview that a succession of spinners cannot provide.

## Adopt, adapt, reject

| Pattern | Classification | Arany beta use |
|---|---|---|
| Separate interactive and automation paths | **Adopt** | Select the command before starting a run; do not initialize terminal modes in `exec` text/JSONL operation. |
| Final answer on stdout, live human state on stderr | **Adopt** | Retains pipe composition while giving an attached human a live view. |
| One canonical event/read model behind every presentation | **Adopt** | The UI may select, fold, and scroll; it may not invent run facts. |
| Primary/child rows and selected-agent detail | **Adapt** | Show one primary row by default and a bounded conditional shelf; `/agents` owns the complete ordered list and details. |
| Causal task/agent navigation | **Adapt** | Use parent assignment, durable cause sequence, waiting-on edges, and handback rather than child chat sessions or mutable task boards. |
| Usage, cost, and context indicators | **Adapt** | Display only values with source metadata; say `unavailable` rather than estimate silently. |
| Inline viewport with native scrollback | **Adopt** | Stable current state at the bottom; bounded committed event lines above it. |
| Alternate screen | **Reject for beta** | It removes native scrollback and demands in-app transcript, search, copy/export, screen-reader, startup-fallback, and restoration parity. |
| Split panes, mouse-first controls, configurable keymaps | **Reject for beta** | Transient picker mouse support with keyboard parity is sufficient; a pane system would turn the harness into a miniature IDE. |
| Durable bounded composer | **Adopt** | One Session accepts ordered Messages; active Runs retain drafts but do not accept hidden in-flight steering. |
| Token streaming or raw child transcript | **Reject** | Model text is untrusted and raw reasoning is not a product artifact. |
| Generic server/SDK/client split | **Reject for beta** | One process and one client need an in-process presentation boundary, not transport. |
| Decorative spinner and timer animation | **Reject** | Event-driven state changes communicate more and satisfy reduced-motion behavior by construction. |
| Color-only state or icon-only state | **Reject** | Every state and severity has a word label. |
| Persisting terminal layout, focus, or scroll position | **Reject** | These are ephemeral presentation preferences, never canonical Events. |

## Exact command and mode contract

### Commands

**Recommendation.** Replace the earlier implicit human/`--jsonl` presentation choice with this explicit command separation:

```text
arany [--state-dir DIR]
      [--screen-reader]
      [--no-color]
      [--provider PROVIDER]
      [--model MODEL]
      [--file FILE]...
      [--dir DIR]...
      [MESSAGE]

arany --continue
arany --resume [SESSION_ID]
arany --fork SESSION_ID

arany [--state-dir DIR] exec
      [--output text|jsonl]
      [--provider PROVIDER]
      [--model MODEL]
      [--file FILE]...
      [--dir DIR]...
      OBJECTIVE

arany [--state-dir DIR] show
      [--output text|jsonl]
      SESSION_OR_RUN_ID
```

`--jsonl` may remain as a compatibility alias for `exec --output jsonl` and `show --output jsonl`; if there is no released compatibility promise yet, document only `--output` and keep one parser spelling. `--provider` and `--model` belong to the provider-routing decision, but the selected canonical identifiers must be present in every presentation.

**Recommendation.** Semantics:

- Bare `arany` requires terminal stdin and stderr. Redirecting stdout alone remains valid, so a human can watch stderr while piping committed assistant results. When `TERM=dumb`, the terminal is too small, or required capabilities are unavailable, it uses the linear attached-terminal fallback and states that choice once; it never emits viewport control sequences first and then silently changes formats.
- `arany --screen-reader` selects that linear attached-terminal presentation deliberately. It is also available through `ARANY_SCREEN_READER=1`. It emits flat, labeled, non-rewriting lines and no animation. `NO_COLOR` affects color only and must not be treated as a screen-reader signal.
- `arany --no-color` and `NO_COLOR` remove color from the inline viewport. State words, attention prefixes, borders, and selection markers remain sufficient.
- `exec --output text` is the default non-interactive contract. It writes deterministic append-only committed status and diagnostics to stderr and the final primary result to stdout. Its bytes do not change when any stream happens to be a TTY.
- `exec --output jsonl` emits deterministic, compact, versioned JSON objects on stdout and typed diagnostics on stderr. No ANSI, progress spinner, cursor control, OSC, terminal title, or human preamble is permitted.
- `show` is deterministic replay, not a live client. `show --output text` prints the reconstructed `RunView` and bounded event timeline to stdout. JSONL prints the stored Events in sequence order. It never initializes raw mode or the terminal renderer.

**Inference.** Requiring terminal stdin and stderr for bare interactive mode, while permitting redirected stdout, preserves Unix composition without making automation ambient-state dependent. The final answer remains a useful stdout artifact; the live UI owns only the controlling input and human-status channel.

### Contract matrix

| Invocation | stderr | stdout | Input ownership | Byte stability |
|---|---|---|---|---|
| bare `arany` with usable terminal | inline composer/footer/shelf, native-scrollback transcript, diagnostics | committed primary results | composer, navigation, and cancellation keys | semantic contract; terminal bytes are not a stable API |
| `arany --screen-reader` or attached linear fallback | flat labeled lines, diagnostics | committed primary results | line input and signals; no raw mode | exact stable line format |
| `exec --output text` on any streams | append-only committed lines, diagnostics | final primary result | signals only; no raw mode | exact stable format |
| `exec --output jsonl` on any streams | typed diagnostics only | one JSON object per line | signals only; no raw mode | exact versioned schema |
| `show --output text` | diagnostics only | replayed view and timeline | none | exact stable format |
| `show --output jsonl` | diagnostics only | stored Events | none | exact versioned schema |

**Implementation gate.** Command separation and bare-interactive eligibility must be covered at the real process boundary with combinations of TTY stdin/stderr/stdout and pipes. A library-only test of `is_terminal()` is insufficient. `exec` output must remain byte-identical across those combinations.

## Exact beta information architecture

### Full layout at 80 columns or wider

**Recommendation.** The live view has four semantic regions. It redraws only after a committed Event, a navigation key, a resize, cancellation, or terminal recovery; it has no idle animation loop.

```text
ARANY  run 7K4P  WORKING  durable #12  calls 3/4  elapsed 00:18
provider openai  model gpt-5  input 18.2k/128k reported  cost unavailable
──────────────────────────────────────────────────────────────────────────────
TEAM
▶ primary    WAITING   waiting on child-b           last change #10
  child-a    DONE      research returned            provider openai / gpt-5
  child-b    RUNNING   comparing provider contracts provider anthropic / opus
──────────────────────────────────────────────────────────────────────────────
SELECTED  root
assigned: user objective (#1)
state: waiting for child-b; child-a result retained
progress: delegated two bounded comparisons; one result joined
next checkpoint: child-b handback or cancellation
usage: input 18.2k, output 2.1k, context 16% [provider reported]
──────────────────────────────────────────────────────────────────────────────
TIMELINE  newest committed events
#08 child-a provider call finished · 6.4k in / 812 out
#09 child-a completed · result retained
#10 primary waiting on child-b · caused by assignments #4 and #5
──────────────────────────────────────────────────────────────────────────────
↑↓ agent  Tab region  Enter detail  t timeline  a attention  ? help  ^C cancel
```

The sample text is illustrative, not a promise that every provider reports every metric.

### Region responsibilities

**Recommendation.** The header contains only run-wide facts:

- short Run ID with full ID available in detail/help and the final receipt;
- run state as a word;
- last committed global sequence, labeled `durable`;
- completed/admitted Provider calls for the pinned Run policy;
- elapsed duration measured by Arany's monotonic clock while live;
- current/default provider and model, or `mixed` when calls differ;
- aggregate token/context/cost facts only when their units and provenance are compatible.

**Recommendation.** The activity shelf is conditional. Idle Sessions show no agent row; a single active Run shows the primary; multi-agent or attention states show at most three stable spawn-ordered rows plus `+N more`. Every row contains the stable agent label, a textual state, one bounded current fact, and either last-change sequence or provider/model. Never reorder rows by activity or completion. `/agents` opens the complete ordered list and detail. The selection marker is redundant with color.

**Recommendation.** Selected-agent detail contains:

- parent and causal assignment sequence;
- bounded assignment objective;
- current Engine state and typed reason;
- most recent safe progress summary, explicitly attributed to the child when authored by a model;
- next observable checkpoint;
- provider/model for the active or most recent call;
- provider-reported or locally measured usage with provenance;
- blocker, cancellation, or terminal error facts;
- terminal result status and whether a partial result was retained.

**Recommendation.** The timeline shows the newest committed semantic Events, not model token deltas and not terminal UI actions. Each row starts with durable sequence and actor. `t` toggles run-wide versus selected-agent filtering. Page Up/Page Down scroll a bounded in-memory window backed by `RunView.events`; `Home`/`End` move to oldest/newest loaded event. `show` remains the complete replay path.

**Recommendation.** An attention band replaces the selected detail when unresolved attention exists:

```text
ATTENTION  child-b failed at provider call #4
reason: provider request timed out after 60s
effect: Run failed without synthesis; child-a result retained for inspection
recovery: Run ID 7K4P · replay through durable #14 with `arany show 7K4P`
```

The beta is read-only and has no permission prompts, so “attention” means failure, cancellation delay, persistence/rendering degradation, or an unrecoverable provider/configuration problem. Do not build a generic approval inbox before effectful Tools exist.

### Narrow terminals

**Recommendation.** Layout changes are semantic rather than proportional:

- At 80 columns and wider, show all four regions.
- From 50 through 79 columns, show header, team, and one focused region; `Tab` switches selected detail and timeline.
- Below 50 columns, show a compact header, one team row per agent, the attention line if present, and a two-line footer. `Enter` replaces the team list with selected-agent detail; `Esc` returns.
- At fewer than 8 rows, abandon the dashboard before raw mode and use the linear attached-terminal presentation with a one-time diagnostic. Do not oscillate modes on every resize.

Truncation must use terminal cell width and grapheme boundaries. Untrusted text is sanitized before width calculation. The final cell may use `…`; a detail view exposes the bounded full value. CJK, combining marks, emoji sequences, and right-to-left controls belong in the width/security corpus.

**Implementation gate.** Exact breakpoints may move after a prototype, but the three semantic layouts and the minimum-row fallback must pass fixtures at 40, 50, 79, 80, and 120 columns before implementation is considered stable.

## Multi-agent semantics the UI requires

### State vocabulary

**Recommendation.** Use finite textual states shared by `RunView`, `exec` text, JSONL, the screen-reader presentation, and the TUI:

| Scope | States |
|---|---|
| Run | `starting`, `working`, `waiting`, `synthesizing`, `cancelling`, `completed`, `failed`, `cancelled`, `interrupted` |
| Agent | `queued`, `running`, `waiting`, `completed`, `failed`, `cancelling`, `cancelled`, `interrupted` |

`waiting` always has a typed reason such as `children`, `provider`, or `store`; the UI renders the reason as a fact. A primary waiting for one child names that child. `failed` names the failed phase and preserves successful siblings. No child disappears from the active Run view when it becomes idle or terminal.

### Causal assignment and orchestrator messages

**Recommendation.** Every child spawn carries `parent_agent_id`, a bounded assignment label/objective, and `caused_by_sequence`. Every root wait contains an ordered `waiting_on` set. Every child handback records whether its result was accepted, retained, rejected as invalid, or absent. The timeline can then render facts such as:

```text
#04 primary assigned child-a: compare terminal lifecycle
#05 primary assigned child-b: compare agent-team feedback
#10 primary waiting on child-b; child-a result retained
#12 child-b handed result to primary; caused by assignment #05
```

**Recommendation.** Do not add an unbounded peer mailbox for this fixed workflow. “Orchestrator messages” in the beta are typed, durable coordination facts: assignment, waiting set change, cancellation request, result handback, and join outcome. If future agents exchange arbitrary authored messages, introduce a separately bounded and versioned `AgentMessage` only then.

### Safe progress without chain-of-thought

**Recommendation.** Render two visibly distinct categories:

- **Engine facts:** call started/finished, assignment created, waiting set changed, result retained, cancellation requested, error classified, Event committed.
- **Agent-authored summary:** a bounded declarative summary of work product or next deliverable, marked `child summary` when applicable. It must not request or expose private reasoning traces.

A useful summary says “Compared the five official CLI contracts; assembling differences in cancellation and scrollback.” It does not say “My reasoning is…” or print hidden scratch work. Provider token streams, reasoning items, raw prompts, and tool payloads never become dashboard text merely because a provider emitted them.

**Recommendation.** Bound each summary in bytes, lines, update frequency, and stored count. The beta can retain the most recent summary per agent plus committed timeline facts. Sanitization removes terminal controls, carriage-return rewriting, backspace, OSC, C0/C1 controls, bidi overrides, and forged Arany prefixes before display; JSONL preserves safe structured values rather than terminal-ready strings.

### Usage, cost, and context

**Recommendation.** Every displayed measurement includes a source class:

- `provider reported`: returned by the provider API;
- `arany measured`: bytes, calls, elapsed duration, or retry count measured locally;
- `calculated`: derived from a documented tokenizer/model limit or pinned price table, with version;
- `estimated`: deliberately approximate and never mixed into a reported total;
- `unavailable`: the honest default.

Context percentage is shown only when Arany has both a token count of known semantics and an authoritative limit for the exact provider/model. Cost is shown only when reported by the provider or calculated from a pinned, named pricing snapshot; mixed currencies or incomparable accounting must not be summed. Cache-read/write tokens remain separate when providers expose them.

**Recommendation.** Provider and model attribution is per Provider call and per AgentRun, not just global. The header may say `mixed`; selected detail identifies the exact canonical provider ID, model ID, call ordinal, and usage source. Endpoint/base URL is configuration provenance and should be shown only in a diagnostic view after secret-safe normalization.

### Unique Arany feedback advantages

**Recommendation.** Arany can be more informative than the reviewed products in six narrow, trustworthy ways:

1. **Durability watermark:** every view says `durable #N`; a truncated observer knows exactly where replay begins.
2. **Causal join ledger:** the primary row names every outstanding child, the assignment sequence that caused it, and which sibling results are already retained.
3. **Complete terminal ledger:** every primary and child ends as completed, failed, cancelled, or interrupted; failed branches never vanish from the active Run view.
4. **Partial-result accounting:** failure and cancellation state which successful child results survived and that synthesis did not occur unless every required child succeeded.
5. **Measurement provenance:** provider/model, context, tokens, and cost never appear as unexplained numbers.
6. **Projection parity:** interactive bare `arany`, text/JSONL `exec`, and `show` expose the same semantic facts from the same committed Events. Selection, folding, and width are the only UI-local differences.

These advantages explain “what happened, why this agent exists, what the root is waiting for, and what is recoverable” without revealing chain-of-thought.

## Keyboard and terminal behavior

### Keyboard model

**Recommendation.** Keep one small, discoverable keyset:

| Key | Active-run behavior |
|---|---|
| `Up` / `Down` | select previous/next agent |
| `Tab` / `Shift+Tab` | cycle team, detail, timeline |
| `Enter` | open/expand selected-agent detail |
| `Esc` | close help/detail overlay; does not cancel a run |
| `t` | toggle run/selected-agent timeline |
| `a` | jump to unresolved attention |
| `PageUp` / `PageDown` | scroll focused timeline/detail |
| `Home` / `End` | first/last loaded item |
| `?` | static help overlay |
| `Ctrl+L` | clear damage and force complete redraw |
| `Ctrl+C` | request graceful run cancellation |
| second `Ctrl+C` while cancelling | force local shutdown after persisting the strongest possible interrupted/cancelled state |
| `Ctrl+Z` on Unix | restore terminal, suspend, then reacquire and redraw after `SIGCONT` |

Do not bind `q` to cancellation during an active Run; an accidental letter must not stop work. After a terminal result, `q`, `Esc`, `Enter`, or EOF may dismiss detail while the Session remains available. Keyboard access is complete. Mouse reporting may activate only while an explicit command, Session, or agent picker owns it; every mouse action has a keyboard equivalent and closing the surface restores normal native scrollback immediately.

### Cancellation

**Recommendation.** The first cancellation action is a domain request, not an immediate process exit. The UI changes to `CANCELLING`, names the active Provider calls, and waits for bounded cooperative cancellation/store closure. A second `Ctrl+C` is explicitly forceful. `SIGTERM` follows the first-stage graceful path with a bounded deadline, then returns the documented signal-compatible exit class. Plain and JSONL modes use the same cancellation owner without raw terminal input.

The final stderr receipt after restoration always includes Session/Run IDs, terminal reason, last durable sequence, retained child-result facts, and the exact replay command. The primary result is printed to stdout only after a documented successful Run; errors never masquerade as a normal result.

### Terminal acquisition and restoration

**Recommendation.** Terminal acquisition is transactional:

1. determine mode and terminal size before the Engine starts;
2. install panic/signal restoration support;
3. create one RAII terminal owner;
4. enable raw mode and only the modes Arany actually uses;
5. start the Engine and event loop;
6. on every exit path, disable modes in reverse order, show the cursor, restore line wrapping and raw mode, then print the final receipt.

Because the beta uses inline mode, it should not enable alternate screen, mouse capture, focus reporting, terminal-title OSC, or clipboard OSC. Bracketed paste is unnecessary without an editor. Fewer enabled modes mean fewer restoration hazards.

**Recommendation.** Restoration runs on normal completion, cancellation, signal, render error, Engine error, panic, startup failure after partial acquisition, and suspend. If restoration itself fails, write a minimal best-effort diagnostic after attempting cleanup: `terminal restoration failed; run reset or restart the terminal`. Never include model or provider payload in that message.

**Fact.** Ratatui's `Terminal` manages double-buffered diffs, cursor synchronization, autoresize, and viewport drawing; current helpers also document restoration on return and panic ([Ratatui `Terminal`](https://docs.rs/ratatui/latest/ratatui/struct.Terminal.html)). **Inference.** Arany still needs its own guard because signal, suspension, fallback-to-linear, and post-restore receipt behavior are product semantics rather than library defaults.

### Resize and redraw

**Recommendation.** Consume Crossterm resize events, obtain the current terminal dimensions, rebuild the semantic layout, and force a complete Ratatui redraw. Coalesce a burst of resize events into one bounded refresh, but never delay a committed terminal/error Event behind an unbounded debounce. `Ctrl+L` invalidates the full buffer. The view does not keep prewrapped model strings; it wraps sanitized bounded text for the current width.

**Fact.** Crossterm's event facility supplies keyboard, mouse, focus, paste, and resize events, and raw mode is required for immediate key event handling ([Crossterm events](https://docs.rs/crossterm/latest/crossterm/event/)). **Recommendation.** Arany subscribes only to key and resize semantics in the beta.

### Scrollback and output capture

**Recommendation.** The inline viewport occupies a bounded bottom region. After each newly committed semantic Event, insert at most one sanitized summary line above the viewport, then redraw current state. Ratatui's inline viewport and `insert_before` API are designed for output preceding an embedded UI ([Ratatui inline viewport](https://docs.rs/ratatui/latest/ratatui/enum.Viewport.html), [Ratatui `insert_before`](https://docs.rs/ratatui/latest/ratatui/struct.Terminal.html#method.insert_before)). Rate-limit/coalesce nonterminal progress summaries so the terminal is not flooded; lifecycle, error, cancellation, and completion facts are never dropped.

Interactive terminal byte sequences are not a capture API. Documentation must tell users and CI to use `arany exec`, `exec --output jsonl`, or `show`. A terminal recorder such as `script` may preserve visual control bytes but is outside the stable contract. On completion, restoration occurs before the durable final receipt, so copied logs end in readable text.

### Accessibility and motion

**Recommendation.** Accessible operation is a first-class presentation, not merely monochrome TUI:

- `--screen-reader` uses linear, append-only, labeled lines and native scrollback;
- it never enters raw mode, rewrites a line, draws boxes, emits a spinner, or requires arrow-key focus;
- every state, severity, selection, and causality edge has a textual label;
- errors and attention appear once when committed and again in the final receipt;
- tables render as sentences or one record per line;
- no information is encoded only by color, icon, column alignment, or cursor position.

**Recommendation.** The standard interactive view also has effectively reduced motion: it redraws on events and user actions only; it has no spinner, shimmer, cursor pulse, or continuously changing elapsed-time ticker. A duration is recomputed on meaningful redraw. Therefore the beta needs no separate reduced-motion flag. If animation is later introduced, explicit `--reduce-motion`/configuration and host detection can be considered, but host detection must never replace an accessible mode.

**Implementation gate.** Ship only after automated assertions prove screen-reader output contains no CSI/OSC/control rewriting and after manual use with VoiceOver on macOS and Orca on Linux. Automated “screen reader detected” probes are not a substitute for those workflows.

### Linux and macOS

**Recommendation.** Linux and macOS receive the same keys and semantic output. Platform-specific code is limited to terminal ownership and Unix job control. Required native evidence includes:

- common terminal emulators plus tmux on each operating system;
- `SIGINT`, `SIGTERM`, `SIGTSTP`, and `SIGCONT` behavior;
- terminal dimensions at startup and resize;
- termios restoration after success, error, panic, cancellation, and suspension;
- UTF-8 width fixtures including combining, CJK, and emoji content;
- closed/broken stderr and lost controlling-terminal behavior;
- no assumption about macOS Option-key escape sequences or Linux desktop services;
- no terminal-name allowlist as correctness policy.

Unknown or incapable attached terminals fail closed to the linear interactive presentation before terminal control is enabled. A nonterminal bare invocation fails with an instruction to use `exec`. Terminal quirks may be used for narrowly documented compatibility fallbacks, never to change Engine behavior.

## Rust implementation shape

### Library choice

**Recommendation.** Add Ratatui and Crossterm only after dependency review. Ratatui supplies the inline viewport, buffered/diff rendering, dimension handling, widgets, and a memory TestBackend. Crossterm supplies Unix terminal control and input/resize events without committing Arany to a web/native UI framework ([Ratatui](https://github.com/ratatui/ratatui), [Crossterm](https://github.com/crossterm-rs/crossterm)).

**Inference.** goose's line-oriented dependencies cannot provide a stable simultaneous team overview without custom cursor logic. OpenTUI would add a Zig/TypeScript/native boundary that contradicts the Rust-first, one-package beta. Direct ANSI/termios implementation would duplicate restoration, event, and portability work without creating product value. Ratatui/Crossterm is the smallest supported choice that matches the needed interface.

**Recommendation.** `expectrl` is a reasonable development-only PTY dependency for native process tests because its official project automates terminal applications through pseudo-terminal interaction and supports async/logging workflows ([expectrl repository](https://github.com/zhiburt/expectrl), [expectrl documentation](https://docs.rs/expectrl/latest/expectrl/)). Pin it and review its transitive/native surface. If it cannot exercise resize and job control reliably on both target systems, add a small test-only Unix PTY helper rather than weakening the product contract.

### Physical modules

**Recommendation.** Keep one package and add only two presentation files:

```text
src/
├── main.rs          CLI parsing, mode selection, composition, exit mapping
├── lib.rs           Engine, AgentRun tree, Events, RunView; no UI logic
├── provider.rs      Provider interface and adapters
├── store.rs         dedicated SQLite owner
├── presentation.rs  pure bounded RunView -> PresentationModel; plain rendering
└── terminal.rs      Ratatui widgets, Crossterm event loop, RAII terminal owner
```

Optional `telemetry.rs` remains independent after its existing trigger. Do not add a renderer trait merely because there are functions for plain and terminal output. Do not create a UI crate, client protocol, async terminal task graph, or general component framework.

`presentation.rs` owns stable labels, semantic grouping, safe summary selection, usage provenance formatting, width-independent row models, and append-only human lines. It is pure and accepts only snapshots/committed updates. `terminal.rs` owns width, folding, focus, scrolling, keys, terminal modes, and drawing. `main.rs` chooses one output path before calling the Engine.

### Engine boundary

**Recommendation.** The Engine exposes committed `RunUpdate` values and a reconstructible `RunView`; it never imports Ratatui, Crossterm, terminal dimensions, key codes, colors, focus, or viewport state. A cancellation handle is an Engine control already needed by signals, not a TUI seam. A bounded update channel has explicit overflow behavior: semantic Events cannot be discarded; if the presentation consumer fails or lags, terminal mode is restored and the process falls back to committed linear summaries or terminates with a typed presentation failure according to the already-durable run state.

Selection, expanded rows, timeline offset, help visibility, last drawn size, and color are ephemeral `terminal.rs` state. Provider/model, usage, assignment cause, waiting reason, terminal status, and safe progress summary are domain/read-model data and must survive `show`.

**Implementation gate.** If the current Event/SessionView/RunView vocabulary cannot reconstruct the recommended facts, evolve that schema before writing widgets. Do not infer assignment causality or child status from arrival order inside the UI.

### Minimum Event/RunView additions

**Recommendation.** Preserve the small event vocabulary where possible, but require these structured fields in committed payloads or reduced views:

- parent agent and causal sequence for spawn/assignment;
- bounded assignment label/objective;
- typed agent/run state transition and reason;
- ordered root `waiting_on` set;
- Provider call ordinal, canonical provider/model IDs, start/finish/outcome;
- usage values plus provenance and exact semantic units;
- bounded safe progress summary plus author class;
- child handback outcome and retained/absent result fact;
- cancellation requester, stage, and terminal disposition;
- classified error safe for user display;
- last committed sequence per row and globally.

Prefer structured payloads on the existing semantic Events to creating a separate terminal-event stream. Introduce a new Event kind only when the fact cannot be faithfully represented and replayed under the current schema. UI frame events, keypresses, selection, and resize are never persisted.

## Verification plan and release gates

### Pure projection and renderer tests

**Fact.** Ratatui's `TestBackend` stores a memory buffer and supports whole-terminal integration tests, cursor assertions, resize, and scrollback inspection; Ratatui recommends direct `Buffer` tests for isolated widgets and `TestBackend` for complete interfaces ([Ratatui `TestBackend`](https://docs.rs/ratatui/latest/ratatui/backend/struct.TestBackend.html)).

**Recommendation.** Use table-driven tests for:

- every run and agent state, wait reason, failure, cancellation stage, and partial-result combination;
- provider/model attribution and every usage-provenance class;
- team order independent of completion race;
- sequence watermark and causal assignment/handback;
- hostile control, bidi, prefix-forgery, long text, invalid-width edge cases;
- full/medium/narrow layouts at the required dimensions;
- monochrome and color semantic equivalence;
- selection/focus/timeline scrolling without changing `RunView`;
- `show`, `exec` text/JSONL, screen-reader output, and TUI fact parity.

Assert cells and semantic text, not terminal escape-byte implementations. Keep a few reviewed frame snapshots only where spatial relationships matter; prefer targeted buffer assertions to large brittle pictures.

### Real-process and PTY tests

**Recommendation.** Retain the existing product-process `ObservationBundle` for `exec` text and JSONL: exit class, exact stdout, exact stderr, reopened Events, and replayed `RunView`. Add a PTY observation lane rather than replacing byte goldens.

The PTY lane must cover:

- bare-interactive eligibility and `exec` invariance with all relevant TTY/pipe combinations;
- no terminal initialization in `exec` text/JSONL, `show`, usage-error, or too-small-terminal fallback;
- first and second `Ctrl+C` behavior;
- resize and `Ctrl+L` full redraw;
- selection and selected-agent detail;
- scrollback insertion of committed lines without duplicated terminal facts;
- normal, Engine-error, render-error, panic, EOF, `SIGTERM`, suspend, and resume restoration;
- terminal settings before and after the child process;
- final receipt printed after restoration;
- deterministic replay of the same persisted run through `show`;
- Linux and macOS native CI jobs, not a single emulated platform.

### Accessibility and security tests

**Recommendation.** Exact-output screen-reader fixtures prove linear labels, absence of CSI/OSC/carriage-return rewriting, one-time lifecycle announcements, actionable errors, and final replay receipt. Manual VoiceOver and Orca checks cover reading order, interruption, copied output, and error recovery.

Fuzz or corpus-test all renderer inputs for ANSI/ECMA-48 controls, OSC hyperlinks/title/clipboard attempts, C0/C1 controls, carriage return, backspace, tabs/newlines, bidi overrides, zero-width and combining characters, oversized strings, forged `[ARANY]`/agent prefixes, secret-shaped text, and invalid provider error payloads. The renderer, not the model, owns all control sequences.

### Gates

| Gate | Evidence required |
|---|---|
| G0 — contract | Architecture, decision register, CLI docs, security rules, and testing rules no longer assert byte-identical TTY output or no TUI. |
| G1 — semantic data | Events and `RunView` reconstruct state, cause, waiting sets, Provider-call attribution, safe summary, usage provenance, errors, and terminal dispositions. |
| G2 — static prototype | `TestBackend` frames and projection tests pass at 40/50/79/80/120 columns for normal, attention, cancellation, and completion states. |
| G3 — terminal safety | Native Linux/macOS PTY tests prove acquisition, cancellation, resize, suspend/resume, panic/error cleanup, and final receipt. |
| G4 — automation parity | Plain and JSONL exact bytes remain stable; no TUI dependency initializes; `show` reconstructs the same facts. |
| G5 — accessibility | Exact linear-output tests plus manual VoiceOver and Orca checks pass; no information is color, icon, or layout only. |
| G6 — security/dependencies | Ratatui, Crossterm, and PTY-test dependency feature/advisory/license review passes; hostile terminal-content corpus is inert. |
| G7 — performance/fallback | No idle redraw loop, bounded channel/history/text, acceptable resize/flicker, and tested early/mid-run fallback behavior. |

**Recommendation.** Until G0–G7 pass, `exec --output text` is the beta-safe executable path. After they pass, bare `arany` may ship as the attached-human default because the deterministic automation command is explicit and separately proven.

## Canonical documents and decisions that must change

The following inventory separates documents whose current canonical statement is directly contradicted from supporting research whose scope amendment would become misleading. The new report must be indexed rather than left as an orphan.

### Directly superseded canonical statements

1. **`docs/architecture/system-overview.md`** — replace the feedback claim that V1 has only append-only stderr/final stdout, no TTY detection, color, cursor, width, or resize. Add the bare-interactive/`exec` command split, Session-aware terminal projection boundary, `presentation.rs`/`terminal.rs`, accessibility fallback, and new verification map. Keep Engine, Events, SessionView/RunView, Provider seam, one package, and one process unchanged.
2. **`docs/architecture/arany-conceptual-overview.md`** — describe the CLI-only durable interactive Session plus deterministic text/JSONL `exec`/`show`; no web UI. Replace rejection of TTY-dependent output and the full-screen-TUI future trigger with the exact inline/no-alternate-screen decision. Update examples, physical topology, experience table, test flow, and non-goals.
3. **`docs/research/next-step-decision-register.md` D-08** — replace the old command/output contract and the explicit prohibition on a TTY branch, color, cursor movement, redraw, width, resize, and raw mode. Record the bare-interactive/`exec` split, terminal-eligibility guard, flags, channel ownership, accessibility, scrollback, cancellation, and restoration.
4. **`docs/research/next-step-decision-register.md` D-12** — add Ratatui and Crossterm to the reviewed direct dependency boundary and the PTY harness as a development dependency. A TUI is no longer “notably absent.”
5. **`docs/research/next-step-decision-register.md` D-14/testing decision and trigger table** — extend the `ObservationBundle` with a separate PTY lane and replace “measured terminal usability requires a richer renderer” as a future trigger with G0–G7 acceptance evidence.
6. **`docs/research/research-closure-audit.md`** — revise the implementation-decision amendment, CLI-feedback amendment, resolved decisions 8/12 as applicable, requirements R-41 and R-54, the CLI feedback evidence row, and all closing summaries that declare append-only feedback closed. Preserve RunView as user-visible truth and OTLP as noncanonical.
7. **`docs/research/README.md`** — add this report to the index and change the summary that SessionView/RunView are rendered only as append-only terminal output or JSONL.

### Supporting decisions that need amendments, not wholesale rewrites

8. **`docs/research/cli-inputs-configuration-and-operability.md`** — supersede sections 1.1, 3.1–3.4, rendering verification, dependency/layout advice, trigger table, and checklist items that require no `is_terminal`, ANSI, cursor, width, resize, raw mode, or alternate presentation. Preserve input bounds, stdout/stderr separation, JSONL determinism, and exact `exec` text-output tests.
9. **`docs/research/cli-user-feedback-patterns-from-agent-harnesses.md`** — its fixed “append-only, no TUI” scope and adopt/adapt/reject table are no longer current. Mark this report as the focused superseding terminal decision while retaining its source comparison and durable receipt/partial-result conclusions.
10. **`docs/research/multi-agent-loop-and-user-feedback.md`** — revise the 2026-09-29 scope amendment that defers interactive rendering. Its broader overview/tree/timeline/attention recommendations should be narrowed to the exact fixed-team information architecture here, not activated as a server/protocol projection system.
11. **`docs/research/testing-strategy-for-rust-cli-harness.md`** — remove the claim that V1 has no TUI tests, preserve exact-output acceptance for `exec` text/JSONL, and add `TestBackend`, PTY, restoration, resize, accessibility, width, and native Linux/macOS evidence owners without duplicating each semantic scenario.
12. **`docs/research/rust-foundation-and-engine-contract.md`** — update `main.rs` rendering responsibility, physical files and dependencies, `RunUpdate`/`RunView` fields, and verification. Keep all Ratatui/Crossterm types out of Engine interfaces.
13. **`docs/research/otlp-observability.md`** — replace statements that the terminal is append-only and that an interactive table was superseded. Preserve the invariant that both terminal projections and OTLP consume committed state and that OTLP cannot affect UI, cancellation, or exit status.
14. **`docs/research/modular-harness-architecture.md`** — revise only its scope amendment that categorically defers TUI. The one-package in-process presentation remains far smaller than the future multi-client architecture analyzed there.

### Repository rules and glossary

15. **`AGENTS.md`** — “CLI-only” remains correct. Update the module-boundary paragraph to name the private/presentation files only after they exist, and add an `agents/terminal.md` pointer when the terminal module has real invariants and workflow. Do not introduce a new crate or client seam.
16. **`agents/security.md`** — preserve the existing hostile-terminal-content rule and extend its verification checklist to Arany-owned CSI, raw mode, cursor, scrollback insertion, panic/signal restoration, resize, OSC prohibition, screen-reader output, and fallback. Model/provider text remains inert.
17. **`agents/testing.md`** — preserve the exact-byte `ObservationBundle` for `exec` text/JSONL and add the PTY/test-backend/native-platform lane as the owner for interactive behavior. State that raw terminal escape bytes are not golden API output.
18. **`CONTEXT.md` or the active domain glossary, if it currently defines terminal concepts** — add `PresentationModel`, interactive display, linear display, and terminal session only if those names enter code. Do not call the terminal a second Client or make selection/layout domain state.

**Inference.** Other research documents that mention a hypothetical future TUI but do not claim current append-only behavior are historical option analysis and need no edit. Search results alone are not a reason to churn them. The list above is the complete currently discovered set of authoritative or directly misleading documents for this decision; G0 requires a fresh repository search before implementation in case later documents add another canonical statement.

## Final recommendation

**Recommendation.** Make the beta feel like a trustworthy control room for three agents, not a scrolling debug log and not a miniature IDE. The right boundary is:

- inline, event-driven, keyboard-readable TTY presentation for a human;
- deterministic `exec` text and JSONL modes for CI, pipes, and capture, plus linear screen-reader interactive mode;
- native scrollback plus durable `show` replay rather than an alternate-screen transcript system;
- explicit team causality, waiting, partial results, provider/model provenance, and terminal disposition;
- no chain-of-thought, raw provider stream, global mouse dependency, general-purpose editor, daemon, or web UI; transient mouse support inside an open picker or panel is allowed;
- two small presentation modules outside Engine;
- release only after native PTY restoration and screen-reader evidence.

That product is materially more informative about multi-agent orchestration than the reviewed harnesses while remaining much smaller than their general-purpose interactive clients.
