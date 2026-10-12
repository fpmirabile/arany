# Arany next steps

This file contains unresolved defects, deferred functionality and verification
still required for the personal Linux beta and broader release. Contracts live
in [domain specs](./docs/specs/README.md); setup instructions live in
[the setup guide](./docs/setup.md) and [local tools](./docs/tools.md). Remove an
item when its completion condition is met; completed work belongs in Git/CI
history. Implementation status and verified support are separate claims.

## Open defects

- **macOS guarded-chat admission deadline.** After integrating Guard changes,
  `native_chat_preserves_turns_with_guarded_tools_and_rejects_invalid_admission`
  exceeded its 30-second deadline in both a full debug run and an isolated repeat.
  A sampled fixture was inspecting code signatures in `SystemPrograms::admit`,
  before Provider invocation. A subsequent full debug run passed unchanged;
  release also passed. Measure cold catalogue admission before attributing the
  delay to terminal rendering or relaxing the fixture deadline.

- **Native Engine stopped-command timeout investigation.** The macOS corpus
  timed out once at its unchanged 30-second case deadline; a diagnostic-enabled
  repeat passed without raising it. The first failure lacked case/stage
  evidence, so its cause is unestablished. On recurrence capture the reported
  case, committed progress and queued hint together with native
  preparation/recovery/cleanup stages before attributing the timeout to
  admission, cancellation or host load.

## Next functional priority

- **Linux host seed ownership after controller death.** Existing
  `/tmp/arany-tools-*` snapshots contain selected Workspace/runtime data and use
  Drop-only removal. `SIGKILL` can retain those private files indefinitely after
  the systemd/cgroup unit ends; the shared unit quota does not bound cumulative
  host seed storage. Bind new snapshots' checked path/device/inode to their
  reservation before copying, retain capacity until validated deletion, and
  prove recovery after actual native termination. Existing unrecorded roots
  must remain untouched; a filename-prefix sweep cannot authorize deletion.

