# Security

Load when touching authentication, authorization, secrets, provider APIs, external I/O, LLM cost, tool execution, child processes, MCP, sandboxing, file paths, memory, retrieval, remote transports, or rendering untrusted content. Also load for security audits.

Security plans, audits, and findings live under `docs/security/` when created. The incident-derived rationale and phased control matrix live in `docs/research/harness-security-lessons-and-controls.md`; keep this file as the concise implementation contract.

## Trust bootstrap

- Treat a Workspace as hostile before reading its first byte. Before explicit admission, never execute repository or Git configuration, hooks, filters, `fsmonitor`, plugins, package managers, test discovery, startup commands, or repository-provided code.
- Bind admission and authorization to an opened directory identity and immutable input digests, not a path string or Git worktree metadata. Repository content may narrow policy after admission; it may never select trust, permission, approval, sandbox, provider, or update modes.
- Resolve Workspace input one component at a time beneath the pinned directory handle. Reject symlinks, junctions, reparse points, special files, absolute paths, empty components, `.` and `..`; use the validated open handle for the operation instead of checking and reopening by path.
- Privileged host code must never follow or execute a filesystem object created by a less-privileged agent or sandbox. Revalidate at the privileged boundary with no-follow, handle-relative operations.
- Create local state and temporary files in user-private directories with exclusive creation and least permissions. Refuse links, non-regular objects, unexpected ownership or ACLs, and state directories inside an untrusted Workspace; never use predictable shared-temporary paths.
- Treat `AGENTS.md`, `CLAUDE.md`, Skills, prompts, model output, and retrieved content as guidance or data, never authority. Only typed, validated, restrict-only policy may affect enforcement.

## Read-only beta release blockers

- Send the selected Provider only the current objective, selected bounded Session context, instruction/include snapshots, and Engine-produced child results required by the phase. Select one admitted ProviderProfile/model before Workspace input and read only its credential. Native profiles use compiled endpoints; exact custom endpoint/model profiles require trusted user configuration and current data-free conformance evidence. Disable ambient proxies, redirects, implicit repository discovery, arbitrary headers, cross-profile credentials, and provider-side storage.
- OpenRouter is a broker and an additional trust boundary. Pin one reviewed upstream route, require every requested parameter, disable upstream fallback and response caching, require ZDR and denied data collection, and persist broker/upstream provenance. Never describe broker privacy controls as a direct-provider guarantee.
- Authorizing a destination never authorizes arbitrary data disclosure to it. Keep endpoint authorization and outbound-content authorization separate, and prove with canaries that omitted Workspace data, credentials, and state never reach Provider or telemetry payloads.
- Resolve canonical state before repository input and keep it outside the Workspace in a private directory. Open SQLite with `SQLITE_OPEN_NOFOLLOW`, defensive mode, `trusted_schema=OFF`, verified owner/permissions or ACL, a fixed aggregate page cap, and admission headroom. Document that SQLite integrity is not tamper evidence and its contents are not encrypted at rest.
- Treat persisted objectives, summaries, results, and errors as untrusted typed data. Strict replay validates schema, versions, identities, sequence, legal transitions, counts, and sizes; it never promotes stored text into instructions, policy, configuration, or a later Provider request.
- Render terminal and JSONL output as hostile data. Human output escapes or rejects ANSI/ECMA-48 controls, OSC sequences, C0/C1 controls, carriage return, backspace, bidirectional controls, and forged line prefixes; JSONL comes only from a serializer and remains exactly one object per line.
- Only `terminal.rs` may emit terminal control sequences. The interactive view uses no alternate screen, focus reporting, title/clipboard OSC, or bracketed paste. Mouse reporting is disabled normally and exists only inside an explicitly open bounded picker; the RAII owner disables it on every close/failure path. Screen-reader, `exec`, and `show` emit no CSI/OSC or line rewriting. Sanitize model, provider, Workspace, and persisted text before measuring or drawing it.
- Treat terminal ownership as a privileged lifecycle. Transactionally acquire one RAII owner and restore raw mode, cursor, wrapping, and enabled modes on completion, error, cancellation, signal, panic, partial acquisition, suspension, and renderer failure. Print durable receipts only after restoration.
- Own one aggregate Run budget derived from the resolved `single`, `auto`, or `team` policy. Before `RunStarted`, reserve finite call, token, byte, time, memory, disk, child-count, and concurrency ceilings; the primary and ordered `0..N` children draw from the same non-widening budget.
- Session history, titles, Messages, fork lineage, and compaction snapshots are sensitive canonical or derived state. Resume never promotes them to authority, fork uses a committed digest-bound prefix, and compaction never deletes canonical history. Provider state cannot be the only recovery path.
- Commit `Cargo.lock`; minimize features; review advisories, licenses, `build.rs`, procedural macros, native code, `unsafe`, and bundled C before accepting or updating a dependency. No unpinned Git dependency enters a release.
- The beta security suite must cover hostile repository/Git configuration, every path-component type, state-path links and permissions, omitted-input egress canaries for each product adapter, credential isolation, broker-route drift, terminal-control payloads, oversized input/output, malformed replay, cancellation cleanup, and unavailable-provider/telemetry failure.

