# Native macOS continuation

## Metadata

- Status: `In progress`
- Owner: Codex
- Last updated: 2026-10-08

## Goal and scope

Complete the native macOS coding-agent harness, including granted file operations, commands, runtime Skills and local MCP. Preserve equivalent product behavior across OSes through suitable native enforcement. Human VoiceOver review and live Provider use remain user-owned.

## Current state and contract owners

Completed account-scope, migration and selected-item authorization behavior belongs to [the credential spec](../../docs/specs/credentials.md) and the remaining [ChatGPT contract](../../agents/chatgpt.md). Homogeneous folder consent and setup-to-chat restoration belong to [the terminal spec](../../docs/specs/terminal.md). [The Tool spec](../../docs/specs/tools.md) owns grants, isolation, resources, cancellation, replay and unavailable-platform rejection. These contracts replace the completed execution instructions.

Native Tool enforcement remains unimplemented; ordinary chat and remembered trust do not complete the granted Tool lifecycle. Checked macOS ACL admission, isolated Keychain save/replacement/lock/token-deletion evidence, native clipboard PNG/text transport and debug active-terminal signal/failure coverage are implemented. Safe handle-relative nested file/Skill discovery is implemented independently of Guard dispatch. Native TLS chain/name coverage uses a generated client-local trust anchor; default system-trust and compiled-endpoint HTTPS process journeys remain open. [The dated native evidence record](../../docs/research/testing-strategy-for-rust-cli-harness.md#native-macos-baseline-2026-10-08) owns completed checks, review outcomes and the account-fixture incident; [next steps](../../NEXT_STEPS.md#beta-2-and-broader-release) owns current gaps and user checks.

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

### Native Guard decision still required

A harmless native Seatbelt profile can execute outside the development sandbox,
but that capability does not attest the common Tool profile. Per-process limits
and process-group termination do not by themselves bound aggregate memory,
process count and scratch bytes or own descendants that leave a group. No native
enforcer satisfying those requirements has been implemented or attested; the
availability check must continue failing closed. Handle-relative file admission
and discovery cannot stand in for that enforcement.

The requested native scope remains the default. A bounded Linux VM would be an
alternative isolation design, not an already approved or installed dependency;
its guest/image provenance, resources, transport and lifecycle need review before
implementation. Do not silently choose that route or weaken the Tool contract.
The [Tool spec](../../docs/specs/tools.md) remains authoritative for the required
bounds. Native terminal, credential and clipboard changes preserve the Engine
boundary; shared path validation and startup clearing need native Linux
re-verification.

1. Implement native macOS Tool enforcement for the complete granted file/command/Skill/MCP lifecycle. Evaluate standard kernel/OS facilities against the common contract and attest the selected profile before dispatch. Extend native successful operation, rejection, cancellation/descendant cleanup, uncertain-effect and replay evidence. Refusal-only evidence does not complete this slice; unresolved native constraints remain named implementation gaps.
2. Complete isolated native account setup/refresh/logout and cross-user evidence. Checked ACL admission and native save/read/replacement/locked/denied/token-deletion evidence are implemented; the isolated Keychain fixture rejects optimized overrides. Human permission-dialog review remains user-owned.
3. Complete compiled-endpoint offline HTTPS and default system-trust evidence, optimized active-CLI isolation, and remaining native terminal loss/fault/picker/tmux owners. Debug active-terminal signal/cancellation/failure coverage and bounded clipboard PNG/text transport are implemented; named-pasteboard service coverage does not replace general-pasteboard UX review.
4. Keep the consolidated native evidence and user-owned VoiceOver/live checks in `NEXT_STEPS.md` current after each remaining slice. Do not delete this plan while native enforcement or process-owner implementation remains.

## Exit criteria

The complete authorized macOS Tool lifecycle has native success, rejection, cancellation, resource and replay evidence; private storage, terminal/process and clipboard implementation gaps are resolved. Remaining user-owned or unavailable-prerequisite checks are explicitly handed off without a support claim. Completed behavior lives in domain contracts and dated outcomes in existing evidence records. Delete this plan under [the planning lifecycle](../README.md#lifecycle) when no implementation remains, updating inbound links in the same change.
