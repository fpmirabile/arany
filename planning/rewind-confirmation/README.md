# Rewind confirmation stays valid while reviewed

## Metadata

- Status: `Done`
- Owner: Fernando Mirabile
- Last updated: `2026-10-24`
- Next action: owner-run native Linux/macOS rewind confirmation evidence is tracked in [NEXT_STEPS](../../NEXT_STEPS.md#account-recovery-and-native-guard-follow-ups).

## Outcome and scope

A live rewind preview remains confirmable until the user answers or its pinned state changes. This is a separate user capability from Tool permission prompts: it has its own preview token, restore selection and overwrite confirmation. It therefore has its own plan under [PLANNING.md](../PLANNING.md). Conversation-only, code-only and combined restore share this feature.

## Research

Inspected revision: `cb87fa2cdd97fbfc2af06f91ebdfcec321f90bf0`.

| Owner | Verified current behavior |
|---|---|
| [tools.rs:284](../../crates/arany/src/tools.rs) | `rewind_pin` sets a 60-second expiry. |
| [engine/rewind.rs](../../crates/arany/src/engine/rewind.rs), 184/192 | Conversation rewind checks the pinned head/prefix and expiry before preparation, then expiry again before finishing. |
| Same file, 320/434–439 | Code restore checks preview expiry, copies it into the effect intent and checks it again before journal admission. |
| [session/rewind.rs](../../crates/arany/src/session/rewind.rs), 101/220 | `WorkspaceRewindStarted`'s record carries `expires_at_ms`; `valid()` requires it to be positive. Removing it changes the durable payload and digest, not just the UI timer. |
| [attached/rewind.rs:294](../../crates/arany-cli/src/cli/attached/rewind.rs), [serve/rewind.rs](../../crates/arany-cli/src/cli/serve/rewind.rs) | Conversation and code previews have 60-second deadlines; serve keeps one preview and returns `expiresAtMs`. |
| [engine.rs:76](../../crates/arany/src/engine.rs), [attached rewind](../../crates/arany-cli/src/cli/attached/rewind.rs) | Error copy says the preview “changed or expired”; it must stop attributing state invalidation to elapsed time. |
| [Serve:153](../../docs/specs/serve.md#rewind) | Token binds thread, checkpoint, head/prefix, plan and observed content; replacement, submission or close invalidates it. Expiry is currently part of that contract. |

[Claude checkpointing](https://code.claude.com/docs/en/checkpointing#rewind-and-summarize) documents explicit restore selection and cancellation, but does not establish a numeric preview lifetime. [Tool approval research](../approvals-wait/README.md#peer-behavior) verifies indefinite ordinary human permission waits in Claude/Codex; it is not proof of rewind internals. The owner requested removing attached-human wait expiry. Apply that rule here without asserting undocumented peer parity.

Rules: [planning/template](../TEMPLATE.md), [security](../../agents/security.md#security-pass-triggers), [documentation checks](../../docs/development.md#verification), [Rust practice](../../agents/skills/rust-clean-code/SKILL.md), [architecture review](../../agents/skills/clean-architecture-review/SKILL.md). Ponytail keeps the existing preview and confirmation path; no new service or dependency.

## Chosen direction

**Decided 2026-10-22 (owner; live-confirmation rule):** remove preview wall-clock expiry. Preserve one live, one-use token; explicit restore/overwrite choice; pinned head/prefix/content/grant/enforcement identity; cancellation; and validation immediately before publication. Changed, replaced, consumed, foreign or closed previews refuse. Never refresh preconditions implicitly or restore authority after restart.

**Decided 2026-10-22 (journal-first guarantee):** remove `expires_at_ms` from the current rewind record and serve preview shape, advance the `WorkspaceRewindStarted` payload version, and ship a registered migration step and fixture under [journal migration](../../docs/specs/execution.md#journal-migration). Recompute/validate affected record bindings through the migration owner; never fabricate a verdict or reusable preview. No compatibility reader or legacy-default preservation project is added.

Engine owns immutable preview validation and journal ordering; terminal/serve own one live confirmation handle. The Guard owns bounded restore execution. This stays within existing in-process interfaces and the separate Guard. The stale-preview error keeps a clear state-change/re-preview meaning; all UI/schema/spec occurrences lose clock-only language in R1. A1 can remove general EffectIntent expiry while preserving the separate preview checks until R1.

Linux/macOS share policy; native restore/cancellation still needs each platform's evidence. Source review claims no native support.

## Contract and scenarios

Update [Tools checkpoints](../../docs/specs/tools.md#checkpoints-and-rewind), [Serve conversation rewind](../../docs/specs/serve.md#rewind), [Terminal rewind](../../docs/specs/terminal.md#rewind), [Execution rewind/payload rules](../../docs/specs/execution.md) and the [Session rewind contract](../../docs/specs/session.md#conversation-rewind).

| Scenario | Required observation / owner |
|---|---|
| Long live confirmation | Advance deterministic time past 60 seconds, confirm unchanged conversation/code/both preview, observe one restore and matching closed-store receipt. [Engine rewind corpus](../../crates/arany/src/engine/rewind.rs), [serve corpus](../../crates/arany-cli/tests/serve/in_process.rs), [attached rewind](../../crates/arany-cli/src/cli/attached/rewind.rs). |
| Drift or token invalidation | Changed head/content/grant, replaced/used token, new submission or closed channel refuses without publication; overwrite remains explicit. Same owners plus [rewind reducer](../../crates/arany/src/session/reducer/rewind_tests.rs). |
| Cancel, crash and uncertainty | No automatic restore/retry or recreated approval; unknown publication remains unknown; strict replay makes zero effect calls. Reducer/Engine owners above. |
| Changed payload | Version registry and migration fixture agree, malformed bindings reject, reopening never creates a live token. [Migration fixtures](../../crates/arany/src/store/migrate/tests.rs), [Event corpus](../../crates/arany/src/session/events/tests.rs). |

## Security and resource model

Approval/capability/stored-shape changes trigger the security pass. Assets are Workspace bytes, checkpoint integrity and user confirmation; client selections and displayed paths are untrusted. A token is not a path grant. Retain private-State admission, no-follow/content checks, bounded file/byte/selection limits, one preview per connection, finite Guard queue/execution/cleanup and terminal headroom. A preview holds bounded metadata, not an indefinitely running restore sandbox. No heartbeat, credential path, new descendant authority or network capability is added. Output overflow fails within existing protocol limits. Unknown effect outcomes stop recovery/continuation as specified.

## Risks and mitigations

- Long review increases drift opportunity → compare original pins at dispatch, never renew them silently.
- Removing only a UI timer leaves Engine or reducer expiry → use the complete inventory above, version the record and verify cold replay.

## Vertical slices

1. **R1 — A live rewind confirmation survives elapsed human time across terminal and serve.** One PR crosses preview selection → Engine head/content admission → one-use confirmation → journal/Guard restore → restored/refused output and schema → owning specs and existing scenarios. Remove both conversation and code expiry checks, wire field, record field and expiry copy together; preserve cancellation, overwrite confirmation and finite execution limits. Include the payload-version step/fixture and late-confirmation, drift, token, crash and replay observations.

## Exit criteria

- **Research gate passed:** current expiry/validation owners and peer evidence limits are explicit.
- **Plan gate passed:** owner policy and guarantees settle the choice; no owner-only question remains.
- **Implement gate:** R1 proves exact output, reopened Events and reduced views plus interactive observations; required formatting, Clippy, targeted tests and full CI belong to implementation. No Cargo checks run for this documentation update.
- Record unavailable native/live evidence in the existing [NEXT_STEPS](../../NEXT_STEPS.md) owner during implementation; close this plan/index row under the [planning lifecycle](../README.md#lifecycle) on delivery.
