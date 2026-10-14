# Arany next steps

This file contains unresolved defects, deferred functionality and verification
still required for the personal Linux beta and broader release. Contracts live
in [domain specs](./docs/specs/README.md); setup instructions live in
[the setup guide](./docs/setup.md) and [local tools](./docs/tools.md). Remove an
item when its completion condition is met; completed work belongs in Git/CI
history. Implementation status and verified support are separate claims.

## Next functional priority

- **Agentic coding teams (Beta 1 scope, current implementation priority).**
  RPI Goals, command changesets, toolchain profiles and a persistent cache,
  flexible limits, registry fetch, Git, a
  narrowing-only Supervisor, per-agent Provider routing and writable team
  workers. The [initiative plan](./planning/agentic-teams/README.md) is
  self-contained and orders the slices; start at Goal Foundation prerequisites
  (lifecycle, cancellation, budgets, plan contract and Workspace lock;
  execution step 3), then A4, dogfooding and the remaining Goal artifacts. The
  approved A4 maxima are 4 GiB memory, 8 GiB
  cache, 1 GiB project storage and 256 tasks; the current cache view remains
  64 MiB until A4 enables the build profile. Native build
  evidence and named-host measurements still precede support claims. Remote
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
| Hosted CI and native Linux regression checks | Publish when authorized, verify the pull-request jobs on a hosted runner (format and Clippy, Linux and macOS debug/release tests with `--cfg arany_ci`), and run the native consent, terminal, setup, Session picker, pinned-Skill and Tool owners, including the production private-mount bootstrap fixture (shared-source success, pre-existing nested-mount refusal and no late propagation), Linux command descendant termination before changeset capture and create/modify/delete publication, rejection, approval and cancellation with closed replay. The full systemd Guard product journey is separate from the topology fixture and is activated by a default-false `linux_native_advanced` manual lane. The product Guard and tmux outcomes remain pending until the exact parents pass on native x86_64 and ARM64. Native Linux command changeset execution remains unverified on the current macOS host. [Workflow](./.github/workflows/ci.yml), [testing rules](./agents/testing.md) |
| macOS runtime mount topology | Run the native mounted-image refusal and selected-runtime command under Seatbelt on hosted macOS CI; the focused `getfsstat` topology fixture and existing guarded unit command cover adapter behavior, but a full product-level injected-mount Ready/GO journey remains unverified. Do not claim a lifetime mount-isolation guarantee. [Tools spec](./docs/specs/tools.md), [Guard rules](./agents/guard.md) |
| Native build profile coverage | After A3/A4, run real offline Arany builds on named Linux and macOS hosts. On macOS specifically trace `cc`/`xcrun` delegation to `/Library/Developer/CommandLineTools` and the selected SDK; grant only measured, exact runtime roots if the actual build demonstrates that need. [Tools spec](./docs/specs/tools.md), [native evidence rules](./agents/references/testing-details.md#native-and-live-evidence) |
| macOS optimized product/account isolation | On a disposable effective-user home, run the full optimized Engine/active-CLI and account setup/refresh/logout/cross-user gates. Changing HOME does not isolate account paths; debug fixtures and override refusal do not prove this gate. [Fixture rules](./agents/references/testing-details.md#host-and-account-isolation) |
| Native TLS to compiled endpoints | Provide isolated resolution/routing and ordinary trust for a compiled-endpoint HTTPS journey. Generated-anchor chain/name rejection alone does not prove a public trusted chain or production-endpoint success. [Security rules](./agents/security.md) |
| Native terminal faults and tmux | The opt-in Linux release lane activates the composer/replay and active-shelf tmux parents on x86_64 and ARM64; keep both outcomes pending until the hosted run passes. Run the remaining native tmux and acquisition/output/input-loss owners, including optimized macOS and supported architectures, and verify restoration on the exact distributed binary. [Terminal fixture rules](./agents/references/testing-details.md#terminal-process-fixtures), [PTY finding](./docs/security/findings/terminal-pty-hangup-spin.md) |
| Native stores and clipboard | Finish isolated Linux keyring-backed product setup and native store lifecycle gates; human Keychain dialog and general-pasteboard checks remain in checklist item 8. [Credentials](./docs/specs/credentials.md), [clipboard](./agents/clipboard.md) |
| Readiness, load and observer faults | Measure cancellation/cleanup and resource enforcement under declared load. [Guard unit rules](./agents/guard-unit.md) |
| Performance | Measure history/input latency, RSS and telemetry on named hosts. Measure the bounded macOS host-PID scan at its nominal supervision interval before choosing another membership primitive or interval. [Native measurement rules](./agents/references/testing-details.md#native-and-live-evidence) |
| Distribution and dependency policy | Verify fresh advisories, license/source bundles, exact source/binary pairing, compiler/native provenance and MSRV. Resolve changed notice exceptions and the unreached standard-library gate without weakening pins. [Release review](./docs/security/beta-license-release-review.md#current-result), [dependency inventory](./docs/security/beta-dependency-inventory.md) |
| Other platforms and enforcers | Dispatch the existing [CI workflow](./.github/workflows/ci.yml) with `linux_arm64=true` against the ref containing this change; the workflow file itself must exist on the default branch. Require both native `ubuntu-24.04-arm` debug and release jobs to pass their `aarch64` host/Rust-target assertions, Linux namespace prerequisites and `--cfg arany_ci` workspace tests; keep this evidence pending until the hosted run completes green. Do not use emulation or skip gates. Separately verify alternative Linux-native enforcement and the Linux build under WSL2 (user namespaces, cgroup v2 and Guard enforcement), and require actual platform behavior evidence before support claims. Native Windows is out of scope. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities), [native CI procedure](./agents/references/testing-details.md#local-verification-and-ci) |

These gates remain unverified until their named procedure completes. Specs and
existing tests describe what must hold; neither implemented code nor an old
passing run establishes broader support.
