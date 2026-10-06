# Slash Skills and review corrections

## Metadata

- Status: `Implemented`
- Owner: Codex
- Last updated: `2026-10-06`

## Scope and decisions

Review the latest coding/terminal change, fix demonstrated regressions and finish beta 1's user-driven Skill discovery. Broader release/manual checks, native macOS, remote MCP/network integration and the previously deferred Anthropic subscription route retain their existing scope in [next steps](../../NEXT_STEPS.md).

The inline `/` menu combines the closed compiled command registry with at most 32 configured runtime Skill names. Textual Cmd/Skill labels distinguish equal names without depending on color. Command selection retains current completion semantics; Skill selection inserts `$NAME ` as an ordinary task draft, never executes it. Exact command names retain precedence. Only explicitly enabled private `tools.json` supplies Skill candidates; folder trust's generated grant supplies no Skills. The CLI seeds metadata before input, with no YAML, credential, catalog or network work on a keystroke. Selecting a Skill does not grant permissions; its content still loads progressively through the existing pinned Guard Tool. Invalid or unavailable configuration produces visible recovery and no Skill candidates.

Correct initial folder-dialog Escape to return to read-only chat, retain Finish/Delegate response capacity independently of Tool argument capacity, and enforce each Provider request's output cap at the shared Engine boundary. Keep modules, public command names, dependencies and canonical schemas unchanged.

## Contracts, security and verification map

[Terminal](../../docs/specs/terminal.md) owns mixed completion and dismissal; [Tools](../../docs/tools.md) owns progressive runtime Skills; [Provider](../../agents/provider.md) and [Engine](../../agents/engine.md) own response limits. Update missing accepted scenarios before implementation and review contract impact at closeout.

Private configuration, credentials and project data remain separate trust boundaries. Only bounded checked configuration names enter the presentation cache; project Skill content cannot register commands or widen grants. New Skill instructions guide the existing selected Provider, never create another route, retry, executable or disclosure permission. Exact file/resource pins and ordinary Run/Tool/call/deadline bounds remain authoritative. No credential or real account record is inspected by the development agent. Live smoke uses Arany's selected-account owner with synthetic tasks, small account-visible models and low effort, no preliminary three-call checks or automatic retries. Record actual calls/usage and closed replay; a local subscription cap is not a remote usage guarantee.

- Extend existing Composer and monochrome/color frame corpora: command/Skill labels, name collisions, prefix filtering, completion without execution, dismissal, draft/caret, invalid names and bounded cache.
- Extend existing native-function corpus: valid large Finish versus oversized Tool arguments, including escaped output and streamed decoding.
- Extend existing approval journey: excessive review usage asks a human without dispatch or fabricated approval; replay retains its rejection.
- Extend existing folder-consent PTY owner: startup Escape returns to composer, creates no trust and restores the terminal.
- Run repository formatting, strict all-target lint, locked offline tests and executable build. Activate affected Linux owners only with synthetic isolated fixtures.
- Run a short selected-account small-model smoke for direct response and a configured Skill plus guarded edit/command path if prerequisites exist. Leave broader manual/release evidence explicitly pending; missing user-owned prerequisites do not stop implementation.

## Closeout

Implemented the mixed slash menu, configured-name admission and primary Skill guidance. Source/security/diff review found no further actionable defect in the changed paths. No dependencies, permissions, public command names or persistence schemas changed. The additive metadata API exposes only already admitted names; content still belongs to the pinned Guard.

The latest coding change introduced three corrected P2 defects:

- Startup folder-dialog Escape exited the product instead of returning to read-only chat (`src/cli/attached.rs`).
- Native Finish/Delegate arguments inherited the 16 KiB Tool-argument limit, rejecting valid escaped answers (`src/provider/openai/wire.rs`).
- Shared Provider acceptance compared review usage to the global 4,096-token cap rather than the request's 256-token cap (`src/engine/run_loop.rs`).

The configured completion PTY proves actual CLI wiring, draft acceptance, no Run and terminal restoration. Composer and monochrome/color frames cover collisions, invalid/bounded names, exact-command precedence and narrow widths. The extended native-function corpus accepts large escaped Finish in complete and streamed encoding while retaining Tool rejection. Activated Linux consent, approval and Skill/MCP owners pass with synthetic data and closed replay. Full locked offline all-target debug and optimized suites, strict all-target lint, formatting and the optimized executable build pass. The three affected native Linux owners also pass in both profiles; ignored unrelated gates remain unrun.

The first sandbox-only test run exposed fixture ownership/socket restrictions. Native execution then exposed a shared legacy-account-root lock: the selected-account override did not isolate migration. Five existing fixture commands now supply private legacy roots; the full parallel debug suite passes without test retries or weaker assertions. The durable workflow rule is recorded beside the credential override in `agents/credentials.md`. Ratatui PTY observation gates the completed Skill draft on its fresh caret position; full text/labels are owned by frames because native redraw emits cell differences.

Real selected-account `gpt-5.6-luna` at low effort completed two single-agent Runs: direct Finish, then Skill → Read → Edit → Command → MCP List → MCP Call → Finish. All six Tools succeeded with attested Guard receipts; the file's exact SHA-256 matched the edit receipt and MCP returned the expected synthetic value. Shipped `show --output jsonl` exactly matched each original export after the store closed (8 and 26 Events). Actual recorded usage totals eight calls, 16,552 input tokens and 732 output tokens, no preliminary probes, no retries and no alternate billing route. Temporary private synthetic artifacts remain under `/tmp`; no production prompt, token or account record was inspected.

The terminal and Tool contracts now own mixed completion/invocation. `NEXT_STEPS.md` closes this functionality and removes user-driven Skill invocation from beta 2 expansion while retaining broader manual/release/platform work and the already deferred Anthropic subscription route. Live evidence is scoped to the exact selected route/model, not full beta release readiness.
