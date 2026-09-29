# Interactive CLI conventions and Arany's command surface

> **Session amendment — 2026-09-29:** [Beta Sessions, teams, terminal, providers, and license](./beta-sessions-teams-terminal-providers-and-license.md) supersedes this report's one-Run boundary and reserved Session commands. Bare `arany` now owns a durable multi-Run Session; `--continue`, `--resume`, and `--fork` are real startup semantics; `/sessions`, `/new`/`/clear`, `/resume`, `/fork`, `/rename`, and `/compact` join the closed registry. `/agents` owns both inspection and next-Run `single|auto|team` policy. Approval and sandbox controls remain deferred until their enforcement exists.

**Status:** canonical beta command recommendation; reconciled into the architecture and decision register

**Retrieved:** 2026-09-29

**Scope:** terminal startup flags, interactive commands, discovery, parsing, lifecycle, interruption, accessibility, automation separation, and the boundary between the read-only beta and future effectful execution

**Evidence policy:** “Fact” reports behavior from official product documentation or upstream source. “Inference” derives a consequence for Arany. “Recommendation” proposes a product decision. “Gate” states evidence required before shipping it.

## Executive decision

**Recommendation.** Arany should adopt the mature coding-harness interaction grammar without copying the mature products' accumulated surface area:

- bare `arany` is the attached-human entry point;
- `arany exec` is the noninteractive, deterministic entry point;
- `arany show` replays a persisted Session or Run;
- the interactive composer treats a leading `/` as local control syntax and opens a discoverable command palette;
- `/help`, `/status`, `/sessions`, `/new` (`/clear` alias), `/resume`, `/fork`, `/rename`, `/compact`, `/agents`, `/provider`, `/model`, `/permissions`, `/quit`, and `/exit` are the complete beta slash-command set;
- Provider, model, and team policy configure the next Run; a started Run pins them for every admitted call;
- `/permissions` truthfully displays the immutable read-only beta capability set; it does not pretend there is an approval mode or sandbox;
- machine mode never interprets slash commands; `arany exec "/status"` sends `/status` as objective text;
- repository content, Skills, providers, and models cannot register commands in the beta; and
- Session lifecycle commands operate only on durable committed state; none performs hidden model work except the visibly metered `/compact` operation.

**Recommendation.** Replace the planned public `arany run` spelling before implementation. The common convention is bare binary for the interactive client: `codex`, `claude`, `opencode`, and `gemini` all do this. Codex calls its automation path `exec`; OpenCode calls its automation path `run`; Claude and Gemini use explicit print/prompt flags. Keeping Arany's attached-human mode under `run` would invert OpenCode's meaning and add a word the other interactive clients omit. Because Arany has no released compatibility contract, an alias has no benefit.

**Recommendation.** Make Session durability a real Engine contract rather than terminal decoration. The beta attached-human flow is:

```text
start or explicitly resume Arany
  -> inspect or change next-Run settings with slash commands
  -> submit one Message
  -> monitor and control its bounded Run
  -> receive the durable result and receipt
  -> submit another Message or exit the durable Session
```

This adds a durable Session transcript, bounded Message composer, and local command palette. An optional positional Message starts the first Run immediately and then leaves the Session interactive. During an active Run, Arany accepts only local controls and retains draft text; it does not steer the in-flight Provider call or queue a hidden follow-up.

**Recommendation.** The beta therefore includes durable Session identity, ordered Messages, an explicit Message-to-Run relationship, bounded deterministic history compilation, resume/fork semantics, compaction provenance, provider/model change rules between Runs, and honest crash behavior. A Run remains the bounded execution and cancellation unit; the terminal must never infer a Session from unrelated Run replay.

## Primary-source comparison

### Entry points and automation

