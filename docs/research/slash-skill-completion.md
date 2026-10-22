# Slash completion across commands and Skills

Source check: 2026-10-22. Question: what Claude Code precedent supports a shared
completion list and direct Skill invocation? Scope: official documentation;
no live Claude Code terminal session was observed.

Claude Code documents one filtered menu containing commands and Skills. Its
current collision policy differs from the owner's requested Arany policy.
Arany's current behavior belongs in the [terminal contract](../specs/terminal.md)
and [Tool contract](../specs/tools.md), not this source investigation.

## Documented peer behavior

| Concern | Evidence |
|---|---|
| Shared completion | Typing `/` lists built-in commands, bundled and user-authored Skills, plugin commands and MCP commands. Adding letters filters this menu. [Interactive mode: Commands](https://code.claude.com/docs/en/interactive-mode#commands). |
| Selection | Tab accepts the selected autocomplete suggestion. [Interactive mode: General controls](https://code.claude.com/docs/en/interactive-mode#general-controls). |
| Invocation | `/skill-name` invokes a Skill directly. A Skill and a legacy custom command share slash invocation syntax. [Skills introduction](https://code.claude.com/docs/en/skills). |
| Visibility | `user-invocable` defaults to true; false hides the Skill from the slash menu and prevents direct `/name` invocation. This differs from `disable-model-invocation`, which controls automatic model use. [Frontmatter reference](https://code.claude.com/docs/en/skills#frontmatter-reference), [invocation controls](https://code.claude.com/docs/en/skills#control-who-invokes-a-skill). |
| Collisions | In local terminal sessions, enterprise, personal and project Skills can replace a same-named built-in command; built-in aliases remain available. [Name resolution](https://code.claude.com/docs/en/skills#resolve-skills-that-share-a-name). |

## Evidence limits and Arany requirements

The literal `(skill)` prefix before descriptions, commands-first tie ordering,
preserving Arany's divided Commands/Skills screen for a bare `/`, verbatim
invocation echo and leading-token accent styling are supplied by the owner
request. The consulted peer documentation establishes shared menu
filtering and direct invocation, but does not establish that exact label or
ranking. Do not present those details as independently observed peer behavior.

The owner requires built-in commands to win name collisions. This intentionally
differs from Claude Code's documented local-terminal precedence, preserving the
identity of Arany's built-in controls when untrusted Skill metadata introduces a
matching name. This is a product requirement, not evidence about Claude Code.

Shared discovery and invocation syntax do not imply shared trust semantics.
Arany's admission, data-only Skill content, and effect authority remain governed
by its [Tool contract](../specs/tools.md).
