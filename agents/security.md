# Security

Load when touching authentication, authorization, secrets, provider APIs, external I/O, LLM cost, tool execution, child processes, MCP, sandboxing, file paths, memory, retrieval, remote transports, or rendering untrusted content, and for security audits.

Plans, audits, and findings live under `docs/security/`. The [incident research](../docs/research/harness-security-lessons-and-controls.md) keeps the evidence behind these controls; this file is the concise working contract.

## Trust bootstrap

Review startup ordering and untrusted-input handling against [Workspace admission and authority](../docs/specs/session.md#workspace-admission-and-authority), [private-directory admission](../docs/specs/credentials.md#account-scope-and-identity) and [Tool path admission](../docs/specs/tools.md#folder-trust-and-approvals). Trace the opened identity through use; a path string or UI label is not evidence of admission. Check each platform's link, mount, case, Unicode and replacement behavior at the owning contract.

## Effectful execution gate

Before shipping an effectful path, require an adversarial conformance suite for the [effect-admission contract](../docs/specs/tools.md#authority-and-effect-admission). Trace intent normalization, deterministic authorization, approval, effective-capability attestation, dispatch, cancellation and cleanup. Review [child authority](../docs/specs/execution.md#collaboration) and [Provider egress](../docs/specs/provider.md#configuration-and-egress) at their own boundaries.

Prefer closed typed operations and explicit argument vectors. Review option-bearing arguments, `--` where supported, pinned working directories, inherited descriptors and interpreter policy; never interpolate model text into a shell or treat shell parsing as enforcement.

## Local services, MCP, and extensions

Review the [extension and transport prerequisites](../docs/specs/tools.md#extension-and-transport-admission) before adding a listener, remote MCP, plugin or update route. The [OAuth callback](../docs/specs/chatgpt.md#authorization-transaction) and [inert rendering](../docs/specs/terminal.md#public-limits-and-compatibility) have separate owners. Review new versions for permission differences and dependencies before accepting them.

## Universal rules

- Validate typed payloads and declared size, depth, count and nesting bounds before allocation-heavy work; verify rejection at the owning boundary.
- Review authentication, authorization and disclosure separately. Trace secret handles and the minimum child environment against [credentials](../docs/specs/credentials.md#limits-and-record-admission) and [Provider egress](../docs/specs/provider.md#configuration-and-egress).
- Check sensitive-data canaries across [diagnostics](../docs/specs/diagnostics.md), [telemetry](../docs/specs/telemetry.md#endpoint-and-content), errors and output; never include secrets or private payloads in commits or audit artifacts.
- Check each outbound path's timeout, retry count, backoff and jitter against its contract; require idempotency before adding retries. Inspect every queue, buffer, decompressor, output and concurrency bound and its overflow path.
- Verify effective platform capabilities rather than accepting a sandboxed boolean; process management alone does not establish isolation. Exercise authentication, signature, schema, policy and attestation failure paths.
- Least privilege: every grant, mount, environment value, sandbox profile rule, syscall allowance, process-exec permission and inserted library is the narrowest the named consumer needs, bound to the exact executable, path or Run that needs it, and absent when that consumer is not selected. Widening an existing deny list or profile names the consumer, lands in the owning spec and keeps a test that pins the narrower remainder.
- Review history and artifacts for explicit retention, export, deletion and encryption policies and verify that derived indexes and summaries can be rebuilt. [Session](../docs/specs/session.md#context-and-compaction) owns context and future retrieval scope.
- Supply chain and unsafe code: dependencies, build scripts, procedural macros, native libraries, development Skills, installers, and update channels are code-execution authority. Commit `Cargo.lock`, minimize features, pin immutable inputs, and review advisories, licenses, `build.rs`, procedural macros, native and bundled C code, and every `unsafe`, FFI, and deserialization boundary before accepting or updating. A native FFI change is reviewed for ABI match with the installed SDK and libc, exact initialized output sizes, bounded buffers, allocation, alignment, ownership and single free, C-string lifetimes, variadic argument types, signal state and unreaped PID lifetime, and never parses or prints arbitrary native diagnostics; a change to the native ACL adapter reruns the native ACL corpus. Accept no unpinned Git dependency in a release. [Extension admission](../docs/specs/tools.md#extension-and-transport-admission) owns runtime-update constraints.

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