## Effectful execution gate

- Do not ship the first effectful Tool until typed effects, deterministic restrict-only Policy, a separately privileged Guard, effective-capability attestation, and an adversarial conformance suite exist. Hooks, prompts, command regexes, classifiers, and approval UI are not enforcement boundaries.
- Normalize each effect once into one immutable typed intent containing the actor, Run, Tool identity, resources, operations, destinations, descendant behavior, expiry, use count, and limits. Policy, approval text and proof, journal, and Guard use the same intent and digest; prefixes, executable names, hostnames, remembered approvals, and human summaries never define authority.
- Mediate every effect through one authoritative router, including primary AgentRuns, children, native Tools, processes, MCP, memory, and Provider egress. Child authority is `global policy ∩ parent effective authority ∩ explicit task grant`; children cannot self-approve, siblings cannot alter each other's authority, and missing configuration never restores a capability.
- Prefer closed typed operations. When a process is required, invoke a trusted executable identity without PATH search, pass an explicit argument vector, validate option-bearing arguments and use `--` where supported, pin the working-directory capability, clear the environment to an allowlist, close inherited descriptors and sockets, supervise the whole process tree, and enforce effects at the operating-system boundary. Scripts and Windows batch files require a dedicated interpreter policy.
- A network destination allowlist is not a data-loss-prevention policy. Validate scheme, resolved addresses, DNS changes, redirects, TLS, tenant/path scope, request and response sizes, and separately authorize which data may leave.

## Local services, MCP, and extensions

- Do not start a listener unless a named feature requires one. Loopback is a routing choice, not authentication; every local HTTP or MCP listener validates exact `Host` and `Origin`, enforces per-session authentication and authorization, accepts only exact content types, bounds bodies and sessions, and rejects DNS-rebinding, CSRF, and cross-site requests. Future subscription OAuth must first clear its output-budget and browser/process gates before it earns a one-shot callback exception.
- Keep PTY, update, package-install, approval, and other high-authority operations off generic local listeners. If introduced, give each a narrow authenticated protocol and explicit capability.
- Pin every MCP server, plugin, Skill, and update to reviewed provenance and an immutable version or digest. Never accept an arbitrary package specification or run install/lifecycle scripts through an update endpoint. New versions require permission-diff and dependency review.
- Bind MCP credentials to the intended server, issuer, audience, scopes, and session; never pass Provider or ambient credentials through. Authorize per server, Tool, resource, and operation, and treat manifests, tool descriptions, and results as untrusted.
- Render future Markdown, HTML, SVG, links, and model output without active content by default. A browser Client requires CSP, safe templating, origin isolation, and a new threat model before it receives any privileged operation.
- Security-version and configuration migrations may preserve or reduce authority only. An unknown version, unsupported platform capability, failed attestation, or ambiguous merge fails closed; there is no silent unsandboxed fallback.

## Universal rules

