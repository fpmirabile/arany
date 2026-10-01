# Arany

Rust-first, CLI-only agent harness. The product and executable are named Arany and `arany`; the repository directory name is not part of the public contract. The current physical architecture and data model live in [docs/architecture/system-overview.md](./docs/architecture/system-overview.md); broader research remains indexed under [docs/research/README.md](./docs/research/README.md). This file contains rules, not duplicated project context.

## Rules to load before editing

- Git operations or a plan containing git steps → [agents/git.md](./agents/git.md)
- Auth, secrets, external I/O, provider APIs, tools, processes, MCP, memory, remote transports, or rendering untrusted content → [agents/security.md](./agents/security.md)
- Provider selection, models, capability evidence, adapter behavior, credentials, routing, or live conformance → [agents/provider.md](./agents/provider.md)
- Saved API accounts, OS keyring access, or Linux D-Bus credential transport → [agents/credentials.md](./agents/credentials.md)
- ChatGPT-plan OAuth, account consent, token storage, or weaker output bounds → [agents/chatgpt.md](./agents/chatgpt.md)
- Sessions, resume/fork/compaction, collaboration policy, agent topology, or context reconstruction → [agents/session.md](./agents/session.md)
- SQLite state admission, Event persistence, migrations, or replay → [agents/store.md](./agents/store.md)
- Interactive terminal behavior, plain output, accessibility, terminal restoration, or presentation state → [agents/terminal.md](./agents/terminal.md)
- Terminal history indexing, navigation, find, or reading anchors → [agents/history.md](./agents/history.md)
- Behavior changes, defect fixes, tests, fixtures, user-visible output, or correctness and release claims → [agents/testing.md](./agents/testing.md)
- Architecture, codebase shape, data model, or a proposed new seam → [docs/architecture/system-overview.md](./docs/architecture/system-overview.md)
- Creating or changing an implementation plan → [planning/PLANNING.md](./planning/PLANNING.md)
- Engine Run coordination, budgets, or Provider outcome handling → [agents/engine.md](./agents/engine.md)
- Work inside a module → its file under `agents/<module>.md`
- Development-skill installation or maintenance → [agents/skills/README.md](./agents/skills/README.md)
- CLI command admission, mode selection, process channels, or `exec` composition → [agents/cli.md](./agents/cli.md)
- OTLP endpoint admission, trace privacy, export bounds, or shutdown → [agents/telemetry.md](./agents/telemetry.md)

Load every relevant file for cross-module changes. A module file contains only module-specific knowledge; these root rules still apply.

When a new module is introduced, add `agents/<module>.md` and link it here. Create the file only when the module has a real boundary, invariant, or workflow to document; avoid empty templates.

## Global rules

1. **English only**: use English for all repository content, including code, comments, documentation, configuration text, user-facing copy, commit messages, and public API names.
2. **Minimal comments**: default to none. Add one short comment when the reason is non-obvious. Architectural reasoning belongs in the relevant design document or ADR.
3. **Simple over clever**: apply [Ponytail](./.agents/skills/ponytail/SKILL.md) to code changes and reviews. Understand the full flow, then prefer existing code, standard/native facilities, and installed dependencies before adding complexity. Complete the agreed objective and acceptance criteria; never reduce requested scope or weaken correctness, security, accessibility, error handling, resource bounds, or required verification to save lines. Favor readable Rust over the shortest diff. Repository rules take precedence over the skill's shortcuts, testing, comment, and output conventions.
4. **Deep modules**: keep interfaces small and hide complexity inside the implementation. Prefer in-process calls. Add a seam only when at least two adapters exist or privilege, process, release, or ownership isolation makes it unavoidable.
5. **Explicit resource bounds**: queues, buffers, caches, request bodies, tool output, artifacts, concurrency, and retry counts have declared limits and overflow behavior.
6. **No historical residue**: remove superseded code cleanly. Documentation describes the current system; git history records the old one.
7. **Never inspect or print `.env`**: read `.env.example` only. Commands may load secrets without echoing them or dumping the process environment.
8. **Unsafe Rust is exceptional**: keep `unsafe` isolated behind a safe interface, document its invariants, and require a dedicated review and test strategy.
9. **Rules stay terse**: keep one authoritative source for each rule. Prefer rewriting or pruning existing entries over appending duplicates.

## Auto-learning

Capture durable conclusions in the same change that reveals them. A durable conclusion is a newly discovered invariant, recurring failure mode, architectural responsibility, user decision, or workflow constraint that a future agent would otherwise need to rediscover.

Route each learning to its narrowest authoritative home:

- Agent behavior shared across the repository → `AGENTS.md` or `agents/*.md`
- Module-specific invariant or workflow → `agents/<module>.md`
- Architecture or product decision → an ADR or architecture document under `docs/`
- Repeatable procedure → a skill
- Commands, dependencies, and configuration already encoded by the repository → keep the repository as the source of truth; do not cache them in prose

Record decisions with context, decision, reason, trade-offs, scope, and current status. Update or prune misleading guidance instead of layering a new rule on top. Mention intentional documentation changes in the commit or PR.

## Module boundaries

The beta is one Cargo package and one long-lived process; only OS-credential calls use a supervised, short-lived same-binary helper. `main.rs` selects a presentation and calls the deep Engine module in `lib.rs`. Session/context lifecycle has enough depth for `session.rs`; pure semantic presentation lives in `presentation.rs`; terminal ownership and drawing live in `terminal.rs`. None may leak Ratatui, Crossterm, key, color, width, hover, or focus types into the Engine. Each Run has one accountable primary AgentRun and an ordered, budget-bounded `0..N` collection of direct read-only children; `single`, `auto`, or explicit `team` policy is pinned per Run, and children do not create nested teams in beta. Provider is the only Engine behavior seam: the scripted fake proves Engine behavior; native OpenAI and Anthropic prove supported switching; exact custom endpoint/model profiles must pass data-free conformance before Workspace disclosure. A built-in OpenRouter profile is deferred until its route/privacy gate; Z.AI remains unadmitted until it can enforce the strict outcome contract. A dedicated standard-library thread owns the sole SQLite connection; replay, instruction resolution, Workspace input, scheduling, SessionView/RunView reduction, and context compilation remain private Engine implementation. OTLP trace export lives in one private `telemetry.rs` module; it is opt-in at runtime, lossy output, never canonical state or an Engine behavior seam.

Create a new module only after its implementation gains depth. Create a crate only after measured dependency, release, privilege, ownership, or compile pressure. Add a daemon only when a second Client or detached execution exists. Add deterministic Policy and the separate Guard before the first effectful Tool ships.

## Workflow

Before non-trivial work:

- Read the relevant module rules and design documents.
- Propose a concrete plan for multi-module or public-interface changes.
- Ask before deleting files, renaming public APIs, restructuring modules, or changing CI and shared infrastructure.
- Keep visual review artifacts under `.lavish/`; they are local and gitignored.

Every implementation plan includes:

- A verification map following [agents/testing.md](./agents/testing.md), covering the successful path and meaningful failure claims without duplicating existing owners.
- Documentation updates when behavior, public interfaces, schemas, configuration, architecture, or workflow changes.
- A security pass when a trigger in [agents/security.md](./agents/security.md) applies.
- A final formatting, linting, test, and diff review using the checks declared by the repository.
- An auto-learning pass that updates the authoritative document when the work produced a durable conclusion.

## Maintaining these instructions

Keep this file limited to knowledge useful in almost every session. Put branch-specific knowledge behind the pointers above. Do not repeat facts an agent can cheaply discover from code or configuration.
