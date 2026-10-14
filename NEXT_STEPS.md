# Arany next steps

This file contains unresolved defects, deferred functionality and verification
still required for the personal Linux beta and broader release. Contracts live
in [domain specs](./docs/specs/README.md); setup instructions live in
[the setup guide](./docs/setup.md) and [local tools](./docs/tools.md). Remove an
item when its completion condition is met; completed work belongs in Git/CI
history. Implementation status and verified support are separate claims.

## Next functional priority

- **Agentic coding teams (Beta 1 scope, current implementation priority).**
  RPI Goals, command changesets, toolchain/cache and flexible limits, registry
  fetch, Git, a
  narrowing-only Supervisor, per-agent Provider routing and writable team
  workers. The [initiative plan](./planning/agentic-teams/README.md) is
  self-contained and orders the slices; start at its next action (execution
  step 2, toolchain profiles, build cache and flexible limits). Remote
  MCP/OAuth, interactive/background jobs and other network grants need their
  own Policy/Guard/protocol evidence; retain the existing
  [Tool scope](./docs/specs/tools.md) until each slice lands.
- **Deferred Anthropic consumer subscriptions.** Native Claude-plan access is
  missing and user-deferred behind the [route/authorization gate](./docs/research/subscription-provider-routes-2026-10.md#decision-boundary-for-beta-1).
  Continue unaffected implementation. API-key billing is a separate route;
  another harness's implementation or credentials do not authorize Arany access.
- **Long-lived history.** Evaluate incremental replay/indexing and a bounded
  archival/new-Session policy before histories reach existing admission quotas.
  Preserve canonical Events, fork lineage and append acknowledgement semantics.

## Terminal continuity evidence

- Exercise classic scrollback, narrow resize/reflow, temporary details, idle `/settings` chat-view transitions and a 200-line streamed response in a real macOS emulator and native Linux terminals, including under WSL2. Synthetic macOS PTYs prove controlled cursor ownership, transcript insertion and restoration; they do not emulate scrollback or prove other OS support.
- With explicit usage consent, repeat OpenAI, Anthropic, ChatGPT and exact-custom streaming conformance against actual selected accounts/endpoints. Offline fixtures prove protocol/lifecycle behavior, not live compatibility. Keep incomplete answers outside future context and confirm recovery after cancellation/crash.
- Check `--screen-reader`, `/transcript`, monochrome labels and managed following with real assistive technology. These usability claims remain unverified.

## Beta 1: final user-owned checks

Perform these after implementation handoff with deliberate consent for usage.
Missing real accounts, OS services, toolchains or another machine do not pause
unaffected implementation. Synthetic tests do not authorize live calls or access
to the user's credential store or clipboard.

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
   MCP. Inspect files and reopened observations; command changes publish only through
   a matching validated changeset receipt, MCP writes stay private, uncertainty
   stops continuation and no action retries itself.
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
| Hosted CI and native Linux regression checks | Publish when authorized, verify the pull-request jobs on a hosted runner (format and Clippy, Linux and macOS debug/release tests with `--cfg arany_ci`), and run the native consent, terminal, setup, Session picker, pinned-Skill and Tool owners, including Linux command descendant termination before changeset capture and create/modify/delete publication, rejection, approval and cancellation with closed replay. Native Linux command changeset execution remains unverified on the current macOS host. [Workflow](./.github/workflows/ci.yml), [testing rules](./agents/testing.md) |
| macOS optimized product/account isolation | On a disposable effective-user home, run the full optimized Engine/active-CLI and account setup/refresh/logout/cross-user gates. Changing HOME does not isolate account paths; debug fixtures and override refusal do not prove this gate. [Fixture rules](./agents/references/testing-details.md#host-and-account-isolation) |
| Native TLS to compiled endpoints | Provide isolated resolution/routing and ordinary trust for a compiled-endpoint HTTPS journey. Generated-anchor chain/name rejection alone does not prove a public trusted chain or production-endpoint success. [Security rules](./agents/security.md) |
| Native terminal faults and tmux | Run available native tmux and acquisition/output/input-loss owners, including optimized macOS and supported architectures; verify restoration on the exact distributed binary. [Terminal fixture rules](./agents/references/testing-details.md#terminal-process-fixtures), [PTY finding](./docs/security/findings/terminal-pty-hangup-spin.md) |
| Native stores and clipboard | Finish isolated Linux keyring-backed product setup and native store lifecycle gates; human Keychain dialog and general-pasteboard checks remain in checklist item 8. [Credentials](./docs/specs/credentials.md), [clipboard](./agents/clipboard.md) |
| Readiness, load and observer faults | Measure cancellation/cleanup and resource enforcement under declared load. [Guard unit rules](./agents/guard-unit.md) |
| Performance | Measure history/input latency, RSS and telemetry on named hosts. Measure the bounded macOS host-PID scan at its nominal supervision interval before choosing another membership primitive or interval. [Native measurement rules](./agents/references/testing-details.md#native-and-live-evidence) |
| Distribution and dependency policy | Repeat the default-off fresh advisory lane before release and after lock changes ([first run passed](https://github.com/fpmirabile/arany/actions/runs/38531269709)); run the updated bundle with its exact four upstream-root MIT supplements and six nested license/attribution materials, then verify source/binary pairing, compiler/native provenance and MSRV. Reconcile the unreached standard-library attribution gate without weakening pins. [Release review](./docs/security/beta-license-release-review.md#current-result), [dependency inventory](./docs/security/beta-dependency-inventory.md) |
| Other platforms and enforcers | Verify native Linux ARM64, alternative Linux-native enforcement and the Linux build under WSL2 (user namespaces, cgroup v2 and Guard enforcement), and require actual platform behavior evidence before support claims. Native Windows is out of scope. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities) |

These gates remain unverified until their named procedure completes. Specs and
existing tests describe what must hold; neither implemented code nor an old
passing run establishes broader support.