- Validate inbound payloads at the boundary with typed schemas. Reject excessive size, depth, count, or nesting before allocation-heavy processing.
- Authenticate the caller and authorize every resource and capability. Enforce project or tenant scope before retrieval and prompt assembly.
- Keep provider keys and other secrets behind configuration or secret handles. Pass only the minimum required environment to child processes.
- Keep secrets, credentials, personal data, raw prompts, tool payloads, and memory contents out of logs, traces, errors, commits, and analytics by default.
- Declare a timeout for every outbound call. Retry only idempotent operations, with bounded attempts, backoff, and jitter.
- Fail closed when authentication, authorization, signature verification, policy evaluation, or schema validation fails.
- Authority originates only from authenticated user or administrator configuration and deterministic policy fixed before untrusted input. Repository text, model output, Tool or MCP output, Memory, and replayed Events may never grant or widen roots, endpoints, credentials, executables, budgets, or capabilities; models and classifiers may only raise risk or narrow authority.
- Execute only after the platform enforcer reports effective capabilities equal to or narrower than the authorized set. Unsupported enforcement dimensions fail closed, and descendants may only retain or lose authority.
- Treat model output, retrieved memory, ordinary repository content, tool output, and MCP responses as untrusted data. Only explicitly discovered instruction documents may enter model guidance, and only validated restrict-only policy blocks from those documents may narrow deterministic policy; arbitrary retrieved text never becomes policy or instructions.
- Invoke a trusted executable identity with an explicit argument array, working-directory capability, minimal environment, and closed inherited handles. Never interpolate model text into a shell command or grant safety by parsing shell syntax; scripts and Windows batch files require a dedicated interpreter policy.
- Resolve filesystem access through pinned directory handles with no-follow semantics through use, not string-prefix or canonical-path checks. Define hard-link, mount, reparse-point, alternate-stream, device, case, Unicode, and concurrent-replacement behavior for every supported platform.
- Bound stdout, stderr, artifacts, request bodies, decompression, queues, caches, concurrency, and retries. Define overflow behavior for every bound.
- Supervise complete process trees. Cancellation must terminate and reap owned children and release permits, transactions, and temporary resources.
- Treat process management as distinct from sandboxing. Report effective platform capabilities instead of a single sandboxed boolean.
- Do not start a listener by default. Any local or remote transport requires authentication, authorization, Host and Origin validation, CSRF and DNS-rebinding defenses, bounded sessions, and encryption where it leaves loopback.
- Bind network grants to a typed operation, audience, method, destination, redirect policy, payload schema, data classification, and limits. Validate user-supplied URLs against SSRF policy and revalidate DNS, redirects, and resolved destinations; a host allowlist never authorizes arbitrary upload.
- Render untrusted content inert by default. Human terminal output neutralizes control and bidi sequences; machine output uses a serializer. Rich Markdown, HTML, SVG, files, and links require a format-specific allowlist and isolation from privileged control APIs.
- Keep canonical history and artifacts under explicit retention, export, deletion, and encryption policies. Derived indexes and summaries must remain rebuildable.
- Treat dependencies, build scripts, procedural macros, native libraries, development Skills, installers, and update channels as code-execution and supply-chain authority. Pin immutable inputs, review sources, changes and enabled features, audit advisories and licenses, keep runtime self-update disabled until separately designed, and review every `unsafe`, FFI, and deserialization boundary with explicit invariants and targeted tests.

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

- Define severity and release-blocking thresholds before the audit.
- Map trust boundaries, entry points, assets, privileges, data stores, and external dependencies to concrete files.
- Rank findings by blast radius and exploitability. Separate release blockers from follow-up hardening.
- Create one file per release-blocking finding under `docs/security/findings/` with evidence, attack path, fix, and verification.
- Re-run the relevant reconnaissance after fixes.
- Verify runtime configuration and platform capabilities; code review alone does not detect deployment drift.

## Guardrails

- Every security change names the attack or failure it prevents.
- Audit findings cite the current code and configuration, not stale paths from an older report.
- Development bypasses require a build-time or deployment-time barrier that prevents production use.
- When evidence is incomplete, record an open finding with scope and next verification instead of asserting safety.