| Product | Attached human | Noninteractive | Resume | Permission/sandbox startup |
|---|---|---|---|---|
| OpenAI Codex CLI | bare `codex [PROMPT]` starts the TUI | `codex exec`; JSONL is explicit | `codex resume`, with `--last` and directory scoping | `--ask-for-approval` and `--sandbox` are separate dimensions |
| Claude Code | bare `claude [query]` | `claude -p`; text, JSON, or stream JSON | `-c` continues latest; `-r` resumes by ID/name; optional fork | `--permission-mode`; dangerous bypass is named and separate |
| OpenCode | bare `opencode [project]` | `opencode run`; optional JSON events | `--continue`, `--session`, and optional `--fork` | permission configuration plus `--auto` |
| Gemini CLI | bare `gemini` or an interactive positional query | `-p`; text, JSON, or stream JSON | `--resume`; latest, index, or UUID | `--approval-mode` and `--sandbox` are separate |

**Fact.** Codex's official command reference says the base command launches the interactive TUI, `codex exec` is for scripted or CI-style work, and `codex resume` reopens an interactive chat. Its global flags expose `--ask-for-approval` independently from `--sandbox`; the upstream shared option types also keep those controls distinct ([Codex command reference](https://developers.openai.com/codex/cli/reference), [Codex shared CLI options](https://github.com/openai/codex/blob/main/codex-rs/utils/cli/src/shared_options.rs)).

