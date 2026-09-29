# Harness security lessons and controls

> **Session/team/custom-profile amendment — 2026-09-29:** The beta security boundary now includes durable Session history, one primary plus ordered aggregate-budget-bounded `0..N` direct children, and exact trusted custom endpoint/model profiles admitted only after data-free conformance. Replace every fixed two-child/four-call bound below with the immutable per-Run `single|auto|team` policy, the eight-child process ceiling, `1` or `N + 2` call reservation, and origin-bound profile evidence. Native scrollback remains; mouse reporting is transient picker-only. OTLP ships last in beta but remains runtime-opt-in. The incident-derived principles, no-effect claim, startup ordering, state hardening, content authorization, inert output, supply-chain gates, and future Guard requirements remain authoritative.

**Status:** implementation input  
**Date:** 2026-09-29  
**Scope:** the Rust-first, CLI-only minimum demonstrator and the gates that must precede effectful Tools, MCP, durable Memory, plugins, remote clients, or additional providers

## Executive decision

The minimum demonstrator can make a small, defensible security claim because it has no Tool execution, child process, MCP client or server, runtime plugin, cross-Session Memory, workspace write, or remote listener. It has one user-selected read-only Workspace, native exact HTTPS origins or one trusted conformance-gated exact custom profile, one private SQLite journal, one accountable primary, and bounded flat direct children. Preserve that narrow claim.

Rust helps with memory safety and precise types, but it does not solve the failures that repeatedly affected coding agents. The recurring failures were authority-selection bugs, trusted configuration loaded before workspace trust, shell syntax misclassification, symlink and time-of-check/time-of-use composition bugs, unauthenticated local servers, untrusted rendering reaching a privileged local API, ambient credential access, and delegated agents receiving more authority than their parent. These are architecture and enforcement failures, not language-memory failures.

The central security invariant should be:

> Untrusted content may propose data and actions, but authority originates only from authenticated user configuration and deterministic policy fixed before untrusted content is consumed. Every descendant, redirect, retry, replay, and adapter may preserve or reduce that authority, never widen it.

For V1, this means repository instructions influence model guidance but never deterministic policy; the CLI-selected root is pinned before any repository file is read; model output cannot select a root, endpoint, credential, executable, or approval scope; provider traffic is the only intentional network capability; persisted events are data, never new instructions; and terminal output has no active rendering.

