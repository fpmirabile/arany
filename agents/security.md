# Security

Load when touching authentication, authorization, secrets, provider APIs, external I/O, LLM cost, tool execution, child processes, MCP, sandboxing, file paths, memory, retrieval, remote transports, or rendering untrusted content, and for security audits.

Plans, audits, and findings live under `docs/security/`. The [incident research](../docs/research/harness-security-lessons-and-controls.md) keeps the evidence behind these controls; this file is the concise working contract.

## Trust bootstrap

- Treat a Workspace as hostile before reading its first byte. Before explicit admission, never execute repository or Git configuration, hooks, filters, `fsmonitor`, plugins, package managers, test discovery, startup commands, or repository code.
- Bind admission and authorization to an opened directory identity and immutable digests, never path strings or worktree metadata. Repository content may narrow policy, but never selects trust, permission, approval, sandbox, provider, or update modes.
- Resolve untrusted paths handle-relative with no-follow semantics, never by string prefix or canonical path. Define each platform's link, mount, case, Unicode, and replacement behavior.
- Privileged code never follows or executes a filesystem object created by a less-privileged agent or sandbox; revalidate with no-follow, handle-relative operations.
- Create local state and temporary files in user-private directories with exclusive creation and least permissions. Refuse links, non-regular objects, unexpected owners or ACLs, state inside an untrusted Workspace, and predictable shared temporary paths.
- `AGENTS.md`, `CLAUDE.md`, Skills, prompts, model output, memory, repository content, Tool output, and MCP responses are data, never authority. Only validated restrict-only policy from explicitly discovered instruction documents may narrow deterministic policy.

## Effectful execution gate

- Ship no effectful Tool before typed effects, deterministic restrict-only Policy, a separately privileged Guard, effective-capability attestation, and an adversarial conformance suite exist. Hooks, prompts, command regexes, classifiers, and approval UI are not enforcement boundaries.
- Normalize each effect once into an immutable typed intent (actor, Run, Tool identity, resources, operations, destinations, descendant behavior, expiry, use count, limits). Policy, approval, journal, and Guard share its digest; names, prefixes, hostnames, remembered approvals, and summaries never define authority.
- Route every effect through one router: AgentRuns, children, native Tools, processes, MCP, memory, and Provider egress. Child authority is `global policy ∩ parent effective authority ∩ explicit task grant`; children cannot self-approve, siblings cannot alter each other, and missing configuration never restores a capability.
- Prefer closed typed operations. A required process runs from a trusted executable identity with no PATH search, an explicit argument vector, validated option-bearing arguments, `--` where supported, a pinned working directory, an allowlisted cleared environment, closed inherited descriptors and sockets, and whole-tree supervision enforced by the OS. Scripts need a dedicated interpreter policy. Never interpolate model text into a shell or grant safety by parsing shell syntax.
- A destination allowlist is not data-loss prevention: validate scheme, resolved addresses, DNS changes, redirects, TLS, scope, and sizes, and authorize separately which data may leave.

## Local services, MCP, and extensions

