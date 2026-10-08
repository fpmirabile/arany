# Native macOS continuation

## Metadata

- Status: `In progress`
- Owner: Codex
- Last updated: 2026-10-08

## Goal and scope

Complete the native macOS coding-agent harness, including granted file operations, commands, runtime Skills and local MCP. Preserve equivalent product behavior across OSes through suitable native enforcement. Human VoiceOver review and live Provider use remain user-owned.

## Current state and contract owners

Completed account-scope, migration and selected-item authorization behavior belongs to [the credential spec](../../docs/specs/credentials.md) and the remaining [ChatGPT contract](../../agents/chatgpt.md). Homogeneous folder consent and setup-to-chat restoration belong to [the terminal spec](../../docs/specs/terminal.md). [The Tool spec](../../docs/specs/tools.md) owns grants, isolation, resources, cancellation, replay and unavailable-platform rejection. These contracts replace the completed execution instructions.

Native file/Skill enforcement is implemented; complete command/MCP enforcement remains open. Ordinary chat and remembered trust do not complete that remaining lifecycle. Checked macOS ACL admission, isolated Keychain save/replacement/lock/token-deletion evidence, native clipboard PNG/text transport and debug active-terminal/consent signal/failure coverage are implemented. Safe handle-relative nested file/Skill discovery, native exclusive atomic file primitives and native executable pin admission are implemented independently of Guard dispatch. Intents and receipts distinguish the accepted resource profile while preserving legacy serialized digest bytes. Native TLS chain/name coverage uses a generated client-local trust anchor; default system-trust and compiled-endpoint HTTPS process journeys remain open. [The dated native evidence record](../../docs/research/testing-strategy-for-rust-cli-harness.md#native-macos-baseline-2026-10-08) owns completed checks, review outcomes and the account-fixture incident; [next steps](../../NEXT_STEPS.md#beta-2-and-broader-release) owns current gaps and user checks.

## Ownership, security and resources

Keep deterministic Policy and the separately enforcing Guard at their [existing Tool boundary](../../agents/tools.md). Choose standard kernel/OS facilities that satisfy the common contract before adding a narrow native adapter; a platform-specific mechanism must not change Engine policy or introduce a substitutable Tool seam. Linux enforcement and its existing owners remain required; Windows support still needs its own implementation and evidence.

Follow [security rules](../../agents/security.md), [credential rules](../../agents/credentials.md), [Store admission](../../agents/store.md), [terminal rules](../../agents/terminal.md) and [clipboard rules](../../agents/clipboard.md) for each affected slice. Attacker-controlled paths, filesystem objects, commands and Tool/MCP output must remain inside admitted grants. Preserve resource/output/deadline bounds, descendant ownership and cancellation; Tools cannot inherit Provider credentials or account-store authority. Never use this user's real account, Keychain, Provider or clipboard to prove native behavior. Synthetic fixtures isolate both stable and legacy account roots.

## Verification map

| Remaining behavior | Successful and failure evidence | Existing owner |
|---|---|---|
| Native granted Tools | Admitted file/command/Skill/MCP operations succeed; denied access, stale grants, missing protection and excessive resources reject. Cancellation owns descendants; uncertain effects and closed SQLite replay retain the common contract. | [Tool scenarios](../../docs/specs/tools.md#acceptance-scenarios-and-evidence-owners), [Tool corpus](../../src/tools/tests.rs), [Workspace security journeys](../../tests/engine_run/workspace_security.rs) |
| Private account admission | Checked native filesystem access rejects unsafe ACLs/objects. Isolated synthetic Keychain items prove save/read/replacement, selected-item authorization, locked/denied behavior and deletion without a plaintext downgrade. | [Credential scenarios](../../docs/specs/credentials.md#acceptance-scenarios-and-evidence-owners), [Store owner](../../src/store/state.rs), [native credential findings](../../docs/security/findings/stalled-credential-store.md) |
| Terminal and TLS processes | Native process owners retain bounded observation, exact restoration, signal/descendant cleanup and failure claims; Linux evidence alone is insufficient. | [Terminal scenarios](../../docs/specs/terminal.md#acceptance-scenarios-and-evidence-owners), [process journeys](../../tests/session_run/active_terminal.rs), [offline TLS journeys](../../tests/session_run/setup/offline_https.rs) |
| Clipboard PNG | A bounded native transport admits supported clipboard image data and rejects malformed/oversized input with owned child cleanup and retained draft behavior. | [Clipboard rules](../../agents/clipboard.md), [image rules](../../agents/image.md), [next steps](../../NEXT_STEPS.md#beta-2-and-broader-release) |

Extend existing owners rather than duplicate their corpus. Run applicable [repository checks](../../CONTRIBUTING.md#verification), native offline debug/release and the required focused process gates for each implementation slice. Record missing prerequisites and unperformed native checks at their evidence owners. Synthetic helper or mode-bit observations do not prove real Keychain authorization or ACL exclusion.

## Remaining execution

### Native Guard implementation

The native file-Tool slice now uses the existing Policy, Guard, GO gate and
receipt. A separately supervised trusted worker admits
List/Read/Search/Write/Edit/Mkdir and pinned Skill data with default-denial
Seatbelt rules, checked live Workspace handles, closed inherited descriptors and
no program execution, fork or spawn. CPU expiry is reset and unblocked before
handler/mask/wait/limit changes are prohibited. The exact owned worker is
supervised and killed/reaped on failure; kernel-confirmed exit retires resource
observation before reap. The native Engine journey owns integrated effects,
stale/linked-file rejection, pre-GO refusal and closed replay; the production
profile corpus owns denied host/network/spawn access and actual CPU expiry.

Completed behavior belongs to the Tool spec and native FFI editing constraints
belong to [Guard rules](../../agents/guard.md). No dependency, daemon or Engine
policy seam was added. This slice does not implement commands/MCP or replace
their required ordinary spawn compatibility. Explicit unsupported grants reject
before inference; remembered macOS trust grants only implemented file/Skill
operations. Optimized full Engine/process isolation, threshold/fault stress and
remaining cancellation/uncertainty evidence stay named at the evidence owner.
Linux enforcement remains at its current boundary and needs native
re-verification for shared changes.

A harmless native Seatbelt profile can execute outside the development sandbox,
but that capability does not attest the common Tool profile. Per-process limits
and process-group termination do not by themselves bound aggregate memory,
process count and scratch bytes or own descendants that leave a group. No native command/MCP
enforcer satisfying those requirements has been implemented or attested; those
grants must continue failing closed. Handle-relative file admission
and discovery cannot stand in for that enforcement.

Commands and MCP servers must execute macOS binaries on the host and retain
ordinary native child-launch compatibility, including `posix_spawn`, as confirmed
by the user. A Linux VM and a fork/exec-only payload profile are outside the accepted scope. Evaluate native controls
against the complete contract; additional implementation is preferable to a
security shortcut, and missing controls cannot silently weaken the Tool contract.
The user accepts supervised 512-MiB memory and 64-task thresholds with nominal
10-ms sampling, plus a hard 15-second per-process CPU-time limit. The
[Tool spec](../../docs/specs/tools.md#limits-and-failure-behavior) owns these native semantics and their
explicit overshoot, swap and aggregate-CPU limitations. This acceptance does not
establish a working enforcer. Native terminal, credential and clipboard changes preserve the Engine
boundary; shared path validation and startup clearing need native Linux
re-verification.

#### Native enforcement design

The candidate keeps host-native execution and the separate Guard. Policy, grants,
credential/state exclusion, offline execution, disposable command changes, the
one-use GO gate and conservative effect/replay semantics remain required. The
resource differences must be reflected in normalized intents, effective-profile
attestation and durable receipts before any macOS dispatch is enabled.

| Dimension | Native candidate | Required evidence and limitation |
|---|---|---|
| Memory and tasks | Implement the accepted supervision contract from the Tool spec. | Confirm aggregation, observer failure, excess termination and effective-profile reporting without claiming peak prevention. |
| CPU | Implement the accepted hard per-process CPU-time limit and existing wall-time deadline. No sufficient CPU mechanism is selected. | A native child inherits soft/hard `RLIMIT_CPU` values of 15 and cannot widen them, but ignoring `SIGXCPU` permits 17 CPU seconds before the finite diagnostic exits itself. This API alone does not meet the contract. Confirm uncatchable expiry, inheritance and resistance to disabling the selected enforcement. |
| Storage | Separate fixed-capacity private native filesystems for disposable project data and scratch, each at most 64 MiB including metadata. | Prove capacity, private mount ownership, path exclusion, crash/error cleanup and refused dispatch when attachment or verification fails. Directory polling or per-file limits do not establish this aggregate ceiling. Not implemented or attested. |
| Descendants | Retain a native process-unit identity independently of mutable process groups. No sufficient mechanism is selected. | Direct `setsid`/`setpgid` denial does not stop spawn attributes. Blocking `SYS_posix_spawn` makes Rust child launch fail with `EPERM`, although the tested system shell still works; the user rejected this compatibility restriction. Prove ordinary fork/spawn inheritance, signal isolation, escape resistance and complete cleanup before dispatch. |

Use a narrow native implementation at the existing Guard boundary rather than a
new daemon, crate or Tool framework. The supervisor receives the same pinned
intent and closed channels, with no inherited Provider/account authority. The
native command/MCP gate remains closed during this review. An observer-only
profile reports the accepted native contract; it cannot attest Linux's hard
resource guarantees. The non-spawning file worker is not a
substitute for the requested command/MCP lifecycle.

1. Implement native macOS Tool enforcement for the complete granted file/command/Skill/MCP lifecycle. Evaluate standard kernel/OS facilities against the common contract and attest the selected profile before dispatch. Extend native successful operation, rejection, cancellation/descendant cleanup, uncertain-effect and replay evidence. Refusal-only evidence does not complete this slice; unresolved native constraints remain named implementation gaps.
2. Complete isolated native account setup/refresh/logout and cross-user evidence. Checked ACL admission and native save/read/replacement/locked/denied/token-deletion evidence are implemented; the isolated Keychain fixture rejects optimized overrides. Human permission-dialog review remains user-owned.
3. Complete compiled-endpoint offline HTTPS and default system-trust evidence, optimized active-CLI isolation, and remaining partial-acquisition/picker/tmux owners. Debug active-terminal signal/cancellation/failure, physical controlling-PTY loss and dead-stderr coverage are implemented; native consent cancellation/signals and stderr disconnection before/after the prompt restore surviving ownership before setup or Session work. Ordinary error reporting preserves failure exit when stderr is unavailable. Native exported-owner unwind passes both presentations and profiles through the shared reader corpus; it does not prove arbitrary shipped-CLI panic sites. Bounded clipboard PNG/text transport is implemented; named-pasteboard service coverage does not replace general-pasteboard UX review.
4. Keep the consolidated native evidence and user-owned VoiceOver/live checks in `NEXT_STEPS.md` current after each remaining slice. Do not delete this plan while native enforcement or process-owner implementation remains.

## Exit criteria

The complete authorized macOS Tool lifecycle has native success, rejection, cancellation, resource and replay evidence; private storage, terminal/process and clipboard implementation gaps are resolved. Remaining user-owned or unavailable-prerequisite checks are explicitly handed off without a support claim. Completed behavior lives in domain contracts and dated outcomes in existing evidence records. Delete this plan under [the planning lifecycle](../README.md#lifecycle) when no implementation remains, updating inbound links in the same change.
