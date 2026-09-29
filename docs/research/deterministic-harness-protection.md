# Deterministic protection for an agent harness

**Status:** research and architectural recommendation  
**Research date:** 2026-09-28  
**Question:** How can the harness enforce parameterized protections deterministically—for example, guarantee that agent-executed code cannot access a database—and should that responsibility live in the engine or in a separate module?

## Reading guide

This report uses four labels deliberately:

- **Fact**: directly supported by a cited primary source: official documentation, source code, a specification, or an original paper.
- **Inference**: a conclusion derived from facts, but not itself promised by a source.
- **Recommendation**: the proposed design for this repository.
- **Gate**: something that must be demonstrated by a spike or conformance test before it becomes a guarantee.

“Deterministic” means that the same normalized policy input produces the same authorization decision, and that allowed effects are enforced by a trusted mechanism at the point where the effect occurs. It does **not** mean bug-free, formally verified, or secure against a compromised kernel. Every guarantee in this document is bounded by the threat model in section 2.

## Executive conclusion

The idea is feasible, but the security objective should be changed from **deterministically detect a dangerous command** to **deterministically prevent an unauthorized effect**.

A command name, shell string, or model explanation does not describe everything the resulting process and its descendants may do. A harmless-looking binary may load code, open a Unix socket, use an inherited file descriptor, call a remote MCP server, or connect to a database directly. Command analysis can improve the user experience, but it is not a security boundary.