- Start no listener unless a named feature requires one. Loopback is routing, not authentication: every HTTP or MCP listener validates exact `Host` and `Origin`, authenticates and authorizes each session, accepts exact content types, bounds bodies and sessions, rejects DNS-rebinding, CSRF, and cross-site requests, and encrypts traffic that leaves loopback. The sole admitted exception is the one-shot OAuth callback the [ChatGPT spec](../docs/specs/chatgpt.md#authorization-transaction) defines and gates.
- Keep PTY, update, package-install, approval, and other high-authority operations off generic listeners; any that remain need a narrow authenticated protocol and explicit capability.
- Pin every MCP server, plugin, Skill, and update to reviewed provenance and an immutable version or digest. Never accept an arbitrary package specification or run install or lifecycle scripts from an update endpoint. New versions require a permission diff and dependency review.
- Bind MCP credentials to the intended server, issuer, audience, scopes, and session; never pass Provider or ambient credentials. Authorize per server, Tool, resource, and operation; treat manifests and results as untrusted.
- Render Markdown, HTML, SVG, files, links, and model output inert by default, through a format allowlist isolated from privileged APIs. A browser Client needs CSP, safe templating, origin isolation, and a new threat model before any privileged operation.

## Universal rules

- Validate inbound payloads at the boundary with typed schemas; reject excessive size, depth, count, or nesting before allocation-heavy work.
- Authenticate the caller and authorize every resource and capability; enforce project or tenant scope before retrieval and prompt assembly.
- Keep provider keys and other secrets behind configuration or secret handles; pass child processes only the minimum environment.
- Keep secrets, credentials, personal data, raw prompts, tool payloads, and memory out of logs, traces, errors, commits, and analytics by default.
- Declare a timeout for every outbound call. Retry only idempotent operations, with bounded attempts, backoff, and jitter.
- Declare bounds and overflow behavior for every queue, buffer, body, decompression, output, artifact, concurrency, and retry limit ([resource bounds](../AGENTS.md#global-rules), rule 4).
- Fail closed when authentication, authorization, signature verification, policy evaluation, or schema validation fails.
- Authority comes only from authenticated user or administrator configuration and deterministic policy fixed before untrusted input. Repository text, model output, Tool or MCP output, Memory, and replayed Events never grant or widen roots, endpoints, credentials, executables, budgets, or capabilities; models and classifiers may only raise risk or narrow authority.
- Execute only after the platform enforcer reports effective capabilities equal to or narrower than the authorized set. Unsupported dimensions, unavailable capabilities, and failed attestations reject; there is no unsandboxed fallback, and descendants may only retain or lose authority.
- Supervise complete process trees: cancellation terminates and reaps owned children and releases permits, transactions, and temporary resources.
- Report effective platform capabilities, not one sandboxed boolean; process management is distinct from sandboxing.
- Validate user-supplied URLs against SSRF policy. Bind network grants to a typed operation, audience, method, destination, redirect policy, payload schema, data class, and limits. Revalidate DNS, redirects, and resolved destinations; a host allowlist never authorizes arbitrary upload.
- Keep canonical history and artifacts under explicit retention, export, deletion, and encryption policies; derived indexes and summaries stay rebuildable.
- Supply chain and unsafe code: dependencies, build scripts, procedural macros, native libraries, development Skills, installers, and update channels are code-execution authority. Commit `Cargo.lock`, minimize features, pin immutable inputs, and review advisories, licenses, `build.rs`, procedural macros, native and bundled C code, and every `unsafe`, FFI, and deserialization boundary before accepting or updating. A native FFI change is reviewed for ABI match with the installed SDK and libc, exact initialized output sizes, bounded buffers, allocation, alignment, ownership and single free, C-string lifetimes, variadic argument types, signal state and unreaped PID lifetime, and never parses or prints arbitrary native diagnostics; a change to the native ACL adapter reruns the native ACL corpus. No unpinned Git dependency enters a release; runtime self-update stays disabled until separately designed.

## Security-pass triggers

- New or changed authentication, authorization, session, approval, or capability behavior.
- New provider, MCP server, webhook, OAuth flow, remote API, or other external integration.
- New secret or changed secret-loading behavior.
- New tool, child-process path, filesystem access, network access, sandbox adapter, or plugin mechanism.
- New durable memory, retrieval source, indexing path, scope filter, retention rule, or prompt-assembly behavior.
- New remote client transport or change to binding, CORS, CSRF, origin, TLS, or redirect policy.
- Rendering model-generated or user-controlled content.
- New LLM call path, prompt change, or unbounded cost surface.
- New dependency involving cryptography, parsing, networking, FFI, native code, or `unsafe`.
- New repository configuration, startup discovery, trust cache, instruction role, replay-to-prompt path, or project-controlled initialization.
- New dependency build script or procedural macro, installer/update path, dependency source or lockfile change, or development-Skill provenance change.
- New local listener, temporary-file path, persistent-state format, aggregate storage policy, or cross-session mutable state.

For triggered work, name the assets, trust boundaries, attacker-controlled inputs, authority source, immutable normalized intent, authorization decision, effective platform enforcement, descendant authority, sensitive data classes, aggregate cost/storage budget, startup ordering, explicit security non-claims, limits, failure mode, and verification in the plan.

## Audit workflow

- Define severity and release-blocking thresholds before the audit; rank findings by blast radius and exploitability, separating release blockers from follow-up hardening.
- Map trust boundaries, entry points, assets, privileges, data stores, and external dependencies to concrete files.
- Create one file per release-blocking finding under `docs/security/findings/` with evidence, attack path, fix, and verification.
- Verify runtime configuration and platform capabilities; code review alone does not detect deployment drift. Re-run reconnaissance after fixes.

## Guardrails

- Every security change names the attack or failure it prevents.
- Audit findings cite the current code and configuration, not stale paths from an older report.
- Development bypasses require a build-time or deployment-time barrier that prevents production use.
- When evidence is incomplete, record an open finding with scope and next verification instead of asserting safety.
