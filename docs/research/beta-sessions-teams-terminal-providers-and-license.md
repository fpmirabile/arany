# Beta sessions, teams, terminal interaction, custom providers, and licensing

**Status:** focused primary-source research and beta recommendation  
**Date:** 2026-09-29  
**Scope:** terminal feedback below the composer, native scrollback, keyboard and mouse behavior, single-agent and team operation, durable multi-turn sessions, custom API-key endpoints, and attribution-preserving licensing  
**Method:** official product documentation, official source repositories, protocol documentation, and authoritative license texts. Product behavior is dated because these interfaces change quickly.

## Executive recommendation

Arany should revise the beta around a durable multi-turn `Session`, not a one-objective process. Each submitted user message starts one bounded `Run`; a Run may use only the primary agent or a flat, dynamically sized team of `0..N` child `AgentRun`s. The topology must be generic over `N`, but `N` remains explicitly bounded by the user-selected collaboration profile and the aggregate Run budget.

The attached terminal should remain an inline, native-scrollback interface. The normal view should look like a coding harness, not a miniature IDE:

1. transcript in normal terminal scrollback;
2. composer at the bottom;
3. one compact status row below the composer; and
4. an activity shelf below that row only while agents need visibility.

The single-agent case is the default visual case. It gets one activity row, not an empty team dashboard. When more agents exist, the same shelf expands to a few stable rows and an `+N more` summary. `/agents` opens the full picker and details. `/team` is not the established cross-product command: Claude Code, Codex, and OpenCode converge more closely on `agents` or `subagents`, so Arany should ship `/agents` and may reconsider `/team` only as a later alias.

Keyboard behavior must be complete. Mouse support should be a transient enhancement inside open pickers and agent/session panels, never a requirement and never globally captured during ordinary transcript use. Global mouse capture conflicts with the chosen native-scrollback and native-selection contract.

The beta should also allow user-owned custom endpoint profiles, but it must not call every endpoint “OpenAI compatible” and hope for the best. A profile names an exact protocol family, endpoint, model, credential source, and capability evidence. Arany admits that exact profile only after a data-free conformance check proves the strict outcome and output-bound contract. Native adapters remain supported products; custom profiles are exact, evidence-qualified integrations.

For the stated license intent—free use and modification while redistributed copies and derivatives preserve author attribution—the strongest standard fit among MIT, BSD-3-Clause, and Apache-2.0 is **Apache License 2.0 with a project `NOTICE` file**. It preserves the standard open-source ecosystem, adds an explicit patent grant, requires changed files to be marked, and gives attribution notices a defined redistribution path. It does not force a person who merely runs a private copy to publish credit, and it does not guarantee an always-visible “Powered by” badge. A custom advertising or use-time attribution clause would be a materially different, legally reviewed choice and is not recommended for beta.

## Decision summary

| Area | Beta recommendation | Why |
|---|---|---|
| Transcript | Normal terminal buffer and native scrollback | Matches the explicit product choice; survives crashes and composes with tmux and terminal search |
| Composer footer | One dense status row below input | Claude Code and Codex demonstrate the familiar location without requiring a dashboard |
| Agent feedback | Activity shelf appears only when work exists; `/agents` opens details | Keeps single-agent use quiet while making team work inspectable |
| Mouse | Keyboard-complete; transient capture only in an open picker/panel | Preserves native scrollback and selection during ordinary use |
| Collaboration | `single`, `auto`, and `team` policies; generic bounded `N`; flat primary-owned team in beta | Avoids forcing a team and avoids the historical exactly-two topology without allowing unbounded work |
| Session model | `Session -> Run -> AgentRun` | Conversation identity, one submitted turn, and per-agent execution have different lifecycles |
| Resume/fork | Resume mutates the same Session; fork creates a new Session at a committed Run boundary | Familiar behavior and deterministic lineage |
| Compaction | Derived, digest-bound context snapshot; canonical transcript remains | Long sessions work without replacing history with an unverifiable summary |
| Provider selection | Default is Session preference; exact provider/model/profile is pinned per Run | Users can switch providers between Runs without changing an in-flight execution |
| Custom endpoints | Named user-owned protocol profiles plus exact conformance evidence | Provides Roo Code-style flexibility without a universal compatibility claim |
| License | Apache-2.0 plus NOTICE | Best standard match for redistributed attribution and modifications |

## 1. What established harnesses actually do

### 1.1 Claude Code: input first, status and agents immediately below it

