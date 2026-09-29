# Terminal presentation

Load when changing interactive terminal behavior, plain or JSONL output, accessibility, cancellation keys, terminal ownership, presentation dependencies, or the user-visible projection of a Run.

The evidence and rationale live in `docs/research/beta-terminal-interface-and-multi-agent-feedback.md`. This file is the concise working contract.

## Product modes

- Bare `arany [PROMPT]` is the attached-human command. It requires terminal stdin and stderr, creates a durable Session, draws one bounded inline interface on stderr, preserves native scrollback, and writes committed assistant results to stdout.
- Without a prompt, bare `arany` opens the Session composer. With a prompt it starts the first Run after validation and remains interactive. `--continue`, `--resume`, and `--fork` own explicit Session entry semantics; never resume implicitly from TTY or directory detection.
- `arany exec --output text|jsonl` is deterministic and noninteractive. It never initializes terminal modes and is the contract for CI, pipes, and captured logs.
- `arany show --output text|jsonl SESSION_OR_RUN_ID` deterministically replays committed Events and never initializes terminal modes.
- Do not infer product behavior from TTY detection. The command selects the contract. An ineligible bare invocation fails before the Engine starts and directs the caller to `exec`.
- `--screen-reader` or `ARANY_SCREEN_READER=1` uses a labeled append-only presentation with no raw mode, redraw, boxes, cursor focus, or animation.
- `--no-color` and `NO_COLOR` disable color. No fact may depend on color, icon, alignment, or cursor position.

## Interactive commands

- The complete beta set is `/help`, `/status`, `/sessions`, `/new`, `/clear`, `/resume`, `/fork`, `/rename`, `/compact`, `/agents`, `/provider`, `/model`, `/permissions`, `/quit`, and `/exit`.
- Commands are compiled trusted local controls. Workspace text, instructions, providers, models, Skills, and persisted data cannot register, shadow, or redefine them.
- Only a single `/` at column zero followed by an exact lowercase registered name is command syntax. `//text` submits `/text` as the objective. Unknown leading commands are local errors with suggestions; they are never billed Provider input.
- While idle, `/provider [id]` and `/model [id]` update Session defaults for the next Run. During a Run they are view-only and report `locked for this run`.
- `/agents` opens agent details and configures `single`, `auto`, or `team` plus the bounded maximum active children for the next Run. It does not mutate an active topology.
- Session commands operate only at committed boundaries. `/clear` is the familiar alias of `/new`; it never means deleting history. `/compact` creates derived context and never replaces canonical history.
- During active work, the composer may retain a draft, but beta does not steer an in-flight Provider call. Local inspection remains available. `/quit` and `/exit` explain that `Ctrl+C` cancels the active Run before the Session exits.
- `exec` and `show` never interpret `/`, `@`, or `!`. A slash-prefixed `exec` objective is literal.
- `/permissions` reports effective facts: explicit bounded reads, no Workspace writes, no process Tools, selected Provider egress only, and no sandbox claim. It cannot change authority.
- Reserve `/approve`, editable `/permissions`, approval flags, and sandbox flags until typed effects, Policy, Guard, and attestation exist.

## Ownership and data

- `presentation.rs` is pure. It maps committed `SessionView` and active `RunView` state into bounded semantic rows and append-only transcript lines.
- `terminal.rs` owns Ratatui widgets, Crossterm input and resize events, inline viewport state, focus, scrolling, and one RAII terminal owner.
- The Engine exposes reconstructible facts and a cancellation handle. It imports no terminal library or presentation state.
- Interactive Arany, screen-reader mode, `exec`, and `show` expose the same committed facts. Draft text, shelf focus, palette selection, folding, width, hover, and loaded timeline window are presentation-local and never canonical Session state.
- Persist assignment cause, waiting sets, typed state reasons, provider-call attribution, usage provenance, safe progress summaries, handback status, cancellation stage, errors, and durable sequence. Never infer them from UI arrival order.
- Show Engine facts and bounded agent-authored work summaries. Never expose chain-of-thought, raw prompts, provider token streams, reasoning items, tool payloads, or raw error bodies.

## Terminal behavior

- Use a Ratatui inline viewport with native scrollback. The normal layout is transcript in native scrollback, composer, one compact status row, and a conditional agent activity shelf. Do not enter alternate screen or enable focus reporting, title/clipboard OSC, or bracketed paste.
- Keep the single-agent case visually primary: no empty team dashboard. Show one primary row during single work; expand to at most three activity rows plus `+N more` only when children exist or attention is required. `/agents` owns the full picker/details.
- Keyboard access is complete. Mouse reporting is disabled during normal transcript/composer use and may be enabled only transiently inside an open command, Session, or agent picker. Every mouse action has a keyboard equivalent, and closing or losing terminal ownership disables mouse reporting before control returns.
- Redraw only for committed updates, resize, or user actions. No spinner, animation, idle refresh, or continuously ticking elapsed clock.
- Keep the primary first and all visible child rows in stable spawn order. Every state and wait has a textual label; completed or failed children never disappear while their Run remains active.
- Use semantic layouts for widths `>=80`, `50..79`, and `<50`. If height is below eight rows or terminal capability is unsafe, select the linear attached-terminal presentation before raw mode and do not oscillate on resize.
- Sanitize untrusted text before width calculation. Truncate by grapheme and terminal-cell width, not bytes or scalar count.
- While idle, `Ctrl+C` clears a nonempty draft; on an empty draft two bounded presses exit. `Ctrl+D` exits only on an empty draft. During a Run, `Ctrl+C` requests graceful cancellation and a second press during cancellation forces bounded local shutdown after the strongest possible durable terminal state. `Esc` closes the focused picker/detail first; `q`, `Ctrl+D`, and slash commands do not cancel active work.
- On Unix, `Ctrl+Z` restores the terminal before suspension and reacquires it after `SIGCONT`.
- Restore raw mode, cursor, wrapping, and every enabled mode on success, failure, cancellation, signal, panic, render error, partial acquisition, and suspension. Print the final receipt only after restoration.
- If the interactive renderer fails after a durable Run has begun, restore the terminal and fall back only to committed sanitized linear facts or a typed presentation failure; never invent state.

## Verification gates

- Pure projection and TestBackend cases cover idle Session, single-agent, bounded team/overflow, attention, context, all agent states, causality, partial results, hostile text, monochrome equivalence, and widths 40/50/79/80/120.
- Native Linux and macOS PTY tests cover composer/palette/draft behavior, Session and agent pickers, transient mouse capture/restoration, exact command parsing, eligibility, raw-mode acquisition, resize, redraw, cancellation stages, EOF/signals, suspend/resume, panic/error cleanup, broken stderr, tmux, and terminal settings before/after.
- Exact screen-reader fixtures contain no CSI, OSC, carriage-return rewriting, or unlabeled state; manually verify VoiceOver on macOS and Orca on Linux before release.
- Raw interactive escape bytes are implementation detail, not golden output. Exact-byte goldens remain for `exec` text/JSONL, screen-reader output, final receipts, and `show`.
- Review Ratatui, Crossterm, and the development-only PTY dependency for features, advisories, licenses, native code, and transitive surface before acceptance.
