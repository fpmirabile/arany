# Arany next steps

This file contains unresolved defects, deferred functionality and verification
still required for the personal Linux beta and broader release. Contracts live
in [domain specs](./docs/specs/README.md); setup instructions live in
[the setup guide](./docs/setup.md) and [local tools](./docs/tools.md). Remove an
item when its completion condition is met; completed work belongs in Git/CI
history. Implementation status and verified support are separate claims.

## Next functional priority

- **Agentic coding teams (Beta 1 scope, current implementation priority).**
  Flexible limits, the remaining RPI Goal workflows, registry fetch, Git, a
  narrowing-only Supervisor, per-agent Provider routing and writable team
  workers. The [initiative plan](./planning/agentic-teams/README.md) is
  self-contained and orders the slices; Goal Foundation has landed under the
  [Goal spec](./docs/specs/goals.md), so continue with A4 flexible limits
  (execution step 4), then dogfooding and the remaining Goal workflows. The
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
| Hosted CI and native Linux regression checks | Publish when authorized, verify the pull-request jobs on a hosted runner (format and Clippy, Linux and macOS debug/release tests with `--cfg arany_ci`), and run the native consent, terminal, setup, Session picker, pinned-Skill and Tool owners, including the production private-mount bootstrap fixture (shared-source success, pre-existing nested-mount refusal and no late propagation), Linux command descendant termination before changeset capture and create/modify/delete publication, rejection, approval and cancellation with closed replay. This topology fixture is separate from the full systemd Guard journey; the latter is not activated by the ordinary `arany_ci` gate and remains a pending product-level native run. Native Linux command changeset execution remains unverified on the current macOS host. [Workflow](./.github/workflows/ci.yml), [testing rules](./agents/testing.md) |
| macOS runtime mount topology | Run the native mounted-image refusal and selected-runtime command under Seatbelt on hosted macOS CI; the focused `getfsstat` topology fixture and existing guarded unit command cover adapter behavior, but a full product-level injected-mount Ready/GO journey remains unverified. Do not claim a lifetime mount-isolation guarantee. [Tools spec](./docs/specs/tools.md), [Guard rules](./agents/guard.md) |
| Native build profile coverage | After A3/A4, run real offline Arany builds on named Linux and macOS hosts. On macOS specifically trace `cc`/`xcrun` delegation to `/Library/Developer/CommandLineTools` and the selected SDK; grant only measured, exact runtime roots if the actual build demonstrates that need. [Tools spec](./docs/specs/tools.md), [native evidence rules](./agents/references/testing-details.md#native-and-live-evidence) |
| macOS optimized product/account isolation | On a disposable effective-user home, run the full optimized Engine/active-CLI and account setup/refresh/logout/cross-user gates. Changing HOME does not isolate account paths; debug fixtures and override refusal do not prove this gate. [Fixture rules](./agents/references/testing-details.md#host-and-account-isolation) |
| Native TLS to compiled endpoints | Provide isolated resolution/routing and ordinary trust for a compiled-endpoint HTTPS journey. Generated-anchor chain/name rejection alone does not prove a public trusted chain or production-endpoint success. [Security rules](./agents/security.md) |
| Native terminal faults and tmux | Run available native tmux and acquisition/output/input-loss owners, including optimized macOS and supported architectures; verify restoration on the exact distributed binary. [Terminal fixture rules](./agents/references/testing-details.md#terminal-process-fixtures), [PTY finding](./docs/security/findings/terminal-pty-hangup-spin.md) |
| Native stores and clipboard | Finish isolated Linux keyring-backed product setup and native store lifecycle gates; human Keychain dialog and general-pasteboard checks remain in checklist item 8. [Credentials](./docs/specs/credentials.md), [clipboard](./agents/clipboard.md) |
| Readiness, load and observer faults | Measure cancellation/cleanup and resource enforcement under declared load. [Guard unit rules](./agents/guard-unit.md) |
| Performance | Repair and recheck the ignored Linux history fixture against the full lifecycle contract: its first `AgentFinished(Finished)` lacks the required preceding `ProviderCallRecorded(Finished)`. On the current baseline, adding only that Event per Run would make the 1,600-Run seed 11,201 Events, above the 10,000-Event lineage limit; additional lifecycle or reservation Events change the final count. Once all required Events are accounted for, rebalance Run count and text/Event mix under the final limits, and assert Event and serialized-payload counts before measuring. Then measure history/input latency and RSS on named Linux/macOS hosts through the 128 MiB HotSession boundary and toward quota-near cold replay, recording shared-Store size. The fixture source does not isolate replay duration, assert payload/database size or sample RSS; the separate `show` gate covers 100 Events. Measure telemetry on named hosts. Measure the bounded macOS host-PID scan at its nominal supervision interval before choosing another membership primitive or interval. [History gate](./tests/session_run/active_terminal/tmux/history_latency.rs), [native measurement rules](./agents/references/testing-details.md#native-and-live-evidence) |
| Distribution and dependency policy | Keep the release-policy workflow passing for the release commit's `Cargo.lock`; verify license/source bundles, exact source/binary pairing, compiler/native provenance and MSRV. Resolve changed notice exceptions and the unreached standard-library gate without weakening pins. The reviewed inventory, MSRV metadata review and notice exceptions still name `jsonschema` 0.56.0 and `unicode-width` 0.2.0; `main` now locks 0.58.6 and 0.2.2, so re-review those packages before updating their pins. [Release review](./docs/security/beta-license-release-review.md#current-result), [dependency inventory](./docs/security/beta-dependency-inventory.md) |
| Other platforms and enforcers | Verify native Linux ARM64, alternative Linux-native enforcement and the Linux build under WSL2 (user namespaces, cgroup v2 and Guard enforcement), and require actual platform behavior evidence before support claims. Native Windows is out of scope. [OS-parity priority](./AGENTS.md#product-and-implementation-priorities) |

These gates remain unverified until their named procedure completes. Specs and
existing tests describe what must hold; neither implemented code nor an old
passing run establishes broader support.
