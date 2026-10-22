# Agent chat switching research

- Question: which peer interaction patterns let a user inspect a child's conversation without changing Arany's execution or authority guarantees?
- Source-check date: 2026-10-21. Arany source was checked at `e8cc700ab3bcc611096ce473336288fa9689a747`; Codex source is pinned below. Claude Code documentation is dated evidence, without a documentation revision pin.
- Conclusion: reuse Codex's empty-composer Alt+Left/Alt+Right navigation and stable spawn order, and Claude Code's selectable agent panel and idle-row grouping. Arany can expose committed child history read-only first, then live updates; selecting a transcript must not imply permission to message its actor.
- Scope: source investigation and proposed direction for one feature, switching between agent chats. No implementation, terminal execution, native-platform support or performance claim. Accepted behavior now belongs to the owning contracts; the [terminal](../specs/terminal.md), [execution](../specs/execution.md) and [Goal](../specs/goals.md#steering-notes-and-pause) contracts remain authoritative.

## Observed Arany source

| Surface or boundary | Verified finding and source |
|---|---|
| Activity shelf | The shelf limits visible agents to three, labels children by ordinal and directs overflow to `/agents`; it supplies progress rows rather than a switch interaction. [presentation/model.rs:8](../../crates/arany-cli/src/presentation/model.rs#L8), [model.rs:100](../../crates/arany-cli/src/presentation/model.rs#L100), [model.rs:150](../../crates/arany-cli/src/presentation/model.rs#L150). |
| Agent inspector | The projection selects agents from the 16 newest Runs, at most nine agents per Run. Choices carry Run number, role and status; details include objective, worker certainty, actor-scoped Supervisor verdicts, usage, summary and result. [presentation/agents.rs:4](../../crates/arany-cli/src/presentation/agents.rs#L4), [agents.rs:30](../../crates/arany-cli/src/presentation/agents.rs#L30), [agents.rs:129](../../crates/arany-cli/src/presentation/agents.rs#L129). |
| Inspector input | Arrows select, PageUp/PageDown scroll and Home/End jump. Enter currently closes the inspector. Linear input accepts numbered selection, `n`/`p` and `q`; its choice buffer is capped at 16 bytes. [terminal/agents.rs:3](../../crates/arany-cli/src/terminal/agents.rs#L3), [agents.rs:46](../../crates/arany-cli/src/terminal/agents.rs#L46), [agents.rs:84](../../crates/arany-cli/src/terminal/agents.rs#L84). |
| Durable actor identity | `AgentSpawned` records Run, AgentRun, role, ordinal, objective, grant and assignment. Display, Tool and finish Events name their AgentRun. [session/events.rs:292](../../crates/arany/src/session/events.rs#L292), [events.rs:348](../../crates/arany/src/session/events.rs#L348), [events.rs:383](../../crates/arany/src/session/events.rs#L383). |
| Available transcript facts | `AgentView` holds identity, ordinal, objective, summary, result, Provider-call metadata, Tools and status. `ProviderCallRecord` holds usage, disposition, delegation and failure metadata, without a raw Provider prompt or reasoning field. These support a journal-derived actor transcript, not a promise of every private message sent to a model. [session/reducer.rs:206](../../crates/arany/src/session/reducer.rs#L206), [events/records.rs:558](../../crates/arany/src/session/events/records.rs#L558). |
| Child Tool ownership | Child admission routes assignments through worker validation and appends the Tool to the selected child's `tools`; child detail projection must preserve that actor identity. [reducer/agent.rs:63](../../crates/arany/src/session/reducer/agent.rs#L63), [agent.rs:118](../../crates/arany/src/session/reducer/agent.rs#L118). |
| Spawn ordering | Replay requires primary ordinal zero, each child ordinal equal to the current agent count and within the pinned policy, then appends the new AgentView. This provides stable order independent of completion order. [reducer/agent.rs:487](../../crates/arany/src/session/reducer/agent.rs#L487), [agent.rs:520](../../crates/arany/src/session/reducer/agent.rs#L520). |
| Existing transcript | The inspected scrollback transcript renders Run objective, Run details and committed assistant answer. It is not an existing actor-selected transcript implementation. [terminal/history.rs:1053](../../crates/arany-cli/src/terminal/history.rs#L1053). |
| Existing keys | Ctrl+Left/Right map to word motion; Alt-modified input is currently discarded. Plain Left/Right move the composer cursor and select slash-menu tabs. Ctrl+O/Ctrl+T open the transcript. Alt+Left/Right therefore needs explicit decoding and admission; plain arrows already have owners. [terminal/input.rs:725](../../crates/arany-cli/src/terminal/input.rs#L725), [input.rs:742](../../crates/arany-cli/src/terminal/input.rs#L742), [terminal/composer.rs:473](../../crates/arany-cli/src/terminal/composer.rs#L473), [composer.rs:615](../../crates/arany-cli/src/terminal/composer.rs#L615). |
| Input barrier | `begin_dialog` disarms selectors. `arm_dialog` discards typeahead through the reader barrier and reports that the caller must redraw before reading input. New transcript and picker surfaces must use the same armed-frame discipline. [terminal.rs:1562](../../crates/arany-cli/src/terminal.rs#L1562), [terminal.rs:1571](../../crates/arany-cli/src/terminal.rs#L1571). |
| Live event path | The attached loop drains at most 64 tapped items per burst. Its committed branch reduces Events, ignores already-applied sequences and catches up on gaps or reduction failure. This is reusable delivery machinery, not evidence that child transcript redraw already exists. [attached/active.rs:585](../../crates/arany-cli/src/cli/attached/active.rs#L585), [active.rs:1293](../../crates/arany-cli/src/cli/attached/active.rs#L1293). |

The [terminal contract](../specs/terminal.md#conversation-progress-and-history)
owns Classic/Fullscreen continuity, linear and screen-reader output, inert
sanitized text, bounded history and event-driven redraw. Its
[inspection section](../specs/terminal.md#selectors) owns `/agents` today. The
[execution contract](../specs/execution.md#researcher-children) bounds a
researcher delegation at eight children, 1,064 child Events, a 2 KiB objective,
16 KiB report and 2 KiB summary. Those are execution bounds, not proposed new
transcript buffers. The pinned child-attempt ceiling applies across a Run,
including replacements and later waves, as [execution.md:17](../specs/execution.md#collaboration)
specifies; the researcher Event bound must not be generalized to every editor
worker lifecycle.

## Codex peer: navigation and attachment

Source revision: `openai/codex@c419fd01a70dd1d05356da443ac44328bda2e6f4`.

- Canonical previous/next bindings are Alt+Left and Alt+Right. Both handlers require no overlay, no modal or popup, and an empty composer including pending text. This preserves editing ownership for a nonempty draft. [multi_agents.rs:106](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/multi_agents.rs#L106), [app/input.rs:459](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/input.rs#L459).
- macOS has platform-gated Alt+b/Alt+f matching because Option+Left/Right can arrive as word-motion sequences. The caller permits the fallback only with enhanced-key reporting unavailable and the composer empty. The source verifies decoded key handling; raw `ESC b`/`ESC f` delivery on a particular terminal remains unverified here. [multi_agents.rs:133](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/multi_agents.rs#L133), [multi_agents.rs:162](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/multi_agents.rs#L162), [app/input.rs:455](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/input.rs#L455).
- Navigation inserts each thread once into first-seen order; previous/next traversal wraps using modular indexing while skipping excluded entries. Closed entries retain their place for inspection. [app/agent_navigation.rs:154](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/agent_navigation.rs#L154), [agent_navigation.rs:245](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/agent_navigation.rs#L245), [agent_navigation.rs:370](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/agent_navigation.rs#L370).
- The `Subagents` picker includes status, a current-selection marker and searchable name/UUID. [app/session_lifecycle.rs:198](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/session_lifecycle.rs#L198).
- Selection activates a saved snapshot and event receiver, then renders the snapshot; closed or unavailable live attachments can become replay-only with explicit saved-transcript feedback. Live attachment and permission to send instructions are separate: parent-owned children are marked view-only, and rendering applies that state. A claim that every live child view is fully interactive is unsupported by this revision. [app/session_lifecycle.rs:591](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/session_lifecycle.rs#L591), [session_lifecycle.rs:629](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/session_lifecycle.rs#L629), [session_lifecycle.rs:700](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/session_lifecycle.rs#L700), [agent_navigation.rs:139](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/agent_navigation.rs#L139), [session_lifecycle.rs:750](https://github.com/openai/codex/blob/c419fd01a70dd1d05356da443ac44328bda2e6f4/codex-rs/tui/src/app/session_lifecycle.rs#L750).

No universal team-size cap or complete memory bound was established from these
Codex files. Arany must retain its own explicit limits.

## Claude Code peer: selection and large teams

Official [agent-team documentation](https://code.claude.com/docs/en/agent-teams#start-your-first-agent-team)
describes an agent panel below the prompt: arrows select, Enter opens a
teammate transcript and enables messaging, and Escape clears selection; Escape
inside a teammate view interrupts its turn. Beyond three idle teammates, extra
idle rows collapse into a count that Enter expands. Working, failed and viewed
teammates remain separate. Idle rows disappear after 30 seconds once all agents
are idle. Split panes are an alternative. These are documented peer behaviors,
not source-code or native-terminal verification.

Official [subagent documentation](https://code.claude.com/docs/en/sub-agents#run-subagents-in-foreground-or-background)
places background agents in the same inspection flow. Its
[fork controls](https://code.claude.com/docs/en/sub-agents#observe-and-steer-running-forks)
describe transcript selection, follow-up messages and Ctrl+Enter delivery
(version-gated at Claude Code v2.1.286). The
[resume section](https://code.claude.com/docs/en/sub-agents#resume-subagents)
documents separate `agent-{agentId}.jsonl` transcripts under the Session's
`subagents/` directory. The [nesting section](https://code.claude.com/docs/en/sub-agents#let-subagents-spawn-their-own-subagents)
describes tree rows with descendant counts and a path back to `main`;
[completed background transcript access](https://code.claude.com/docs/en/sub-agents#finished-subagents-in-the-panel-and-task-list)
through `/tasks` has a documented 30-second window.
These illustrate accessible actor history and
interactive follow-up as distinct capabilities; Arany need not import their
messaging semantics or timer-based hiding.

## Proposed direction and unresolved questions

**Original proposal at source-check time:** the plan separated a read-only snapshot switch, committed live updates and a scalable
`/agents` picker into independently usable slices. Adopt the verified Codex key
gates and wrap order; adapt Claude Code's idle grouping under Arany's redraw and
resource constraints. Keep the composer read-only or disabled with an explicit
viewing label. Escape returns to the primary rather than interrupting a child.
Every entry, return and selection path needs armed-frame admission, and Classic,
Fullscreen and linear/screen-reader equivalents need equal behavior.

**Owner question at source-check time:** child messaging or steering would
change a guarantee. [Goals](../specs/goals.md#steering-notes-and-pause) admits
Run-scoped notes only to primary planning/synthesis calls and explicitly
excludes child calls and Tool review. Viewing cannot reinterpret a note as
child-scoped authority or replay an effect. Any future proposal needs an owner
decision covering durable identity, journal/reducer admission, bounds,
authority narrowing and crash/cancellation semantics.

**Unverified:** actual Option-key byte sequences and enhanced-key fallback on
macOS terminals, Linux terminal coverage, screen-reader usability, large-team
latency and memory, and Codex's complete scale bounds. Source inspection does
not establish those claims. Native/manual procedures belong in
[Next steps](../../NEXT_STEPS.md) when implementation creates the corresponding
evidence obligation.