- **Deferred Anthropic consumer subscriptions.** Native Claude-plan access is
  missing and user-deferred behind the [route/authorization gate](./docs/research/subscription-provider-routes-2026-10.md#decision-boundary-for-beta-1).
  Continue unaffected implementation. API-key billing is a separate route;
  another harness's implementation or credentials do not authorize Arany access.
- **Larger coding workloads.** Add reviewed runtime/cache mounts, bounded build
  profiles and explicit artifact integration when actual workloads need them.
  Remote MCP/OAuth, network/disclosure grants, interactive/background jobs,
  package installation and child effects need their own Policy/Guard/protocol
  evidence; retain the existing [Tool scope](./docs/specs/tools.md).
- **Long-lived history.** Evaluate incremental replay/indexing and a bounded
  archival/new-Session policy before histories reach existing admission quotas.
  Preserve canonical Events, fork lineage and append acknowledgement semantics.

## Terminal continuity evidence

- Exercise classic scrollback, narrow resize/reflow, temporary details, idle `/view` transitions and a 200-line streamed response in a real macOS emulator and native Linux/Windows terminals. Synthetic macOS PTYs prove controlled cursor ownership, transcript insertion and restoration; they do not emulate scrollback or prove other OS support.
- With explicit usage consent, repeat OpenAI, Anthropic, ChatGPT and exact-custom streaming conformance against actual selected accounts/endpoints. Offline fixtures prove protocol/lifecycle behavior, not live compatibility. Keep incomplete answers outside future context and confirm recovery after cancellation/crash.
- Check `--screen-reader`, `/transcript`, monochrome labels and managed following with real assistive technology. These usability claims remain unverified.

## Beta 1: final user-owned checks

Perform these after implementation handoff with deliberate consent for usage.
Missing real accounts, OS services, toolchains or another machine do not pause
unaffected implementation. Synthetic tests do not authorize live calls or access
to the user's credential store or clipboard.

- **Native Linux shared-capacity recovery before PR #6 merge.** Run the
  retained failed-unit owner
  `tools::guard::linux::tests::retained_failed_unit_is_reset_before_capacity_is_reclaimed`
  with `--ignored` on an isolated Linux account with a systemd user manager and
  delegated cgroup v2. Verify native termination, failed-job reset and reservation
  reclamation alongside the existing Tool journey. The Linux ARM64 container
  proves compilation and cross-process FIFO/settings behavior, not those native
  manager guarantees.

1. **Broader ChatGPT workflows.** For other accounts/models, verify sign-in,
   visible model/effort, a direct task, follow-up, team and cancellation with
   closed Session inspection. Review the selected billing route and local-only
   output bound; cancellation does not guarantee remote work or usage stopped.
   Follow [setup](./docs/setup.md) and [execution](./docs/specs/execution.md).
2. **Credential lifecycle.** Verify setup, restart/reuse, replacement, reconnect,
   refresh and logout with the intended Linux backend, including interruption
   during submission/compaction admission. Check retained drafts, no pre-Run
   receipt/retry, uncertainty and pinned-backend refusal without silent downgrade.
   Private-file storage remains explicitly consented plaintext. The
   [credential contract](./docs/specs/credentials.md) owns expected outcomes.
3. **Trust and approvals.** Verify folder prompt/navigation/cancellation,
   remembered consent, file/folder mentions and all approval modes. Test exact
   Allow once/Deny and command review on a disposable project; highlighting alone
   executes nothing. Evaluate real-model Tool encoding and AI-review quality,
   including resumed/delegated edits. Follow [Tools](./docs/specs/tools.md).
4. **Terminal and accessibility.** Review long/multiline drafts, slash/Skill
   tabs, settings persistence, model/effort selection, history, narrow/colorless
   layouts, VS Code's first message under its sticky header, theme contrast and
   VoiceOver/Orca. During work, compaction and slow catalog refresh, edit, filter,
   change reasoning, cancel, resize/suspend and recover the same draft/caret.
   Verify no queued task or implicit retry. The [terminal contract](./docs/specs/terminal.md)
   owns the interaction scenarios.
5. **API-key and custom routes.** With separate usage authorization, verify
   OpenAI/Anthropic direct/team compatibility and explicit Anthropic workspace
   selection. A configured custom route requires exact endpoint/model conformance
   before Workspace disclosure. These checks can incur API charges and inherit
   no ChatGPT-plan compatibility claim. Follow [Provider rules](./agents/provider.md).
6. **Clipboard and model vision.** Verify framed emulator paste and explicit
   text/PNG reads on the intended Linux terminal, retained editing, active-work
   behavior, modal discard and rejection without submission. Test native
   Wayland/X11 service interoperability and deliberately authorized image-only/
   text-plus-image Runs with closed replay. Confirm metadata-only human history,
   private image-bearing JSONL and lossy pixel compaction. Follow
   [clipboard rules](./agents/clipboard.md) and [image rules](./agents/image.md).
7. **Real coding workflow.** Review private grants on a disposable small project,
   then inspect/edit, run an offline check, load pinned Skill data and call local
   MCP. Inspect files and reopened observations; subprocess writes stay in the
   private snapshot, uncertainty stops continuation and no action retries itself.
   Validate the chosen real model's strict Tool encoding before claiming support.
8. **Native macOS stores and clipboard.** On a macOS account, verify setup,
   restart/reuse, replacement, refresh and logout through the login Keychain,
   including Allow/Always Allow/Deny and cancellation at the selected-item
   startup authorization dialog; Allow Once may prompt again because the OS owns
   per-item persistence. Read text and PNG from the
   general pasteboard, then confirm rejection of other images and oversized
   content without submission. Follow the [credential contract](./docs/specs/credentials.md)
   and [clipboard rules](./agents/clipboard.md).

## Beta 2 and broader release

| Pending gate | Completion condition and owner |
| --- | --- |
| Hosted CI and native Linux regression checks | Publish when authorized, verify the pull-request jobs on a hosted runner (format and Clippy, Linux and macOS debug/release tests with `--cfg arany_ci`), and run the native consent, terminal, setup, Session picker, pinned-Skill and Tool owners. [Workflow](./.github/workflows/ci.yml), [testing rules](./agents/testing.md) |
| macOS optimized product/account isolation | On a disposable effective-user home, run the full optimized Engine/active-CLI and account setup/refresh/logout/cross-user gates. Changing HOME does not isolate account paths; debug fixtures and override refusal do not prove this gate. [Fixture rules](./agents/references/testing-details.md#host-and-account-isolation) |
| Native TLS to compiled endpoints | Provide isolated resolution/routing and ordinary trust for a compiled-endpoint HTTPS journey. Generated-anchor chain/name rejection alone does not prove a public trusted chain or production-endpoint success. [Security rules](./agents/security.md) |
| Native terminal faults and tmux | Run available native tmux and acquisition/output/input-loss owners, including optimized macOS and supported architectures; verify restoration on the exact distributed binary. [Terminal fixture rules](./agents/references/testing-details.md#terminal-process-fixtures), [PTY finding](./docs/security/findings/terminal-pty-hangup-spin.md) |
| Native stores and clipboard | Finish isolated Linux keyring-backed product setup and native store lifecycle gates; human Keychain dialog and general-pasteboard checks remain in checklist item 8. [Credentials](./docs/specs/credentials.md), [clipboard](./agents/clipboard.md) |
| Readiness, load and observer faults | Measure cancellation/cleanup and resource enforcement under declared load. [Guard unit rules](./agents/guard-unit.md) |
| Performance | Measure history/input latency, RSS and telemetry on named hosts. Measure the bounded macOS host-PID scan at its nominal supervision interval before choosing another membership primitive or interval. [Native measurement rules](./agents/references/testing-details.md#native-and-live-evidence) |
| Distribution and dependency policy | Verify fresh advisories, license/source bundles, exact source/binary pairing, compiler/native provenance and MSRV. Resolve changed notice exceptions and the unreached standard-library gate without weakening pins. [Release review](./docs/security/beta-license-release-review.md#current-result), [dependency inventory](./docs/security/beta-dependency-inventory.md) |
| Other platforms and enforcers | Verify native Linux ARM64, implement Windows and alternative Linux-native enforcement, and require actual platform behavior evidence before support claims. Windows CI remains deferred. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities) |

These gates remain unverified until their named procedure completes. Specs and
existing tests describe what must hold; neither implemented code nor an old
passing run establishes broader support.
