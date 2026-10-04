# Agent instructions and product specifications

**Date:** 2026-10-04  
**Scope:** primary-source guidance and a proposed documentation split; no instruction discovery, runtime behavior, or documentation migration is implemented by this note.

## Established patterns

There is useful convergence around concise instructions, scoped detail, and maintained specifications, but not one universal directory layout or loading mechanism.

- **A short instruction entry point with conditional references.** OpenAI recommends routing agents to architecture, database, or deployment documents when the corresponding task needs them, rather than loading every document before every edit. It also recommends narrow skill triggers and progressive disclosure. This is model-specific guidance, not permission to remove Arany's security or verification requirements. [OpenAI: rethinking skills and prompts](https://developers.openai.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra).
- **Scoped rules rather than eager imports.** Claude Code recommends concise global instructions and path-scoped rules for specialized work. Its `@path` imports expand into startup context: splitting a file through imports alone does not save context. Unscoped `.claude/rules/` files also load at launch; rules with matching `paths` load conditionally. Skills suit reusable procedures. [Claude Code memory and rules](https://code.claude.com/docs/en/memory).
- **Nested instructions where the client supports them.** The public AGENTS.md guidance recommends nested files for subprojects and treats the nearest applicable file as more specific. AGENTS.md itself is ordinary Markdown, not a standardized import engine. [AGENTS.md guidance](https://agents.md/).
- **Behavior specifications separate from execution plans.** Spec Kit separates feature requirements from technical planning and tasks, with project principles in a constitution. Its existing-project guide recommends starting with a bounded change, retaining existing conventions, and deciding whether completed specs remain historical or become living contracts. It does not require retrospectively specifying the whole application. [Spec Kit workflow](https://github.com/github/spec-kit#spec-driven-development), [existing-project guide](https://github.github.io/spec-kit/guides/existing-projects.html).
- **Current contracts separate from proposed changes.** OpenSpec keeps domain requirements and concrete scenarios in current specs, while proposed deltas live in change folders. Its maintainers explicitly recommend incremental adoption in existing repositories, not wholesale conversion of old documents. [OpenSpec concepts](https://github.com/Fission-AI/OpenSpec/blob/main/docs/concepts.md), [existing-project guide](https://github.com/Fission-AI/OpenSpec/blob/main/docs/existing-projects.md).
- **Small decision records for rationale.** Michael Nygard's ADR pattern records one consequential architectural decision with context, decision, status, and consequences; replaced decisions become superseded rather than silently losing their rationale. An ADR explains why, rather than replacing the current behavioral contract. [Documenting Architecture Decisions](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions).

## Discovery is not a Markdown link

Codex documents a startup instruction chain from the project root to the current working directory, with configured size limits and filename precedence. That is not a promise that every nested file below a root session will be dynamically loaded for every later edit. [Codex AGENTS.md guide](https://developers.openai.com/codex/guides/agents-md).

Claude Code's current AGENTS.md support depends on version and competing CLAUDE.md files; its on-demand discovery and import handling are client-specific. A prose request or ordinary Markdown link still requires the agent to open the target. Do not assume `@` imports or nested discovery behave identically across tools. [Claude Code instruction loading](https://code.claude.com/docs/en/memory#agentsmd).

The Agent Skills standard explicitly describes progressive loading: metadata first, the selected skill body next, supporting resources as needed. Merely moving a large rule file into a skill would not make its body smaller once activated. [Agent Skills specification](https://agentskills.io/specification#progressive-disclosure).

## Recommendation for Arany — inference

The repository already has the right partial separation: [AGENTS.md](../../AGENTS.md) routes to module instructions and identifies [the architecture overview](../architecture/system-overview.md) as canonical context. Refine this organization rather than installing a new framework or replacing every rule with a giant spec.

| Material | Narrow authoritative home | Purpose |
|---|---|---|
| Repository-wide guardrails and task routing | `AGENTS.md` | How an agent works and when to read more |
| Module-specific editing constraints and recurring gotchas | `agents/<module>.md` | What a developer must preserve while changing that module |
| Durable product behavior | Proposed `docs/specs/<domain>.md` | Successful behavior, rejection behavior, bounds, recovery, and non-claims |
| Physical ownership and dependency structure | Existing `docs/architecture/` | How the implementation is organized |
| Consequential trade-offs and rejected alternatives | Existing decision register; focused ADRs when needed | Why an architectural decision was made |
| Repeatable development procedures | Existing development Skills | How to perform a particular task |
| Temporary tasks and verification handoff | Existing `planning/` and `NEXT_STEPS.md` | What remains to be done |

Begin with one overloaded topic, such as terminal behavior. Classify each paragraph before moving it: agent workflow stays in module rules; product guarantees move to a focused spec; rationale belongs in an existing architecture document or ADR; historical test results stay evidence rather than instructions. Keep an explicit trigger in the module rule pointing to its spec. Move each meaning once, replacing the old copy with a pointer; relocating text without pruning duplication only creates two sources of truth.

A small spec needs scope, agreed behavior, important failure scenarios, resource limits, compatibility boundaries, and links to the existing verification owners. Distinguish the agreed contract from its implementation and verification status: a documented requirement is not proof that it works, and an old passing test is not a timeless release guarantee. Do not duplicate code/configuration constants unless the specification establishes their public meaning.

Preserve security-critical editing constraints and Arany's rule that Markdown guidance grants no execution authority. Reuse the [decision register](./next-step-decision-register.md) initially rather than creating ADRs for every existing choice.

This proposal concerns development-agent documentation. Arany itself snapshots the exact Workspace-root `AGENTS.md`, with exact root `CLAUDE.md` only as an absence fallback; that is a separate contract from Codex or Claude Code discovery. Moving guidance into specs does not make the executable automatically follow links, `@` imports, or nested instructions. A migration must not expand included outbound content, Provider admission, credential access, Policy, or Guard authority. [Current Arany architecture](../architecture/system-overview.md#1-what-the-beta-must-prove).

Use word/token volume and relevance, not line count alone, to assess the result. Validate a small migration with representative tasks: a harmless text fix should not load unrelated contracts; a Provider or persistence change must still reach all relevant boundaries and tests. Adopt framework automation only if manual spec/change maintenance becomes a demonstrated problem. No framework, file movement, or authoritative rule change is required to answer this research question.
