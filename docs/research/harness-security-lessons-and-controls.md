# Harness security lessons and controls

**Source review:** 2026-09-29.
**Status:** retained incident evidence and security rationale; not current
implementation, platform support or release verification.

This investigation began with a read-only demonstrator. Its implementation
checklist has been absorbed by [security rules](../../agents/security.md),
[Tool behavior](../specs/tools.md), [execution](../specs/execution.md) and
[credential behavior](../specs/credentials.md). This report retains the incidents
and standards explaining those controls. Current gaps belong in
[next steps](../../NEXT_STEPS.md).

Rust's memory safety does not prevent authority-selection mistakes, startup
before trust, path races, unauthenticated listeners, active untrusted rendering
or delegation that widens permissions. The evidence below explains why those
boundaries require deterministic enforcement.

## Evidence method

This report separates evidence into three classes:

- **Incident:** a vendor or project advisory, a vendor retrospective, or a maintainer-authored security disclosure describing an observed flaw.
- **Standard:** a normative specification or security guide. It describes required practice, not proof that this harness is vulnerable.
- **Inference:** a recommendation derived from the incidents, standards, and the repository's selected architecture. Inferences are labeled where they are not direct requirements of a cited source.

GitHub issues are not treated as confirmed advisories. Where used, they are identified as issue reports and support a test case, not a vulnerability claim.

## Incident and lesson matrix

