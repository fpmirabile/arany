# Arany

Arany is a Rust-first, CLI-only agent harness. The product and executable are named Arany and `arany`; the repository directory name is not part of the public contract. This file guides agents developing Arany. [Documentation map](./docs/README.md) locates contracts and their owners.

## Product and implementation priorities

1. **Build the harness.** Arany owns agent execution, context, Tools, permissions and durable Sessions. Keep product policy in the Engine and platform, Provider and presentation details at their existing boundaries. Evaluate changes against the complete task lifecycle, including failure, cancellation and recovery. The [system overview](./docs/architecture/system-overview.md) owns the architecture.
2. **Design for Linux and macOS parity.** Arany targets Linux and macOS with equivalent behavior. Native Windows is out of scope: Windows users run the Linux build under WSL2. For every implementation, consider platform impact on both targets; when OS facilities are involved, prefer standard/library capabilities with equivalent guarantees, then narrow native adapters. Do not assume a shell, path layout, desktop, distribution or service manager. Document unavoidable differences and unavailable capabilities in the owning contract; unsupported security enforcement fails closed. A Linux-first milestone is delivery order, not the product boundary. Native evidence is required before claiming support; current gaps belong in [NEXT_STEPS.md](./NEXT_STEPS.md).
3. **Apply Ponytail on every implementation.** Read [Ponytail](./.agents/skills/ponytail/SKILL.md) before implementing, refactoring or reviewing code; install it through [project skill setup](./agents/skills/README.md#install-for-a-harness) if missing. Prefer existing code, the standard library, OS facilities and installed dependencies; add only complexity the agreed task or a demonstrated constraint requires. Never reduce scope, correctness, security, accessibility, error handling, resource bounds or verification to save lines; repository rules win over the skill's shortcuts.

## Rules to load before editing

| Task or boundary | Read |
|---|---|
| Git operations, including planned steps | [Git](./agents/git.md) |
| Auth, secrets, I/O, APIs, Tools, processes, MCP, memory, transports, untrusted rendering | [Security](./agents/security.md) |
| Providers, models, capabilities, credentials, routing, conformance | [Provider](./agents/provider.md) |
| SSE framing, provisional answer extraction and credential-safe streaming | [Provider streaming](./agents/provider-streaming.md) |
| Saved accounts, keyrings, credential transport | [Credential spec](./docs/specs/credentials.md), [credential rules](./agents/credentials.md) |
| ChatGPT-plan OAuth, consent, tokens, weaker output bounds | [ChatGPT](./agents/chatgpt.md) |
| Sessions, resume/fork/compaction, collaboration, topology, context | [Session](./agents/session.md) |
| SQLite admission, Events, replay | [Store](./agents/store.md) |
| Persistent Workspace build-cache identity, quota, archive and recovery | [Build cache](./agents/build-cache.md) |
| Shared private-directory admission and native ACL checks | [Private filesystem](./agents/private-fs.md) |
| Terminal behavior, output, accessibility, restoration, presentation | [Terminal](./agents/terminal.md) |
| Ratatui backend, Inline cursor responses and scoped clearing | [Terminal backend](./agents/terminal-backend.md) |
| Clipboard transport, clients, workers, results | [Clipboard](./agents/clipboard.md) |
| Image admission, multimodal context, replay | [Image](./agents/image.md) |
| History indexing, navigation, find, anchors | [History](./agents/history.md) |
| Behavior, fixes, tests, output, correctness/release claims, beta continuation, verification handoff | [Testing](./agents/testing.md) |
| Documentation and agent-instruction routing | [Documentation checks](./agents/testing.md#documentation-and-instruction-changes) |
| Architecture, codebase shape, data model, new seams | [System overview](./docs/architecture/system-overview.md) |
| Creating or changing implementation plans | [Planning](./planning/PLANNING.md) |
| Product behavior, fixes, acceptance scenarios, documentation ownership | [Spec workflow](./docs/specs/README.md) and the owning contract |
| Run coordination, budgets, Provider outcomes | [Engine](./agents/engine.md) |
| Model-driven Tools, commands, runtime Skills, MCP, Guard | [Tools](./agents/tools.md) |
| Runtime profile discovery, exact identities and folder-trust selection | [Toolchains](./agents/toolchains.md) |
| Native Guard launch, sandbox profiles, resources, FFI and process ownership | [Guard](./agents/guard.md) |
| Shared Guard capacity, FIFO waiting and user settings | [Guard admission](./agents/guard-admission.md) |
| Native macOS coalition ownership, control transport and resource supervision | [Guard unit](./agents/guard-unit.md) |
| Explicit native fixture stage capture and timeout observations | [Guard fixture](./agents/guard-fixture.md) |
| Development skill installation or maintenance | [Project skills](./agents/skills/README.md) |
| CLI admission, modes, channels, `exec` composition | [CLI](./agents/cli.md) |
| OTLP endpoints, privacy, bounds, shutdown | [Telemetry](./agents/telemetry.md) |
| Development logs, failure stages, stack traces | [Diagnostics](./agents/diagnostics.md) |

Open the linked files whose triggers apply, including `agents/<module>.md` for the module being edited; links are ordinary documents, not automatic imports. Start with the task's contract and rules and do not preload everything. Use the [documentation map](./docs/README.md#find-the-owner-for-a-task) when ownership is unclear; [development setup and checks](./docs/development.md) owns shared commands.

When a new module is introduced, add `agents/<module>.md` and link it here. Create the file only when the module has a real boundary, invariant, or workflow to document; avoid empty templates.

## Global rules

1. **English only**: use English for all repository content, including code, comments, documentation, configuration text, user-facing copy, commit messages, and public API names.
2. **Minimal comments**: default to none. Add one short comment when the reason is non-obvious. Architectural reasoning belongs in the relevant design document or ADR.
3. **Deep modules**: keep interfaces small and hide complexity inside the implementation. Prefer in-process calls. Add a seam only when at least two adapters exist or privilege, process, release, or ownership isolation makes it unavoidable. No speculative frameworks, traits or configuration.
4. **Explicit resource bounds**: queues, buffers, caches, request bodies, decompression, stdout/stderr, tool output, artifacts, concurrency, and retry counts have declared limits and overflow behavior. Goal usage follows its pinned settings; this never removes finite per-action, storage, request, Tool, and platform bounds. See the [Goal contract](./docs/specs/goals.md#lifecycle-budgets-and-recovery) for usage defaults.
5. **No historical residue**: remove superseded code cleanly. During pre-beta, keep one current internal format without compatibility readers, automatic migrations or relocation fallbacks. Reject unsupported stored formats without changing their data. External protocol support remains governed by its owning contract. Documentation describes the current system; git history records the old one.
6. **Never inspect or print `.env`**: read `.env.example` only. Commands may load secrets without echoing them or dumping the process environment.
7. **Unsafe Rust is exceptional**: keep `unsafe` isolated behind a safe interface, document its invariants, and require a dedicated review and test strategy.

## Auto-learning

When a change reveals a non-obvious invariant, a recurring failure mode or an ownership decision, record it once at its owner from the [documentation map](./docs/README.md#where-each-fact-belongs), in the same change. Fix misleading guidance instead of adding a duplicate. Do not add task transcripts, check logs or facts readable from code.

## Module boundaries

Read the [system overview](./docs/architecture/system-overview.md#2-runtime-architecture) and relevant module rules before changing a boundary. The overview owns package/process shape, dependency direction and state ownership.

Create a module only after its implementation gains depth; a crate needs measured dependency, release, privilege, ownership or compile pressure. Add a daemon only for a second Client or detached execution. Effectful Tools require deterministic Policy and the separate Guard.

## Workflow

1. **Read, then plan only when the change is big.** Read the triggered rules and the affected flow; for behavior work, find the owning contract and its scenarios. For multi-module or public-interface changes, propose a short plan first: scope, Ponytail choice, OS impact or none, triggered [security pass](./agents/security.md#security-pass-triggers) and how the change will be verified. Ask before deleting files, renaming public APIs, restructuring modules or changing CI/shared infrastructure unless the request already authorizes it; preserve existing user work under [Git rules](./agents/git.md).
2. **Verify by target locally, fully in CI.** Run formatting, Clippy and the tests that cover what you touched as described in [verification](./docs/development.md#verification); the complete workspace suite runs in CI on every pull request. After pushing, watch the PR's CI: a red CI blocks handoff and must be fixed, not explained. Until a stable pre-beta, synthetic tests plus green CI are sufficient evidence for a change; live-account, native and manual checklists stay in [NEXT_STEPS.md](./NEXT_STEPS.md), never block day-to-day work, and remain the only basis for claiming native or live support.
3. **Touch documentation once, and only when behavior changed.** If observable behavior, a limit or a recovery path changed, update the owning spec under the [spec workflow](./docs/specs/README.md). A refactor, a fix that restores specified behavior or an adequate unchanged contract needs no documentation edit. Review the final diff with Ponytail, keep local visual artifacts in gitignored `.lavish/`, and report what changed, the checks actually run and any limitation.

## Maintaining these instructions

Keep this file to knowledge useful in almost every session, one authoritative source per rule, module details behind conditional links and machine-local preferences in personal configuration. When changing routing, check links and representative tasks.
