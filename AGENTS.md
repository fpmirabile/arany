# Arany

Arany is a Rust-first, CLI-only agent harness. The product and executable are named Arany and `arany`; the repository directory name is not part of the public contract. This file guides agents developing Arany. [Documentation map](./docs/README.md) locates contracts and their owners.

## Product and implementation priorities

1. **Build the harness.** Arany owns agent execution, context, Tools, permissions and durable Sessions. Keep product policy in the Engine and platform, Provider and presentation details at their existing boundaries. Evaluate changes against the complete task lifecycle, including failure, cancellation and recovery. The [system overview](./docs/architecture/system-overview.md) owns the architecture.
2. **Design for OS parity.** Arany must be OS-agnostic: aim for equivalent behavior on Linux, macOS and Windows. For every implementation, consider platform impact; when OS facilities are involved, prefer standard/library capabilities with equivalent guarantees, then narrow native adapters. Do not assume a shell, path layout, desktop, distribution, service manager or Unix semantics. Document unavoidable differences and unavailable capabilities in the owning contract; unsupported security enforcement fails closed. A Linux-first milestone is delivery order, not the product boundary. Native evidence is required before claiming support; current gaps belong in [NEXT_STEPS.md](./NEXT_STEPS.md).
3. **Apply Ponytail on every implementation.** Read and apply [Ponytail](./.agents/skills/ponytail/SKILL.md) before implementation, refactoring or code review, and check the final diff against it. If missing, follow [project skill setup](./agents/skills/README.md#install-for-a-harness). Understand the affected flow, then prefer existing code, the standard library, suitable OS facilities and installed dependencies. Add only complexity required by the agreed task or a demonstrated constraint. Favor readable Rust; never reduce scope, correctness, security, accessibility, error handling, resource bounds or verification to save lines. Repository rules govern conflicts with the skill's shortcuts, testing, comment and output conventions.

## Rules to load before editing

| Task or boundary | Read |
|---|---|
| Git operations, including planned steps | [Git](./agents/git.md) |
| Auth, secrets, I/O, APIs, Tools, processes, MCP, memory, transports, untrusted rendering | [Security](./agents/security.md) |
| Providers, models, capabilities, credentials, routing, conformance | [Provider](./agents/provider.md) |
| Saved accounts, keyrings, credential transport | [Credential spec](./docs/specs/credentials.md), [credential rules](./agents/credentials.md) |
| ChatGPT-plan OAuth, consent, tokens, weaker output bounds | [ChatGPT](./agents/chatgpt.md) |
| Sessions, resume/fork/compaction, collaboration, topology, context | [Session](./agents/session.md) |
| SQLite admission, Events, migrations, replay | [Store](./agents/store.md) |
| Terminal behavior, output, accessibility, restoration, presentation | [Terminal](./agents/terminal.md) |
| Clipboard transport, clients, workers, results | [Clipboard](./agents/clipboard.md) |
| Image admission, multimodal context, replay | [Image](./agents/image.md) |
| History indexing, navigation, find, anchors | [History](./agents/history.md) |
| Behavior, fixes, tests, output, correctness/release claims, beta continuation, verification handoff | [Testing](./agents/testing.md) |
| Architecture, codebase shape, data model, new seams | [System overview](./docs/architecture/system-overview.md) |
| Creating or changing implementation plans | [Planning](./planning/PLANNING.md) |
| Product behavior, fixes, acceptance scenarios, documentation ownership | [Spec workflow](./docs/specs/README.md) and the owning contract |
| Run coordination, budgets, Provider outcomes | [Engine](./agents/engine.md) |
| Model-driven Tools, commands, runtime Skills, MCP, Guard | [Tools](./agents/tools.md) |
| Development skill installation or maintenance | [Project skills](./agents/skills/README.md) |
| CLI admission, modes, channels, `exec` composition | [CLI](./agents/cli.md) |
| OTLP endpoints, privacy, bounds, shutdown | [Telemetry](./agents/telemetry.md) |
| Development logs, failure stages, stack traces | [Diagnostics](./agents/diagnostics.md) |

Open the linked files whose triggers apply, including `agents/<module>.md` for the module being edited. These are ordinary documents, not automatically loaded nested instructions. Follow affected boundaries without loading unrelated rules. Root rules still apply; [CONTRIBUTING.md](./CONTRIBUTING.md) owns setup and check commands.

When a new module is introduced, add `agents/<module>.md` and link it here. Create the file only when the module has a real boundary, invariant, or workflow to document; avoid empty templates.

## Global rules

1. **English only**: use English for all repository content, including code, comments, documentation, configuration text, user-facing copy, commit messages, and public API names.
2. **Minimal comments**: default to none. Add one short comment when the reason is non-obvious. Architectural reasoning belongs in the relevant design document or ADR.
3. **Deep modules**: keep interfaces small and hide complexity inside the implementation. Prefer in-process calls. Add a seam only when at least two adapters exist or privilege, process, release, or ownership isolation makes it unavoidable. No speculative frameworks, traits or configuration.
4. **Explicit resource bounds**: queues, buffers, caches, request bodies, tool output, artifacts, concurrency, and retry counts have declared limits and overflow behavior.
5. **No historical residue**: remove superseded code cleanly. Documentation describes the current system; git history records the old one.
6. **Never inspect or print `.env`**: read `.env.example` only. Commands may load secrets without echoing them or dumping the process environment.
7. **Unsafe Rust is exceptional**: keep `unsafe` isolated behind a safe interface, document its invariants, and require a dedicated review and test strategy.

## Auto-learning

Capture newly discovered invariants, recurring failure modes, ownership decisions and workflow constraints in the same change. Use the narrowest authoritative home in the [documentation map](./docs/README.md#where-each-fact-belongs); behavior changes follow the [spec workflow](./docs/specs/README.md), including unmigrated domains. Record the context, decision, reason, trade-offs, scope and status. Update misleading guidance instead of adding duplicates; do not cache discoverable commands or configuration in prose. Mention intentional documentation changes in the handoff, commit or PR.

## Module boundaries

The beta is one Cargo package and one long-lived process, with bounded credential and Guard helpers for blocking work and privilege isolation. Provider is the only substitutable Engine behavior seam. Terminal/platform types stay outside the Engine; SQLite owns canonical history and telemetry is lossy output. Read the [system overview](./docs/architecture/system-overview.md#2-runtime-architecture) for current module ownership and data flow, and the relevant module rules before changing a boundary.

Create a module only after its implementation gains depth; a crate needs measured dependency, release, privilege, ownership or compile pressure. Add a daemon only for a second Client or detached execution. Effectful Tools require deterministic Policy and the separate Guard.

## Workflow

1. Read the triggered rules and affected flow. For behavior work, identify the owning contract and acceptance scenarios before implementation.
2. Propose a concrete plan for multi-module or public-interface changes. Include the harness responsibility, Ponytail choice, OS impact, documentation changes, triggered security pass and verification map under [testing rules](./agents/testing.md). Cover successful and meaningful failure paths through existing evidence owners. State when there is no OS impact.
3. Ask before deleting files, renaming public APIs, restructuring modules or changing CI/shared infrastructure unless the current request already authorizes it. Preserve existing user work under [Git rules](./agents/git.md).
4. Before closing, review the final diff with Ponytail, run applicable [repository checks](./CONTRIBUTING.md#verification), and capture durable learnings. Documentation-only work needs document/link review, not a Rust rebuild. Keep local visual artifacts in gitignored `.lavish/`.
5. After planning and before closing any implementation, review contract impact under the [spec workflow](./docs/specs/README.md). Update missing or changed requirements and scenarios; an adequate unchanged contract needs no edit. Report the changes, checks actually run, limitations and relevant platform evidence.

## Maintaining these instructions

Keep this file limited to knowledge useful in almost every session. Keep one authoritative source per rule; rewrite or prune instead of appending duplicates. Put module details behind conditional links, and verification history in its evidence owner. Keep machine-local paths and developer-specific tool preferences in personal agent configuration. When changing routing, check links and representative tasks; do not repeat facts an agent can cheaply discover from code or configuration.
