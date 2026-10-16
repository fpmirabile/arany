# Arany

Arany is a Rust-first, CLI-only harness for multi-agent coding teams that can be trusted to work autonomously. The product and executable are named Arany and `arany`; the repository directory name is not part of the contract. This file guides agents developing Arany; the [documentation map](./docs/README.md) locates contracts and owners.

## Non-negotiable guarantees

Multi-agent coordination builds on five guarantees. Never weaken them for speed, convenience or parity with another harness; a guarantee costing more than a peer's unguarded path is a defect to fix, not a price to accept:

- **Journal-first durability.** Every Provider call, Tool intent, effect, approval verdict and team transition (spawn, lease, handover, integration) is a committed Event before the next step; the reducer validates each transition; strict replay never re-executes.
- **Honest uncertainty.** An effect with an unknown outcome is Uncertain: it stops its continuation, is reported unknown and never retries itself.
- **Per-effect isolation.** Every mutation, process, Skill script and MCP call runs in the Guard under a pinned, typed grant and publishes only through validated changesets with receipts. Read-only operations run under the same admitted directory capability and bounds, in-process or in the Guard, and never widen disclosure. Repository content and model text never select trust; unsupported enforcement fails closed. The only exception: a per-command run outside the Guard that the user approves after a sandbox-caused failure, never automatic, journaled and marked unguarded. Other guarantees still apply.
- **Durable, budgeted Goals.** Budgets, pause, steering, cancellation and recovery survive exit and crashes; exhaustion closes the Goal, not the guarantee.
- **Safe AI cage (hostile-input neutralization).** Untrusted repositories, files, Tool outputs, MCP metadata and Skill text are data, never instructions or executable configuration; Git runs only on a neutralized private view. Agents can only narrow authority, never widen it: children never exceed the parent's grant, and hooks and reviews only narrow.

If a guarantee blocks work, record it and raise it with the owner; never weaken it silently.

Everything else (agent loop, wire formats, defaults, permission modes, hooks, agent definitions, context strategy, UX, embedding surfaces) follows proven harness designs (priority 4); diverge only when a guarantee requires it.

## Product and implementation priorities