**Observed fact.** Claude Code describes its status line as a persistent bar at the bottom of the interface. It is rendered in its own row above built-in footer badges and can display model, context usage, cost, Git status, or other session data. Its agent panel is explicitly below the prompt, and each visible subagent row defaults to name, description, and token count. The same interface uses arrows to select a teammate and Enter to open it. Sources: [Claude Code status line](https://code.claude.com/docs/en/statusline), [interactive mode](https://code.claude.com/docs/en/interactive-mode), and [agent teams](https://code.claude.com/docs/en/agent-teams).

**Observed fact.** Claude Code's agent panel does not render every idle teammate forever. Working, failed, and selected teammates keep rows; surplus idle agents collapse behind an `N idle agents` row, and idle rows can hide after the panel becomes idle. This is direct evidence that team visibility benefits from progressive disclosure rather than an always-expanded control room. Source: [Claude Code agent teams](https://code.claude.com/docs/en/agent-teams).

**Observed fact.** Claude Code has both a classic scrolling renderer and a fullscreen renderer. Fullscreen mode owns scrolling inside Claude Code, while the classic path uses the terminal's normal scrolling behavior. The fullscreen transcript can be written back to native scrollback for terminal search. Mouse hover and click are documented for fullscreen command/file suggestion lists and diff interactions. Sources: [terminal configuration](https://code.claude.com/docs/en/terminal-config) and [interactive mode](https://code.claude.com/docs/en/interactive-mode).

**Inference.** The familiar part worth copying is not “fullscreen TUI.” It is the stable spatial contract: transcript, prompt, compact footer, and a conditional activity panel. Arany can preserve that contract while choosing native scrollback.

### 1.2 Codex: a compact footer and an explicit scrollback choice

**Observed fact.** Current Codex source has a configurable bottom status line whose default items are model-with-reasoning and current directory. Its footer can combine that status line with the currently viewed agent label, while instructional states temporarily replace it. Sources: [`core/src/config/mod.rs`](https://github.com/openai/codex/blob/main/codex-rs/core/src/config/mod.rs) and [`tui/src/bottom_pane/footer.rs`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/bottom_pane/footer.rs).

**Observed fact.** Codex now supports both an owned alternate-screen transcript and inline mode. Its configuration documents `tui.alternate_screen = "never"` as inline mode that preserves scrollback, while current automatic behavior normally chooses alternate screen outside a special Terminal-over-SSH case. Source: [`tui/src/lib.rs`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/lib.rs).

**Observed fact.** Codex source exposes a searchable agent picker, fast agent navigation, retained completed agents, `/agents` and `/subagents` command surfaces, and footer labeling of the viewed agent. Source: [`tui/src/app/session_lifecycle.rs`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/app/session_lifecycle.rs), [`tui/src/multi_agents.rs`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/multi_agents.rs), and [`tui/src/slash_command.rs`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/slash_command.rs).

**Inference.** Codex validates two Arany choices at once: a one-line contextual footer is sufficient by default, and native scrollback should be a deliberate supported presentation rather than an accident of terminal detection.

### 1.3 OpenCode: keyboard-rich navigation, child sessions, and explicit mouse interactions

**Observed fact.** OpenCode's TUI uses `/` for commands, `Ctrl+P` for the command palette, `/agents` or a keybinding for selecting agents, `/sessions` for returning to stored sessions, and separate keys for session tabs. Child sessions have explicit parent/child navigation. Its documented diff viewer supports both complete keyboard scrolling and mouse selection/wheel behavior. Sources: [OpenCode TUI](https://opencode.ai/v2/docs/cli/tui/) and [OpenCode keybinds](https://opencode.ai/v2/docs/cli/keybinds/).

**Observed fact.** OpenCode distinguishes primary agents from subagents. A primary agent owns a Session; a subagent runs in a fresh foreground or background child Session, and the parent controls which configured subagents it may launch. Source: [OpenCode agents](https://opencode.ai/v2/docs/agents/).

**Inference.** A single “agent mode” selector and a multi-agent runtime are compatible. The UI does not need to be in a permanent “team screen”; the primary interaction stays a Session, with child execution exposed only when present.

## 2. Recommended terminal contract

### 2.1 Default layout

Arany should own only a bounded bottom viewport. Committed transcript lines move into native scrollback above it. The default bottom region is:

```text
┌────────────────────────────────────────────────────────────────────┐
│ Ask Arany…                                                        │
└────────────────────────────────────────────────────────────────────┘
 auth-refactor · anthropic/sonnet · read-only · auto · context 68%
 ● primary  working  Comparing provider contracts
```

When a team exists, the activity shelf grows without changing the transcript model:

```text
┌────────────────────────────────────────────────────────────────────┐
│ Add cancellation tests after the current run                     │
└────────────────────────────────────────────────────────────────────┘
 auth-refactor · openai/gpt-x · read-only · team 3/5 · context 61%
 ● primary       synthesizing  Waiting on security-reviewer
 ✓ api-reviewer  finished      3 compatibility gaps
 ● security      working       Checking credential boundaries
   +2 more · /agents or ↑↓ to inspect
```

The text is illustrative, not final copy. The semantic priorities are normative.

### 2.2 Default facts and density

The persistent status row should contain, in priority order:

1. Session name or short stable ID;
2. selected provider/model for the next Run, or the pinned provider/model during a Run;
3. effective permission profile;
4. collaboration policy (`single`, `auto`, or `team N`); and
5. remaining context percentage when known.

The conditional activity shelf should contain:

1. agent stable name or role;
2. typed state;
3. one bounded safe activity summary; and
4. an attention marker for failure, approval, or waiting.

Do not put the following in the always-visible default: full token accounting, full call budgets, cost tables, filesystem paths, Git status, event sequence, timeline, provider request IDs, telemetry state, or every completed agent. They belong in `/status`, `/agents`, the durable transcript, or `show`.

At narrow widths, preserve state and attention before model, context, or Session labels. Truncate facts as semantic fields; do not horizontally scroll or wrap the status line into a dashboard.

### 2.3 Single-agent and team behavior

The activity shelf is data-driven:

- No active Run: no agent row.
- One active primary: one row.
- Multiple active or attention-requiring agents: up to three rows by default.
- More agents: one `+N more` row.
- Failed and blocked agents stay visible until acknowledged or the Run ends.
- Completed agents produce a committed transcript line and then may leave the shelf.

This keeps a one-agent harness quiet and lets a team scale without changing to a different application.

### 2.4 Keyboard and mouse

**Recommendation.** Keyboard operation is complete and normative:

- `/` opens/filter commands;
- `/agents` opens agent details;
- `/sessions` opens Session history;
- Up/Down selects an activity row only when the shelf has focus;
- Enter opens the selected agent's bounded transcript/details;
- Escape returns focus or closes a picker before it has cancellation meaning; and
- PageUp/PageDown, terminal search, selection, and copy remain native transcript operations.

**Recommendation.** Mouse support is transient:

- normal transcript/composer mode does not enable terminal mouse reporting;
- opening a command palette, Session picker, or agent picker may enable mouse reporting for that bounded surface when the terminal supports it;
- hover changes selection, click accepts, wheel scrolls only the open picker;
- closing, suspending, crashing, or losing terminal ownership disables mouse reporting before returning control; and
- every mouse action has the same visible keyboard action.

Terminal mouse reporting is process-global, not region-local. Capturing it continuously would intercept wheel and selection gestures that the user expects native scrollback to own. Transient capture is therefore the smallest design that honors both requested behaviors. If reliable transient capture fails on a supported terminal, the picker remains keyboard-only; Arany must not switch the whole transcript to alternate screen.

### 2.5 Why `/agents`, not `/team`

**Observed fact.** Claude Code uses an agent panel and `/agents`; Codex exposes `/agents` and `/subagents`; OpenCode exposes `/agents`. Claude Code creates a team from natural-language instruction rather than a `/team` command. Sources: [Claude Code commands](https://code.claude.com/docs/en/commands), [Codex slash command source](https://github.com/openai/codex/blob/main/codex-rs/tui/src/slash_command.rs), and [OpenCode TUI](https://opencode.ai/v2/docs/cli/tui/).

**Recommendation.** `/agents` is the beta command. It should:

- show the primary and all active/recent child AgentRuns;
- select an agent for bounded details;
- show the current collaboration policy and resource bounds; and
- before a Run, let the user choose `single`, `auto`, or `team` and a maximum active-child value.

`/team` is understandable English, but it is not the shared convention. Do not add a second public spelling until users demonstrate a discovery problem.

## 3. Team semantics without a forced team

### 3.1 Evidence from existing harnesses

**Observed fact.** Claude Code says teams are useful when parallel work is genuinely independent and warns that they add coordination overhead and substantially more token use. Its documentation recommends starting with three to five teammates, says there is no hard product limit, and notes diminishing returns. It currently allows one team per Session, no nested teams, and cannot restore in-process teammates on Session resume. Source: [Claude Code agent teams](https://code.claude.com/docs/en/agent-teams).

**Observed fact.** OpenAI's hosted multi-agent protocol imposes no fixed total-agent or tree-depth limit, but defaults `max_concurrent_subagents` to three and recommends that default. It keeps root and child contexts separate and applies compaction separately. Source: [OpenAI Responses multi-agent guide](https://developers.openai.com/api/docs/guides/responses-multi-agent).

**Observed fact.** OpenCode makes the primary/subagent distinction explicit and gives subagents fresh child Sessions with their own configured permissions. Source: [OpenCode agents](https://opencode.ai/v2/docs/agents/).

### 3.2 Beta collaboration policy

Arany should pin a `CollaborationPolicy` before each Run:

```text
single
  child spawn authority = 0

auto(max_active_children = N)
  primary may delegate when parallelism is useful

team(max_active_children = N)
  user explicitly requests team-oriented decomposition
```

The Session stores the default policy. A Run stores the resolved immutable policy. Changing the Session default affects only the next Run.

`N` is a numeric input, not a compiled topology. The scheduler admits a child only when all of these allow it:

```text
effective child capacity = minimum of
  Session policy maximum
  process safety maximum
  remaining aggregate Provider-call budget
  remaining aggregate token/byte/time budget
  configured Provider concurrency limit
```

The beta topology should be flat: one primary owns `0..N` child AgentRuns. Child agents may message or hand results to the primary, but cannot create a nested team. This removes the artificial exactly-two constraint while preserving a tractable authority and cancellation tree. The domain types should not encode a two-child array; use an ordered bounded collection.

The process safety maximum is still required. “Supports N” means topology and scheduling are generic over an explicitly bounded N; it does not mean unbounded spawning. The product default should be three active children because this aligns with OpenAI's current recommended concurrency and sits inside Claude Code's documented three-to-five practical range. Users may configure a larger value before a Run, subject to the global hard resource ceiling and budget admission.

### 3.3 Team lifecycle

- A Session always has one primary agent identity.
- A Run begins with only the primary AgentRun.
- Team state appears only after the first admitted child.
- Agent names and causal assignments are stable for that Run.
- A Session may remember collaboration preferences and reusable role definitions, but it does not pretend live model processes survived exit.
- On resume, historical agents remain inspectable; no child is silently restarted.
- A new Run can create a new team under the Session's current policy.
- Cancellation flows primary-to-children, but every child receives and persists its own terminal disposition.

This deliberately avoids Claude Code's documented “resumed lead points at teammates that no longer exist” failure mode.

## 4. Durable Session versus Run

### 4.1 Evidence and terminology

**Observed fact.** Claude Code treats an interactive conversation as a persistent Session, supports `--continue`, `--resume`, and `--fork-session`, and exposes `/clear`, `/compact`, `/resume`, `/branch`/`/fork`, and `/rename`. Forking on resume creates a new Session ID. Sources: [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference) and [commands](https://code.claude.com/docs/en/commands).

**Observed fact.** OpenCode offers stored Sessions, tabs, new/resume controls, compaction, forking, undo/redo, and child Sessions. Sources: [OpenCode TUI](https://opencode.ai/v2/docs/cli/tui/) and [keybinds](https://opencode.ai/v2/docs/cli/keybinds/).

**Observed fact.** Codex exposes fresh, resume, and fork lifecycles. Its app-server API models a durable thread containing turns; resume rejoins the existing thread while fork creates a new thread and records lineage. Sources: [Codex Python SDK API](https://github.com/openai/codex/blob/main/sdk/python/docs/api-reference.md), [`app/session_lifecycle.rs`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/app/session_lifecycle.rs), and [`protocol/v2/thread.rs`](https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/protocol/v2/thread.rs).

**Inference.** Arany's existing `Run` is not a Session. It is one execution caused by one submitted user message. Reusing the term for both would make resume, fork, budgets, Provider switching, and failure recovery ambiguous.

### 4.2 Recommended domain model

```text
Session
├── id, title, workspace identity, lifecycle
├── parent Session + fork boundary, when forked
├── default Provider/model/profile
├── default CollaborationPolicy
├── context snapshots[]
└── Runs[] in committed order
    └── Run
        ├── id, user input, terminal result
        ├── pinned Provider/model/profile
        ├── pinned permission and collaboration policy
        ├── aggregate budgets and usage
        └── AgentRuns[]
            └── primary or child execution state and result
```

Definitions:

- **Session:** durable conversation and branching identity across process exits.
- **Run:** one accepted user submission from `RunStarted` to one terminal outcome.
- **AgentRun:** one primary or child agent's execution within a Run.
- **Context snapshot:** derived compaction material for a committed Session prefix; never canonical history.

One Session has at most one active Run in beta. A completed, failed, cancelled, or interrupted Run remains part of history. The next user message creates another Run; it does not mutate the prior Run.

### 4.3 Command semantics

| Surface | Meaning |
|---|---|
| `arany` | Start a new persistent interactive Session |
| `arany "prompt"` | Start a new Session, execute the first Run, and remain interactive |
| `arany --continue`, `-c` | Resume the most recently used Session for the admitted Workspace |
| `arany --resume [SESSION]` | Resume an exact Session or open the Session picker |
| `arany --fork SESSION` | Create a new Session from the source's latest committed Run |
| `/sessions` | Open Session picker |
| `/resume [session]` | Replace the current idle view with an existing Session after confirmation if there is a draft |
| `/new [name]` | Start an empty Session; `/clear` may be its familiar alias |
| `/fork [name]` | Fork the current Session at the latest committed Run boundary |
| `/rename [name]` | Rename Session metadata only |
| `/compact [focus]` | Create a derived context snapshot after the current Run is idle |
| `arany exec ...` | Deterministic one-Run automation by default; Session continuation requires an explicit Session ID |

Do not implicitly resume because the current directory matches. Explicit `--continue` is familiar and prevents stale conversation state from silently entering a new task.

### 4.4 Resume

Resume reconstructs the same Session identity and its committed history. It does not:

- rerun an interrupted Provider call;
- recreate historical child processes;
- infer that an incomplete Run succeeded;
- adopt the current provider or permission setting into old Runs; or
- trust a cached Provider response identifier as canonical history.

An interrupted Run remains `Interrupted`. The resumed Session offers explicit retry as a new Run, preserving the failed attempt and its usage evidence.

### 4.5 Fork

Fork creates a new Session ID and immutable lineage:

```text
source_session_id
source_boundary_run_id
source_boundary_sequence
source_prefix_digest
```

The new Session begins with the source's committed conversation prefix and inherited defaults, which the user may change before its first new Run. Later source changes never appear in the fork. Forking during an active Run is rejected or delayed until a committed boundary; it never copies a half-written execution.

### 4.6 Compaction

**Observed fact.** Claude Code and OpenCode expose explicit `/compact`, and Claude Code also supports automatic compaction. OpenAI's compaction API returns a compacted context intended to become the next request context, while the Codex Session remains separately durable. Sources: [Claude Code commands](https://code.claude.com/docs/en/commands), [OpenCode TUI](https://opencode.ai/v2/docs/cli/tui/), and [OpenAI compaction guide](https://developers.openai.com/api/docs/guides/compaction).

**Recommendation.** Arany should never replace or delete canonical Session history when compacting. Persist a derived snapshot with:

- Session ID and covered-through Run/sequence;
- ordered source-event digest;
- provider, model, prompt/schema version, and timestamp;
- bounded summary or opaque provider compaction item;
- size/token estimates and content digest; and
- success/failure provenance.

The context compiler selects the newest valid snapshot whose covered prefix is an ancestor of the requested boundary, then appends uncompacted committed Runs. A fork before that boundary cannot reuse the later snapshot. A provider-specific opaque compaction item is only reusable with the exact compatible provider/profile; a provider-neutral textual snapshot is portable but must be labeled model-authored derived context.

Manual `/compact` runs only while idle and exposes its Provider call and usage. Automatic compaction may run only at a committed Run boundary after a visible threshold warning. If compaction fails, the Session remains intact and the next Run either fits without it or fails before Provider invocation with a clear context-capacity error.

### 4.7 Provider switching inside a Session

The Session owns a default provider/model/profile for the next Run. Each Run persists its resolved provider selection. `/provider` and `/model` may change the Session default only while no Run is active.

Canonical conversation state is Arany's provider-neutral committed messages, results, and snapshots. Provider response IDs, prompt-cache identifiers, and encrypted compaction items are optional exact-adapter accelerators. They cannot be the only path to resume, because switching from OpenAI to Anthropic, Z.AI, or a custom endpoint must recompile the next request from Arany state.

## 5. Custom API-key endpoints without fake compatibility

### 5.1 What Roo Code and OpenCode provide

**Observed fact.** Roo Code lets a user select an OpenAI-compatible provider and configure Base URL, API key, model ID, maximum output tokens, context window, and feature claims. Its own documentation warns that not all models support native tool calling and that some nominally compatible providers only partially implement the tool API. Source: [Roo Code OpenAI-compatible provider documentation](https://github.com/RooCodeInc/Roo-Code/blob/main/apps/docs/docs/providers/openai-compatible.md).

**Observed fact.** OpenCode V2 custom providers explicitly combine a runtime protocol package, credential environment variables, base URL, and an explicit model catalog. OpenCode also notes that discovery cannot infer tool support for vLLM, so discovered models start with tools disabled. Source: [OpenCode providers](https://opencode.ai/v2/docs/providers/).

**Inference.** “The endpoint accepts an OpenAI-shaped request” does not establish strict tool arguments, JSON Schema enforcement, output limits, finish reasons, usage semantics, cancellation, privacy, or error behavior. Model discovery proves inventory, not harness compatibility.

### 5.2 Arany profile format

Custom profiles must live in trusted user configuration outside the Workspace and be selected before Workspace content is read. Conceptually:

```toml
[providers.acme]
protocol = "openai-responses"
base_url = "https://llm.acme.example/v1"
api_key_env = "ACME_API_KEY"
model = "qwen3-coder"
outcome_encoding = "strict-json-schema"
privacy = "user-asserted"
```

Initial protocol families should be closed enums, not plugin strings:

- `openai-responses`;
- `openai-chat-completions`; and
- `anthropic-messages` only if the existing native adapter can safely parameterize origin without confusing credentials or wire semantics.

The first beta may reasonably ship only one custom family, preferably `openai-responses`, while native OpenAI and Anthropic remain separate. A protocol family reuses wire mechanics; it does not make the custom service an OpenAI or Anthropic product.

Do not accept raw API keys in CLI arguments or repository configuration. The beta profile names an environment variable or OS secret handle. Do not support arbitrary headers, shell credential commands, ambient proxy settings, or Workspace-defined profiles in the first release.

### 5.3 Endpoint admission

Before a custom profile can receive Workspace data, `arany provider check PROFILE` should run a synthetic, bounded conformance sequence with no repository content. Evidence is keyed by:

```text
Arany version
protocol adapter version
normalized endpoint origin
model ID
outcome encoding
requested output cap
capability-test version
evidence timestamp and expiry
```

The check must prove at least:

- exact authentication header behavior without credential reflection;
- no redirect and no ambient proxy path;
- strict `Delegate | Finish` outcome enforcement for the exact model;
- provider-enforced maximum output token support;
- refusal, truncation, malformed outcome, and finish-reason mapping;
- response-size and time bounds;
- usage fields labeled by provenance rather than assumed exact;
- cancellation behavior; and
- preservation of the endpoint/model identity in receipts.

If the endpoint cannot prove the strict outcome contract, Arany reports that exact missing capability and does not run a degraded team protocol. It may be configurable, but it is not admitted for product execution. There is no prompt-only repair path and no automatic fallback to another protocol or model.

### 5.4 Network and credential boundary

- Require HTTPS for non-loopback endpoints; allow HTTP only for explicit numeric loopback development profiles.
- Normalize and pin scheme, host, port, and base path.
- Resolve and reject private, link-local, metadata-service, multicast, and changed destinations unless the profile is explicitly a loopback profile.
- Disable redirects, cookies, ambient proxies, and automatic credential forwarding.
- Bind one credential source to one profile origin and never try it against discovered hosts.
- Require explicit model IDs; do not auto-discover by default.
- Treat privacy and retention as user assertions unless the exact service provides verifiable controls.
- Record “custom endpoint, user configured” in every receipt; never label it as native OpenAI or native Anthropic.

### 5.5 Support language

Use these product claims:

- **Native supported:** Arany owns and live-tests the official adapter/profile.
- **Broker constrained:** exact OpenRouter route and privacy gates passed.
- **Custom verified:** exact endpoint/model/profile passed the named Arany conformance version.
- **Custom unverified:** selectable for `provider check`, not admitted to a Run.

Avoid “supports every OpenAI-compatible provider.” Compatibility is a result for one exact profile, not a trait inferred from a Base URL.

## 6. License recommendation

This section is engineering research, not legal advice. The repository uses the public identity `fpmirabile` for copyright and attribution.

### 6.1 MIT

**Observed fact.** MIT grants broad use, modification, distribution, sublicensing, and sale rights, provided the copyright and permission notices appear in all copies or substantial portions. Source: [OSI MIT License](https://opensource.org/license/mit).

**Assessment.** MIT does preserve the author's legal notice in redistributed substantial copies. It is simple and familiar. It does not require modified files to be marked, provide a structured `NOTICE` propagation mechanism, or include an express patent grant. It meets a basic “keep my copyright line” intent, but it is not the strongest standard expression of the requested provenance.

### 6.2 BSD-3-Clause

**Observed fact.** BSD-3-Clause requires source redistributions to retain the copyright, conditions, and disclaimer, and binary redistributions to reproduce them in documentation or other distribution materials. Its third clause prevents using the copyright holder's or contributors' names to endorse derived products without permission. Source: [OSI BSD-3-Clause License](https://opensource.org/license/bsd-3-clause).

**Assessment.** BSD-3-Clause provides clear source and binary notice retention plus a useful non-endorsement rule. Like MIT, it lacks Apache-2.0's changed-file rule, NOTICE mechanism, and explicit patent license.

### 6.3 Apache-2.0 plus NOTICE

**Observed fact.** Apache-2.0 grants copyright and patent rights. Redistribution requires a copy of the license, prominent notices in modified files, retention of applicable attribution notices, and—when the original includes a `NOTICE` file—a readable copy of those NOTICE attributions in the derivative's NOTICE, source/documentation, or a normal third-party notices display. The NOTICE content is informational and cannot modify the license. Source: [Apache License 2.0, sections 2–4](https://www.apache.org/licenses/LICENSE-2.0).

**Assessment.** This is the closest standard match to “free, but derivatives and redistributions preserve author credit.” It also provides clearer provenance when modified files circulate and an explicit patent grant useful for an agent harness.

Repository files:

```text
LICENSE   exact, unmodified Apache License 2.0 text
NOTICE    project attribution notice
```

The selected NOTICE attribution is:

```text
Arany
Copyright 2026 fpmirabile

Created and maintained by fpmirabile:
https://github.com/fpmirabile/arany
```

The README may repeat this credit, but README text is not a substitute for LICENSE and NOTICE.

### 6.4 Why not add a custom attribution clause

**Observed fact.** The Apache Software Foundation says that modifying Apache-2.0 creates a different license, that creating a license is non-trivial, and that legal advice is appropriate. OSI advises using an approved license and warns that small unapproved variants cannot be presumed Open Source. Sources: [ASF licensing FAQ](https://www.apache.org/foundation/license-faq.html) and [OSI FAQ](https://opensource.org/faq).

A clause such as “every user must display credit in the UI, terminal, website, or service” raises unresolved questions:

- Does private use trigger it?
- Does an internal company deployment trigger it?
- Does a hosted service count if no binary is distributed?
- Where and how prominent must credit be?
- Does removing the CLI while reusing a library trigger it?
- Is it compatible with downstream standard licenses and package policies?
- Can the result still accurately be called Open Source without OSI review?

That friction is contrary to “free” in the ordinary developer expectation. Apache-2.0 plus NOTICE protects redistribution attribution without inventing these obligations. If always-visible product credit is a later business requirement, handle product identity through a reviewed trademark policy or a lawyer-drafted license decision, not an extra sentence appended to Apache-2.0.

### 6.5 Exact decision

Adopt:

```text
SPDX-License-Identifier: Apache-2.0
```

with a project NOTICE. Do not dual-license with MIT: recipients could choose MIT and avoid the Apache-specific NOTICE and changed-file obligations, defeating the reason for choosing Apache-2.0. Do not call a modified license “Apache.”

## 7. Integrated beta command surface

The resulting interactive set remains familiar:

```text
/help
/status
/sessions
/new
/clear        alias of /new
/resume
/fork
/rename
/compact
/agents
/provider
/model
/permissions
/quit
/exit
```

`/agents` covers both single-agent and team operation. `/provider` and `/model` update only the next-Run defaults while idle. `/permissions` reports effective capabilities; it does not invent approval controls before effectful Tools exist.

Suggested startup surface:

```text
arany [PROMPT]
arany --continue
arany --resume [SESSION]
arany --fork SESSION
arany --collaboration single|auto|team
arany --max-agents N
arany --provider PROFILE
arany --model MODEL

arany exec --output text|jsonl PROMPT
arany show --output text|jsonl SESSION_OR_RUN_ID
arany provider check PROFILE
```

Naming may be refined, but these are distinct semantics. `--max-agents` should state whether the primary counts; the least ambiguous contract is “maximum concurrently active child agents,” with the primary excluded.

## 8. Persistence consequences

The current Run-only Event journal needs a Session identity before the beta can honestly support resume and fork. The smallest coherent extension is still event-oriented:

```text
Event
  sequence
  session_id
  run_id?          null for Session-only events
  agent_run_id?    null outside agent execution
  kind
  version
  bounded payload
  created_at
```

Minimum additional event semantics:

- `SessionStarted`
- `SessionRenamed`
- `SessionDefaultChanged`
- `SessionForked`
- `ContextCompacted`
- existing Run and AgentRun lifecycle events, now scoped to Session

Session lists and titles may use a rebuildable index, but the journal remains canonical. Compaction payloads are derived and digest-bound. Fork lineage is immutable. Provider/profile and collaboration policy are stored in `RunStarted`, not inferred from the Session's current defaults during replay.

The database should not store live terminal focus, panel expansion, mouse hover, selected picker row, or scroll position. Those are presentation state.

## 9. Verification map

### 9.1 Terminal

- PTY proof that Arany never enters alternate screen in the native-scrollback contract.
- Transcript remains selectable/searchable after hundreds of committed lines.
- Single-agent snapshots contain no empty team chrome.
- Team shelf cases cover 1, 3, and N agents; overflow is deterministic.
- Keyboard reaches every picker action.
- Transient mouse mode activates only inside the intended surface and is disabled on close, error, panic, cancellation, suspend, and process exit.
- Mouse-disabled behavior is identical semantically.
- Screen-reader mode exposes the same facts without cursor movement or mouse dependence.

### 9.2 Teams

- Property/scenario tests vary `N`, call budget, token budget, and Provider concurrency.
- Single mode proves no child admission even if model output requests one.
- Auto/team modes never exceed effective capacity.
- Cancellation reaches every admitted child.
- Completed/failed children remain inspectable after leaving the activity shelf.
- Resume never fabricates a live historical child.

### 9.3 Sessions

- Multiple successful Runs survive process exit and replay in order.
- Resume creates no Run until new input is accepted.
- Fork reproduces exactly the committed prefix and never observes later source events.
- Interrupted Run remains interrupted; retry gets a new Run ID.
- Compaction failure leaves canonical history unchanged.
- Snapshot digest mismatch or incompatible provider item is rejected.
- Provider switches between Runs preserve transcript semantics and keep old attribution.

### 9.4 Custom providers

- Credentials are read only for the selected profile.
- Redirect, proxy, DNS rebinding, credential reflection, oversized response, timeout, malformed schema, ignored output cap, and route/model drift cases fail before Workspace disclosure where possible.
- Conformance evidence is invalidated by endpoint, model, adapter, Arany version, or capability-test changes.
- Receipts distinguish native, broker, custom verified, and custom unverified.
- A catalog or `/models` response alone never unlocks team execution.

### 9.5 Packaging and license

- Release archives and binaries include exact `LICENSE` and `NOTICE` files.
- Package metadata reports `Apache-2.0`.
- Dependency license checks do not confuse Arany's NOTICE with third-party notices.
- Changed-file and attribution expectations are described in contribution/release documentation without adding license terms.

## 10. Sequencing recommendation

1. Introduce Session identity and multi-Run replay before claiming interactive multi-turn behavior.
2. Replace the exactly-two child shape with a bounded ordered collection and collaboration policy; prove single-agent first, then `N` children.
3. Refactor the terminal to the composer + status row + conditional activity shelf, retaining native scrollback.
4. Add `/sessions`, `/new`, `/resume`, `/fork`, `/rename`, `/compact`, and `/agents` against real durable semantics.
5. Add transient mouse support only after keyboard and terminal-restoration tests pass.
6. Add the custom endpoint profile and data-free conformance gate after native adapters establish the reference contract.
7. Add runtime-opt-in OTLP at the end of beta as already planned; Session, Run, and AgentRun IDs make its topology more useful.
8. Add exact Apache-2.0 `LICENSE` and `NOTICE` once the copyright holder and preferred author credit are confirmed.

## 11. Final conclusions

- The user's preferred bottom section is consistent with the strongest existing terminal harness pattern.
- Native scrollback and global mouse capture conflict; keyboard-complete plus transient panel mouse is the honest combination.
- `/agents` is more familiar than `/team`; team presentation should emerge from runtime state rather than replace the whole UI.
- A real harness beta needs durable Sessions. `Session`, `Run`, and `AgentRun` must remain separate concepts.
- The team topology should be generic over bounded `N`, with a quiet single-agent path and a flat primary-owned beta team.
- Provider and model switching belongs between Runs. Arany's canonical context must stay provider-neutral.
- Custom API-key endpoints are feasible if admission is attached to an exact protocol/endpoint/model conformance result, not an “OpenAI compatible” label.
- Apache-2.0 plus NOTICE is the recommended standard license for free use and redistribution with preserved author attribution. It does not impose public credit on private users, and Arany should not invent a custom attribution clause for beta.

## Primary sources

### Harness interfaces and sessions

- [Claude Code interactive mode](https://code.claude.com/docs/en/interactive-mode)
- [Claude Code status line](https://code.claude.com/docs/en/statusline)
- [Claude Code terminal configuration](https://code.claude.com/docs/en/terminal-config)
- [Claude Code agent teams](https://code.claude.com/docs/en/agent-teams)
- [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference)
- [Claude Code commands](https://code.claude.com/docs/en/commands)
- [Codex configuration source](https://github.com/openai/codex/blob/main/codex-rs/core/src/config/mod.rs)
- [Codex footer source](https://github.com/openai/codex/blob/main/codex-rs/tui/src/bottom_pane/footer.rs)
- [Codex slash commands source](https://github.com/openai/codex/blob/main/codex-rs/tui/src/slash_command.rs)
- [Codex Session lifecycle source](https://github.com/openai/codex/blob/main/codex-rs/tui/src/app/session_lifecycle.rs)
- [Codex CLI resume/fork source](https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs)
- [Codex Python SDK API](https://github.com/openai/codex/blob/main/sdk/python/docs/api-reference.md)
- [OpenAI Responses multi-agent guide](https://developers.openai.com/api/docs/guides/responses-multi-agent)
- [OpenAI compaction guide](https://developers.openai.com/api/docs/guides/compaction)
- [OpenCode TUI](https://opencode.ai/v2/docs/cli/tui/)
- [OpenCode keybindings](https://opencode.ai/v2/docs/cli/keybinds/)
- [OpenCode agents](https://opencode.ai/v2/docs/agents/)

### Provider profiles

- [OpenCode providers](https://opencode.ai/v2/docs/providers/)
- [Roo Code OpenAI-compatible provider documentation](https://github.com/RooCodeInc/Roo-Code/blob/main/apps/docs/docs/providers/openai-compatible.md)

### Licensing

- [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0)
- [Apache licensing and distribution FAQ](https://www.apache.org/foundation/license-faq.html)
- [OSI MIT License](https://opensource.org/license/mit)
- [OSI BSD-3-Clause License](https://opensource.org/license/bsd-3-clause)
- [OSI FAQ](https://opensource.org/faq)
