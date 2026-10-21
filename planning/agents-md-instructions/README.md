# Follow AGENTS.md instructions like Codex

## Metadata

- Status: `In progress` (S1 and S2 landed; next: S3)
- Owner: Arany maintainers
- Last updated: `2026-10-21`

## Outcome and scope

The model follows Workspace `AGENTS.md` instructions as behavior rules for work in their scope, the way Codex CLI does, while authority stays with Policy and the Guard. One feature: how instruction files reach the model. It covers the root file and its `CLAUDE.md` fallback (S1), nested files from the Workspace root to the launch directory (S2) and one user-global file (S3). Skills, explicit includes, agent definitions and on-demand loading of files below the launch directory stay as they are.

Owner decision (2026-10-21): "I prefer Codex's treatment of AGENTS.md. They are rules to follow always." The owner approved changing the Safe AI cage wording accordingly.

## Research

Findings and citations live in [AGENTS.md instructions research](../../docs/research/agents-md-2026-10.md). They are decision-relevant here:

- Before S1, Arany rendered the root file as a `workspace_guidance (untrusted)` JSON data block, the primary prompt listed it as untrusted data, and the model did not follow it.
- Codex sends a user-role `# AGENTS.md instructions for {directory}` message wrapped in `<INSTRUCTIONS>` without escaping. Its prompt says to obey files in scope, that deeper files win and that direct instructions take precedence. It joins root-to-cwd files with blank lines under one shared 32 KiB budget, silently truncating overflow. Its global file lives in `CODEX_HOME`.
- Claude Code also delivers `CLAUDE.md` as a user message: behavioral context, with enforcement left to permissions, hooks and the sandbox. OpenCode's global file is `~/.config/opencode/AGENTS.md`.
- Only `RunConfig.instruction_digest`, the digest of the raw file, is journaled. S1 therefore changes no Event payload. S2 does, because several files need directory-and-digest pins.

## Contract and scenarios

