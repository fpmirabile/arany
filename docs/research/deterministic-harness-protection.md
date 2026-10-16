# Deterministic protection: enforcement rationale and alternatives

**Source review:** 2026-09-28.
**Question:** what would establish that agent-executed code cannot access a
protected resource, and why does enforcement need a separate boundary?

[Security rules](../../agents/security.md), [Tool behavior](../specs/tools.md)
and [Tool spec](../specs/tools.md) own the accepted contract. This report
retains threat-model reasoning and source comparisons. The illustrative no-DB
property and alternative backends are not available profiles or implementation
instructions. [Platform research](./effect-guard-platform-base-2026-10.md) and
[macOS recovery research](./macos-guard-crash-recovery-in-harnesses-2026-10.md)
contain the later focused investigations; [next steps](../../NEXT_STEPS.md)
owns missing evidence.

Facts below refer to the source-review date. Inferences and alternatives do not
establish current upstream behavior or Arany platform support.

## Why enforcement is a separate responsibility

A command name, shell string or model explanation does not describe every effect
of a process and its descendants. The reference-monitor model requires mediation
of every access, resistance to tampering and a mechanism small enough to analyze.
[NIST](https://csrc.nist.gov/glossary/term/reference_monitor) and
[Saltzer and Schroeder](https://web.mit.edu/saltzer/www/publications/protection/Basic.html)
provide the underlying principles: fail-safe defaults, complete mediation,
separation of privilege and least privilege.

The Engine owns lifecycle; deterministic Policy owns authorization; Guard owns
effective isolation and supervision. The process boundary is justified by
privilege and failure ownership. It does not require one crate per concept or a
policy-language framework. A local sandbox also cannot constrain a remote MCP
server: every route capable of the protected effect needs its own mediation.

## 1. Define the security property before the configuration

“Cannot access the DB” is ambiguous until the protected subject, target, effect, duration, and residual channels are named.

**Illustrative strict property:**

> During run `R`, every untrusted tool process and descendant is unable to establish an external network connection, resolve or connect to a host Unix/abstract socket, read database credentials, inherit an existing database handle, invoke a database-capable tool/MCP server, or read the harness memory store directly. The trusted engine may continue to call the model provider and its own storage adapters outside that sandbox.

That property is stronger and more testable than “block Postgres” because it denies the classes of effect that make database access possible.

An alternative, weaker property is sometimes necessary:

> During run `R`, untrusted processes may send brokered HTTP requests only to an approved endpoint set, but cannot open raw sockets or receive credentials. They cannot directly connect to the named database resources.

This permits package registries or documentation sites, but it cannot prove that an allowed endpoint will never act as a proxy to a database. The UI and logs must distinguish these guarantees instead of showing one `sandboxed: true` flag.

## 2. Threat model

### 2.1 Protected assets

- Database contents, integrity, availability, roles, and credentials.
- Harness memory, including an embedded SQLite file or remote memory service.
- Provider/API credentials and user credentials available on the host.
- Files outside the authorized workspace and protected paths inside it.
- The integrity of policy configuration, approvals, grants, and audit receipts.
- Host availability: CPU, memory, process identifiers, disk space, output buffers, and network capacity.

### 2.2 Attacker-controlled inputs

- Model output and proposed tool arguments.
- Repository files, build scripts, tests, package hooks, generated binaries, and downloaded dependencies.
- Tool and MCP responses.
- File names, symlinks, redirects, DNS answers, archive contents, and process output.
- A child process that intentionally tries to escape restrictions or exhaust resources.

The model is not treated as malicious by default, but the design assumes prompt injection or a mistaken plan can make it request the same effects as malicious code.

### 2.3 Trusted computing base

For a local native sandbox, the trusted computing base includes:

- the operating-system kernel and enabled isolation mechanisms;
- the small guard/policy/enforcement process;
- the normalized policy snapshot and trusted configuration origin;
- the composition code that passes only intended file descriptors, mounts, environment entries, and IPC handles;
- any network or secret broker used to mediate allowed access.

The engine is trusted to maintain run state but should not possess the authority to bypass a non-overridable guard policy. Provider clients, UIs, repository configuration, plugins, MCP servers, and tool processes are outside the protection runtime's trusted computing base.

### 2.4 Limits of the illustrative property

- Kernel, hypervisor, firmware, or guard-process compromise.
- Microarchitectural side channels.
- Preventing a human from using their ordinary terminal outside the harness.
- Proving that an allowed external service cannot itself read a database.
- Supporting a strict profile on a platform whose required primitives did not pass conformance tests.

Any product claiming this property needs these exclusions in its contract. “Deterministic” must never be presented as “unbreakable.”

## 3. Why effect-level enforcement is required

### 3.1 Command matching is an approval aid

Codex rules compare argument prefixes and specially parse only a constrained class of shell wrappers and compound commands; the official documentation explains the exact cases and applies the most restrictive matching rule. [[Codex rules](https://learn.chatgpt.com/docs/agent-configuration/rules)] **Fact** Claude Code's documentation similarly warns that a command can be written in a different form and not match a prefix rule, while its permission parser handles known compound operators and wrappers. [[Claude Code permissions](https://code.claude.com/docs/en/permissions)] **Fact**

Both products distinguish those pre-execution decisions from OS enforcement. Claude's documentation states directly that permission decisions are based on command strings or a classifier, while the OS sandbox continues to constrain the running process even if an allowed command does more than its name suggests. [[Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing#how-sandboxing-relates-to-permissions-and-permission-modes)] **Fact** OpenAI likewise documents sandbox modes as the technical boundary for filesystem and network access, with approval policy controlling when Codex asks before running commands. [[Codex sandboxing](https://learn.chatgpt.com/docs/sandboxing)] [[Codex approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security)] **Fact**

**Inference:** a command rule is useful for deciding whether to ask the user, but it cannot establish “no DB” for arbitrary native code. The actual database effects occur through filesystem access, socket creation/connection, credential acquisition, another privileged tool, or an already-open handle. Those are the points that must be mediated.

### 3.2 Structured execution still matters

Explicit executable identity, arguments, working directory, environment and limits make a launch auditable without interpolating model text into a shell command. They still cannot predict the executable's effects. The [Tool contract](../specs/tools.md) owns the admitted launch shape and interpreter policy; OS enforcement supplies the effect boundary.

### 3.3 Detection and observability are not enforcement

System-call tracing, eBPF telemetry, audit logs, and model classifiers can reveal or explain attempted access. If they observe an effect only after the kernel allowed it, they are too late to provide the property. They belong in verification and incident response, not in the primary allow/deny path.

## 4. Enforcement backends and honest portability

### 4.1 Linux native: best first target

Linux has complementary primitives, not one magic sandbox:

- Landlock restricts ambient filesystem and network rights for a process and descendants. The reviewed documentation included filesystem controls, TCP/UDP port rules, abstract Unix-socket scoping, ABI discovery, and `no_new_privs`. [[Landlock userspace API](https://cdn.kernel.org/doc/html/latest/userspace-api/landlock.html)] **Fact**
- A network namespace isolates devices, protocol stacks, routing/firewall state, ports, and the abstract Unix-socket namespace. [[Linux `network_namespaces(7)`](https://man7.org/linux/man-pages/man7/network_namespaces.7.html)] **Fact**
- seccomp reduces the exposed syscall surface, but the kernel documentation explicitly says it is not a sandbox by itself. [[Linux seccomp documentation](https://kernel.org/doc/html/latest/userspace-api/seccomp_filter.html)] **Fact**
- cgroup v2 provides hard process-count and memory controls, CPU bandwidth control, and whole-subtree termination through `cgroup.kill`. [[Linux cgroup v2](https://cdn.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html)] **Fact**
- Bubblewrap constructs mount, user, PID, network, IPC, and other namespaces, but its maintainers explicitly state that it is a construction tool and the security boundary depends entirely on the arguments supplied. [[Bubblewrap README](https://github.com/containers/bubblewrap/blob/main/README.md)] **Fact**

Landlock's official example recommends degrading gracefully across older ABIs. That is appropriate for ordinary applications seeking incremental hardening. [[Landlock compatibility example](https://cdn.kernel.org/doc/html/latest/userspace-api/landlock.html)] **Fact** It is **not** sufficient for a profile that promises a required restriction. The guard must map each profile requirement to a minimum backend capability and fail closed if the running kernel lacks it.

The investigated Linux composition combined mount visibility, network isolation, Landlock, seccomp, namespace separation and cgroup resources. These mechanisms cover different effects; their presence alone does not prove the complete boundary. [Tool spec](../specs/tools.md) own Arany's selected composition.

### 4.2 macOS

Apple's supported App Sandbox uses kernel-enforced entitlements to limit files, network connections, devices, and other resources; embedded command-line helpers inherit the containing application's sandbox configuration. [[Apple App Sandbox](https://developer.apple.com/documentation/security/app_sandbox)] [[Protecting user data with App Sandbox](https://developer.apple.com/documentation/security/protecting-user-data-with-app-sandbox)] **Fact** Codex and Claude Code both document using Seatbelt for local command isolation on macOS. [[Codex sandboxing](https://learn.chatgpt.com/docs/sandboxing)] [[Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing#os-level-enforcement)] **Fact**

The original comparison left dynamic per-run policy and supportability unresolved. Later Arany-specific evidence lives in [macOS recovery research](./macos-guard-crash-recovery-in-harnesses-2026-10.md); this comparison neither requires a VM fallback nor proves a native guarantee.

### 4.3 Windows

Windows AppContainer/LPAC is a real least-privilege boundary: access to files, registry, processes, credentials, devices, and network is denied unless the necessary capabilities and ACLs are present. [[Microsoft AppContainer isolation](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation)] [[Launching an AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer)] **Fact** Windows Job Objects manage process trees, enforce process/time/memory limits, and can terminate the tree when the owning handle closes. [[Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)] **Fact** Windows Filtering Platform provides kernel network enforcement and can classify by application or user identity. [[Windows Filtering Platform](https://learn.microsoft.com/en-us/windows/win32/fwp/about-windows-filtering-platform)] [[Application Layer Enforcement](https://learn.microsoft.com/en-us/windows/win32/fwp/application-layer-enforcement--ale-)] **Fact**

Compatibility is the hard part. Codex's reviewed Windows documentation described elevated mode using dedicated lower-privilege sandbox users, filesystem boundaries, firewall rules, and local security policy. Its unelevated fallback instead uses a restricted token and ACLs and provides weaker network isolation. [[Codex Windows sandbox](https://learn.chatgpt.com/docs/windows/windows-sandbox)] **Fact**

**Gate:** compare LPAC plus brokered resources against a dedicated-principal/WFP design using the same conformance suite. A one-time elevated setup may be required for strong outbound denial. If neither backend satisfies the strict profile on a host, report `ProtectionUnavailable`; do not use proxy variables as a security fallback.

### 4.4 Higher-assurance or multi-tenant execution

A VM, remote worker or hardened application kernel is an alternative for hostile multi-tenant code, with a separate operational cost and threat model. gVisor interposes a userspace application kernel between the workload and host kernel, uses Linux primitives as defense in depth, and documents both its stronger isolation shape and its residual limits. [[gVisor architecture](https://gvisor.dev/docs/architecture_guide/intro/)] [[gVisor security model](https://gvisor.dev/docs/architecture_guide/security/)] **Fact** It also documents workload-dependent overhead, especially for I/O and networking. [[gVisor production guide](https://gvisor.dev/docs/user_guide/production/)] **Fact**

Such an alternative needs a concrete requirement and evidence before adoption; it is not an Arany implementation commitment.

### 4.5 WASI for narrow plugins, not a shell replacement

For plugins compiled to WebAssembly, WASI/Wasmtime provides directory capabilities through preopened directories and denies access outside them. [[Wasmtime WASI tutorial](https://github.com/bytecodealliance/wasmtime/blob/main/docs/WASI-tutorial.md)] **Fact** This is attractive for purpose-built tools, but it cannot run arbitrary existing developer commands unchanged. The reviewed Wasmtime advisories also demonstrate why runtime patching and adversarial conformance tests remain necessary even with a capability design. [[Wasmtime security advisories](https://github.com/bytecodealliance/wasmtime/security/advisories)] **Fact**

## 5. Lessons from the reviewed Codex and Claude Code behavior

The goal is not to claim broad superiority without equivalent testing. Both products already combine deterministic OS isolation with configurable approvals.

| Product behavior supported by official docs | Lesson for this harness |
|---|---|
| Codex has `read-only`, `workspace-write`, and `danger-full-access` sandbox modes, configurable writable roots/network, managed allowed modes, environment filtering, command rules, and managed requirements. [[Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)] | Expose policy as structured configuration and let administrators prevent weaker modes. |
| Codex uses OS-level sandboxing for local enforcement and supports configurable approval policies. [[Codex sandboxing](https://learn.chatgpt.com/docs/sandboxing)] [[Codex approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security)] | Keep probabilistic review outside the enforcement boundary; this project can choose never to auto-widen strict profiles. |
| Claude's Bash sandbox uses Seatbelt on macOS and Bubblewrap on Linux/WSL2, applies restrictions to descendants, and separates sandboxing from permission modes. [[Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing)] | Preserve the separation between intent/approval and effect containment. |
| Claude can require `failIfUnavailable`, disable unsandboxed retry, and lock managed read/domain settings. [[Claude managed sandbox settings](https://code.claude.com/docs/en/sandboxing#enforce-sandboxing-with-managed-settings)] | Make fail-closed behavior a first-class profile requirement, not a hidden global switch. |
| Claude documents that default readable host credentials need explicit protection and that some settings/exclusions can widen policy. [[Claude credential protection](https://code.claude.com/docs/en/sandboxing#protect-credentials)] | Default to an allowlisted filesystem/environment and treat every bypass/exclusion as an explicit attested capability. |

**Inference:** explicit requested, granted and effective capabilities explain an enforcement decision more accurately than an intelligent-command-detector claim. Backend receipts and adversarial tests must establish the restrictions actually applied.

## Advisory models and evidence limits

The original investigation considered TypeSafe/Jev as an optional intent adviser.
Its [source guidance](https://github.com/typesafe-ai/skills/blob/main/skills/typesafe-ai/SKILL.md)
distinguishes typed output from truth. A classifier may suggest a restriction or
review; it cannot establish containment or widen a deterministic grant. No adviser
integration is implied by retaining this comparison.

A strict no-DB claim also excludes credentials, inherited connections, host Unix
sockets, container-control sockets and indirect Tool/MCP access. Database-side
roles and authentication can add defense in depth, but cannot replace the local
boundary. [PostgreSQL role attributes](https://www.postgresql.org/docs/current/role-attributes.html),
[host authentication](https://www.postgresql.org/docs/current/auth-pg-hba-conf.html).
Data already copied into the admitted Workspace remains a separate disclosure
question.

An enforcement receipt describes installed restrictions and observed execution;
it cannot prove an uncompromised kernel, exactly-once effects or remote-service
behavior. Attestations belong to the prepared execution, not a reusable promise
for another process. [Testing rules](../../agents/testing.md) own verification;
active work belongs in [planning](../../planning/README.md).
