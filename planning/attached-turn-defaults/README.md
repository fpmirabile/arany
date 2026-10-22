# Attached-turn Goal defaults

## Metadata

- Status: `Ready`
- Owner: Fernando Mirabile
- Last updated: `2026-10-22`

## Outcome and scope

New Goals use peer defaults across attached chat, `exec` and `serve`: no aggregate Provider-call, token or active-time ceiling, with 16 Runs and every finite per-action/storage bound retained. The harness continues to account, persist, cancel and recover work durably. This delivers the budget half of [Daily use first item 5](../../NEXT_STEPS.md#daily-use-first); [draft attachments](../large-draft-attachments/README.md) ships independently.

## Research

Repository references below name the inspected file and line at this worktree. Peer references identify the inspected upstream source or official documentation; they are comparisons, not Arany contracts.

| Finding | Evidence |
|---|---|
| The actual default is **64 Provider calls, no token ceiling, 1,800,000 ms (30 minutes) active time, 16 Runs**. The finite call/time defaults are also the largest accepted finite values, so choosing 256 calls or two hours would require changing validation, not just settings. | [session/events/goal.rs:4–34,48–60](../../crates/arany/src/session/events/goal.rs); [goals.md:27](../../docs/specs/goals.md#lifecycle-budgets-and-recovery) |
| Attached preferences use `GoalBudget::default()`. Submission loads those preferences and applies the budget only to a new Goal; resume uses the pinned budget. Missing settings use defaults; invalid records have a visible attached fallback, while unsafe State access errors propagate. | [attached/controls.rs:77–87,330–389](../../crates/arany-cli/src/cli/attached/controls.rs); [attached/active.rs:441–458](../../crates/arany-cli/src/cli/attached/active.rs); [terminal.md:140–142,291](../../docs/specs/terminal.md#interactive-commands-and-completion) |
| These preferences are shared. `exec` and `serve` load `attached::execution_limits`; one shared default reaches all three clients. | [attached.rs:40–44](../../crates/arany-cli/src/cli/attached.rs); [exec.rs:322–326](../../crates/arany-cli/src/cli/exec.rs); [serve/threads.rs:163–178](../../crates/arany-cli/src/cli/serve/threads.rs) |
| Calls include primary, children, review and compaction, plus charged fallback/retry attempts. Active time excludes approval/capacity waits and pause. Exhaustion closes a Goal as Failed; cancellation leaves it Interrupted. An exhausted Goal requires explicit continuation as a new Goal, not reopening or an automatic retry. | [goals.md:21–41,69–75](../../docs/specs/goals.md); [execution.md:183,213](../../docs/specs/execution.md) |
| There is no separate fixed primary Tool-step ceiling. Finite request, context, Event, Store and native action bounds still apply when Goal dimensions are unlimited. `/settings` already offers unlimited calls and active time. | [execution.md:135](../../docs/specs/execution.md#accepted-outcomes); [goals.md:41](../../docs/specs/goals.md); [attached/controls.rs:240–261](../../crates/arany-cli/src/cli/attached/controls.rs) |
| **How often ordinary turns exhaust the default: unknown.** The roadmap names the problem but supplies no sample, numerator, denominator or workload definition. The deterministic Goal corpus proves exhaustion behavior, not its real-world frequency. Lossy diagnostics/telemetry cannot supply a complete denominator. No private Session data was inspected. | [NEXT_STEPS.md:16–22](../../NEXT_STEPS.md); [goals.md:119–123](../../docs/specs/goals.md#acceptance-scenarios-and-evidence-owners); [diagnostics.md:7](../../docs/specs/diagnostics.md); [telemetry.md:5,21](../../docs/specs/telemetry.md) |

Peer snapshots were inspected on 2026-10-22, not executed. They do not establish native support or freedom from account/service quotas.

| Peer | Interactive limit and exhaustion behavior | Primary evidence |
|---|---|---|
| Claude Code | `--max-turns` has no default limit and applies to print mode only; when set, exhaustion exits with an error. `--max-budget-usd` is also print-only. The docs establish no numeric ordinary interactive turn/step/token ceiling; that is not proof about every closed-source internal stop. | [CLI flags](https://code.claude.com/docs/en/cli-reference#cli-flags) |
| Codex CLI | The inspected ordinary loop has no fixed step counter; it continues for Tool work/pending input, stops without follow-up, and compacts for context pressure. Failed auto-compaction ends the turn with an error. An optional shared rollout token budget is **disabled by default**; when enabled it needs a positive limit, charges response usage, and propagates `SessionBudgetExceeded` through the turn error path. Context capacity is a separate bound. | Revision `342ed69a174c206c1de52a59491b131e0753cfa5`: [loop:459–489](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/core/src/session/turn.rs#L459-L489), [continuation/compaction:580–725](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/core/src/session/turn.rs#L580-L725), [feature default:1975–1980](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/features/src/lib.rs#L1975-L1980), [config:2974–3003](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/core/src/config/mod.rs#L2974-L3003), [budget error:11–16](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/core/src/agent/control/budget.rs#L11-L16), [usage:3209–3220](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/core/src/session/turn.rs#L3209-L3220), [error:891–906](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/core/src/session/turn.rs#L891-L906) |
| Codex `exec` and app-server | `exec` sends app-server `TurnStart`, then waits for notifications or interruption without a whole-turn timer or step counter. It uses the same core turn loop and default-disabled rollout token budget above. App-server completes a turn on model completion or explicit interruption; command execution deadlines are per action, not turn ceilings. | Pinned [exec submission](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/exec/src/lib.rs#L1210-L1280), [event loop](https://github.com/openai/codex/blob/342ed69a174c206c1de52a59491b131e0753cfa5/codex-rs/exec/src/lib.rs#L1300-L1389); official [app-server lifecycle](https://learn.chatgpt.com/docs/app-server#typical-flow) |
| OpenCode | Optional agent `steps`; without it, the agent continues until model stop or user interruption. At the configured limit, it requests a final text-only summary/remaining work and disables Tool execution. No default cumulative whole-turn token limit is established by these sources. | [Agent max steps](https://opencode.ai/docs/agents/#max-steps); revision `248bfbe1117da1e7cc23bd41273188318afec3a5`: [limit and final call:210–240](https://github.com/anomalyco/opencode/blob/248bfbe1117da1e7cc23bd41273188318afec3a5/packages/core/src/session/runner/llm.ts#L210-L240), [Tool guard:90–100](https://github.com/anomalyco/opencode/blob/248bfbe1117da1e7cc23bd41273188318afec3a5/packages/core/src/session/runner/step.ts#L90-L100), [summary instruction:1–16](https://github.com/anomalyco/opencode/blob/248bfbe1117da1e7cc23bd41273188318afec3a5/packages/core/src/session/runner/max-steps.ts#L1-L16) |

The shared answer is no default aggregate turn ceiling, including headless work; account quotas, context capacity and individual operation deadlines are separate. Arany retains its journaled shared ledger and finite per-action admission. Do not copy OpenCode's final inference after exhaustion into an exhausted Arany Goal: the extra call would require already-admitted budget.

Working rules: [AGENTS.md:23–24](../../AGENTS.md#product-and-implementation-priorities), [planning RPI gates](../PLANNING.md), [security triggers](../../agents/security.md#security-pass-triggers), [Goal evidence owners](../../docs/specs/goals.md#acceptance-scenarios-and-evidence-owners) and [documentation checks](../../docs/development.md#verification). Design review uses the pinned [Ponytail source](https://github.com/DietrichGebert/ponytail/blob/e3ba2aa6f1e6f0bc4d69eb09c9f0d0a93af56156/skills/ponytail/SKILL.md), [Rust practice](../../agents/skills/rust-clean-code/SKILL.md) and [architecture review](../../agents/skills/clean-architecture-review/SKILL.md).

## Contract and scenarios

- Owning specs: [Goals](../../docs/specs/goals.md#lifecycle-budgets-and-recovery) owns defaults and charging; [Terminal settings](../../docs/specs/terminal.md#interactive-commands-and-completion) owns their display/selection. [Serve](../../docs/specs/serve.md) and the `exec` adapter consume the same default. New Goals use `max_provider_calls: None`, `max_tokens: None`, `max_active_ms: None`, `max_runs: 16`.
- Success: fresh attached, `exec` and `serve` submissions complete scripted work beyond 64 calls and 30 active minutes. `/settings` shows unlimited dimensions before submission; committed `GoalStarted`, usage and replay agree. Explicitly selected finite limits still reject the next inadmissible action with a committed exhaustion receipt.
- Rejection/recovery: unsafe settings access fails admission; malformed settings follow the existing visible recovery/default path and never supply unchecked values. Cancellation/pause/crash preserve Interrupted/Uncertain facts and charges. Resume uses that Goal's pinned budget; exhaustion requires explicit continuation into a new Goal, never automatic retry.
- Evidence owners: [Goal corpus](../../crates/arany/tests/engine_run/goals.rs), [stop projection](../../crates/arany/src/session/stop.rs), [settings corpus](../../crates/arany-cli/src/cli/attached/controls.rs), [slash journeys](../../crates/arany-cli/tests/session_run/slash_completion.rs), [active terminal](../../crates/arany-cli/tests/session_run/active_terminal.rs), [exec adapter](../../crates/arany-cli/src/cli/exec.rs) and [serve conformance](../../crates/arany-cli/src/cli/serve). Native/live daily-use evidence remains in [NEXT_STEPS](../../NEXT_STEPS.md).

## Chosen direction

- **Decided 2026-10-22 (owner; peer parity above):** use unlimited call/token/active-time defaults for attached turns, `exec` and `serve`. Keep 16 Runs and every finite per-action, context, storage and Guard bound. No arbitrary larger ceiling or client-specific budget policy.
- **Decided 2026-10-22 (owner):** Arany has no users or saved settings to preserve. Remove legacy-default detection, conversion questions, compatibility branches and migration work. This does not remove current user-selected limits or the immutable budget of a running Goal.
- Simplest sufficient solution after Ponytail: change the existing shared `GoalBudget::default()` and its displayed defaults, using already-supported nullable dimensions. Keep finite-value validation and the settings loader's existing admission/recovery flow. No new budget type, setting, record shape or migration.
- Linux/macOS: identical policy and serialization; no new OS API, dependency or native-support claim.
- Ownership: CLI translates settings into `GoalBudget`; Engine validates/pins it; GoalLedger reserves/settles usage; reducer validates history ([system overview](../../docs/architecture/system-overview.md#2-runtime-architecture)). The seam stays in-process. Journal acknowledgment precedes dispatch; cancellation does not refund calls, and lost acknowledgment never authorizes retry.

## Security and resource model

The [security pass](../../agents/security.md#security-pass-triggers) applies to execution/session policy and default-unlimited LLM cost. Assets are paid usage, private Session data, journal capacity and Workspace effects. User settings or compiled defaults select budgets; repository/model text cannot. `GoalStarted` commits the normalized budget before Provider calls. Policy, typed grants, Guard enforcement and descendant authority remain unchanged.

There is **no aggregate dollar-spend guarantee**. Arany deliberately retains 16 Runs, the 10,000-Event lineage limit, terminal headroom and finite request/response, context, Store, action-time and Tool bounds ([Goals](../../docs/specs/goals.md#lifecycle-budgets-and-recovery), [Execution](../../docs/specs/execution.md#persistence-bounds)): peer default-unlimited execution cannot override journal-first durability, durable budget accounting or per-effect isolation. Resource exhaustion stops dependent work durably; unknown effects stop continuation without retry. Startup/private-State admission and content retention remain unchanged.

Verify default and explicitly finite settings, malformed/unsafe admission, shared multi-role charges, crash/pause accounting, no dispatch after exhaustion and strict replay with zero Provider/effect calls. Record evidence and non-claims in the implementation PR.

## Risks and mitigations

- Higher paid usage → display unlimited dimensions plainly; retain pause/cancel and selectable finite limits.
- One adapter accidentally retains the previous default → exercise absent settings through attached, `exec` and `serve`, observing committed budgets and completed work.
- Aggregate limits mistaken for action limits → prove finite action timeout, Run-count and storage rejection still apply.
- Unknown exhaustion frequency → make no usage-rate claim; peer parity, not a fabricated workload percentile, determines the default.

## Vertical slices

1. **One PR: new Goals use peer defaults across attached, exec and serve with durable accounting intact.** Cross shared defaults/settings display → all three submission adapters → Engine validation → journal reservations → outcome presentation → reopen/replay. Update Goals and Terminal once, plus Serve only where its contract describes defaults. Extend existing settings and adapter journeys for a 65th call, active time beyond 30 minutes, explicitly selected finite-limit rejection, malformed/unsafe settings and pinned-budget resume. Use deterministic paused time after process I/O. Reuse the Goal corpus for multi-role charges, cancellation/crash and strict replay; verify exact visible receipts and reopened file-backed Events. No compatibility corpus for older defaults, benchmark or new infrastructure.

## Exit criteria

- **Research gate passed:** current defaults, callers, exhaustion and peer interactive/headless behavior have source evidence; real-world exhaustion frequency remains unknown and is not a premise.
- **Plan gate passed:** owner decisions and peer evidence resolve the policy; status is `Ready`, with no residual owner question.
- **Implement gate:** the vertical slice proves success, finite rejection, cancellation, crash and replay through existing owners. Formatting, Clippy, targeted tests and required full CI pass. Builds use this worktree's `./target` or the team Cargo wrapper, never `CARGO_TARGET_DIR` or `/tmp` output.
- Delivered behavior belongs in the owning specs. Missing native/live checks remain in the existing NEXT_STEPS section; close this plan and its index row under the [planning lifecycle](../README.md#lifecycle).