Before implementation starts, the current architecture should be amended by the implementation plan or a focused ADR with the release blockers in [V1 requirements](#v1-release-blocking-requirements). Most existing security decisions are strong. The material gaps are startup ordering, immutable authority provenance, store-path hardening, replay poisoning rules, deterministic terminal escaping, aggregate disk/cost admission, dependency/build-time authority, and explicit security non-claims.

## Evidence method

This report separates evidence into three classes:

- **Incident:** a vendor or project advisory, a vendor retrospective, or a maintainer-authored security disclosure describing an observed flaw.
- **Standard:** a normative specification or security guide. It describes required practice, not proof that this harness is vulnerable.
- **Inference:** a recommendation derived from the incidents, standards, and the repository's selected architecture. Inferences are labeled where they are not direct requirements of a cited source.

GitHub issues are not treated as confirmed advisories. Where used, they are identified as issue reports and support a test case, not a vulnerability claim.

## Security claim and non-claim for V1

### Claimed after all gates pass

V1 may claim:

- The process reads only explicitly selected, bounded Workspace files through a pinned root capability and rejects symlinks in the selected path.
- The model cannot cause local command execution, workspace mutation, arbitrary file reads, arbitrary network access, nested delegation, or spawning more children than the pinned Run policy and beta hard ceiling permit.
- Provider requests go only to the compiled official endpoint through a client with redirects, proxies, cookies, retries, and ambient credentials disabled.
- The provider key is accepted only from the named environment variable, retained in a secret type, and excluded from canonical state and observability.
- Canonical state is a local, user-private SQLite event journal with typed, bounded events and deterministic replay.
- Human output does not interpret model-controlled ANSI, OSC, Markdown, HTML, or terminal-control sequences. JSONL is emitted only by a serializer.
- Optional V1 OTLP export is content-free, lossy, loopback-only, and incapable of affecting the run result.

These claims must be verified on every supported operating system, not inferred from one platform's behavior.

### Explicitly not claimed

V1 must not claim:

- containment of arbitrary executable code, because no effectful Tool sandbox exists yet;
- protection from a malicious provider returning misleading text, beyond preventing that text from becoming authority;
- confidentiality from the local OS account, administrators, debuggers, swap, backups, or a compromised host;
- encryption at rest for the SQLite journal;
- safe execution inside an untrusted repository if startup reads or future integrations are added outside the pinned Workspace loader;
- a multi-user or tenant isolation boundary;
- exact monetary enforcement once a provider request is in flight;
- availability against the provider, OS, disk failure, or an adversary who controls the user's account;
- a security boundary from Rust alone, `cap-std` alone, SQLite alone, or process supervision alone.

## Threat model

### Protected assets

1. The user's files outside the selected Workspace and all Workspace files not explicitly selected.
2. The integrity of the Workspace, repository, shell startup files, credentials, and host configuration.
3. `OPENAI_API_KEY` and any ambient process credentials, proxy settings, tokens, cookies, SSH agents, or inherited handles.
4. The integrity and confidentiality of canonical Events, run objectives, child summaries, final results, and provider metadata.
5. The integrity of capability decisions, instruction precedence, run/agent identity, budgets, and replayed state.
6. Provider spend, local CPU, memory, disk, descriptors, threads, sockets, and elapsed time.
7. User perception: terminal content must not forge prompts, approvals, status, links, or commands.
8. The software supply chain: Rust crates, build scripts, procedural macros, native libraries, installed development skills, release artifacts, and update paths.

### Adversaries and attacker-controlled input

- A malicious or compromised repository author controls ordinary files, root instruction Markdown, names, encodings, symlinks, Git metadata, and timing of concurrent filesystem changes.
- A prompt-injected or malicious model controls every provider output byte, including structured-looking content, URLs, paths, child objectives, summaries, terminal control characters, and claims about user intent.
- A compromised provider response path controls HTTP status, headers, compression, content length behavior, malformed structured output, and latency.
- A local unprivileged process may race path operations, create links or predictable temporary files, consume resources, probe future local ports, and observe world-readable state.
- Dependency authors and compromised registries may execute code during build, publish malicious updates, or introduce native/unsafe code.
- For future MCP, plugin, Tool, and remote-client work, a malicious server, plugin, website, OAuth client, DNS resolver, proxy, tool binary, or delegated agent is in scope.

### Trusted components

- The compiled harness binary and its reviewed dependency graph.
- The OS kernel and security mechanisms whose effective capabilities are positively attested.
- The OS-account user selecting CLI paths and configuration before the run.
- The fixed provider origin for confidentiality and integrity of the TLS connection, but not provider output as an authority source.

Repository instructions, provider responses, restored Events, OTLP collectors, future MCP peers, and model-generated plans are never authority sources.

### Trust boundaries and data flow

```text
authenticated CLI values
        |
        v
startup validation -- pins authority --> Workspace capability / fixed provider / state root
        |                                      |
        | reads bounded bytes                  | sends bounded request
        v                                      v
untrusted repository guidance -----------> Provider ----------> untrusted output
        |                                      |
        +---------------- typed data ----------+
                               |
                               v
                    deterministic reducer
                     /                  \
          canonical SQLite Events      escaped CLI / optional content-free OTLP
```

No arrow from untrusted content is allowed to change a root, credential, endpoint, policy, process, budget, instruction role, or descendant authority.

## Incident and lesson matrix

| Evidence | Observed failure | Root security mistake | Control for this harness |
| --- | --- | --- | --- |
| **Incident — OpenAI Codex, GHSA-w5fx-fh39-j5rw / CVE-2025-59532.** A model-generated working directory became the sandbox writable root, including outside the user-started folder. The fix anchored and canonicalized the boundary to the session-start path. [Advisory](https://github.com/openai/codex/security/advisories/GHSA-w5fx-fh39-j5rw) | Model data selected an enforcement root. | Confused authority provenance; validation was applied to an attacker-selected boundary. | Only the CLI may select the Workspace. Pin its identity before model/repository input. A model-generated path is a relative request under that capability, never a root. |
| **Incident — Anthropic Claude Code, GHSA-j4h9-wv2m-wrf7 / CVE-2025-59041.** Repository-controlled Git configuration reached command templating before workspace trust and enabled command execution. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-j4h9-wv2m-wrf7) | Code ran before the trust boundary existed. | Trust dialog and policy were placed after repository-dependent initialization. | Before trust/pinning, do not parse repository configuration, invoke Git, expand templates, resolve hooks, load `.env`, or start integrations. V1 should invoke none of them. |
| **Incident — Claude Code containment retrospective.** Anthropic reports repository hooks were parsed/executed before consent; an internal red-team prompt exfiltrated `~/.aws/credentials` in 24 of 25 trials when environment boundaries did not stop it; and users approved about 93% of prompts. [Vendor retrospective](https://www.anthropic.com/engineering/how-we-contain-claude) | Probabilistic defenses and frequent approvals did not protect ambient authority. | Treating user confirmation or model behavior as a security boundary. | Remove authority rather than ask repeatedly. V1 has no local execution. Future Guard must deny filesystem/network/environment capabilities independently of approvals. |
| **Incident — Claude Code, GHSA-mmgp-wc2j-qcv7 / CVE-2026-33068.** A repository `.claude/settings.json` could select `bypassPermissions` before trust and silently skip the trust flow. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-mmgp-wc2j-qcv7) | Untrusted project configuration disabled its own guard. | Policy/configuration source confusion. | Repository files cannot grant, bypass, or configure deterministic authority. Restrict-only repository policy may only intersect a higher-level grant after schema validation. |
| **Incident — Claude Code, GHSA-q5hj-mxqh-vv77 / CVE-2026-40068.** A crafted Git worktree `commondir` pointed to an already trusted repository and bypassed trust, enabling hook execution. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-q5hj-mxqh-vv77) | A related path/metadata identity was mistaken for the trusted object. | Path aliasing and trust keyed by a weak identity. | Do not infer trust from Git metadata, textual path ancestry, worktree relationships, or remembered names. Pin the opened directory object; V1 does not inspect `.git`. |
| **Incident — Claude Code, GHSA-vp62-r36r-9xqp / CVE-2026-39861.** A sandboxed process created an outward symlink that a later unsandboxed application followed, producing an arbitrary write. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-vp62-r36r-9xqp) | Two individually plausible components composed into a boundary bypass. | Validation and use occurred in different authority domains. | Use handle-relative no-follow operations through check and use. Never hand a model-controlled or sandbox-created pathname to a more privileged component. |
| **Incident — Claude Code, GHSA-7835-87q9-rgvv / CVE-2026-55607.** Git worktree behavior, external navigation, symlink manipulation, and `fsmonitor` combined to write outside a macOS sandbox. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-7835-87q9-rgvv) | Tool-specific filesystem behavior exceeded the apparent sandbox path model. | Assuming an application-level path rule represented effective OS behavior. | Later Tools require adversarial platform tests using the real executable and filesystem features. Unsupported enforcement dimensions fail closed; do not advertise a generic `sandboxed` boolean. |
| **Incident — Claude Code, GHSA-fg94-h982-f3mm / CVE-2026-54316.** An allowed `huggingface.co` host still provided attacker-controlled repository paths that formed an exfiltration channel for files, environment values, and output. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-fg94-h982-f3mm) | A destination allowlist was confused with an information-flow policy. | Egress identity alone says nothing about the sensitivity of uploaded data. | Future egress requires typed operation, method, path/template, audience, payload schema, size, and data-class controls. A host allowlist alone never permits arbitrary request content. |
| **Incident — Claude Code, GHSA-5cwg-9f6j-9jvx / CVE-2026-35603.** Windows managed settings were accepted without owner/ACL validation, letting a lower-privileged user influence privileged execution. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-5cwg-9f6j-9jvx) | A policy file was trusted because of its pathname. | No provenance or writable-by-attacker check. | Every policy/config root must be opened no-follow and checked for expected owner and permissions/ACL. If the platform cannot establish that property, ignore it or fail closed according to the documented source. |
| **Incident — Claude Code, GHSA-4vp2-6q8c-pvq2.** A predictable world-readable temporary response file enabled disclosure and symlink-based arbitrary overwrite. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-4vp2-6q8c-pvq2) | Sensitive data used a shared predictable pathname with permissive mode. | Unsafe temporary-file creation and path-based reopen. | V1 should create no response temp files. Future temporary objects must be private, atomic, no-follow, handle-owned, randomly named, lifecycle-bounded, and never reopened by pathname. |
| **Incident — OpenCode, GHSA-vxw4-wv6m-9hhh / CVE-2026-22812.** The CLI automatically started an unauthenticated local HTTP server with permissive CORS and shell, PTY, and file endpoints; websites or local processes could execute commands. [Advisory](https://github.com/anomalyco/opencode/security/advisories/GHSA-vxw4-wv6m-9hhh) | Loopback was treated as authentication. | Privileged ambient listener, unsafe browser trust, and broad local API. | V1 starts no listener. A future daemon needs explicit opt-in, per-session authentication and authorization, strict origins/CSRF, host validation, TLS where remote, bounded sessions, and no shell-shaped endpoint. |
| **Incident — OpenCode, GHSA-c83v-7274-4vgp.** Unsanitized LLM Markdown/HTML plus a configurable server URL produced script execution on the trusted local origin and then terminal-command execution. [Advisory](https://github.com/anomalyco/opencode/security/advisories/GHSA-c83v-7274-4vgp) | Untrusted presentation crossed into a privileged control origin. | Rendering and authority were composed without an isolation boundary. | V1 renders no Markdown/HTML/ANSI/OSC. Future rich rendering must use an inert renderer in an origin/process with no control capability. Never trust content because the model emitted it. |
| **Issue report — OpenCode #6527.** A plan-mode parent reportedly spawned a subagent that did not inherit edit denial and changed files. This is not a project security advisory. [Issue](https://github.com/anomalyco/opencode/issues/6527) | Delegation widened authority. | Parent policy was not monotonically inherited. | Child capability = parent capability intersect task grant intersect global policy. A child cannot request, approve, or manufacture a wider grant. Test this invariant exhaustively. |
| **Incident — GitHub Copilot CLI, GHSA-g8r9-g2v8-jv6f / CVE-2026-29783.** Bash parameter transformations, assignments, indirection, and nested substitutions hid arbitrary execution inside commands classified as read-only. [Advisory](https://github.com/advisories/GHSA-g8r9-g2v8-jv6f) | A complex language was classified with an incomplete safety parser. | Command-name/pattern allowlists were treated as effect enforcement. | Never grant safety from shell text classification. Future processes use a fixed executable identity plus argv, no shell, and OS-enforced filesystem/network/process capabilities. Shell tools, if ever added, are always fully effectful. |
| **Incident — Gemini CLI, GHSA-wpqr-6v78-jr5g / CVE-2026-12537.** Headless CI auto-trusted workspace configuration and `.env`; permissive mode bypassed a fine-grained allowlist, enabling prompt-injection-driven execution. [Advisory](https://github.com/advisories/GHSA-wpqr-6v78-jr5g) | Noninteractive mode silently changed the trust and permission contract. | Convenience flags became authority escalation. | Headless/CI must be no more permissive than interactive mode. V1 has no bypass flag, `.env` loading, or project config. Future override modes must be visibly typed capabilities and independently constrained by Guard. |
| **Issue report — Gemini CLI #11510.** A project issue reported that the shell allowlist examined only the first command in a pipeline. It is issue evidence, not an advisory. [Issue](https://github.com/google-gemini/gemini-cli/issues/11510) | Partial parsing authorized a compound command. | Approval did not bind the complete executed effect. | Normalize the entire typed effect once; policy, approval display, digest, journal, and executor consume that same immutable value. No prefix or first-command approval. |
| **Incident — MCP TypeScript SDK, GHSA-w48q-cv73-mx4w / CVE-2025-66414.** DNS-rebinding protection was off by default for unauthenticated localhost HTTP servers; malicious websites could invoke tools or access resources. Stdio was unaffected. [Advisory](https://github.com/modelcontextprotocol/typescript-sdk/security/advisories/GHSA-w48q-cv73-mx4w) | Localhost HTTP was exposed to browser-origin confusion. | Insecure default and no host validation/authentication. | Prefer supervised stdio for future local MCP. If HTTP exists, require authentication, exact origin/host validation, DNS-rebinding protection, and explicit enablement. |
| **Incident — MCP TypeScript SDK, GHSA-345p-7cg4-v4c7 / CVE-2026-25536.** Reused transport/server instances misrouted responses and notifications across concurrent clients because request IDs collided and mutable transport state was shared. [Advisory](https://github.com/modelcontextprotocol/typescript-sdk/security/advisories/GHSA-345p-7cg4-v4c7) | Cross-principal mutable session state was reused. | Session identity and transport lifetime were not isolated. | One authenticated principal/session owns one protocol/transport state machine. Never use a JSON-RPC request ID as an authorization identity. Close and erase state on session end. |
| **Incident — official Gemini CLI discussion #8385.** Google reported Gemini CLI itself was not compromised, while installs during an npm ecosystem attack could have fetched a compromised transitive dependency. [Maintainer discussion](https://github.com/google-gemini/gemini-cli/discussions/8385) | Correct first-party code did not eliminate dependency-install risk. | Build/install code and transitive packages are part of the trusted computing base. | Commit `Cargo.lock`; review lock and feature changes; audit advisories; inventory build scripts, proc macros, native code, and unsafe; pin release inputs; do not run runtime self-update. |

The matrix supports three broad conclusions:

1. **Authority provenance matters more than intent classification.** The most damaging bugs let model output, repository configuration, path aliases, or convenience modes influence the enforcement boundary.
2. **Security properties must survive composition.** A sandbox plus an unsandboxed helper, an allowlisted host plus arbitrary request bodies, or inert-looking Markdown plus a privileged local origin can be unsafe even when each component looks acceptable alone.
3. **Approvals are evidence of user intent, not containment.** Anthropic's observed approval rate and the command-classification bypasses show why an approval prompt cannot compensate for ambient filesystem, network, credential, or process authority.

## Standard-derived requirements

### Prompt injection and policy confusion

OpenAI describes prompt injection as analogous to social engineering and recommends limiting impact even when an attack succeeds, including source/sink analysis and deterministic constraints rather than input filtering alone. [OpenAI security research](https://openai.com/index/designing-agents-to-resist-prompt-injection/)

The OpenAI Codex Security threat model similarly treats repositories, model output, symlinks, and imported artifacts as data rather than authorization and warns that local processes can inherit environment credentials. [Codex Security policy](https://github.com/openai/codex-security/blob/main/SECURITY.md)

**Inference for this harness:** instruction discovery is not policy discovery. `AGENTS.md` or its fallback `CLAUDE.md` is bounded, untrusted model guidance. Even a future validated `harness-policy` block is restrict-only and cannot select a new path, endpoint, credential, executable, provider, Tool, agent count, or budget. The parser must reject unknown policy fields and ambiguous duplicates; it must not ask a model to interpret policy.

### MCP and OAuth

The MCP security specification requires exact redirect URI validation, per-client consent, `state` for CSRF protection, token audience validation, and prohibition of token passthrough. It also identifies SSRF during OAuth metadata discovery, DNS rebinding, session hijacking, local-server startup risk, and stdio proxy escalation. [MCP Security Best Practices](https://modelcontextprotocol.io/specification/2025-11-25/basic/security_best_practices) The authorization specification defines protected-resource metadata and OAuth interactions but does not turn a bearer token or session ID into authorization for every Tool. [MCP authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization) The transport specification distinguishes stdio from Streamable HTTP and requires validation of `Origin` for HTTP servers. [MCP transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)

**Inference for this harness:** MCP is not a Provider-shaped trait and not a trusted extension mechanism. It must remain behind the same typed Tool intent, Policy, Approval, and Guard pipeline as a local executable. Tokens must be scoped to one server/audience and stored behind secret handles. Discovery URLs, redirects, DNS answers, proxy routing, and final socket destinations all require deterministic validation. A local MCP subprocess receives an empty-by-default environment and stdio transport; installing or starting its exact executable is a separate explicit action.

### Shell and process invocation

Rust's `std::process::Command` passes arguments without shell interpretation, but its documentation warns that `.bat` files on Windows may be implicitly interpreted by `cmd.exe` and that safe escaping may not always be possible for untrusted arguments. [Rust `Command`](https://doc.rust-lang.org/std/process/struct.Command.html), [Rust process module](https://doc.rust-lang.org/std/process/)

**Inference for this harness:** direct argv is necessary but not sufficient. Future Tool manifests must identify an executable by a trusted installation identity, not a repository-controlled PATH lookup. Reject scripts and Windows batch files unless a dedicated, reviewed interpreter Tool owns their grammar and full effect authority. Clear ambient environment and handles, set an explicit working-directory handle, and enforce descendant limits at the OS boundary.

### SQLite

SQLite exposes `SQLITE_OPEN_NOFOLLOW` to prevent opening a database through a symbolic link. [SQLite open flags](https://www.sqlite.org/c3ref/open.html) SQLite's security guidance recommends defensive mode and disabling trusted schema when a database may be altered by an attacker. [SQLite security guidance](https://sqlite.org/security.html) SQLite also warns against network filesystems, changing database identity with links while open, and inheriting a live connection across `fork()`. [How to corrupt SQLite](https://www.sqlite.org/howtocorrupt.html)

**Inference for this harness:** the state directory and database are security-sensitive inputs even though the user owns them. Open the private state root no-follow; validate its owner/permissions or ACL; create the database atomically with private permissions; use `SQLITE_OPEN_NOFOLLOW`, defensive mode, and `trusted_schema=OFF`; verify a local filesystem policy; and keep the sole connection on the dedicated thread. A corrupt or schema-incompatible journal fails closed and remains inspectable; replay never turns stored text into instructions.

### Supply chain and native code

Cargo recommends committing `Cargo.lock` for applications to preserve resolved dependency versions. [Cargo lock-file guidance](https://doc.rust-lang.org/cargo/guide/cargo-toml-vs-cargo-lock.html) Cargo build scripts execute before compilation and can compile native code or perform arbitrary tasks. [Cargo build scripts](https://doc.rust-lang.org/cargo/reference/build-scripts.html) Registry checksums detect mismatched package contents but do not make a malicious package version safe. [Cargo source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html) RustSec's advisory database and `cargo-audit` provide ecosystem vulnerability checks. [RustSec advisory database](https://github.com/rustsec/advisory-db), [`cargo-audit`](https://github.com/rustsec/rustsec/tree/main/cargo-audit)

**Inference for this harness:** `rusqlite` with bundled SQLite intentionally adds C, FFI, and build-script code to the trusted computing base. Record that exception and test the compiled SQLite options/hardening at runtime. The application crate should use `#![forbid(unsafe_code)]`; any later unsafe or FFI addition needs an isolated safe interface, documented invariants, adversarial tests, and explicit review. Dependency review must include enabled features, build scripts, proc macros, native compilation, sources, licenses, advisories, and MSRV—not only crate names.

### Memory and excessive agency

OWASP lists memory poisoning, tool abuse, excessive agency, and denial-of-wallet among agent risks. These are general guidelines, not product incidents. [OWASP AI Agent Security Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html), [OWASP Excessive Agency](https://genai.owasp.org/llmrisk/llm062025-excessive-agency/) OpenAI's ChatGPT agent system card describes disabling Memory at launch as a prompt-injection mitigation and restricting terminal network access. [System card](https://deploymentsafety.openai.com/chatgpt-agent)

**Inference for this harness:** V1 should keep Memory absent. Canonical Events are history, not Memory and not instruction input. A later Memory feature requires provenance, scope, data class, retention, explicit promotion, rebuildable indexes, and a rule that retrieved records remain untrusted data. No model may write directly into an instruction or deterministic-policy channel.

## V1 release-blocking requirements

Priority `P0` means the minimum demonstrator must not ship without the control and its test. `P1` means required hardening before a public release; it may be implemented after the first local fake-provider proof only if the proof is clearly non-production.

### P0 — Establish authority before consuming untrusted input

1. Parse only CLI syntax and trusted process configuration needed to identify the command.
2. Reject `.env`, repository configuration, Git configuration, hooks, nested instruction discovery, implicit includes, config interpolation, and runtime plugins.
3. For `run`, resolve the caller-supplied Workspace argument independently of repository/model data; open and pin the directory capability before reading `AGENTS.md`, `CLAUDE.md`, or includes.
4. Record a stable Workspace identity suitable for diagnostics and revalidation without treating a path string as the capability.
5. The absolute display path may be stored for the user, but every read is handle-relative, component-by-component, no-follow. Reject symlinks, reparse points, `..`, absolute paths, alternate data streams, device names, and platform-specific namespace escapes.
6. Model-generated child objectives and summaries are values only. They cannot change root, includes, endpoint, provider, model, environment, credentials, agent topology, timeout, output cap, state directory, or telemetry endpoint.

**Verification gate:** an adversarial startup suite places malicious settings, `.env`, Git config, hooks, symlinks, worktrees, device names, and instruction text in and around the Workspace. Before the first provider call, the only opened repository files are the exact validated instruction fallback and explicit include handles. A model path outside the pinned root is rejected without an ambient open.

### P0 — Keep V1 non-effectful

- Expose no shell, subprocess, filesystem-write, patch, browser, MCP, plugin, dynamic library, callback URL, local server, arbitrary HTTP, or self-update path.
- Compile the Engine against only the `Provider` seam. Do not add a latent generic Tool registry or permission bypass for future use.
- Root and children have the same non-effectful capability set; children receive only bounded objective data and immutable Workspace snapshots. They cannot delegate.
- Treat the admitted semantic topology—one accountable primary, `0..N` direct children, and one or `N + 2` Provider calls—as a security bound, not only a functional test. The aggregate budget and hard child ceiling are immutable after `RunStarted`.

**Verification gate:** static dependency/API review plus integration tests demonstrate that malicious provider output resembling a command, URL, file request, tool call, policy block, nested delegation, or children above the admitted limit is rejected as a protocol outcome and causes no local or network effect other than admitted Provider calls.

### P0 — Harden Workspace snapshots

- Select exact root `AGENTS.md`; only if absent select exact root `CLAUDE.md`. If opening the first file fails for a reason other than absence, return the error instead of falling back.
- No imports, recursive discovery, parent search, nested instructions, or provider-directed reads in V1.
- Open the Workspace root and every relative component no-follow; verify the final object is a regular file; enforce size before allocation; read into an immutable byte buffer; require strict UTF-8; close the handle; use only that snapshot for the run.
- Define and test concurrent mutation behavior. The result must be either the bytes from the opened file object or a deterministic input-changed failure, never a later reopen by path.
- Avoid metadata-then-open authorization. Where the platform cannot guarantee handle-relative no-follow resolution, declare Workspace protection unavailable rather than falling back to path-prefix checks.

**Verification gate:** race harnesses repeatedly replace every path component and final file with symlinks or the platform equivalent during resolution on every supported host. Linux and macOS are blocking beta hosts. Windows runs the same reparse-point and path-namespace corpus before Windows support is advertised. No read may escape the root. Tests include hard links according to the documented policy; the current symlink rule alone does not address hard-link aliases.

### P0 — Harden provider networking and secrets

- Compile exact origins and paths for native Providers. Admit a custom profile only from trusted user configuration after exact data-free protocol/origin/model conformance; reject repository- or model-selected overrides.
- Use rustls with certificate and hostname validation. Disable redirects, environment/system proxies, `.netrc`, cookies, connection-auth negotiation, and automatic retries.
- Resolve and connect only as required for the admitted exact origin. Do not expose a generic URL fetcher. Bound connect, request, and total elapsed time separately; bound compressed and decompressed response bytes.
- Set the native provider's strongest documented storage controls, including `store:false` for OpenAI; send only the assembled bounded request; never send state-directory paths, unrelated environment values, or host metadata.
- Read only the credential environment reference named by the selected native or verified custom profile. Do not enumerate, log, persist, format with `Debug`, include in panic text, attach to telemetry, or pass it to another process. Clear the secret as early as practical while documenting that zeroization cannot guarantee removal from all allocator, TLS, kernel, swap, or provider copies.
- Redact authorization values and provider-response bodies from errors. Preserve a bounded request ID and status only where safe.
- No automatic retry. A timeout or lost response after bytes may have been sent is an ambiguous outcome and consumes one slot from the admitted Run call budget.

**Verification gate:** run through a hostile local proxy environment and assert no proxy is contacted; return redirects to loopback/private/file/alternate hosts and assert no follow; return chunked/compressed oversized bodies and assert bounded rejection; search Events, human output, JSONL, panic/error fixtures, and OTLP captures for a canary key and prompt.

### P0 — Harden canonical SQLite state

- Resolve the state directory before any repository input. It must be an explicit CLI path or documented per-user default, never repository/model controlled.
- Refuse a state root or existing database that is a symlink/reparse point, has unexpected type, is not owned by the current user, or is writable/readable beyond the documented private policy. On Unix require directory `0700` and database/sidecars `0600`; on Windows assert the effective ACL rather than assuming the LocalAppData path is private.
- Open the database with read-write/create and `SQLITE_OPEN_NOFOLLOW`; enable defensive mode and `trusted_schema=OFF`; use the selected rollback-journal mode; keep the only connection on the dedicated thread; never place it on a network filesystem or inherit it across a future fork.
- Authenticate structure, not content: validate schema version, Event kind/version, typed payload, IDs, phase/outcome pair, sequence continuity, legal transition, count, and payload bounds during replay. Unknown kinds or impossible histories fail closed with a typed corruption/incompatibility error.
- Stored objective, instruction-derived summary, child result, and final result are untrusted data. `show` may reduce and display them, but replay must never insert them into system/developer instructions, policy, configuration, or a new provider request.
- Define lifetime storage admission. Recommended V1 default: initialize a 4096-byte page size and a `max_page_count` of 65,536 (256 MiB), refuse a new run unless at least 4 MiB of database headroom remains, and never silently delete canonical Events. If measurement changes these constants, record one fixed replacement before release.
- Handle disk-full or terminal-Event failure honestly. Never report success unless the terminal Event is durably committed; preserve the last acknowledged prefix for recovery.

**Verification gate:** tests cover link substitution, permissive modes/ACLs, corrupt headers, unknown schema and Event versions, oversized payloads, illegal transitions, sequence gaps, disk-full/max-page failure, crash at every transaction boundary, and deterministic `Interrupted` recovery. A canary Event containing instruction-like text must never affect a later run.

### P0 — Make output inert

- Human output replaces or visibly escapes ESC, C0 controls other than deliberately emitted newline/tab policy, C1 controls, carriage return, backspace, bidi overrides/isolation controls, and other characters able to rewrite terminal state. Do this at the output boundary for every untrusted field.
- Never interpret Markdown, HTML, SVG, OSC hyperlinks, ANSI SGR, terminal titles, or model-provided file links.
- Construct status labels and error prefixes from trusted enums. Keep untrusted text in an explicitly delimited, length-bounded field so it cannot forge an approval or status line.
- JSONL must be produced by a real JSON serializer and remain one complete object per line. Do not manually interpolate or terminal-sanitize the JSON representation; JSON escaping is the boundary.
- Broken pipe is a normal bounded output termination condition and must trigger run cancellation without a panic dump containing sensitive state.

**Verification gate:** golden tests inject every C0/C1 byte, ESC/CSI/OSC/DCS sequences, CR/backspace, bidi controls, invalid UTF-8 at byte boundaries, long lines, fake prompts, and JSON delimiters into every untrusted field. A terminal emulator/parser fixture observes no control action.

### P0 — Enforce aggregate concurrency, time, and cost bounds

- One Run owns one budget object. The primary and every admitted child draw from it; delegation never creates new call, token, time, memory, or byte budgets.
- Reserve one direct-call slot or the full `N + 2` team-call budget and maximum aggregate output-token allowance before `RunStarted`. Reject admission if the configured operational budget cannot cover the worst case.
- Enforce the pinned child limit and beta hard ceiling, `max_output_tokens=4096` per call, bounded request/response/event sizes, 120-second per-call deadline, 300-second Run deadline, two-second cancellation grace, bounded channels, and zero retries.
- Count input/output sizes before cloning or queueing. Cancellation closes producers, releases permits, and prevents late results from becoming terminal state.
- Persist provider-reported usage as bounded numeric metadata when available. Treat monetary cost as an estimate tied to a versioned operator-supplied price table; a hard currency cap cannot recall an in-flight request.
- Ensure a stalled SQLite thread, blocked output sink, or telemetry exporter cannot retain unbounded provider tasks.

**Verification gate:** deterministic fake time and provider tests cover a child stall, concurrent children reaching limits, oversized structured output, cancellation races, closed receivers, SQLite backpressure, broken stdout, and a response arriving after cancellation. Total observed calls never exceed the admitted semantic budget.

### P0 — Preserve monotonic multi-agent authority

For every future capability type, define:

```text
effective_child = global_policy ∩ parent_effective ∩ explicit_task_grant
```

The operation is intersection over typed sets and numeric minima. There is no textual merge. A parent cannot delegate a capability it lacks; a child cannot approve its own request; sibling results cannot alter another sibling's authority; orchestrator messages are guidance within the recipient's existing capability; cancellation and revocation propagate downward.

V1's effective set contains only provider invocation with fixed endpoint/model constraints, immutable bounded input snapshots, bounded Event append through the Engine, and inert progress/result emission. It contains no OS Tool capability.

**Verification gate:** property tests generate parent/task/global capability sets and assert child authority is a subset of each. Integration tests inject grant-looking text into root plans, child objectives, sibling results, persisted Events, and provider errors; effective authority is unchanged.

### P1 — Minimize the Rust and dependency trusted computing base

- Put `#![forbid(unsafe_code)]` at the application crate root. Do not weaken it with a local `allow`; isolate a justified exception behind a separately reviewed crate/module boundary if one becomes necessary.
- Commit `Cargo.lock`. Pin the Rust toolchain and minimum supported version. Use minimal dependency features; do not enable Tokio `full` or unused protocol stacks.
- Inventory direct and transitive build scripts, proc macros, FFI/native code, unsafe code, cryptography, parsers, and network clients. The bundled SQLite choice is an explicit native-code exception.
- CI must run formatting, Clippy with warnings denied according to repository policy, tests, dependency license/source policy, duplicate/feature review, and RustSec audit. An unavailable advisory feed must produce a visible result rather than a false pass.
- Release from a clean, reviewed lockfile with reproducible input provenance and checksums/signatures for published artifacts. V1 has no automatic updater.
- Development skills are code-equivalent instructions. The current `skills-lock.json` records source names, paths, and computed content hashes but no immutable source revision. Until that is fixed, every skill update requires manual source-revision capture, content-diff review, hash verification, and a clean working tree; no installed skill may grant runtime harness authority.

**Verification gate:** CI fails on application `unsafe`, an unreviewed build script/native dependency/source, lockfile drift, a known RustSec advisory above the declared threshold, or a skill hash/source mismatch.

### P1 — Panic and diagnostic containment

- Expected hostile input returns typed errors, not panics.
- Production output does not enable backtraces automatically. Panic hooks and error reporters must not dump request bodies, secret-bearing structs, environment variables, or database contents.
- Use redaction-safe wrapper types and tests for `Debug`/`Display` on secret-bearing and provider types.
- Fuzz instruction parsing, structured provider output, Event decoding/replay, path validation, and terminal escaping. Bound every decoder before allocation-heavy work.

## Gate before the first effectful Tool

No executable, write-capable filesystem operation, arbitrary fetch, MCP Tool, browser, Git command, patch application, or plugin action may ship until all of the following exist.

### One immutable `EffectIntent`

The Engine creates one normalized typed value containing:

- effect ID and run/agent identity;
- exact Tool identity and version/digest;
- exact executable/interpreter identity and complete argv, or a typed non-process operation;
- pinned Workspace identity and handle-relative working directory;
- requested filesystem read/write paths and creation semantics;
- requested network scheme, method, audience, destination, redirect policy, and payload data class;
- minimum environment names/secret handles and inherited-handle policy;
- process, descendant, CPU, memory, output, artifact, descriptor, and deadline limits;
- idempotency/retry class;
- parent effective capability and task grant.

Normalize once. Policy evaluates this value. Approval text is rendered from it. `ApprovalProof` binds its cryptographic digest plus policy version, user/session, expiry, and single-use effect ID. The journal records the same digest. Guard executes the same value. Any mutation invalidates approval.

Never implement “remember this command,” executable-name-only, path-prefix, URL-host-only, first-pipeline-command, or regex-only authorization. A reusable rule must itself be a reviewed typed policy object with scope and expiry.

### Deterministic Policy

- Default deny; closed schemas; unknown Tool/capability/field denies.
- Repository policy is restrict-only. Model/classifier output may raise a risk signal or force denial/review, never grant.
- Path, executable, endpoint, credential, and budget selection comes from trusted configuration plus the user request, not model text.
- Descendant capability is a monotonic intersection.
- A deny cannot be converted to allow by approval. Approval can satisfy only an explicit `RequireApproval` result within the pre-authorized maximum.
- Decisions are pure over the normalized intent, versioned policy, and authenticated context, producing a stable reason code and policy digest.

### Separate Guard

Policy decides; Guard enforces. The Guard is a separate privilege/process boundary before effectful Tools, not merely a Rust module in the Engine process. It must:

- establish filesystem, network, process, environment, resource, and descendant restrictions before the executable runs;
- positively attest effective capabilities and reject any unsupported dimension with `ProtectionUnavailable`;
- broker sensitive opens/connects where pre-execution sandbox rules cannot safely express the typed intent;
- supervise the complete process tree and guarantee cancellation/timeout termination and reaping;
- capture bounded stdout/stderr without allowing terminal interpretation;
- return observed effects and enforcement metadata for audit without sensitive bodies;
- never accept raw model strings as policy.

### Filesystem enforcement

- Capability-relative opens, no-follow on every component, regular-file/type checks, and create/replace semantics bound to directory handles.
- No privileged component reopens a less-privileged pathname.
- Atomic create/replace with explicit overwrite rules; private random temporary objects; no shared predictable temp paths.
- Explicit handling for hard links, mounts, bind mounts, reparse points, alternate streams, device nodes, sockets, FIFOs, case folding, Unicode normalization, and filesystem-specific behavior.
- Workspace revision/identity is revalidated at execution. A stale intent is denied, not silently rebound.

### Process enforcement

- Exact trusted executable, explicit argv, no shell or PATH search, minimal environment, closed inherited handles/descriptors, pinned working-directory handle, and no user-controlled loader variables.
- Unix process group/session or stronger containment plus child subreaper behavior where needed; Windows Job Object with `KILL_ON_JOB_CLOSE` or stricter. Windows Job Objects provide group termination and resource limits but are not filesystem/network sandboxes. [Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)
- Deny nested shells/interpreters unless the Tool manifest deliberately grants that interpreter's full authority.
- No reliance on command parsing to infer read-only behavior.

### Network enforcement

- Default deny inside the Tool environment. A broker owns DNS and socket creation.
- Policy binds scheme, exact audience, method, port, path/template, redirect count, request schema, payload data class, and byte limits—not merely hostname.
- Reject credentials in URLs, non-HTTP schemes unless explicitly modeled, IP literals when domain identity is required, loopback, link-local, multicast, unspecified, private, carrier-grade NAT, metadata-service, Unix socket, and platform-special destinations unless the typed Tool explicitly exists for them.
- Resolve through the trusted broker; validate every address; pin the connected address; bind TLS name and HTTP `Host` to the authorized identity; revalidate every redirect and prevent HTTPS downgrade. Do not trust ambient proxy or DNS configuration silently.
- Network authorization does not authorize uploading sensitive data. The request builder must accept only typed, classified fields permitted for that audience.

### Platform support matrix

| Platform | V1 read-only demonstrator | First effectful Tool release gate |
| --- | --- | --- |
| Linux | Workspace/store handle tests plus fixed provider egress; no Tool sandbox claim. | Linux-first Guard combining namespace/mount isolation, Landlock where effective, seccomp, cgroup/resource limits, brokered egress, minimal environment, process-tree supervision, and runtime capability attestation. Kernel/filesystem matrix must be explicit. |
| macOS | Workspace/store no-follow tests plus fixed provider egress; no Seatbelt claim. | A supported, distributable enforcement design with filesystem and network denial, process-tree control, and adversarial tests. Private/unstable Seatbelt assumptions are not a product contract; if a dimension cannot be enforced, return `ProtectionUnavailable`. |
| Windows | Not supported in the beta. Reparse-point/ACL/path-namespace, console-control, persistence, terminal, and packaging suites must pass before support is advertised. | AppContainer/LPAC or stronger filesystem/token isolation, Windows Filtering Platform or a trusted broker for egress, Job Object for descendants/resources, explicit handle inheritance, and NT path/reparse/ADS tests. |
| Other | Unsupported unless all V1 file/store/network invariants have a tested implementation. | Unsupported by default. No permissive fallback. |

The existing deterministic-protection research is correct to make capability attestation, rather than a `sandboxed` boolean, the contract. The security lesson from real incidents is to test compositions: sandbox plus helper, tool plus Git, symlink plus privileged renderer, DNS plus redirects, and parent plus child.

## Gates for deferred features

### MCP

Required before enabling any MCP integration:

1. MCP Tool calls pass through the same `EffectIntent`/Policy/Approval/Guard path as native Tools.
2. Local servers use exact executable identity, minimal environment, separate privilege, bounded stdio, and explicit installation/start consent. Prefer stdio; do not auto-start an HTTP listener.
3. HTTP servers/clients authenticate every request. Loopback is not identity. Validate `Origin`/Host, resist DNS rebinding, apply CSRF protection, and bind sessions to authenticated principals.
4. OAuth uses exact registered redirect URIs, `state`, PKCE where applicable, per-client consent, HTTPS outside loopback development, audience validation, and no token passthrough. Never open an authorization URL using a shell.
5. Discovery, metadata, redirects, DNS, proxies, and final sockets obey the brokered SSRF policy.
6. One principal/session owns one MCP protocol and transport instance; no mutable cross-client reuse; random session IDs are identifiers, not authentication.
7. MCP resources, prompts, Tool descriptions, errors, and results are untrusted data with size/depth/count limits. Server-declared capabilities do not grant local capabilities.
8. Sampling/elicitation cannot silently spend budget, access Memory, or widen authority. Every nested action is charged to and bounded by the originating run.

### Plugins, skills, and updates

- V1 has no runtime plugins and no self-update.
- A future plugin is untrusted executable content. Prefer an out-of-process or capability-safe WASI component boundary over in-process dynamic loading.
- A signed package proves publisher/key provenance, not safety. Require an immutable artifact digest, transparent version/source, reviewed permission manifest, compatibility/API version, revocation path, and deterministic install/update diff.
- Installation does not activate authority. Per-run effective capabilities remain an intersection with user/global policy.
- Never execute repository-local plugins or skills merely because they are present. Instruction-only skills remain untrusted guidance unless installed through the trusted developer workflow.
- Update metadata and package downloads use a fixed authenticated origin, rollback/freeze protections, signature verification, atomic install, and a recoverable previous version. No repository/model-selected update channel.

### Durable Memory and retrieval

- Store provenance, creator, workspace/tenant, run, timestamp, data class, schema version, and retention with every record.
- Separate immutable evidence from derived summaries/indexes. Derived state is rebuildable and never more authoritative than its source.
- Model/tool content enters quarantine as untrusted data. Promotion to a reusable user fact or instruction requires deterministic schema plus explicit user action; policy can never be written through Memory.
- Retrieval always enforces scope before ranking and prompt assembly. Cross-user, cross-workspace, and cross-agent visibility are deny-by-default.
- Bound record size/count, retrieval results, index work, retention, and deletion. Provide export and deletion semantics before collecting sensitive content.
- Test poisoning, stale facts, malicious instructions, scope confusion, deletion propagation, and reconstruction from canonical evidence.

### Remote client or daemon

- A second Client or detached execution may justify a daemon; until then, do not start a listener.
- Define principal/session identity, authentication, per-command authorization, CSRF/origin policy, TLS, rate limits, body/decompression limits, idle/absolute session expiry, concurrent-run quotas, and audit correlation before remote exposure.
- Do not expose shell-, PTY-, arbitrary-file-, or arbitrary-fetch-shaped endpoints. Expose typed Engine commands with the same deterministic policy.
- Separate the presentation origin from any privileged control API. Rich untrusted content cannot share a credential-bearing/control origin.

### OTLP

The selected runtime-opt-in OTLP design is suitably narrow: trace-only, binary HTTP/protobuf, explicit numeric loopback endpoint, no redirect/proxy, typed allowlist, bounded lossy queue, and nonfatal failure. It ships last in beta; preserve these constraints.

Additional gates:

- Construct span names and attributes only from trusted enums and bounded numeric identifiers. Do not accept a generic attribute map anywhere in Engine code.
- Prohibit objectives, instructions, summaries, results, paths, repository names, provider bodies, headers, exception messages, database payloads, user IDs, and secrets by type and test.
- Collector response bodies are untrusted and bounded; they are never surfaced verbatim or persisted.
- Telemetry failure cannot block, retry a provider call, change Event ordering, or change final status. Shutdown remains bounded.
- Document that even content-free timestamps, run IDs, model names, durations, and token counts are metadata and may be sensitive. OTLP is opt-in.

## Deterministic verification program

### Release gate suite

| Gate | Required evidence | Failure behavior |
| --- | --- | --- |
| Authority provenance | Trace of every root/endpoint/model/credential/budget source; malicious repository/model fixtures cannot alter them. | Abort before `RunStarted` or reject provider outcome. |
| Workspace escape | Platform race corpus for symlink/reparse/hard-link/path namespace, including concurrent replacement. | `InvalidInput` or `ProtectionUnavailable`; no ambient fallback. |
| Startup trust | Instrumented test proves no repository config/Git/hook/plugin/`.env` processing before Workspace pinning. | Abort without provider call. |
| Provider egress | Hostile proxy/DNS/redirect/body/TLS fixtures; only fixed origin contacted; canary secret absent from all outputs. | Bounded `ProviderError`; zero retry. |
| Store integrity | Mode/ACL/no-follow tests; corruption/schema/event-transition/disk-full/crash corpus; stable replay. | Typed state error; never fabricate success. |
| Output safety | Full terminal-control and structured-output corpus through human and JSONL modes. | Escaped/replaced human text or valid serialized JSON only. |
| Multi-agent bounds | Property tests for subset authority and integration tables spanning direct mode, representative `team(N)` cases, configured limits, and the hard ceiling. | Reject invalid plan/result and cancel Run. |
| Resource/cost | Fake-clock saturation tests for all queues, bytes, tokens, call slots, deadlines, disk admission, and cancellation. | Deterministic bound error; no leaked task/permit. |
| Secret hygiene | Canary API key, objective, and result scanned across Events, stdout/stderr, errors, panic fixtures, and OTLP capture. | Test failure and release block. |
| Supply chain | Locked sources, reviewed feature/build-script/native/unsafe inventory, RustSec and license/source policy, reproducible release provenance. | CI/release failure. |
| Platform capability | Matrix reports actual available V1 protections; later Guard attests each requested capability. | Unsupported platform/dimension fails closed. |

### Fuzz and property targets

- CLI/config parsing with long Unicode, NUL, platform path prefixes, duplicates, and precedence conflicts.
- Instruction fallback and strict UTF-8 snapshot loading under concurrent mutation.
- Structured provider response schema, nesting, unknown fields, duplicate fields, huge numeric/string values, and outcome/phase mismatch.
- Event encoding/decoding, legal transition reducer, schema migration, and interrupted-tail recovery.
- Human output escaping and JSONL serialization.
- Future `EffectIntent` canonicalization/digest stability, policy monotonicity, approval binding, URL normalization, and path resolution.

Fuzzers run with explicit memory/time corpus limits. A fuzzer finding becomes a regression test at the narrowest boundary.

### Adversarial scenario corpus

Maintain named tests derived from the incidents:

- `model_cwd_cannot_rebind_workspace`
- `repository_config_never_precedes_workspace_pin`
- `repository_cannot_select_permission_bypass`
- `git_metadata_does_not_confer_trust`
- `sandbox_created_link_never_crosses_privilege_boundary`
- `allowed_host_does_not_allow_arbitrary_upload`
- `policy_file_requires_trusted_owner_and_acl`
- `temporary_output_is_private_and_nofollow` when temp output exists
- `no_ambient_local_http_server`
- `rendered_model_output_has_no_control_capability`
- `child_authority_is_subset_of_parent`
- `shell_syntax_never_classified_as_read_only` when shell support is considered
- `headless_mode_does_not_widen_authority`
- `mcp_transport_is_not_shared_across_principals`
- `localhost_requires_authentication`

## Comparison with current repository decisions

### Strong and already aligned

The following existing decisions should remain authoritative:

- `agents/security.md` already requires typed bounds, deterministic policy, fail-closed effective-capability attestation, monotonic descendant authority, minimal child environments, no shell interpolation, path/symlink policy, complete process-tree supervision, SSRF/redirect validation, inert rendering, retention policy, and unsafe/FFI review.
- `deterministic-harness-protection.md` correctly separates Policy from Guard, uses a normalized effect/digest, rejects platform capability gaps, and treats DNS, redirects, process descendants, and symlinks as enforcement concerns.
- `instruction-markdown-and-policy-enforcement.md` correctly loads exact root `AGENTS.md` before exact root fallback `CLAUDE.md`, keeps ordinary Markdown as guidance, and makes any structured repository policy restrict-only.
- `provider-and-tool-runtime.md` correctly binds approvals to exact immutable effects, separates provider protocol from Tools, minimizes environment, and makes Engine own admission/retry/cancellation.
- `cli-inputs-configuration-and-operability.md` correctly selects one ambient Workspace root, forbids `.env`, fixes the provider endpoint, disables V1 ANSI/Markdown rendering, defines JSONL, and validates before `RunStarted`.
- `rust-foundation-and-engine-contract.md` correctly uses one current-thread runtime, a dedicated SQLite thread/connection, immutable Workspace snapshots, strict phase/outcome types, bounded queues, and only one Provider seam.
- `otlp-observability.md` correctly makes traces optional, lossy, content-free, loopback-only, bounded, and noncanonical.
- `system-overview.md`, the closure audit, and the decision register correctly keep beta to one process, bounded flat direct children, an admitted aggregate Provider-call budget, zero retries, and no effectful Tool.

### Missing or too weak

| Area | Current state | Required correction |
| --- | --- | --- |
| Startup trust ordering | Workspace inputs are pinned, but the universal security rules do not state that no repository-dependent parsing/execution may occur before the boundary exists. | Add a startup-before-trust invariant and explicit prohibition on project config, Git, hooks, `.env`, and plugins before pinning/consent. |
| Authority provenance | Deterministic policy is required, but trusted sources for roots/endpoints/executables/budgets are not stated as a universal rule. | Say repository/model/tool/memory/replay content never originates or widens authority. |
| Approval/execution identity | The deep research describes effect digests, but `agents/security.md` does not require one normalized object shared by policy, approval, journal, and execution. | Add an immutable-intent rule and reject prefix/name/regex-only approvals. |
| Multi-agent delegation | Descendants may only retain/lose authority, but the exact intersection and sibling/orchestrator rule are implicit. | State `child = global ∩ parent ∩ task`, no self-approval, no sibling authority mutation. |
| Store opening | Architecture has private permissions and `trusted_schema=OFF`, but not `SQLITE_OPEN_NOFOLLOW`, defensive mode, ownership/ACL checks, local-filesystem policy, or aggregate DB limit. | Make these V1 release gates. |
| Replay/memory poisoning | Events are canonical and typed, but the prohibition against feeding Event text back as instruction/policy is not explicit. | State replay validates legal typed history and persisted content remains untrusted data. |
| Terminal boundary | “No ANSI” and sanitize rendering are present, but the exact control-character/bidi behavior is not deterministic. | Specify ESC/C0/C1/CR/backspace/bidi treatment and serializer-only JSONL. |
| Persistent resource bound | Per-event/per-run limits are strong; lifetime SQLite growth is unbounded. | Add a fixed database page cap and admission headroom; never silently prune canonical state. |
| Cost reservation | Four calls/tokens are bounded, but pre-run aggregate reservation and child budget inheritance are not explicit. | One Engine-owned budget; reserve call/token maxima before `RunStarted`; all descendants charge it. |
| Supply chain | Unsafe/native review exists, but build scripts, proc macros, lock updates, sources, release provenance, and development skills are not covered. | Add dependency/build authority and skill/update review rules. |
| Security non-claims | Documents describe future Guard but could be read as general sandbox safety. | Publish the V1 claim/non-claim and platform support matrix. |
| Local listeners | Universal rule says loopback by default, which is too broad for a CLI and can imply loopback is sufficient. | Say no listener by default; loopback still requires authentication, authorization, Origin/Host/CSRF, and DNS-rebinding defenses. |
| URL/egress data policy | SSRF/redirect validation exists, but an allowed destination could still receive arbitrary sensitive data. | Bind network authority to operation/method/audience/payload schema/data class, not host alone. |
| Executable identity | argv is required, but PATH lookup, scripts, batch files, interpreters, and executable replacement are not addressed. | Require trusted executable identity/digest and platform-aware interpreter policy. |
| Temporary files | No V1 need and no explicit rule. | Prefer no temp file; otherwise private atomic no-follow handle-owned creation and bounded cleanup. |
| Cross-principal session state | Deferred MCP/daemon docs do not state protocol/transport instance isolation. | One authenticated principal/session per mutable protocol/transport state machine. |

### Overly broad or ambiguous rules

1. **“Bind servers to loopback by default”** should not imply that starting a server is a normal CLI default or that loopback authenticates callers. Replace it with “Do not start a listener unless the product feature requires one; loopback listeners still require authentication and browser-origin defenses.”
2. **“Sanitize untrusted Markdown, HTML, SVG, file names, and links”** leaves the sanitizer and active-content policy unspecified. Prefer inert rendering by default, with format-specific allowlists and isolation only when rich rendering is necessary.
3. **“Validate and canonicalize paths”** can encourage check-then-use path logic. Require handle-relative no-follow resolution and a stated hard-link/reparse/mount policy; canonical strings are for display/comparison, not sufficient authority.
4. **“Invoke processes with an executable and argument array”** is necessary but incomplete on Windows and for interpreters. Add exact executable identity, no PATH lookup, minimal environment/handles, and the `.bat`/script rule.
5. **“Authenticate the caller and authorize every resource”** assumes a caller identity exists. For the local V1 CLI, the OS user and explicit CLI selection are the authority source; do not invent a remote authentication abstraction before there is a remote client.

## Exact recommendations for `agents/security.md`

Keep the file terse. The following edits capture durable rules without copying this report.

### Apply under “Universal rules”

Add the non-overlapping bullets below. Where a bullet replaces the current path, server, or rendering rule, replace the old wording instead of appending a duplicate.

```markdown
- Establish trust boundaries before consuming project-controlled input. Do not parse or execute repository configuration, Git configuration, hooks, `.env`, plugins, or project commands before the Workspace is pinned and the required trust decision is complete.
- Authority originates only from authenticated user or administrator configuration and deterministic policy. Repository text, model output, tool or MCP output, retrieved Memory, and replayed Events may narrow policy through a validated restrict-only schema but can never grant or widen roots, endpoints, credentials, executables, budgets, or capabilities.
- Normalize an effect once into an immutable typed intent. Policy, approval text and proof, journal records, and the executor use the same intent and digest; command prefixes, executable names, hostnames, and remembered approvals are not sufficient authorization identities.
- Derive child authority as the intersection of global policy, parent effective authority, and the explicit task grant. Children cannot self-approve, siblings cannot change each other's authority, and orchestrator messages do not grant capabilities.
- Resolve filesystem access through pinned directory handles with no-follow semantics through use, not by string-prefix checks. Define hard-link, mount, reparse-point, alternate-stream, device, and concurrent-replacement behavior for every supported platform.
- Treat persisted history as untrusted typed data during replay. Validate schema, versions, identities, sequence, legal transitions, counts, and sizes; never promote replayed text into instructions, policy, configuration, or new authority.
- Do not start a listener unless the feature requires one. Loopback is not authentication; local servers still require per-session authentication and authorization, Host and Origin validation, CSRF and DNS-rebinding defenses, and bounded sessions.
- Bind network grants to a typed operation, audience, method, destination, redirect policy, payload schema, data classification, and limits. A host allowlist alone never authorizes arbitrary upload or secret-bearing content.
- Render untrusted content inert by default. Human terminal output must neutralize control and bidi sequences; machine output must use a format serializer. Rich rendering requires a format-specific allowlist and isolation from privileged control APIs.
- Open security-sensitive state and policy files no-follow and verify type, owner, and permissions or ACL. Bound aggregate persistent growth; never silently discard canonical history on overflow.
- Treat dependencies, build scripts, procedural macros, native libraries, development skills, installers, and update channels as code execution and supply-chain authority. Pin immutable inputs, review changes and enabled features, audit advisories, and keep runtime self-update disabled unless separately designed.
```

### Replace the existing process bullet

Replace the current process bullet with:

```markdown
- Invoke a trusted executable identity with an explicit argument array, working-directory capability, minimal environment, and closed inherited handles. Never interpolate model text into a shell command or grant safety based on parsing shell syntax; scripts and Windows batch files require a dedicated interpreter policy.
```

### Add security-pass triggers

```markdown
- New repository configuration, startup discovery, trust cache, instruction role, replay-to-prompt path, or project-controlled initialization.
- New dependency with a build script or procedural macro, new installer/update path, or any change to dependency source, lockfile, native code, or development skill provenance.
- New local listener, temporary-file path, persistent-state format, aggregate storage policy, or cross-session mutable state.
```

### Add to the triggered-work checklist

```markdown
For triggered work, also name the authority source, immutable normalized intent, effective platform enforcement, descendant authority, sensitive data classes, aggregate cost/storage budget, startup ordering, and explicit security non-claims.
```

Do not copy product-specific CVEs into `agents/security.md`; this report is their durable evidence trail. The root rule file should keep only the generalized invariant.

## Implementation sequencing

1. Add the V1 claim/non-claim and the P0 controls to the first implementation plan.
2. Implement startup/config parsing and Workspace/state capability opening before provider or agent-loop code.
3. Build adversarial path/store/output tests against the platform APIs selected by the implementation.
4. Implement the typed Event journal/reducer and verify corruption, crash, disk, and replay-poisoning behavior.
5. Implement the scripted Provider and durable Session loop for direct and bounded-team Runs with one aggregate budget.
6. Implement the fixed OpenAI adapter and hostile-network/secret-canary tests.
7. Add optional content-free OTLP only after the core gates pass.
8. Run dependency/native/build-script review and publish the supported-platform security matrix before calling the demonstrator releasable.
9. Treat the first effectful Tool as a new security milestone requiring Policy, ApprovalProof, and the separate Guard; do not grow it incrementally inside the V1 Engine.

## Final assessment

The planned architecture is substantially safer than a typical coding-agent MVP because it begins with a read-only proof, a fixed agent topology, a single Provider seam, pinned Workspace inputs, typed Events, bounded resources, and no effectful Tools. The primary-source incidents show that this narrowness is valuable only if it remains literal. Latent generic execution, repository-controlled startup configuration, path-string validation, permissive local HTTP, or a convenience bypass would invalidate the claim even if the happy path never uses them.

The minimum implementation is ready to begin after the P0 requirements are incorporated into its plan. Effectful Tools are not ready to begin until the normalized effect, deterministic Policy, exact approval binding, and separate platform Guard have concrete interfaces and adversarial tests. MCP, plugins, Memory, remote clients, and rich rendering remain later milestones with independent release gates; none should be smuggled into the Provider or CLI abstraction.