| Evidence | Observed failure | Root security mistake |
| --- | --- | --- |
| **Incident — OpenAI Codex, GHSA-w5fx-fh39-j5rw / CVE-2025-59532.** A model-generated working directory became the sandbox writable root, including outside the user-started folder. The fix anchored and canonicalized the boundary to the session-start path. [Advisory](https://github.com/openai/codex/security/advisories/GHSA-w5fx-fh39-j5rw) | Model data selected an enforcement root. | Confused authority provenance; validation was applied to an attacker-selected boundary. |
| **Incident — Anthropic Claude Code, GHSA-j4h9-wv2m-wrf7 / CVE-2025-59041.** Repository-controlled Git configuration reached command templating before workspace trust and enabled command execution. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-j4h9-wv2m-wrf7) | Code ran before the trust boundary existed. | Trust dialog and policy were placed after repository-dependent initialization. |
| **Incident — Claude Code containment retrospective.** Anthropic reports repository hooks were parsed/executed before consent; an internal red-team prompt exfiltrated `~/.aws/credentials` in 24 of 25 trials when environment boundaries did not stop it; and users approved about 93% of prompts. [Vendor retrospective](https://www.anthropic.com/engineering/how-we-contain-claude) | Probabilistic defenses and frequent approvals did not protect ambient authority. | Treating user confirmation or model behavior as a security boundary. |
| **Incident — Claude Code, GHSA-mmgp-wc2j-qcv7 / CVE-2026-33068.** A repository `.claude/settings.json` could select `bypassPermissions` before trust and silently skip the trust flow. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-mmgp-wc2j-qcv7) | Untrusted project configuration disabled its own guard. | Policy/configuration source confusion. |
| **Incident — Claude Code, GHSA-q5hj-mxqh-vv77 / CVE-2026-40068.** A crafted Git worktree `commondir` pointed to an already trusted repository and bypassed trust, enabling hook execution. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-q5hj-mxqh-vv77) | A related path/metadata identity was mistaken for the trusted object. | Path aliasing and trust keyed by a weak identity. |
| **Incident — Claude Code, GHSA-vp62-r36r-9xqp / CVE-2026-39861.** A sandboxed process created an outward symlink that a later unsandboxed application followed, producing an arbitrary write. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-vp62-r36r-9xqp) | Two individually plausible components composed into a boundary bypass. | Validation and use occurred in different authority domains. |
| **Incident — Claude Code, GHSA-7835-87q9-rgvv / CVE-2026-55607.** Git worktree behavior, external navigation, symlink manipulation, and `fsmonitor` combined to write outside a macOS sandbox. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-7835-87q9-rgvv) | Tool-specific filesystem behavior exceeded the apparent sandbox path model. | Assuming an application-level path rule represented effective OS behavior. |
| **Incident — Claude Code, GHSA-fg94-h982-f3mm / CVE-2026-54316.** An allowed `huggingface.co` host still provided attacker-controlled repository paths that formed an exfiltration channel for files, environment values, and output. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-fg94-h982-f3mm) | A destination allowlist was confused with an information-flow policy. | Egress identity alone says nothing about the sensitivity of uploaded data. |
| **Incident — Claude Code, GHSA-5cwg-9f6j-9jvx / CVE-2026-35603.** Windows managed settings were accepted without owner/ACL validation, letting a lower-privileged user influence privileged execution. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-5cwg-9f6j-9jvx) | A policy file was trusted because of its pathname. | No provenance or writable-by-attacker check. |
| **Incident — Claude Code, GHSA-4vp2-6q8c-pvq2.** A predictable world-readable temporary response file enabled disclosure and symlink-based arbitrary overwrite. [Advisory](https://github.com/anthropics/claude-code/security/advisories/GHSA-4vp2-6q8c-pvq2) | Sensitive data used a shared predictable pathname with permissive mode. | Unsafe temporary-file creation and path-based reopen. |
| **Incident — OpenCode, GHSA-vxw4-wv6m-9hhh / CVE-2026-22812.** The CLI automatically started an unauthenticated local HTTP server with permissive CORS and shell, PTY, and file endpoints; websites or local processes could execute commands. [Advisory](https://github.com/anomalyco/opencode/security/advisories/GHSA-vxw4-wv6m-9hhh) | Loopback was treated as authentication. | Privileged ambient listener, unsafe browser trust, and broad local API. |
| **Incident — OpenCode, GHSA-c83v-7274-4vgp.** Unsanitized LLM Markdown/HTML plus a configurable server URL produced script execution on the trusted local origin and then terminal-command execution. [Advisory](https://github.com/anomalyco/opencode/security/advisories/GHSA-c83v-7274-4vgp) | Untrusted presentation crossed into a privileged control origin. | Rendering and authority were composed without an isolation boundary. |
| **Issue report — OpenCode #6527.** A plan-mode parent reportedly spawned a subagent that did not inherit edit denial and changed files. This is not a project security advisory. [Issue](https://github.com/anomalyco/opencode/issues/6527) | Delegation widened authority. | Parent policy was not monotonically inherited. |
| **Incident — GitHub Copilot CLI, GHSA-g8r9-g2v8-jv6f / CVE-2026-29783.** Bash parameter transformations, assignments, indirection, and nested substitutions hid arbitrary execution inside commands classified as read-only. [Advisory](https://github.com/advisories/GHSA-g8r9-g2v8-jv6f) | A complex language was classified with an incomplete safety parser. | Command-name/pattern allowlists were treated as effect enforcement. |
| **Incident — Gemini CLI, GHSA-wpqr-6v78-jr5g / CVE-2026-12537.** Headless CI auto-trusted workspace configuration and `.env`; permissive mode bypassed a fine-grained allowlist, enabling prompt-injection-driven execution. [Advisory](https://github.com/advisories/GHSA-wpqr-6v78-jr5g) | Noninteractive mode silently changed the trust and permission contract. | Convenience flags became authority escalation. |
| **Issue report — Gemini CLI #11510.** A project issue reported that the shell allowlist examined only the first command in a pipeline. It is issue evidence, not an advisory. [Issue](https://github.com/google-gemini/gemini-cli/issues/11510) | Partial parsing authorized a compound command. | Approval did not bind the complete executed effect. |
| **Incident — MCP TypeScript SDK, GHSA-w48q-cv73-mx4w / CVE-2025-66414.** DNS-rebinding protection was off by default for unauthenticated localhost HTTP servers; malicious websites could invoke tools or access resources. Stdio was unaffected. [Advisory](https://github.com/modelcontextprotocol/typescript-sdk/security/advisories/GHSA-w48q-cv73-mx4w) | Localhost HTTP was exposed to browser-origin confusion. | Insecure default and no host validation/authentication. |
| **Incident — MCP TypeScript SDK, GHSA-345p-7cg4-v4c7 / CVE-2026-25536.** Reused transport/server instances misrouted responses and notifications across concurrent clients because request IDs collided and mutable transport state was shared. [Advisory](https://github.com/modelcontextprotocol/typescript-sdk/security/advisories/GHSA-345p-7cg4-v4c7) | Cross-principal mutable session state was reused. | Session identity and transport lifetime were not isolated. |
| **Incident — official Gemini CLI discussion #8385.** Google reported Gemini CLI itself was not compromised, while installs during an npm ecosystem attack could have fetched a compromised transitive dependency. [Maintainer discussion](https://github.com/google-gemini/gemini-cli/discussions/8385) | Correct first-party code did not eliminate dependency-install risk. | Build/install code and transitive packages are part of the trusted computing base. |

The matrix supports three broad conclusions:

1. **Authority provenance matters more than intent classification.** The most damaging bugs let model output, repository configuration, path aliases, or convenience modes influence the enforcement boundary.
2. **Security properties must survive composition.** A sandbox plus an unsandboxed helper, an allowlisted host plus arbitrary request bodies, or inert-looking Markdown plus a privileged local origin can be unsafe even when each component looks acceptable alone.
3. **Approvals are evidence of user intent, not containment.** Anthropic's observed approval rate and the command-classification bypasses show why an approval prompt cannot compensate for ambient filesystem, network, credential, or process authority.

## Standard-derived requirements

### Prompt injection and policy confusion

OpenAI describes prompt injection as analogous to social engineering and recommends limiting impact even when an attack succeeds, including source/sink analysis and deterministic constraints rather than input filtering alone. [OpenAI security research](https://openai.com/index/designing-agents-to-resist-prompt-injection/)

The OpenAI Codex Security threat model similarly treats repositories, model output, symlinks, and imported artifacts as data rather than authorization and warns that local processes can inherit environment credentials. [Codex Security policy](https://github.com/openai/codex-security/blob/main/SECURITY.md)

### MCP and OAuth

The MCP security specification requires exact redirect URI validation, per-client consent, `state` for CSRF protection, token audience validation, and prohibition of token passthrough. It also identifies SSRF during OAuth metadata discovery, DNS rebinding, session hijacking, local-server startup risk, and stdio proxy escalation. [MCP Security Best Practices](https://modelcontextprotocol.io/specification/2025-11-25/basic/security_best_practices) The authorization specification defines protected-resource metadata and OAuth interactions but does not turn a bearer token or session ID into authorization for every Tool. [MCP authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization) The transport specification distinguishes stdio from Streamable HTTP and requires validation of `Origin` for HTTP servers. [MCP transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)

### Shell and process invocation

Rust's `std::process::Command` passes arguments without shell interpretation, but its documentation warns that `.bat` files on Windows may be implicitly interpreted by `cmd.exe` and that safe escaping may not always be possible for untrusted arguments. [Rust `Command`](https://doc.rust-lang.org/std/process/struct.Command.html), [Rust process module](https://doc.rust-lang.org/std/process/)

### SQLite

SQLite exposes `SQLITE_OPEN_NOFOLLOW` to prevent opening a database through a symbolic link. [SQLite open flags](https://www.sqlite.org/c3ref/open.html) SQLite's security guidance recommends defensive mode and disabling trusted schema when a database may be altered by an attacker. [SQLite security guidance](https://sqlite.org/security.html) SQLite also warns against network filesystems, changing database identity with links while open, and inheriting a live connection across `fork()`. [How to corrupt SQLite](https://www.sqlite.org/howtocorrupt.html)

### Supply chain and native code

Cargo recommends committing `Cargo.lock` for applications to preserve resolved dependency versions. [Cargo lock-file guidance](https://doc.rust-lang.org/cargo/guide/cargo-toml-vs-cargo-lock.html) Cargo build scripts execute before compilation and can compile native code or perform arbitrary tasks. [Cargo build scripts](https://doc.rust-lang.org/cargo/reference/build-scripts.html) Registry checksums detect mismatched package contents but do not make a malicious package version safe. [Cargo source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html) RustSec's advisory database and `cargo-audit` provide ecosystem vulnerability checks. [RustSec advisory database](https://github.com/rustsec/advisory-db), [`cargo-audit`](https://github.com/rustsec/rustsec/tree/main/cargo-audit)

### Memory and excessive agency

OWASP lists memory poisoning, tool abuse, excessive agency, and denial-of-wallet among agent risks. These are general guidelines, not product incidents. [OWASP AI Agent Security Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html), [OWASP Excessive Agency](https://genai.owasp.org/llmrisk/llm062025-excessive-agency/) OpenAI's ChatGPT agent system card describes disabling Memory at launch as a prompt-injection mitigation and restricting terminal network access. [System card](https://deploymentsafety.openai.com/chatgpt-agent)

## Scope of the retained evidence

The incidents support authority provenance, complete mediation and testing across
trust boundaries. They do not prove that Arany is vulnerable to the same flaws,
that a named mitigation is sufficient on every OS, or that current release gates
have passed. Source dates remain unchanged; recheck advisories before using them
to assess an upstream version. Accepted controls live in the owners linked above,
and scoped Arany findings remain under `docs/security/findings/`.