The reference-monitor model provides the right target: the enforcement mechanism must be invoked for every access, resist tampering, and be small enough to analyze and test. [[NIST reference monitor definition](https://csrc.nist.gov/glossary/term/reference_monitor)] **Fact** Saltzer and Schroeder's original principles add fail-safe defaults, complete mediation, separation of privilege, and least privilege. [[Saltzer and Schroeder, *The Protection of Information in Computer Systems*](https://web.mit.edu/saltzer/www/publications/protection/Basic.html)] **Fact**

**Recommendation:** divide the responsibility into three layers:

1. **The domain and engine own semantics.** They define effect classes, capability requests, approvals, grants, run binding, and the invariant that no effect executes without a valid grant.
2. **A deterministic policy module owns decisions.** It validates profiles, merges trusted policy sources by intersection, and turns a typed request into `Deny`, `NeedsApproval`, or a bounded `Grant`. It performs no network, filesystem, model, or clock I/O during evaluation.
3. **A separate protection runtime owns enforcement.** A small `harness-guard` process is the final policy decision and enforcement point. Platform adapters configure kernel or VM isolation, start the process, supervise the whole process tree, and return an attestation of the restrictions actually installed. The engine must reject the run if that attestation cannot satisfy the requested protection level.

```text
untrusted model / repository / tool request
                    │
                    ▼
             typed EffectRequest
                    │
          engine: lifecycle only
                    │
                    ▼
        harness-guard: final decision
       policy snapshot + approval proof
                    │
             Deny / Grant
                    │
                    ▼
      platform enforcement adapter
  Linux / macOS / Windows / VM / remote
                    │
          EnforcementAttestation
                    │
         exact granted process tree
```

This is one **conceptual protection module**, but it merits a separate process because it has a different privilege level, trust boundary, platform dependency set, failure mode, and lifecycle from the engine. The engine should not be able to turn a string into ambient host authority.

For a strict “no database access” profile, “no DB tool” is necessary but insufficient. The protected process must also receive no database credential, no inherited database connection, no visible database Unix socket or Docker socket, no route to a database endpoint, and no alternate tool/MCP path that can perform the operation outside the local sandbox. If arbitrary outbound network access remains available, the strongest honest claim is only “no **direct** access to the named database”; an allowed service could still proxy or expose the data.

Jev can be useful as an **optional adviser** that classifies intent, chooses an explanation, or prioritizes review. It cannot be the component that grants authority. TypeSafe's own guidance says typed output guarantees the interface, not truth, and that confidence thresholds must be validated for the target consequences. [[TypeSafe official skill](https://github.com/typesafe-ai/skills/blob/main/skills/typesafe-ai/SKILL.md)] **Fact** A probabilistic result may recommend a narrower profile or a human review; it must never widen the deterministic capability envelope.

## 1. Define the security property before the configuration

“Cannot access the DB” is ambiguous until the protected subject, target, effect, duration, and residual channels are named.

**Recommended strict property:**

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

### 2.4 Explicit non-goals for the first version

- Kernel, hypervisor, firmware, or guard-process compromise.
- Microarchitectural side channels.
- Preventing a human from using their ordinary terminal outside the harness.
- Proving that an allowed external service cannot itself read a database.
- Supporting a strict profile on a platform whose required primitives did not pass conformance tests.

These exclusions must appear in product documentation. “Deterministic” must never be presented as “unbreakable.”

## 3. Why effect-level enforcement is required

### 3.1 Command matching is an approval aid

Codex rules compare argument prefixes and specially parse only a constrained class of shell wrappers and compound commands; the official documentation explains the exact cases and applies the most restrictive matching rule. [[Codex rules](https://learn.chatgpt.com/docs/agent-configuration/rules)] **Fact** Claude Code's documentation similarly warns that a command can be written in a different form and not match a prefix rule, while its permission parser handles known compound operators and wrappers. [[Claude Code permissions](https://code.claude.com/docs/en/permissions)] **Fact**

Both products distinguish those pre-execution decisions from OS enforcement. Claude's documentation states directly that permission decisions are based on command strings or a classifier, while the OS sandbox continues to constrain the running process even if an allowed command does more than its name suggests. [[Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing#how-sandboxing-relates-to-permissions-and-permission-modes)] **Fact** OpenAI likewise documents sandbox modes as the technical boundary for filesystem and network access, with approval policy controlling when Codex asks before running commands. [[Codex sandboxing](https://learn.chatgpt.com/docs/sandboxing)] [[Codex approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security)] **Fact**

**Inference:** a command rule is useful for deciding whether to ask the user, but it cannot establish “no DB” for arbitrary native code. The actual database effects occur through filesystem access, socket creation/connection, credential acquisition, another privileged tool, or an already-open handle. Those are the points that must be mediated.

### 3.2 Structured execution still matters

The harness should accept an `ExecSpec` containing an executable, an argument array, a working-directory handle, an explicit environment map, limits, and a requested protection profile. It should not interpolate model text into a shell command.

This prevents a separate class of injection and makes the request auditable, but it still does not predict the behavior of the executable. `ExecSpec` is the safe launch interface; the sandbox is the effect boundary.

### 3.3 Detection and observability are not enforcement

System-call tracing, eBPF telemetry, audit logs, and model classifiers can reveal or explain attempted access. If they observe an effect only after the kernel allowed it, they are too late to provide the property. They belong in verification and incident response, not in the primary allow/deny path.

## 4. Capability-oriented policy model

This report uses “capability-oriented” rather than claiming a pure object-capability system. A **capability request** names a desired effect on a resource. A **grant** is an opaque, scoped, expiring authority issued by the guard after policy evaluation. Possessing a textual path or tool name is not itself authority.

### 4.1 Effect catalog

Start with a closed, versioned set of effect classes:

| Effect class | Example resource scope | Enforcement point |
|---|---|---|
| `fs.read` | directory/file handle | mount/LSM/AppContainer/VM policy |
| `fs.write` | workspace subtree | mount/LSM/ACL policy |
| `process.exec` | executable identity or visible toolchain | process launcher plus filesystem policy |
| `net.connect` | none, broker, or endpoint identity | network namespace/firewall/proxy |
| `net.listen` | local port and interface | sandbox network policy |
| `secret.use` | secret ID plus destination audience | secret broker, never raw environment by default |
| `tool.invoke` | native tool ID and operation | engine tool router |
| `mcp.invoke` | server ID and tool ID | MCP router; local process sandbox is insufficient |
| `memory.read/write` | workspace/session scope | memory adapter authorization |
| `db.connect/query` | database ID, role, operation | dedicated DB adapter or broker |

The explicit `db.*` class is valuable even though network and secrets ultimately enforce it: it gives the user a semantic control and creates auditable intent. The lower-level effects remain necessary because a shell program can implement a database client without invoking a named database tool.

### 4.2 Requested, granted, and effective capabilities

Do not collapse three different states:

```text
requested: what a model/tool says it needs
granted:   what deterministic policy authorizes
effective: what the selected backend proves it installed
```

Execution is allowed only if `effective` is at least as restrictive as `granted` and satisfies every required protection. Missing enforcement is an error, not an implicit approval request.

A grant should be bound to:

- authenticated actor and policy owner;
- workspace, session, and run IDs;
- normalized request digest;
- exact operations and resource handles;
- creation, expiry, and maximum-use count;
- policy snapshot/version digest;
- approval evidence, if required;
- a nonce to prevent replay.

The grant must not contain a raw reusable secret. For external services, it should identify a secret handle and audience that only the broker can resolve.

### 4.3 Decision algebra

Use a small decision type:

```text
Deny(reason_code)
NeedsApproval(request_digest, maximum_approvable_scope)
Grant(scoped_capabilities, limits, policy_digest)
```

`NeedsApproval` is not authority. A human approval can select only within the policy's `maximum_approvable_scope`; it cannot override a managed hard deny such as `db.* = forbidden` or `network = none`.

Policy errors, unknown fields, unsupported profile versions, missing context, stale approvals, failed signature/digest checks, and enforcement setup errors all resolve to `Deny` or `ProtectionUnavailable`.

### 4.4 Policy provenance and merging

Recommended precedence:

```text
managed/admin hard constraints
          ∩
user hard constraints
          ∩
workspace policy (may only narrow)
          ∩
run profile and individual grant
```

The merge operation is intersection, not last-writer-wins and not array concatenation that can accidentally reopen access. Repository policy is attacker-controlled input: it may request less authority but cannot grant itself more. All source documents are normalized into one immutable policy snapshot before a run starts.

Repository instruction files such as `AGENTS.md` or `CLAUDE.md` may be one of those workspace policy sources, but only a closed, typed policy block may enter deterministic authorization. Their ordinary prose remains model guidance, not an enforceable security boundary. The loader must preserve file and directory provenance, reject malformed or unknown security fields, and apply repository-authored rules as narrow-only restrictions. The exact discovery, precedence, fallback, and compilation contract is defined in [Instruction Markdown and deterministic policy enforcement](./instruction-markdown-and-policy-enforcement.md).

Live widening creates races and destroys reproducibility. **Recommendation:** restrictions may be narrowed for a running process, but an approval that widens authority creates a new grant generation and a fresh protected process. Landlock itself composes restrictions in layers, which is compatible with monotonic narrowing. [[Linux Landlock documentation](https://cdn.kernel.org/doc/html/latest/userspace-api/landlock.html)] **Fact**

### 4.5 Parameter schema

The stored format can be TOML, but it should deserialize into a closed typed schema with `deny_unknown_fields` semantics. The example is illustrative, not a finalized public API:

```toml
version = 1
profile = "strict-no-db"

[requirements]
isolation_tier = "native-strong"
fail_if_unavailable = true
allow_unsandboxed_retry = false

[filesystem]
default = "deny"
read = ["handle:workspace", "handle:toolchain-ro"]
write = ["handle:workspace"]
execute = ["handle:toolchain-ro", "handle:workspace"]
allow_host_unix_sockets = false

[network]
mode = "none"
allow_loopback_inside_namespace = true

[secrets]
expose = []
broker = []

[tools]
allow = ["process.exec", "fs.read", "fs.write"]
deny = ["db.*", "mcp.*"]

[process]
max_children = 64
wall_time_ms = 300_000
cpu_time_ms = 120_000
memory_bytes = 2_147_483_648
stdout_bytes = 16_777_216
stderr_bytes = 16_777_216

[approval]
may_expand_filesystem = false
may_expand_network = false
may_expand_secrets = false
```

The names under `handle:` are resolved by trusted composition code into already-open directory handles or backend-specific objects. Untrusted text must not become a privileged path after a check-then-use gap.

For brokered web access, replace `network.mode = "none"` with a structured broker policy. The broker, not the child, resolves DNS and opens the connection; it revalidates redirects, destination address classes, method, port, TLS name, response size, and timeouts. A hostname allowlist implemented only through DNS or proxy environment variables is not a complete boundary.

## 5. Recommended module and process placement

### 5.1 Domain

The domain owns stable, implementation-free types and invariants:

- `EffectClass`, `ResourceRef`, `CapabilityRequest`, and `GrantId`;
- protection requirement and effective-capability vocabulary;
- approval binding to one normalized request and scope;
- the invariant “a side effect cannot begin without a current grant.”

It must not know Landlock, Seatbelt, AppContainer, Cedar, OPA, Tokio, shell syntax, IP tables, or provider schemas.

### 5.2 Engine

The engine owns orchestration:

- request admission and run state;
- pausing and resuming for an approval;
- selecting a named protection profile;
- refusing to dispatch without an accepted attestation;
- recording sanitized decision and execution events;
- cancelling the run and asking the guard to terminate its complete process tree.

The engine is not the final enforcement point and never resolves secret handles for untrusted processes.

### 5.3 Policy module

Begin with a small Rust library containing:

- schema validation and normalization;
- policy provenance and intersection;
- pure request evaluation;
- stable reason codes;
- grant serialization/digests;
- property tests for default deny, monotonicity, and no widening.

It can initially use ordinary Rust data rather than inventing a general policy language. If policy authoring becomes a product surface, Cedar is a credible Rust-native candidate: its official implementation is a Rust crate, supports schemas and validation, defaults to deny, and lets a matching `forbid` override permits. [[Cedar repository](https://github.com/cedar-policy/cedar)] [[Cedar authorization algorithm](https://docs.cedarpolicy.com/auth/authorization.html)] **Fact**

One important integration caveat is that Cedar reports policy evaluation errors in diagnostics and otherwise skips the errored policy. The application can choose a different result. [[Cedar authorization algorithm](https://docs.cedarpolicy.com/auth/authorization.html#request-authorization)] **Fact** For this harness, any validation or evaluation error in a security-relevant policy snapshot should make the request fail closed.

OPA/Rego is another mature option and explicitly separates policy decision from enforcement; its official guidance documents default-deny policy style. [[OPA documentation](https://www.openpolicyagent.org/docs)] [[OPA policy security FAQ](https://www.openpolicyagent.org/docs/faq#how-do-i-write-policies-securely)] **Fact** It adds a Go/Wasm/sidecar integration and a more general language. **Recommendation:** do not choose Cedar or OPA before the capability schema and real authoring needs exist. A small typed evaluator is deeper and easier to verify for the first profiles.

### 5.4 Guard process

`harness-guard` should be a separate, deliberately small process and the final authority. Its narrow interface is conceptually:

```text
prepare(policy_snapshot, capability_request, approval_proof?)
    -> Denial | PreparedExecution(grant, attestation)

spawn(prepared_execution, exec_spec)
    -> ProtectedProcessHandle

cancel(process_handle)
    -> TerminationReceipt
```

The guard:

- authenticates its local caller and validates message size/version;
- evaluates or re-evaluates the final request against its policy snapshot;
- resolves approved resource handles without following attacker-controlled paths later;
- clears inherited environment and closes all file descriptors except an explicit allowlist;
- configures filesystem, network, identity, syscall, and resource boundaries before untrusted code runs;
- creates and supervises the complete child process tree;
- never offers a generic “run unsandboxed” fallback for strict profiles;
- returns a structured attestation and bounded audit receipt.

This split follows separation of privilege and reduces the amount of code that holds setup authority. Codex's current Windows documentation similarly separates one-time elevated sandbox setup from lower-privilege command execution and verification. [[Codex Windows sandbox](https://learn.chatgpt.com/docs/windows/windows-sandbox)] **Fact**

### 5.5 Tools, MCP, memory, and providers remain separate effect paths

Complete mediation means every path to the effect is covered:

- A native process goes through `harness-guard`.
- A first-party function tool goes through the deterministic tool router.
- A local MCP server is itself run with a declared profile or is treated as separately trusted.
- A remote MCP call is authorized at the router; the local sandbox cannot constrain what the remote server does.
- Harness memory stays behind the memory adapter and workspace/session authorization. Its SQLite file, if any, is outside mounts visible to tools.
- Provider API calls occur in the trusted provider adapter; provider keys never enter tool environments.

If a remote MCP server has database authority, `network = none` on the child process does not prevent the engine from asking that server to query the database. Therefore `mcp.invoke` and `tool.invoke` must be part of the same capability decision.

## 6. Worked example: strict “no DB” run

Assume the user asks the agent to run tests while forbidding all database access.

### 6.1 Admission

1. The client selects `strict-no-db`.
2. The engine records the profile ID but does not interpret free text such as “probably no database.”
3. The guard loads the trusted profile snapshot. Repository files cannot widen it.
4. The model proposes `cargo test` as an executable plus argument array.
5. The request asks for workspace read/write, toolchain read/execute, process spawn, and bounded output. It requests no network, secret, DB, MCP, or memory capability.

### 6.2 Preparation

The guard creates a fresh protection domain before starting the command:

- mount/filesystem view containing only the workspace, read-only toolchain/runtime files, synthetic temp space, and minimal devices;
- no host home directory, credential directories, harness state database, SSH agent socket, database socket directories, Docker/container runtime socket, or host `/proc` view;
- empty explicit environment with only approved non-secret entries;
- no inherited descriptors other than stdio and a constrained control/output channel;
- an isolated network namespace with no external interface or veth;
- syscall hardening and no-new-privileges;
- CPU, memory, PID, output, and wall-clock limits;
- whole-tree termination tied to the guard lifecycle.

The guard then returns an attestation such as:

```text
backend: linux-native-v1
filesystem: mount-allowlist + landlock-abi-10
network: new-netns/no-veth; external-connect=false
unix-sockets: host-paths-not-mounted; abstract-namespace-isolated
secrets: none
inherited-fds: [stdin, stdout, stderr, control]
resources: cgroup-v2(memory=2GiB, pids=64, cpu=120s)
unsandboxed-retry: false
policy-digest: sha256:...
```

The engine checks this structured result against the profile. It does not accept a Boolean `sandboxed` field.

### 6.3 Attempts and expected outcomes

| Attempt from any descendant | Deterministic barrier |
|---|---|
| `psql` to a cloud or LAN host | no external network interface/route |
| database client to `127.0.0.1` | isolated loopback contains no host DB |
| connect to a filesystem Unix socket | socket path is not mounted/readable |
| connect to an abstract Unix socket | separate Linux network namespace |
| read `DATABASE_URL`, cloud keys, or `.pgpass` | explicit environment and filesystem allowlists |
| use an already-open DB connection | guard closes non-allowlisted inherited descriptors before exec |
| mount the host or gain privilege | no-new-privileges, dropped capabilities, namespace and syscall policy |
| ask a DB MCP tool | router did not expose/grant `mcp.invoke` for that server/tool |
| read the harness memory SQLite file | memory storage is outside the visible filesystem and process |
| reach the host through Docker | Docker/container socket is not visible |
| fork until the host fails | PID and memory limits plus whole-cgroup kill |

The property still depends on the kernel and guard being correct. It also does not protect data already copied into the workspace before the run.

### 6.4 Database-side defense in depth

If the organization controls the database, use a separate no-login/no-privilege role or network authentication policy as another independent layer. PostgreSQL, for example, only permits roles with `LOGIN` as initial connection identities, and its host-based authentication denies access when no record matches. [[PostgreSQL role attributes](https://www.postgresql.org/docs/current/role-attributes.html)] [[PostgreSQL `pg_hba.conf`](https://www.postgresql.org/docs/current/auth-pg-hba-conf.html)] **Fact**

That is defense in depth, not the primary local guarantee: a leaked privileged credential or another database product could otherwise bypass it.

## 7. Enforcement backends and honest portability

### 7.1 Linux native: best first target

Linux has complementary primitives, not one magic sandbox:

- Landlock restricts ambient filesystem and network rights for a process and descendants. Current documentation includes filesystem controls, TCP/UDP port rules, abstract Unix-socket scoping, ABI discovery, and `no_new_privs`. [[Landlock userspace API](https://cdn.kernel.org/doc/html/latest/userspace-api/landlock.html)] **Fact**
- A network namespace isolates devices, protocol stacks, routing/firewall state, ports, and the abstract Unix-socket namespace. [[Linux `network_namespaces(7)`](https://man7.org/linux/man-pages/man7/network_namespaces.7.html)] **Fact**
- seccomp reduces the exposed syscall surface, but the kernel documentation explicitly says it is not a sandbox by itself. [[Linux seccomp documentation](https://kernel.org/doc/html/latest/userspace-api/seccomp_filter.html)] **Fact**
- cgroup v2 provides hard process-count and memory controls, CPU bandwidth control, and whole-subtree termination through `cgroup.kill`. [[Linux cgroup v2](https://cdn.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html)] **Fact**
- Bubblewrap constructs mount, user, PID, network, IPC, and other namespaces, but its maintainers explicitly state that it is a construction tool and the security boundary depends entirely on the arguments supplied. [[Bubblewrap README](https://github.com/containers/bubblewrap/blob/main/README.md)] **Fact**

Landlock's official example recommends degrading gracefully across older ABIs. That is appropriate for ordinary applications seeking incremental hardening. [[Landlock compatibility example](https://cdn.kernel.org/doc/html/latest/userspace-api/landlock.html)] **Fact** It is **not** sufficient for a profile that promises a required restriction. The guard must map each profile requirement to a minimum backend capability and fail closed if the running kernel lacks it.

**Recommended Linux composition:** mount namespace/allowlist for visibility, network namespace for no egress, Landlock as an additional monotonic layer, seccomp for attack-surface reduction and dangerous escape primitives, user/PID/IPC namespaces, and cgroup v2 for resources and cancellation. No single item replaces the others.

### 7.2 macOS

Apple's supported App Sandbox uses kernel-enforced entitlements to limit files, network connections, devices, and other resources; embedded command-line helpers inherit the containing application's sandbox configuration. [[Apple App Sandbox](https://developer.apple.com/documentation/security/app_sandbox)] [[Protecting user data with App Sandbox](https://developer.apple.com/documentation/security/protecting-user-data-with-app-sandbox)] **Fact** Codex and Claude Code both document using Seatbelt for local command isolation on macOS. [[Codex sandboxing](https://learn.chatgpt.com/docs/sandboxing)] [[Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing#os-level-enforcement)] **Fact**

**Gate:** prove that the available, supportable mechanism can express per-run dynamic read/write/network policies for arbitrary developer toolchains without relying on an unstable private interface. Until that spike passes, the strict cross-platform promise should use a managed VM/remote Linux worker on macOS rather than silently weakening the profile.

### 7.3 Windows

Windows AppContainer/LPAC is a real least-privilege boundary: access to files, registry, processes, credentials, devices, and network is denied unless the necessary capabilities and ACLs are present. [[Microsoft AppContainer isolation](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation)] [[Launching an AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer)] **Fact** Windows Job Objects manage process trees, enforce process/time/memory limits, and can terminate the tree when the owning handle closes. [[Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)] **Fact** Windows Filtering Platform provides kernel network enforcement and can classify by application or user identity. [[Windows Filtering Platform](https://learn.microsoft.com/en-us/windows/win32/fwp/about-windows-filtering-platform)] [[Application Layer Enforcement](https://learn.microsoft.com/en-us/windows/win32/fwp/application-layer-enforcement--ale-)] **Fact**

Compatibility is the hard part. Codex's current Windows documentation describes elevated mode using dedicated lower-privilege sandbox users, filesystem boundaries, firewall rules, and local security policy. Its unelevated fallback instead uses a restricted token and ACLs and provides weaker network isolation. [[Codex Windows sandbox](https://learn.chatgpt.com/docs/windows/windows-sandbox)] **Fact**

**Gate:** compare LPAC plus brokered resources against a dedicated-principal/WFP design using the same conformance suite. A one-time elevated setup may be required for strong outbound denial. If neither backend satisfies the strict profile on a host, report `ProtectionUnavailable`; do not use proxy variables as a security fallback.

### 7.4 Higher-assurance or multi-tenant execution

For hostile multi-tenant code, offer an isolation tier backed by a VM, remote worker, or hardened application kernel. gVisor interposes a userspace application kernel between the workload and host kernel, uses Linux primitives as defense in depth, and documents both its stronger isolation shape and its residual limits. [[gVisor architecture](https://gvisor.dev/docs/architecture_guide/intro/)] [[gVisor security model](https://gvisor.dev/docs/architecture_guide/security/)] **Fact** It also documents workload-dependent overhead, especially for I/O and networking. [[gVisor production guide](https://gvisor.dev/docs/user_guide/production/)] **Fact**

This should be a profile/tier selection, not a different domain model. The same grant and attestation vocabulary can drive native Linux, gVisor, or a remote microVM.

### 7.5 WASI for narrow plugins, not a shell replacement

For plugins compiled to WebAssembly, WASI/Wasmtime provides directory capabilities through preopened directories and denies access outside them. [[Wasmtime WASI tutorial](https://github.com/bytecodealliance/wasmtime/blob/main/docs/WASI-tutorial.md)] **Fact** This is attractive for purpose-built tools, but it cannot run arbitrary existing developer commands unchanged. Current Wasmtime advisories also demonstrate why runtime patching and adversarial conformance tests remain necessary even with a capability design. [[Wasmtime security advisories](https://github.com/bytecodealliance/wasmtime/security/advisories)] **Fact**

## 8. What to learn from current Codex and Claude Code

The goal is not to claim broad superiority without equivalent testing. Both products already combine deterministic OS isolation with configurable approvals.

| Product behavior supported by official docs | Lesson for this harness |
|---|---|
| Codex has `read-only`, `workspace-write`, and `danger-full-access` sandbox modes, configurable writable roots/network, managed allowed modes, environment filtering, command rules, and managed requirements. [[Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)] | Expose policy as structured configuration and let administrators prevent weaker modes. |
| Codex uses OS-level sandboxing for local enforcement and supports configurable approval policies. [[Codex sandboxing](https://learn.chatgpt.com/docs/sandboxing)] [[Codex approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security)] | Keep probabilistic review outside the enforcement boundary; this project can choose never to auto-widen strict profiles. |
| Claude's Bash sandbox uses Seatbelt on macOS and Bubblewrap on Linux/WSL2, applies restrictions to descendants, and separates sandboxing from permission modes. [[Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing)] | Preserve the separation between intent/approval and effect containment. |
| Claude can require `failIfUnavailable`, disable unsandboxed retry, and lock managed read/domain settings. [[Claude managed sandbox settings](https://code.claude.com/docs/en/sandboxing#enforce-sandboxing-with-managed-settings)] | Make fail-closed behavior a first-class profile requirement, not a hidden global switch. |
| Claude documents that default readable host credentials need explicit protection and that some settings/exclusions can widen policy. [[Claude credential protection](https://code.claude.com/docs/en/sandboxing#protect-credentials)] | Default to an allowlisted filesystem/environment and treat every bypass/exclusion as an explicit attested capability. |

**Recommended differentiator:** do not advertise a more intelligent command detector. Offer a clearer capability vocabulary, profile provenance, non-overridable hard constraints, exact requested/granted/effective states, a portable conformance suite, and receipts that prove which backend restrictions were active for each run.

## 9. Optional Jev integration

`jev.dev` itself currently resolves to the personal site of Jev Forsberg. This report interprets “Jev” as TypeSafe AI's decision model, whose official documentation is at `docs.typesafe.ai`; `tryjev.dev` is not its official documentation. [[jev.dev](https://www.jev.dev/)] [[TypeSafe introduction](https://docs.typesafe.ai/introduction)] **Fact**

Jev is a TypeSafe “System One” decision model that accepts state plus typed questions and returns structured choice/score/yes-no results with probabilities. [[TypeSafe documentation index](https://docs.typesafe.ai/llms.txt)] **Fact** That makes it useful for narrow semantic judgments, but it remains probabilistic and remote.

Safe optional uses:

- explain a deterministic denial in user-facing language;
- classify an ambiguous request into a proposed profile before policy evaluation;
- estimate whether a user should see a high-friction warning;
- prioritize audit events for human review;
- suggest the smallest capability request, which deterministic policy may reduce or deny.

Forbidden use:

```text
Jev says “safe” → grant DB/network/secret access
```

Permitted composition:

```text
Jev suggests “likely needs package registry”
    → code constructs a typed, bounded request
    → deterministic policy denies, asks, or grants
    → guard enforces the resulting grant
```

The integration must be opt-in because it sends selected state to an external service. Minimize and redact inputs; never send credentials, raw private repository contents, database data, or unrestricted transcripts. Give it an explicit timeout and cost bound. If the adviser is unavailable, the deterministic policy continues with its configured default—normally deny or human review. An adviser outage must never cause policy widening.

## 10. Performance and the “fast core” goal

Protection adds real latency: policy evaluation, local IPC, namespace or VM setup, filesystem construction, and process spawn. It should not be assumed away. It can, however, be kept off the token-stream hot path:

- validate and normalize profiles once, caching immutable compiled snapshots by digest;
- evaluate only at run admission and effect requests, not per generated token;
- keep the guard process warm while creating a fresh isolation domain per run or grant generation;
- stream bounded stdout/stderr directly through backpressured channels;
- keep provider inference outside the tool sandbox and never proxy token streaming through the guard;
- measure profile compilation, guard IPC, cold/warm preparation, spawn, cancellation, and cleanup at p50/p95/p99.

Authorization decisions themselves may be cached only when actor, workspace, request, policy digest, approval, and freshness constraints are identical. Enforcement attestations are per prepared execution and must not be reused as proof for a different process.

**Gate:** establish budgets after the Linux prototype. A reasonable product target is “sandbox overhead is small relative to tool runtime and model generation,” but this report does not invent a millisecond promise without measurements.

## 11. Audit receipts without leaking secrets

Record one structured receipt for each decision/execution:

- run/request/grant IDs and normalized request digest;
- profile ID, policy version/digest, and trusted source set;
- decision plus stable reason codes;
- approval actor, scope, and expiry, never the full sensitive prompt by default;
- backend name/version and effective capability attestation;
- process start/exit/cancel/timeout/resource-limit outcome;
- counts and categories of denied effects when the OS exposes them;
- output truncation state and retained artifact references.

Do not log raw environment values, secret handles that can be redeemed, command output without retention policy, full private paths when unnecessary, provider prompts, or database content.

The receipt is evidence of configuration and observed execution, not a proof that the kernel contains no vulnerability.

## 12. Phased validation plan

### Phase 0 — invariant and adversary corpus

- Write the exact strict and brokered “no DB” guarantees.
- Enumerate every effect path: native tools, function tools, MCP, memory, provider, browser/computer use, and future plugins.
- Define backend capability vocabulary and `ProtectionUnavailable` behavior.
- Build tiny adversarial test programs that attempt TCP/UDP, loopback, DNS, filesystem and abstract Unix sockets, inherited descriptors, credential files, SSH agent, Docker socket, `/proc`, namespace creation, privilege transitions, fork bombs, output floods, and cancellation races.

**Exit gate:** every guarantee maps to one enforcement mechanism and one executable negative test.

### Phase 1 — pure policy core

- Implement typed profile parsing with unknown-field rejection and explicit versioning.
- Implement provenance and intersection semantics.
- Implement `Deny` / `NeedsApproval` / `Grant` plus stable receipts.
- Property-test default deny, commutativity/idempotence of restriction intersection, monotonic narrowing, non-overridable forbids, stale/replayed grant rejection, and policy-error denial.
- Fuzz profile parsing, request normalization, and grant serialization.

**Exit gate:** no test input can produce more authority after adding a restriction.

### Phase 2 — Linux strict profile

- Build the separate guard and Linux adapter.
- Start with no network and allowlisted filesystem visibility.
- Add descriptor/environment scrubbing, namespaces, Landlock, seccomp, cgroup v2, timeout, output bounds, and whole-tree cancellation.
- Run the adversary corpus on each supported kernel/ABI combination.
- Verify effective configuration from outside the child as well as testing failures from inside it.

**Exit gate:** the strict no-DB suite passes, setup failures fail closed, and the attestation reports exact primitives instead of `sandboxed=true`.

### Phase 3 — brokered network and secrets

- Add an HTTP-only egress broker with destination classification, DNS/redirect revalidation, TLS name checks, size/time limits, and no raw child sockets.
- Add audience-bound secret substitution in the broker; do not reveal the real secret to the child.
- Test direct socket bypass, proxy-variable bypass, QUIC/UDP, literal/private/link-local addresses, DNS rebinding, redirects, alternate encodings, request smuggling boundaries, and compromised allowed endpoints.

**Exit gate:** the product language distinguishes “no egress” from “brokered egress” and documents the latter's residual trust.

### Phase 4 — tool/MCP/memory complete mediation

- Route every non-process effect through the same capability vocabulary.
- Ensure remote MCP capabilities cannot be inferred from the local sandbox profile.
- Keep harness storage and provider credentials outside untrusted process visibility.
- Add cross-route tests: an operation denied through native network must also be denied through function tools and MCP.

**Exit gate:** there is no unmediated engine adapter capable of the protected effect.

### Phase 5 — macOS, Windows, and high-assurance tiers

- Run the identical semantic conformance suite against each adapter.
- Publish per-platform capability matrices and minimum versions.
- Refuse strict profiles on unsupported hosts; offer a VM/remote worker instead.
- Measure compatibility and latency separately from security conformance.

**Exit gate:** a profile name has the same guarantee across every platform that advertises support.

### Phase 6 — optional policy language and AI adviser

- Introduce Cedar/OPA only if real authoring complexity exceeds the typed evaluator.
- If Jev is added, evaluate it on representative labeled requests and keep it advisory.
- Test adviser outage, latency, privacy minimization, low-confidence routing, and the invariant that no adviser output can widen a hard policy.

## 13. Limitations and unresolved design choices

1. **Approval channel:** a human approval must be bound to the exact request digest and maximum scope. The client-to-guard trust path needs a protocol design.
2. **Filesystem semantics:** package managers, compilers, and language servers need more than the workspace. The toolchain read set and cache strategy require compatibility measurement.
3. **macOS supportability:** Seatbelt is proven in existing products, but the supportable API and dynamic policy shape for this project need a spike.
4. **Windows compatibility:** LPAC/AppContainer is strong but may reject common developer workflows; a dedicated-principal design may require privileged setup.
5. **Allowed-network identity:** domain names are usability identifiers, not stable security identities. The egress broker needs a precise DNS, redirect, IP-range, TLS, and proxy model.
6. **Plugin trust:** in-process native plugins share the engine's authority and cannot be sandboxed meaningfully. Untrusted extensions must be out of process or WASI/remote.
7. **Policy evolution:** profile and grant versioning must define compatibility and rollback behavior; a newer binary must not reinterpret an old grant more broadly.
8. **Availability:** strict resource limits can break legitimate builds. Limits need named profiles and observable failure reasons, not silent relaxation.

## 14. Final recommendation

Proceed with the module, using this boundary:

```text
arany-domain
  capability/effect vocabulary and lifecycle invariants

arany-engine
  run orchestration, approval pause/resume, attestation checks

harness-policy (small Rust library; may begin as a private module)
  typed profiles, provenance, intersection, pure decisions, receipts

harness-guard (separate process)
  final policy decision, privileged setup, spawn, supervision, attestation

platform adapters
  linux-native first; macOS/Windows/VM only after conformance gates
```

Build the Linux `strict-no-db` vertical slice first. It tests the deepest design questions with the strongest available native primitives and establishes the cross-platform conformance contract. Do not begin with AI classification, a general policy language, or command-pattern coverage.

The defining invariant should be:

> Model output may request authority; only deterministic policy may grant it; only the protection runtime may exercise it; and execution stops when the effective platform boundary cannot prove the requested restriction.

That gives the harness a stronger, clearer foundation than a growing list of dangerous-command patterns while preserving the option to use Jev or another model as a user-selected advisory layer.

## Primary sources consulted

- [NIST reference monitor definition](https://csrc.nist.gov/glossary/term/reference_monitor)
- [Saltzer and Schroeder, *The Protection of Information in Computer Systems*](https://web.mit.edu/saltzer/www/publications/protection/Basic.html)
- [Linux Landlock userspace API](https://cdn.kernel.org/doc/html/latest/userspace-api/landlock.html)
- [Linux seccomp documentation](https://kernel.org/doc/html/latest/userspace-api/seccomp_filter.html)
- [Linux cgroup v2 documentation](https://cdn.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html)
- [Linux network namespaces manual](https://man7.org/linux/man-pages/man7/network_namespaces.7.html)
- [Bubblewrap official repository](https://github.com/containers/bubblewrap)
- [Apple App Sandbox documentation](https://developer.apple.com/documentation/security/app_sandbox)
- [Microsoft AppContainer isolation](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation)
- [Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)
- [Microsoft Windows Filtering Platform](https://learn.microsoft.com/en-us/windows/win32/fwp/windows-filtering-platform-start-page)
- [gVisor security architecture](https://gvisor.dev/docs/architecture_guide/intro/)
- [Wasmtime WASI tutorial](https://github.com/bytecodealliance/wasmtime/blob/main/docs/WASI-tutorial.md)
- [Cedar official repository and documentation](https://github.com/cedar-policy/cedar)
- [Open Policy Agent official documentation](https://www.openpolicyagent.org/docs)
- [OpenAI Codex security guidance](https://learn.chatgpt.com/docs/security)
- [OpenAI Codex sandboxing](https://learn.chatgpt.com/docs/sandboxing)
- [OpenAI Codex approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security)
- [OpenAI Codex Windows sandbox](https://learn.chatgpt.com/docs/windows/windows-sandbox)
- [Claude Code sandbox documentation](https://code.claude.com/docs/en/sandboxing)
- [Claude Code permission documentation](https://code.claude.com/docs/en/permissions)
- [TypeSafe official documentation index](https://docs.typesafe.ai/llms.txt)
- [TypeSafe official agent skill](https://github.com/typesafe-ai/skills/blob/main/skills/typesafe-ai/SKILL.md)
- [PostgreSQL roles and client authentication](https://www.postgresql.org/docs/current/client-authentication.html)