1. **Build the harness.** Arany owns agent execution, context, Tools, permissions and durable Sessions. Keep product policy in the Engine and platform, Provider and presentation details at their existing boundaries, and evaluate changes against the complete task lifecycle, including failure, cancellation and recovery. The [system overview](./docs/architecture/system-overview.md) owns the architecture.
2. **Design for Linux and macOS parity.** Arany targets Linux and macOS with equivalent behavior. Native Windows is out of scope (use the Linux build under WSL2). Consider platform impact on both targets; for OS facilities prefer standard/library capabilities with equivalent guarantees, then narrow native adapters. Assume no shell, path layout, desktop, distribution or service manager. Document unavoidable differences in the owning contract; unsupported security enforcement fails closed. A Linux-first milestone is delivery order, not the product boundary. Native evidence is required before claiming support; current gaps belong in [NEXT_STEPS.md](./NEXT_STEPS.md).
3. **Apply Ponytail on every implementation.** Read [Ponytail](./.agents/skills/ponytail/SKILL.md) before implementing, refactoring or reviewing code (install via [project skill setup](./agents/skills/README.md#install-for-a-harness) if missing). Prefer existing code, the standard library, OS facilities and installed dependencies; add only complexity the task or a demonstrated constraint requires. Never cut scope, correctness, security, accessibility, error handling, resource bounds or verification to save lines; repository rules win over the skill's shortcuts. Also apply [rust-clean-code](./agents/skills/rust-clean-code/SKILL.md) and [clean-architecture-review](./agents/skills/clean-architecture-review/SKILL.md) to every implementation and review.
4. **Prefer proven harness designs.** When established harnesses (Claude Code, Codex CLI, OpenCode, Goose, Aider) share a working answer for a behavior, default or wire format, replicate it; diverge only for a documented Arany constraint (security, durability, strict replay), naming the peer behavior and reason in the owning contract.

## Rules to load before editing

**Documentation ownership.** `agents/` holds general working practice only. `docs/specs/<domain>.md` is the only home of behavior, decisions, limits and scenarios; load a spec only when the task changes or verifies that behavior. Implementation invariants live in the tests that pin them. State each fact in one home and link elsewhere. A split-out crate gets its own `agents/<unit>.md`; a module does not.

| Task or boundary | Read |
|---|---|
| Git operations, including planned steps | [Git](./agents/git.md) |
| Auth, secrets, I/O, APIs, Tools, processes, MCP, memory, transports, untrusted rendering, audits | [Security](./agents/security.md) |
| Behavior, fixes, tests, correctness or release claims, deferred evidence, handoff | [Testing](./agents/testing.md) |
| Native, PTY, tmux, account, storage or process fixtures; CI reproduction | [Fixture procedures](./agents/references/testing-details.md) |
| Documentation, instructions, development skills | [Documentation checks](./agents/testing.md#documentation-and-instruction-changes), [Project skills](./agents/skills/README.md) |
| Architecture, data model, new seams; plans | [System overview](./docs/architecture/system-overview.md), [Planning](./planning/PLANNING.md) |
| Behavior, scenarios, spec ownership | [Spec workflow](./docs/specs/README.md) and the owning spec below |
| Providers, models, custom endpoints, streaming, usage | [Provider spec](./docs/specs/provider.md) |
| ChatGPT-plan OAuth, consent, tokens, catalogs | [ChatGPT spec](./docs/specs/chatgpt.md) |
| Saved accounts, keyrings, credential helper | [Credential spec](./docs/specs/credentials.md) |
| Sessions, resume, fork, compaction, context | [Session spec](./docs/specs/session.md) |
| Goals, budgets, steering, `exec` | [Goals spec](./docs/specs/goals.md) |
| Runs, outcomes, Events, replay, persistence | [Execution spec](./docs/specs/execution.md) |
| Tools, commands, Skills, MCP, Guard, folder trust, toolchains, build cache | [Tool spec](./docs/specs/tools.md) |
| Terminal, output, accessibility, history, clipboard, images, CLI modes | [Terminal spec](./docs/specs/terminal.md) |
| OTLP telemetry; development logs and failure stages | [Telemetry](./docs/specs/telemetry.md), [Diagnostics](./docs/specs/diagnostics.md) |

Open only the linked files whose triggers apply; they are not automatic imports. Use the [documentation map](./docs/README.md#find-the-owner-for-a-task) when ownership is unclear; [development setup and checks](./docs/development.md) owns shared commands.

## Global rules

1. **English only**: all repository content, including code, comments, documentation, configuration, user-facing copy, commit messages and public API names.
2. **Minimal comments**: default to none; add short `//` lines only for a non-obvious reason beside its code. No `//!` or multi-line doc blocks in internal code: behavior belongs in specs, implementation invariants in tests. Architectural reasoning belongs in design documents or ADRs.
3. **Deep modules**: keep interfaces small and hide complexity inside the implementation. Prefer in-process calls. Add a seam only with two adapters or when privilege, process, release or ownership isolation requires it. No speculative frameworks, traits or configuration.
4. **Explicit resource bounds**: queues, buffers, caches, request bodies, decompression, stdout/stderr, tool output, artifacts, concurrency, and retry counts have declared limits and overflow behavior. Goal usage follows its pinned settings ([defaults](./docs/specs/goals.md#lifecycle-budgets-and-recovery)); this never removes finite per-action, storage, request, Tool and platform bounds.
5. **No historical residue**: remove superseded code cleanly. During pre-beta, keep one current internal format without compatibility readers, automatic migrations or relocation fallbacks. Reject unsupported stored formats without changing their data; external protocol support stays with its owning contract. Documentation describes the current system; git history records the old one.
6. **Never inspect or print `.env`**: read `.env.example` only. Commands may load secrets without echoing them or dumping the process environment.
7. **Unsafe Rust is exceptional**: keep `unsafe` isolated behind a safe interface, document its invariants, and require a dedicated review and test strategy.

## Auto-learning

Record a found invariant, failure or ownership decision once: behavior, limits and decisions in the owning spec ([spec workflow](./docs/specs/README.md)); an implementation invariant as a test, or a non-obvious reason as one `//` line beside its code; a working practice in its general rule. Fix misleading guidance instead of duplicating it; add no task transcripts, check logs or facts readable from code.

## Module boundaries

Read the [system overview](./docs/architecture/system-overview.md#2-runtime-architecture) and the relevant module rules before changing a boundary; the overview owns package and process shape, dependency direction and state ownership.

Create a module only once it has depth; a crate needs measured dependency, release, privilege, ownership or compile pressure. Add a daemon only for a second Client or detached execution. Effectful Tools require deterministic Policy and the separate Guard.

## Workflow

1. **Read, then plan only when the change is big.** Read the triggered rules and affected flow; for behavior work, the owning contract and its scenarios. For multi-module or public-interface changes, plan first: scope, Ponytail choice, OS impact or none, triggered [security pass](./agents/security.md#security-pass-triggers) and verification of success and meaningful failure paths through existing evidence owners. Ask before deleting files, renaming public APIs, restructuring modules or changing CI/shared infrastructure unless the request authorizes it; preserve user work ([Git rules](./agents/git.md)).
2. **Verify by target locally, fully in CI.** Run formatting, Clippy and the tests covering what you touched ([verification](./docs/development.md#verification)); CI runs the full workspace suite on every pull request. After pushing, watch CI; stacked PRs are fine ([Git rules](./agents/git.md#pull-requests)). A red CI blocks handoff; if the failure is demonstrably unrelated or infrastructural, say so with the run link and ask the user. Until a stable pre-beta, synthetic tests plus green CI are sufficient evidence; live, native and manual checklists stay in [NEXT_STEPS.md](./NEXT_STEPS.md), never block day-to-day work, and are the only basis for claiming native or live support.
3. **Touch documentation once, and only when behavior changed.** If observable behavior, a limit or a recovery path changed, update the owning spec ([spec workflow](./docs/specs/README.md)); a refactor, a fix restoring specified behavior or an adequate contract needs no edit. Review the final diff with Ponytail, keep local visual artifacts in gitignored `.lavish/`, and report changes, checks run, limitations and platform evidence.

## Maintaining these instructions

Keep this file to knowledge useful in almost every session: one source per rule, details behind conditional links, machine-local preferences in personal configuration. When changing routing, check links and representative tasks.