**Fact.** Claude Code documents bare interactive startup, `-p` for print/SDK mode, `-c` for the most recent directory-scoped conversation, and `-r` for an ID or name. Its permission flag accepts several modes, and its documentation distinguishes permission prompts from the isolation a sandbox or external container provides ([Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference), [Claude Code permission modes](https://code.claude.com/docs/en/permission-modes)).

**Fact.** OpenCode starts its TUI when invoked without a command and uses `opencode run` for scripting. Both paths support model selection in `provider/model` form and continuation by latest session or explicit session ID ([OpenCode CLI](https://opencode.ai/docs/cli/)).

**Fact.** Gemini's positional query remains interactive, `-p` forces noninteractive mode, `-i` explicitly supplies an initial interactive prompt, and `--resume` accepts latest, an index, or a UUID. It exposes `--approval-mode`, `--sandbox`, `--screen-reader`, and explicit machine output formats ([Gemini CLI reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/cli-reference.md), [Gemini configuration reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md)).

**Inference.** The strong convention is not a particular approval-mode vocabulary. It is a product split: the bare binary owns an attached terminal contract and a named flag or subcommand owns the machine contract. Arany should copy that split, not borrow `run` from one product while giving it the opposite meaning.

### Slash-command discovery and execution

**Fact.** Codex opens a filtered slash popup when the user types `/`. Its source represents commands as a closed enum with user-visible descriptions and deliberate presentation order. Current commands cover model and permission selection, agents, context compaction, new/resumed chats, status, and exit ([Codex slash-command documentation](https://developers.openai.com/codex/cli/slash-commands), [Codex `SlashCommand` source](https://github.com/openai/codex/blob/main/codex-rs/tui/src/slash_command.rs)).

**Fact.** Claude Code opens and filters a command menu on `/`, recognizes a command only when it starts the message, and treats following text as arguments. It can show completion in the middle of a prompt, but a mid-prompt command is not executed. Commands submitted while a response runs are normally queued, while selected read-only inspection commands can run immediately ([Claude Code commands](https://code.claude.com/docs/en/commands), [Claude Code interactive mode](https://code.claude.com/docs/en/interactive-mode)).

**Fact.** OpenCode documents `/help`, `/compact`, `/models`, `/new`, `/sessions`, and `/exit`, with a `Ctrl+X` leader for many equivalent keybindings. Its command list is deliberately smaller than Claude Code's. It also uses `@` for file references and `!` for direct shell execution ([OpenCode TUI](https://opencode.ai/docs/tui/), [OpenCode keybindings](https://opencode.ai/docs/keybinds/)).

**Fact.** Gemini distinguishes `/` meta-commands, `@` context inclusion, and `!` shell execution. It supports both built-ins and reloadable custom command definitions. Its upstream processor handles commands as typed results such as prompts, dialogs, confirmations, and handled actions rather than sending every slash-prefixed string to the model ([Gemini commands](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/commands.md), [Gemini slash-command processor](https://github.com/google-gemini/gemini-cli/blob/main/packages/cli/src/ui/hooks/slashCommandProcessor.ts)).

**Inference.** Users expect `/` to mean client control and expect typing `/` to teach them what is available. They do not expect every mature harness to have the same command inventory. Arany can feel familiar with a closed, small command set as long as discovery, naming, argument help, disabled-state explanations, and keyboard behavior are consistent.

### Status, model, permissions, and agents

**Fact.** Codex `/status` reports model, approval policy, writable roots, and context/token information; `/permissions` changes what the agent can do without asking; `/agent` navigates agent threads. Codex distinguishes `/permissions` from `/approve`: the latter retries one recent action denied by automatic review ([Codex slash commands](https://developers.openai.com/codex/cli/slash-commands)).

**Fact.** Claude Code `/model` selects a model, `/permissions` manages allow/ask/deny rules, `/tasks` inspects background work, and `/compact` summarizes long context. Permission modes and sandboxing are explicitly different controls ([Claude Code commands](https://code.claude.com/docs/en/commands), [Claude Code permission modes](https://code.claude.com/docs/en/permission-modes)).

**Fact.** Gemini provides `/model`, `/permissions`, `/policies`, `/agents`, `/stats`, and `/compress`. Its policy engine resolves `allow`, `deny`, or `ask_user`, and treats `ask_user` as deny in noninteractive mode ([Gemini commands](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/commands.md), [Gemini policy engine](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/policy-engine.md)).

**Fact.** OpenCode uses `allow`, `ask`, and `deny` permission outcomes and distinguishes one-time approval, a session-scoped matching approval, and rejection. Its `--auto` switch auto-approves requests not explicitly denied ([OpenCode permissions](https://opencode.ai/docs/permissions/), [OpenCode CLI](https://opencode.ai/docs/cli/)).

**Inference.** `/permissions` is the most stable cross-product noun. “Approval” is only one possible decision on one requested effect, while permissions, deterministic policy, and sandbox enforcement answer different questions. Arany should not collapse them into one `--approval` switch.

### Interrupt, EOF, redraw, and suspension

**Fact.** Claude Code uses `Ctrl+C` to interrupt a running operation, clear an idle draft, and exit on a second idle press. `Ctrl+D` exits only from an empty prompt and asks for a second press. `Esc` stops the current response or closes the active dialog, `Ctrl+L` redraws, and Unix `Ctrl+Z` suspends ([Claude Code interactive mode](https://code.claude.com/docs/en/interactive-mode)).

**Fact.** Gemini uses `Ctrl+C` to cancel the current request or quit from empty input, `Ctrl+D` to exit from an empty input buffer, `Esc` to cancel the current focus/dialog, `Ctrl+L` to redraw, and `Ctrl+Z` to suspend ([Gemini keyboard shortcuts](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/keyboard-shortcuts.md)).

**Fact.** OpenCode maps `Escape` to session interruption and `Ctrl+C`, `Ctrl+D`, and its leader-plus-`q` sequence to application exit ([OpenCode keybindings](https://opencode.ai/docs/keybinds/)). Codex documents `Ctrl+C` and `/exit` for leaving the session and supports queuing or injecting input while work is active ([Codex CLI features](https://developers.openai.com/codex/cli/features)).

**Inference.** There is no universal single-key contract. Context sensitivity is the convention: overlays consume `Esc`; nonempty drafts consume editing keys; active work has an explicit interruption path; empty idle input can exit. Arany should preserve its already-designed two-stage `Ctrl+C` cancellation and terminal-restoration contract rather than imitate one product's exact overloaded behavior.

## Exact beta surface

### Process commands

```text
arany [GLOBAL_OPTIONS] [OBJECTIVE]
arany exec [GLOBAL_OPTIONS] --output text|jsonl OBJECTIVE
arany show [--state-dir DIR] --output text|jsonl RUN_ID
arany --help
arany --version
```

`GLOBAL_OPTIONS` for the beta are:

```text
--state-dir DIR
--workspace DIR
--provider PROVIDER
--model MODEL
--include RELATIVE_PATH        repeatable
--otlp-endpoint URL
--screen-reader                attached-human mode only
--no-color                     attached-human mode only
```

The provider research remains authoritative for environment variables, credentials, endpoint profiles, and the absence of an implicit provider or model default.

**Recommendation.** Add only `-m` as a short alias for `--model` if short aliases are desired. Do not use `-p`: Claude and Gemini use it for prompt/print, Codex uses it for profile, and OpenCode also uses it for password in some subcommands. Long `--provider` is clear and infrequent.

**Recommendation.** Bare `arany` requires terminal stdin and stderr. With no Message, it opens the Session composer. With a positional Message, it validates admission, starts the first Run immediately, and remains interactive afterward. `arany exec` never initializes terminal state, never opens a picker, never asks a question, and fails closed when required configuration is missing.

**Recommendation.** Do not expose these flags in the read-only beta:

- `--approval`, `--approval-mode`, `--ask-for-approval`, or `--permission-mode`;
- `--sandbox` or any “full auto”/“yolo” alias;
- `--resume`, `--continue`, or `--fork`;
- `--prompt` or `-p`;
- `--json` on attached-human mode; and
- arbitrary `--config key=value` overrides.

The first two groups would imply effectful authority and isolation that do not exist. Resume/fork would imply a Session or resumable execution checkpoint. Prompt flags duplicate the positional objective and conflict across products. Machine formatting already belongs to `exec --output` and `show --output`.

### Beta slash commands

| Command | Before Run | During Run | Result |
|---|---:|---:|---|
| `/help [command]` | yes | yes | Show the searchable command list or exact syntax for one command. |
| `/status` | yes | yes | Show trusted configuration, Run state, durable sequence, budget, and usage provenance. |
| `/sessions` | yes | list only | Open the Session picker; switching is idle-only. |
| `/new`, `/clear` | yes | no | Create a new durable Session; `/clear` never deletes history. |
| `/resume [id]`, `/fork [id]` | yes | no | Resume an exact Session or fork one at a committed boundary. |
| `/rename [label]`, `/compact` | yes | no | Change the Session label or perform visible metered derived-context compaction. |
| `/agents` | yes | yes | Inspect AgentRuns and configure next-Run `single|auto|team(N)`; an active topology is locked. |
| `/provider [id]` | view/set | view only | Open the admitted-provider picker or select an exact native/verified profile. A started Run is locked. |
| `/model [id]` | view/set | view only | Open a provider-filtered model picker or select an exact supported model ID. A started Run is locked. |
| `/permissions` | yes | yes | Display effective beta capabilities: bounded explicit reads, fixed provider egress, no Tools, no process execution, no workspace writes, and no sandbox claim. |
| `/quit` | yes | explain only | Exit an idle Session after restoration. During a Run, explain that `Ctrl+C` requests cancellation. |
| `/exit` | yes | explain only | Exact alias of `/quit`. |

**Recommendation.** `/status`, `/agents`, and `/permissions` are local read-only projections over trusted configuration and committed `RunView`. They may run immediately during provider work. They never become Provider input and never consume a Provider call.

**Recommendation.** `/provider` and `/model` mutate only the next-Run selection. After `RunStarted`, they show the pinned selection and the message `locked for this run`; they do not queue a change. This preserves one provider/model/route across every admitted call and avoids a hidden partial failover. A later Run in the same Session may choose differently.

**Recommendation.** `/agents` owns both inspection and the next-Run policy: `single`, `auto(N)`, or `team(N)`. The compact default view remains single-agent and expands team activity only when delegation occurs. Use `/agents`, rather than inventing `/team`, because users already associate the word with agent inspection and configuration.

**Recommendation.** `/permissions` uses effective facts, not reassuring adjectives. A useful beta rendering is:

```text
Permissions
  workspace read: explicit --include snapshots only
  workspace write: denied (no write-capable Tool exists)
  process execution: denied (no process Tool exists)
  network: selected Provider endpoint only
  provider/model: openai / <model> (locked after RunStarted)
  sandbox: not applicable; Arany makes no sandbox claim
```

It must not expose credential presence beyond a safe configured/not-configured result and must never print paths or values that the security contract classifies as sensitive.

### Commands deliberately absent from beta

| Reserved command | Why it is absent | Activation requirement |
|---|---|---|
| `/plan` | The beta is already effect-free; “plan mode” would not change effective capabilities. | A future effectful mode transition backed by deterministic Policy/Guard. |
| `/approval`, `/approve` | There is no pending typed effect to approve. | `/approve` may exist only for one immutable pending intent; never as a blanket mode. |
| `/sandbox` | No effectful Tool or sandbox adapter exists. | Effective-capability attestation and platform enforcement. |
| `/diff`, `/undo`, `/restore` | Arany does not edit files. | Effect journal/checkpoint design plus deterministic restoration limits. |
| `/tools`, `/mcp`, `/skills` | No runtime Tool, MCP, or command-extension seam ships in the minimum slice. | Their respective security and extension gates. |
| `!command` | Direct shell mode bypasses the agent but is still an effectful process surface. | Process Tool, Policy, Guard, explicit user-originated intent rules, and PTY/job design. |
| `@path` | Workspace input is already an explicit bounded `--include` snapshot. | A separately designed interactive attachment flow with identical path admission. |

**Recommendation.** Reserve these names in documentation and parser tests; do not show disabled placeholders in the main beta palette. If a user types one exactly, return a local, actionable explanation. Never forward it to the Provider as an objective.

## Command parsing and collision rules

**Recommendation.** Use one closed typed command registry owned by the presentation layer. Each entry contains canonical name, aliases, availability predicate, argument parser, help text, and handler class. Do not represent commands as prompt templates.

Parsing rules are exact:

1. Only interactive attached-human input can interpret slash commands.
2. A command must begin at column zero with one `/` followed by a registered command or alias and then end-of-input or ASCII whitespace.
3. Command names are lowercase ASCII and case-sensitive. Completion may filter case-insensitively, but execution never uses fuzzy matching.
4. The palette opens on a bare `/` and filters as text is entered. Arrow keys move; `Tab` completes; `Enter` executes the selected exact command or submits the exact typed command.
5. A slash elsewhere in an objective is literal text. There is no mid-prompt execution.
6. `//text` at column zero escapes the command namespace and submits `/text` as the objective. The palette must teach this escape.
7. An unknown leading `/name` is a local error with nearest-name suggestions and the `//` escape hint. It is never silently dropped and never sent to the Provider.
8. Arguments use a command-specific typed parser. There is no shell splitting, interpolation, command substitution, or generic `key=value` mutation.
9. Slash commands are not appended to objective history, persisted as model-visible Messages, or exported as Provider content. Security-relevant configuration changes may produce a typed local audit Event only if the canonical Event design explicitly adopts one.
10. `exec` and `show` never interpret `/`, `@`, or `!` specially.

**Inference.** Exact execution plus fuzzy discovery avoids two failures: a typo performing the wrong local action, and a local command accidentally becoming paid Provider input. The explicit `//` escape also makes absolute paths, protocol-like text, and prose beginning with `/` representable.

## Keyboard and lifecycle contract

### Preflight composer

- `Enter` submits the objective or executes the exact selected command.
- `Tab` accepts command completion while the palette is open.
- `Up`/`Down` navigate command suggestions; outside the palette they navigate only current-process draft history.
- `Esc` closes the palette or dialog and preserves the draft.
- `Ctrl+L` performs a full redraw. It never clears persisted state or starts a new Run.
- `Ctrl+C` clears a nonempty draft. On an empty draft, the first press shows an exit hint and the second bounded press exits.
- `Ctrl+D` exits only when the draft is empty; otherwise it keeps normal forward-delete behavior where the terminal can distinguish it.
- `Ctrl+Z` restores the terminal before suspension and reacquires it after `SIGCONT`.

### Active Run

- free-text input is disabled; the footer says `Run active · / for commands · Ctrl+C to cancel`;
- `/` opens the local command palette containing only commands available during a Run;
- `Esc` closes the active overlay or returns focus to the Run view; it does not cancel;
- first `Ctrl+C` requests durable graceful cancellation;
- second `Ctrl+C` during cancellation forces bounded local shutdown after the strongest possible terminal state;
- `Ctrl+D`, `q`, and `/quit` do not abandon active work; `/quit` explains how to cancel;
- `SIGTERM` requests bounded cancellation and restoration; and
- final receipt/output appears only after terminal restoration.

### Machine mode

- `exec` has no composer, palette, confirmation, or terminal mode;
- missing required configuration is a startup error before `RunStarted`;
- a request that would need interaction fails closed rather than waiting on stdin;
- `SIGINT` requests cancellation and returns the documented interrupted exit class after bounded cleanup; and
- stdout/JSONL contracts remain deterministic and independent of attached TTY state.

**Inference.** This deliberately does not copy Claude/OpenCode's `Esc`-to-interrupt behavior. Arany's beta is a team monitor rather than a continuously streaming chat response, and `Esc` already has safe overlay/focus meaning. The visible `Ctrl+C` hint keeps cancellation conventional and unambiguous.

## Accessibility and help

**Recommendation.** The command surface must work without a popup:

- `/help` prints a labeled, stable command list in screen-reader mode;
- `/help model` prints exact syntax, current value, and whether the value is locked;
- completion announcements include the command name, short purpose, position, and availability;
- disabled actions have a textual reason, never only dim color;
- no command requires a mouse, icon interpretation, or spatial navigation;
- the screen-reader presentation never enters raw mode or emits CSI/OSC;
- the `//` literal escape and `Ctrl+C`/`Ctrl+D` behavior appear in help; and
- terminal width may change layout but never command vocabulary or semantics.

Claude Code documents flat labeled output and native scrollback for screen-reader mode, while Gemini exposes an explicit `--screen-reader` flag. Those precedents support an explicit accessible contract rather than relying on terminal auto-detection ([Claude Code accessibility](https://code.claude.com/docs/en/accessibility), [Gemini CLI reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/cli-reference.md)).

## Security consequences

**Recommendation.** Treat the command parser as an authority boundary:

- built-in command names and handlers are compiled trusted code;
- repository files, `AGENTS.md`, `CLAUDE.md`, provider output, persisted text, and future Skills cannot add, shadow, alias, or change a beta command;
- commands never become prompt text unless the user uses the explicit `//` escape;
- provider/model pickers enumerate only adapters and models admitted by the capability matrix;
- `/permissions` renders effective deterministic facts and cannot widen them;
- a future permission change must compile to deterministic Policy and be attested by the Guard before the UI reports it active;
- a future approval binds to one immutable typed intent and digest; neither `/approve all` nor prompt text can create authority;
- status/help output uses trusted labels and sanitized bounded values;
- commands never print credentials, raw provider errors, hidden prompts, chain-of-thought, or untrusted terminal controls; and
- command extensions remain out of beta because loading repository-defined command templates would create a new instruction, process, and supply-chain boundary.

**Inference.** Other products' familiar permission labels are UX, not proof of containment. Arany can reuse `/permissions` while implementing a stronger internal contract. It must never claim a sandbox merely because a status row says “read only.”

## Verification map

| Claim | Evidence owner |
|---|---|
| `arany` and `exec` select different contracts without TTY guessing | real-process matrix over stdin/stdout/stderr TTY and pipe combinations |
| Slash commands never reach the Provider | scripted Provider captures compiled inputs for every command and collision case |
| Provider/model lock at `RunStarted` | state-transition tests plus interactive buffer assertions |
| `/status`, `/agents`, and `/permissions` reflect committed truth | projection tables over every Run/Agent state and preflight state |
| Command discovery and exact execution agree | one registry contract test enumerates help, aliases, completion, parser, and handlers |
| Unknown commands are not dropped or billed | parser corpus with `/unknown`, near misses, whitespace, arguments, and `//` escape |
| `exec` treats slash-prefixed objectives literally | exact-byte subprocess fixtures and fake-Provider input assertion |
| No effectful beta command exists | registry exhaustiveness test and security review |
| Screen-reader command use is complete | exact linear fixtures with no CSI/OSC/carriage-return rewriting plus VoiceOver/Orca checks |
| Cancel/exit/EOF behavior is safe | Linux/macOS PTY tests across draft, palette, active Run, cancellation, and post-restore receipt |
| Help never lies about availability | table-driven state/feature matrix generated from the same registry metadata |

The parser corpus must include:

- empty input, `/`, `//`, `///`, `/help`, `/help model`, and trailing spaces;
- `/Help`, `/modelx`, `/model/id`, `/tmp/file`, ` /help`, and `explain /status`;
- Unicode confusables, bidi/control characters, combining text, and oversized arguments;
- quoted-looking arguments, backticks, `$()`, semicolons, pipes, and newlines as inert text;
- every alias and every reserved command;
- every command before start, during work, during cancellation, and after terminal state; and
- noninteractive objectives beginning with `/`, `@`, and `!`.

## Future activation order

### When Session becomes real

Add `/resume`, `/new` with `/clear` as an alias, `/compact`, `/rename`, and `/fork` only after these decisions are explicit:

1. a Session owns ordered user turns and Runs;
2. a turn defines exactly which previous material enters Provider context;
3. history has hard byte/token/turn bounds and no silent truncation;
4. compaction is an attributed durable transformation with evaluation and replay rules;
5. resume distinguishes continuing a completed conversation from recovering an interrupted Run;
6. Provider/model changes apply at a documented turn boundary; and
7. deleting or branching a Session has clear retention and descendant behavior.

Bare `arany` is already a true multi-Run Session in beta. Later work may deepen editing, steering, and cross-Session Memory without changing the entry point. `arany exec` remains deterministic one-Run automation, and `arany show` remains replay.

### Before the first effectful Tool

Add `--permission-mode` and editable `/permissions` only after typed effects, deterministic Policy, a separate Guard, effective-capability attestation, and adversarial conformance exist. Prefer these future concepts:

- `read-only`: no effectful Tools are exposed;
- `ask`: unmatched authorized intents require explicit approval;
- `deny-unmatched`: only preauthorized intents run; interactive approval is not offered.

Exact final names and values belong to the Policy/Guard implementation plan. Do not precommit to `auto`, `accept-edits`, `full-auto`, or `yolo`; those labels mix user convenience with enforcement and do not describe Arany's typed capability model.

Add a future `--sandbox <profile>` only when the named profile maps to verified platform enforcement and the status view can report effective capabilities dimension by dimension. Approval mode and sandbox profile remain separate.

`/approve` may then approve exactly one displayed immutable pending intent or a deterministic restricted rule derived from it. `/approval` should not exist as an alias because it obscures whether the user is inspecting policy, changing a mode, or approving one request.

## Architecture reconciliation required

This recommendation intentionally changes only the terminal adapter and public command grammar. It preserves:

- one admitted Run using either a direct answer or bounded `0..N` direct children;
- one accountable primary AgentRun and a generic bounded team policy;
- Events and `RunView` as canonical execution truth;
- one selected provider/model/route per Run;
- `exec` and `show` as deterministic noninteractive contracts;
- no effectful Tool, process, MCP, plugin, or sandbox claim; and
- the existing terminal restoration and accessibility design.

Canonical documents must nevertheless reconcile these direct conflicts before implementation:

1. replace `arany run` with bare `arany` as the attached-human command;
2. replace “no prompt editor” with one bounded preflight objective composer and a local command palette;
3. state that active work accepts local controls only, not natural-language steering or queued turns;
4. add the closed beta slash-command table and parsing rules;
5. keep `/resume`, `/clear`, and `/compact` out of beta while `Session`/`Message` remain absent;
6. add the command-parser security boundary and verification corpus; and
7. preserve stdout/stderr ownership and post-restoration final receipt behavior.

## Final recommendation

Arany can feel immediately familiar without copying another harness blindly. Use the conventions users already know—bare interactive binary, explicit `exec`, `/` discovery, `/help`, `/status`, `/model`, `/permissions`, `/agents`, Session lifecycle commands, conventional cancellation, and strict machine-mode separation—while keeping the beta honest about which controls are enforced.

The important non-copy is as valuable as the copy: do not expose approval, sandbox, resume, compaction, shell, or extension commands until the core concepts and enforcement they name actually exist. Familiar vocabulary should make Arany easier to learn; it must not make the security or persistence model less precise.