- Owning spec: [Session: Workspace admission and authority](../../docs/specs/session.md#workspace-admission-and-authority) owns discovery, bounds, framing and precedence. The [Safe AI cage guarantee](../../AGENTS.md#non-negotiable-guarantees) states the behavior/authority split.
- Evidence owners: the [render corpus](../../crates/arany/src/provider/render/tests.rs) owns the exact framing, neutralization, labelled assignment data and role prompts. The [startup journey](../../crates/arany-cli/tests/session_run/startup_trust.rs) proves the bytes on the wire through the real CLI and Engine. The [custom-profile journey](../../crates/arany-cli/tests/session_run/custom.rs) proves that children receive the same block. The [RunStarted payload pin](../../crates/arany/src/session/events/tests.rs) and [migration fixture](../../crates/arany/src/store/migrate/tests.rs) prove the stored instruction pins.

| Scenario | Outcome |
|---|---|
| Root `AGENTS.md`, or only `CLAUDE.md` | First user message `# AGENTS.md instructions for AGENTS.md` (or `CLAUDE.md`), a blank line, `<INSTRUCTIONS>`, the text, `</INSTRUCTIONS>`; primary, read-only, researcher and editor prompts carry the follow/precedence/no-authority rule |
| Text contains an opening or closing tag in any case or spacing, a heading line, or forged role or catalog text | Each tag-like `<` is sent as `&lt;` and each heading line gets a leading `\`; the block is one text block that ends only at its real closing line |
| Child with an agent definition or editor assignment | The child receives the same instruction block; definition body, assignment and historical source travel in a separate `assignment_context (untrusted)` data block |
| File over 64 KiB, link, hard link, non-UTF-8 | Rejected at admission with the existing errors before any Provider call; nothing is truncated |
| Instructions ask for wider authority | Policy and Guard decide as before; the prompt states that the file never grants or widens anything |
| Resume, fork, replay | Replay reads only the journaled digest and never renders; a later Run snapshots the file again |
| S2: nested files root to launch directory, one over the shared cap | Root-first blocks, deeper wins; total overflow rejects the turn |
| S3: user-global file present | Rendered first; project files follow and win |

## Chosen direction

- **Framing (S1):** a user-role message ahead of history, Codex's heading and tags. The heading names the file's Workspace-relative path instead of Codex's directory. That keeps the `CLAUDE.md` fallback visible, gives nested files (S2) their own scope and avoids disclosing a host path, consistent with hooks seeing only `/workspace`. Neutralize every file-supplied tag-like `<`, whitespace and `/` then `instructions` in any case, as `&lt;`, and prefix a line-leading `# AGENTS.md instructions` heading of any case and `#` count with `\`. Neither replacement adds a `<` or a line-leading `#`, so one pass cannot form a new match. Codex does not neutralize; Arany does because file text must not reopen the conversation outside the block.
- **Prompts:** one shared sentence adapted from Codex's AGENTS.md spec, in each role prompt that can receive instructions. The editor prompt moves to `prompts/editor.txt` beside the others. The Tool reviewer never receives instructions, so it keeps refusing project-supplied instructions.
- **Children:** split the root instruction block from per-child data. `RunInput.workspace_guidance` becomes a typed `WorkspaceGuidance {path, text}`, and the new `assignment_context` carries agent definitions, worker assignments and historical source as labelled data. Before S1 these texts shared one blob with the instructions. Following that blob would have promoted them.
- **Compaction:** `pinned_goal.project_instructions` carries each file's exact path and text, root first. The primary request head still carries the blocks after compaction, so the summarizer need not restate them.
- **Overflow: reject, not truncate.** The rationale and the divergence from Codex live in the [Session contract](../../docs/specs/session.md#workspace-admission-and-authority).
- **Nested (S2):** the launch directory is the process working directory of `exec` and attached chat when it lies inside the Workspace; `serve` and `acp` read the root only. For each directory from the root down to it, take `AGENTS.override.md`, else `AGENTS.md`, else `CLAUDE.md`, with the same no-follow handle walk. Directories above the Workspace are never read, as the startup journey already pins. Allow at most 32 directories and 64 KiB in total across files. One message carries one wrapped block per file, root first. `RunStarted` 19 replaces `instruction_digest` with ordered `{directory, file, digest}` sources plus the launch directory. Version 18 cannot prove whether its digest came from `AGENTS.md` or `CLAUDE.md`, so the `store::migrate` step maps it to the root source with a null `file`.
- **User-global (S3):** `STATE_DIR/AGENTS.md`, Arany's analogue of Codex's `CODEX_HOME/AGENTS.md`. The State directory is already the admitted private home of user-authored configuration (`tools.json`, `permissions.json`, `agents/`) and follows `--state-dir`/`ARANY_STATE_DIR`, so tests never touch real user files. `~/.config/arany` (OpenCode's shape) would add a second unadmitted discovery root. The file is read under existing private-record admission with its own 64 KiB cap, outside the project budget as in Codex, and rendered first with the heading `# AGENTS.md instructions for user AGENTS.md`.
- **OS impact:** none for S1, which is text framing over the existing snapshot. S2 reuses the capability-relative no-follow walk on Linux and macOS; S3 reuses State admission. Synthetic tests suffice; no native claim changes.

## Security and resource model

Security pass (trigger: instruction role and prompt-assembly change).

- **Assets and authority source:** grants, permission mode, approvals, sandbox, roots, credentials, Provider routes and Goal budgets. They come only from authenticated user configuration, deterministic Policy and the Guard, all fixed before Workspace input is read. Nothing in this plan reads instruction text when deciding any of them.
- **Attacker-controlled input:** the repository's instruction files, and in S2 every nested file on the launch path. Following them changes what the model proposes, never what is permitted. Each proposal still passes typed intent normalization, Policy, approval and Guard admission. Descendants inherit the parent's narrowed grant regardless of the text. A hostile file can do what hostile data could already attempt: steer the answer within the granted authority. Direct user and system instructions rank above it.
- **Why authority is unaffected:** enforcement never consults model intent. The prompt sentence is advisory, and the Policy and Guard checks that already contain hostile model output contain an obedient model as well. No new capability, grant source, trust record or startup step is added.
- **Untrusted Workspaces:** Codex skips project files in untrusted projects. Arany keeps loading them because untrusted and headless Runs are already read-only by Policy, so following the file can only shape a read-only answer. This keeps `exec` behavior uniform across trust states.
- **Wrapper integrity:** opening and closing tags in any case or spacing, and heading lines, are neutralized. Agent definitions, assignments, includes, Skills and Tool output stay in labelled data blocks, outside the followed block.
- **Bounds:** 64 KiB per file, rejected before allocation beyond the limit. S2 adds a 64 KiB shared total and a 32-level depth limit. S3 adds a separate 64 KiB cap. Existing context admission counts instruction bytes against the compiled-content ceiling, and the wrapper adds under 80 bytes plus at most three bytes per neutralized tag or heading.
- **Replay and disclosure:** strict replay never renders or rereads instructions. No instruction text enters Events, telemetry or diagnostics; only the digest does.
- **Non-claims:** model obedience is not enforcement and is not measured here; that belongs to behavioral evaluation.

## Risks and mitigations

- A model treats instructions as permission → the prompt says they never grant or widen anything, and Policy and Guard enforce regardless.
- Untrusted child text gets followed → children receive it as `assignment_context (untrusted)`, and the render corpus pins the separation.
- S2 widens disclosure to ancestors outside the Workspace → the walk starts at the admitted root handle; the startup journey's omitted-parent canary stays.

## Vertical slices

1. **S1, root instructions followed.** Root `AGENTS.md`/`CLAUDE.md` is rendered in Codex framing as the first user message, with tags and headings neutralized and the follow/precedence/no-authority rule in every receiving role prompt. Children get the same block plus separate assignment data. Spec and guarantee text are updated. Evidence: render corpus, prompt pin, wire bytes in the startup journey, children in the custom journey. No stored shape changes.
2. **S2, nested instructions (landed).** Files from root to launch directory load root first, deeper wins, one wrapped block each, 64 KiB shared and 32 directories, overflow rejects. `RunStarted` 19 records ordered directory/file/digest pins and the launch directory through a `store::migrate` step. Evidence: the [session input corpus](../../crates/arany/src/session/tests.rs) (order, override and fallback per directory, launch outside, at and through links, shared cap, depth, links), the [render corpus](../../crates/arany/src/provider/render/tests.rs) nested and forged-path rows, the Engine rejection rows in [engine_run](../../crates/arany/tests/engine_run.rs), the subdirectory [startup journey](../../crates/arany-cli/tests/session_run/startup_trust.rs) with omitted parent, sibling and shadowed-fallback canaries and replay pins, and the [migration fixture](../../crates/arany/src/store/migrate/tests.rs).
3. **S3, user-global instructions.** `STATE_DIR/AGENTS.md` is admitted as a private record, rendered first with its own 64 KiB cap and pinned beside the project sources. Evidence: State-admission rejection rows for permissions, links and size, plus a journey with `--state-dir` showing order and precedence text.

## Exit criteria

- S1 to S3 observations above pass in CI on Linux and macOS lanes; journeys run the real CLI against a loopback Provider.
- The session spec states discovery, bounds, overflow, framing and precedence; the guarantee bullet keeps "never carry authority" and "agents can only narrow authority".
- After S3 lands, delete this plan, remove its [planning index](../README.md#active-plans) row and resolve item 5's `AGENTS.md` part in [NEXT_STEPS.md](../../NEXT_STEPS.md).
