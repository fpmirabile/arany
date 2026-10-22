# AGENTS.md instructions research

- Question: how do peer harnesses present `AGENTS.md`-style project instructions to the model, and how can Arany make the model follow them without letting file text carry authority?
- Source-check date: 2026-10-21. Arany source was checked at `a3bbe160`; Codex source is pinned to `openai/codex` commit `a6839a481223fb5faab539a9eb2b7174ae29e560` (`main` on that date). Claude Code and OpenCode documentation is dated evidence.
- Conclusion: copy Codex's user-role `# AGENTS.md instructions for …` / `<INSTRUCTIONS>` framing and its prompt rules (follow in scope, deeper wins, direct instructions win). Arany's Policy and Guard already decide authority independently of model text, so following the file for behavior changes no authority boundary.
- Scope: source investigation for one feature. The [Session contract](../specs/session.md#workspace-admission-and-authority) is authoritative.

## Arany before the change

| Finding | Source |
|---|---|
| Only exact Workspace-root `AGENTS.md` loads, with exact `CLAUDE.md` as an absence-only fallback, each a no-follow, single-link, UTF-8 snapshot of at most 64 KiB; an oversized file rejects with `InputTooLarge`, never truncates. | [session/input.rs:11](../../crates/arany/src/session/input.rs#L11), [input.rs:131](../../crates/arany/src/session/input.rs#L131), [engine.rs:162](../../crates/arany/src/engine.rs#L162) |
| The file was rendered as JSON data, `{"scope":"workspace_guidance (untrusted)","data":…}`, inside the current user message, and the primary prompt listed "Workspace guidance" among untrusted data. In dogfooding the model said it had not read `AGENTS.md` and did not follow it. | `render.rs` `current_message` and `prompts/primary.txt` at `a3bbe160` |
| Only the raw file digest is journaled (`RunConfig.instruction_digest`); no source, path or rendered form is stored, so changing the framing changes no Event payload. | [session/events/records.rs:146](../../crates/arany/src/session/events/records.rs#L146) |
| Children reused the root guidance string and appended the agent definition body, the editor assignment and historical source facts to it, so these untrusted texts shared one blob with the instructions. | `engine/run_loop/children.rs` `child_request` at `a3bbe160` |
| In-Goal Tool compaction passes the instruction text to the summarizer as `pinned_goal.project_instructions`. | [engine/run_loop/tool_compaction.rs:132](../../crates/arany/src/engine/run_loop/tool_compaction.rs#L132) |
| Authority comes only from authenticated user configuration, Policy and the Guard; hook input never shows a host path (`cwd` is always `/workspace`). | [Session authority](../specs/session.md#workspace-admission-and-authority), [Tools hook input](../specs/tools.md#hook-execution) |

## Codex CLI

| Finding | Source |
|---|---|
| Instructions are a `user`-role item: `# AGENTS.md instructions for {directory}\n\n<INSTRUCTIONS>\n{text}\n</INSTRUCTIONS>`. The text is interpolated without escaping, so a literal `</INSTRUCTIONS>` closes the block early. | [core/src/context/user_instructions.rs](https://github.com/openai/codex/blob/a6839a481223fb5faab539a9eb2b7174ae29e560/codex-rs/core/src/context/user_instructions.rs) |
| Discovery walks from the project root (nearest `project_root_markers`, default `.git`) down to the working directory and takes at most one file per directory: `AGENTS.override.md`, then `AGENTS.md`, then `project_doc_fallback_filenames`. Files concatenate root first, joined by blank lines; user instructions come first, separated by `\n\n--- project-doc ---\n\n`. | [core/src/agents_md.rs](https://github.com/openai/codex/blob/a6839a481223fb5faab539a9eb2b7174ae29e560/codex-rs/core/src/agents_md.rs) |
| `project_doc_max_bytes` (default 32 KiB) is one remaining budget shared by all project files; a file over the remainder is truncated and only a `tracing` warning is logged. Whitespace-only files are skipped; user instructions are outside the budget. Untrusted projects load no project docs. | [core/src/agents_md.rs](https://github.com/openai/codex/blob/a6839a481223fb5faab539a9eb2b7174ae29e560/codex-rs/core/src/agents_md.rs), [AGENTS.md guide](https://learn.chatgpt.com/docs/agent-configuration/agents-md) |
| The global file is `AGENTS.override.md` or `AGENTS.md` in the Codex home directory (`~/.codex`, or `CODEX_HOME`). | [AGENTS.md guide](https://learn.chatgpt.com/docs/agent-configuration/agents-md) |
| The base prompt's "AGENTS.md spec": a file's scope is the directory tree rooted at its folder; obey it for every file touched in scope; more deeply nested files win; direct system, developer and user instructions take precedence; root-to-cwd contents are already in context. | [core/gpt_5_2_prompt.md](https://github.com/openai/codex/blob/a6839a481223fb5faab539a9eb2b7174ae29e560/codex-rs/core/gpt_5_2_prompt.md) |

## Claude Code and OpenCode

| Finding | Source |
|---|---|
| `CLAUDE.md` content arrives as a user message after the system prompt; Claude "treats them as context, not enforced configuration", and enforcement belongs to permissions, hooks and the sandbox. | [Claude Code memory](https://code.claude.com/docs/en/memory) |
| Claude Code loads managed, user (`~/.claude/CLAUDE.md`), project and local files; ancestors of the working directory load at launch and subdirectory files load when a file there is read. It reads `AGENTS.md` only when no `CLAUDE.md` exists, the inverse of Arany's fallback. | [Claude Code memory](https://code.claude.com/docs/en/memory) |
| OpenCode's global rules file is `~/.config/opencode/AGENTS.md`, with `~/.claude/CLAUDE.md` used only when it is absent. | [OpenCode rules](https://opencode.ai/docs/rules/) |

## Implications

- Peers agree that instruction files steer behavior and that authority stays with permissions and sandbox. Arany's Policy and Guard already enforce authority regardless of what the model is told, so following the file needs no new enforcement; it needs only the framing and prompt rules.
- Naming the file's Workspace-relative path in the heading, rather than Codex's directory, keeps the `CLAUDE.md` fallback's provenance visible, gives nested files (planned) their own scope, and avoids disclosing a host path.
- Codex's silent truncation means a followed rule set can be cut without the user seeing it; Arany's existing reject-at-admission keeps the pinned digest equal to the bytes the model follows.
- The user-global file maps to Arany's State directory, the analogue of `CODEX_HOME`: it is already the private, admitted home of user-authored configuration (`tools.json`, `permissions.json`, `agents/`).
