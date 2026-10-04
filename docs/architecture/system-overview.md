# Arany beta architecture

**Status:** canonical first implementation  
**Date:** 2026-10-04
**Design rule:** build a familiar durable harness, then deepen it without replacing its Engine

The research documents describe the larger design space. This document defines the beta code and product contract; the reconciled gates live in the [decision register](../research/next-step-decision-register.md). Anything absent here is not an empty module waiting to be scaffolded.

## 1. What the beta must prove

Arany is an interactive Session-oriented CLI, not a one-shot team demo. Bare `arany` creates a durable Session; every submitted user Message starts one bounded Run; each Run has one accountable primary AgentRun and may admit `0..N` direct read-only children.

The full product contract is successful when it proves all of these together:

1. a Session survives process exit, explicit resume, multiple Runs, provider changes between Runs, compaction, and deterministic replay;
2. `single`, `auto`, and `team` policies all use the same reusable loop and a generic ordered child collection rather than a fixed two-worker shape;
3. the primary cannot finish team work before every required child has a persisted terminal disposition;
4. the default terminal keeps a navigable committed transcript in the primary screen, conditional agent activity above a fixed-bottom composer, and a compact status row below input;
5. keyboard access is complete, while mouse reporting exists only transiently inside an open picker or detail panel;
6. native OpenAI and Anthropic plus every admitted exact custom endpoint/model profile satisfy the same semantic Provider contract;
7. exact Workspace-root `AGENTS.md` loads first, with exact `CLAUDE.md` as absence-only fallback;
8. `exec` and `show` expose the same committed facts without terminal initialization; and
9. cancellation reaches the primary and every admitted child without fabricating completion; and
10. explicitly enabled primary Tools use typed one-use intents, restrict-only Policy, native Guard enforcement, bounded continuation and durable non-executing replay.

The current personal beta 1 implementation milestone is narrower: build a local Linux executable and complete the current OS user's ChatGPT account, model/effort, durable Engine and chat flows, plus an explicitly enabled local coding foundation for guarded file operations, offline commands, progressive Skills and stdio MCP. Automated synthetic evidence owns these implementation claims. Real sign-in, plan-consuming turns/tool loops, remaining native credential checks, and manual UX/accessibility testing belong to the final user handoff; they do not block implementation and remain unverified support claims until performed. The broader contract above is not a claim that beta 1 has cross-platform or redistribution evidence. Native macOS validation, portable distribution, and additional performance gates follow after beta 1; the [decision register](../research/next-step-decision-register.md) records the scope and Anthropic subscription authorization boundary.

Startup resolves trusted user/process configuration, admits a private state root outside the Workspace, selects one ProviderProfile, and pins the caller-selected Workspace before consuming project-controlled input. Admission never executes or discovers Git/repository configuration, `.env`, hooks, plugins, packages, tests, or startup commands. Instruction and explicit include files remain bounded immutable no-follow snapshots. Default Runs are read-only; `--tools` adds only the private host-approved grant and separately enforcing Linux Guard described in [local tools](../tools.md). Unsupported native enforcement fails closed. Live tool-capable model and native macOS claims remain unverified.

## 2. Runtime architecture

There is one Rust package, one long-lived process, and one substitutable Engine behavior seam. Short-lived same-binary children isolate blocking OS-credential calls and effectful Tool execution at separate privilege boundaries; neither is a second Client or daemon.

```mermaid
flowchart LR
    User([User or script]) --> CLI[Arany CLI]

    subgraph Process[Main Arany process]
        CLI --> Engine[Deep Engine]
        CLI --> Terminal[Inline terminal or deterministic output]
        Engine --> Session[Session lifecycle + context]
        Engine --> Loop[Reusable AgentRun loop]
        Loop --> Primary[Primary AgentRun]
        Primary --> Children[Ordered 0..N child AgentRuns]
        Primary --> Tools[Private Tool router + Policy]
        Engine --> Store[(SQLite owner thread)]
        Store --> Views[SessionView + RunView]
        Views --> Presentation[Pure PresentationModel]
        Presentation --> Terminal
        Engine -. committed safe facts .-> OTLP[Private OTLP projection]
    end

    Loop --> Provider{Provider interface}
    Provider --> Fake[Strict scripted Provider]
    Provider --> OpenAI[Native OpenAI]
    Provider --> Anthropic[Native Anthropic]
    Provider --> Custom[Exact verified custom profile]
    Engine --> Workspace[Pinned Workspace input]
    Tools --> Guard[Bounded same-binary native Guard]
    Guard --> Files[Typed preconditioned file operations]
    Guard --> Snapshot[Offline command / Skill / MCP resources]
    OTLP -. opt-in .-> Collector[Loopback Collector]
    CLI --> CredentialHelper[Bounded OS-credential helper]
    CredentialHelper --> Keyring[OS keyring]

    classDef deep fill:#173b34,color:#fff,stroke:#5eead4,stroke-width:2px;
    classDef canonical fill:#2b2346,color:#fff,stroke:#c4b5fd,stroke-width:2px;
    class Engine,Session,Loop,Primary,Children deep;
    class Store canonical;
```

Provider is the only substitutable Engine behavior because several implementations exist at release. Scheduling, Session/context compilation, SQLite, replay, Workspace input, reducers, terminal presentation, and telemetry are private Engine responsibilities. Native OpenAI and Anthropic are supported adapters. A custom profile is admitted only for one exact protocol/origin/model/capability-evidence tuple. A built-in constrained OpenRouter profile is deferred until its broker-route and privacy gates pass; Z.AI is not admitted while it lacks the required strict outcome contract.

The current-thread Tokio coordinator owns async lifecycle. One named standard-library thread owns the sole synchronous SQLite connection. A bounded telemetry worker owns OTLP export. Neither storage nor telemetry blocks Provider progress on the coordinator.

## 3. Physical beta shape

```text
Cargo.toml
src/
├── main.rs          CLI grammar, mode selection, exit mapping
├── cli.rs           shared CLI output and state-path selection
├── cli/
│   ├── attached.rs  attached Session lifecycle and composer loop
│   ├── attached/
│   │   ├── active.rs    active Run and compaction terminal handling
│   │   ├── agents.rs    agent inspector command handling
│   │   ├── controls.rs  trusted local command handlers
│   │   ├── picker.rs    private Session picker input and selection
│   │   ├── models.rs    bounded attached model catalog browsing
│   │   ├── run.rs       selected native Provider Run composition
│   │   └── setup.rs     attached account selection
│   ├── chatgpt.rs  consented OAuth account access and selected catalog
│   ├── chatgpt/
│   │   ├── account.rs  bounded protected subscription account records
│   │   ├── account/
│   │   │   └── tests.rs synthetic renewal and storage cases
│   │   ├── callback.rs bounded one-result loopback HTTP callback
│   │   ├── catalog.rs  bounded account-scoped subscription model listing
│   │   ├── consent.rs  versioned account-bound warning acceptance
│   │   ├── exchange.rs bounded token/JWKS redemption
│   │   ├── exchange/
│   │   │   ├── revoke.rs private discovered-endpoint token revocation
│   │   │   └── tests.rs synthetic issuer and signed-identity corpus
│   │   ├── identity.rs strict signed ID-token admission
│   │   └── registration.rs stable host and issued-client registration
│   ├── credentials.rs protected default native API account access
│   ├── credentials/
│   │   └── keyring_helper.rs supervised bounded OS-store process
│   ├── exec.rs      one-Run command composition and durable receipt
│   └── provider.rs  model catalog and data-free custom profile check commands
├── lib.rs           small public interface for the deep Engine
├── diagnostics.rs   closed local debug failure stages and bounded stack frames
├── engine.rs        Run admission, budgets, Provider outcomes
├── engine/
│   ├── compaction.rs private manual and duplicate-safe automatic compaction
│   ├── lifecycle.rs private Session create/resume/fork/default operations
│   ├── progress.rs  committed bounded Run observation
│   ├── run_loop.rs  private Run orchestration and terminal transitions
│   └── run_loop/
│       └── children.rs private bounded child scheduling and synthesis
├── session.rs       Session module interface
├── session/
│   ├── events.rs    typed canonical facts and scope validation
│   ├── reducer.rs   strict Session/Run/AgentRun replay
│   ├── input.rs     pinned no-follow Workspace snapshots
│   ├── input/
│   │   └── race_tests.rs ignored Linux component-replacement gate
│   ├── context.rs   deterministic bounded history selection
│   ├── lineage.rs   canonical Event-prefix digest for fork replay
│   └── compaction.rs bounded summary input and snapshot validation
├── provider.rs      Provider semantic contract and adapter selection
├── provider/
│   ├── custom.rs     private exact custom-profile admission
│   ├── custom/
│   │   ├── adapter.rs      verified semantic custom Provider
│   │   ├── destination.rs  bounded resolution and pinned HTTP destination
│   │   ├── check.rs        synthetic conformance and evidence orchestration
│   │   └── transport.rs    shared bounded custom wire and egress
│   ├── anthropic.rs  native Messages key and bounded HTTP behavior
│   ├── anthropic/
│   │   ├── wire.rs  strict Messages documents and decoding
│   │   └── wire/
│   │       └── tests.rs  offline wire corpus
│   ├── effort.rs     reviewed native model-specific effort admission
│   ├── image.rs      bounded immutable PNG values and semantic origin
│   ├── catalog.rs    bounded account-scoped native model discovery
│   ├── native_check.rs  exact-key synthetic conformance and evidence admission
│   ├── openai.rs     native Responses key and bounded HTTP behavior
│   └── openai/
│       ├── subscription.rs bounded ChatGPT-plan Provider and synthetic probe
│       ├── subscription/
│       │   └── tests.rs synthetic stream and local-limit corpus
│       ├── wire.rs  strict Responses documents and decoding
│       └── wire/
│           └── tests.rs  offline wire corpus
├── tools.rs         private grant admission and concrete effect router
├── tools/
│   ├── config.rs    private closed configuration and resource pins
│   ├── types.rs     semantic calls, immutable intents and receipts
│   ├── fs.rs        no-follow snapshots and atomic native mutations
│   ├── guard.rs     exact unit/cgroup ownership and helper lifecycle
│   ├── guard/
│   │   └── profile.rs Linux namespace facts and compiled seccomp profile
│   ├── mcp.rs       bounded legacy stdio tools protocol and schema checks
│   ├── skills.rs    bounded portable metadata, inside the Guard only
│   └── tests.rs     deterministic grant/schema/receipt corpus
├── store.rs         private SQLite owner and Store interface
├── store/
│   ├── evidence.rs  bounded expiring custom and native Provider evidence
│   ├── journal.rs   SQLite setup, migration, append, fork writes
│   ├── journal/
│   │   └── crash_tests.rs ignored Linux append transaction-death gate
│   ├── replay.rs    bounded strict Event loading and lineage resolution
│   ├── state.rs     private state-root admission inside the store boundary
│   └── state/
│       └── lock.rs  per-Session and account-replacement locks, private file checks
├── presentation.rs  pure linear projections and sanitization
├── presentation/
│   ├── agents.rs    bounded semantic agent inspection
│   └── model.rs     bounded semantic status and activity rows
├── terminal.rs      attached terminal ownership and lifecycle
├── terminal/
│   ├── agents.rs    agent inspector navigation and state
│   ├── commands.rs  closed trusted slash registry and parser
│   ├── clipboard.rs explicit bounded local OS-client lifecycle
│   ├── composer.rs  bounded grapheme-safe draft editing
│   ├── input.rs     bounded reader ownership and semantic input mapping
│   ├── input/
│   │   └── raw.rs   bounded Unix keys, pointer events and paste framing
│   ├── linear.rs    labeled append-only terminal output
│   ├── view.rs      inline Ratatui frame drawing
│   └── view/
│       └── tests.rs  inline frame and picker TestBackend cases
├── telemetry.rs     private opt-in OTLP trace lifecycle and SDK owner
└── telemetry/
    ├── config.rs    numeric-loopback endpoint admission
    ├── tests.rs     topology, privacy, and transport fixtures
    └── tests/
        └── performance.rs ignored Linux release queue-saturation measurement
tests/
├── session_run.rs   journal, replay, and process-level output
├── session_run/
│   ├── active_terminal.rs Linux signals, cancellation, suspend, and restoration PTYs
│   ├── active_terminal/
│   │   ├── acquisition.rs  Linux partial-acquisition restoration PTY
│   │   ├── broken_stderr.rs Linux active-renderer fault PTY
│   │   ├── panic.rs Linux terminal-owner panic-unwind PTY
│   │   └── output_window.rs Linux signal during restored-output PTY
│   ├── agent_inspector.rs Linux active/idle inspector PTYs
│   ├── auto_compaction.rs Linux attached threshold and compaction process journey
│   ├── custom.rs    exact custom-profile process journey in the same test target
│   ├── disk_fault.rs Linux Store file-growth recovery release gate
│   ├── disk_fault/
│   │   ├── enospc.rs ignored Linux private-tmpfs ENOSPC recovery gate
│   │   ├── enospc/
│   │   │   └── product.rs ignored shipped-exec private-tmpfs ENOSPC gate
│   │   └── product.rs ignored Linux shipped-exec failure-channel gate
│   ├── live.rs      ignored paid native direct/team conformance journeys
│   ├── loopback.rs  shared bounded local Provider process fixture
│   ├── performance.rs ignored Linux release replay and team process measurements
│   ├── performance/
│   │   ├── memory.rs  ignored Linux two-file and capped-response RSS measurement
│   │   └── startup.rs ignored two-file startup upper-bound measurement
│   ├── session_picker.rs Linux transient mouse-capture picker PTY
│   ├── session_picker/
│   │   └── termination.rs Linux picker signal and suspend restoration PTY
│   ├── slash_completion.rs Linux Tab, ghost placeholder, and restoration PTY
│   ├── setup.rs    Linux hidden-key cancellation and terminal restoration PTY
│   ├── startup_trust.rs Linux adversarial repository-startup process gate
│   ├── tools.rs     native effect journey, hostile peers, quotas and replay
│   ├── tools/
│   │   └── native_exec.rs shipped native-adapter command continuation over local TLS
│   └── telemetry.rs product-process trace and canonical replay journey
├── common/
│   └── process.rs   bounded concurrent product-output capture including exit/EOF
├── engine_run.rs   scripted Provider and Workspace input journey
└── engine_run/
    ├── performance.rs ignored release two-child core Run measurement
    ├── workspace_security.rs explicit include admission corpus
    └── performance/
        └── cancellation.rs ignored release team cancellation measurement
```

This remains one package and one long-lived process. OS credentials use a supervised same-binary helper; explicitly requested clipboard access uses a bounded trusted OS-client child under terminal ownership. Neither becomes an agent Tool. The private `tools` implementation is one concrete router with deterministic Policy and a native Guard privilege seam, not a generic execution framework or fakeable second Engine behavior seam. The `session/` files remain one deep module, and ChatGPT's stream remains private OpenAI implementation. `engine.rs` holds orchestration behind `Engine::run`, keeping `lib.rs` readable. Add a crate only after measured release, privilege, ownership, dependency or compile pressure.

## 4. Public command contract

```text
arany [GLOBAL_OPTIONS] [PROMPT]
arany --setup
arany --continue
arany --resume [SESSION_ID]
arany --fork SESSION_ID
arany exec [GLOBAL_OPTIONS] --provider openai|anthropic|chatgpt|custom:NAME --model ID [--effort LEVEL] --output text|jsonl PROMPT
arany show [--state-dir DIR] --output text|jsonl SESSION_OR_RUN_ID
arany provider check PROFILE
arany provider check openai|anthropic MODEL --effort LEVEL --accept-cost [--state-dir DIR]
arany provider check chatgpt MODEL --effort LEVEL --accept-cost
arany provider models openai|anthropic|custom:NAME [--state-dir DIR]
arany provider models chatgpt
arany provider logout chatgpt
```

Bare `arany` starts a new persistent Session with a sole saved native API account, a sole selected ChatGPT account, or empty defaults when neither exists. If both protected records exist, it asks for the billing route before loading either credential on every new start; cancelling creates no Session, and an invalid chosen account never falls back to the other route. The ordinary composer is available after selection; submitting an unconfigured objective gives local `/setup` guidance before Provider admission or Workspace input. Idle `/setup` chooses an access method, then either native Provider, hidden API key and catalog-only automatic low-effort model selection, or ChatGPT account reuse, reauthorization, or consented new sign-in followed only by its visible catalog and an automatic low-effort model default. An already selected ChatGPT account offers `Use saved`, `Reconnect`, and `Connect new`; with multiple saved accounts, `Use saved` lists short account IDs and validates an explicit switch under the user account lock. A switch pins account-only defaults until its catalog supplies the automatic model/effort default, while cancelling that picker preserves the old selection. Reauthorization reads metadata and pins the saved backend without loading an old token. Explicit `--setup` opens the wizard before creating a new Session; a completed ChatGPT catalog selection pins the account/model/low effort without inference, while catalog cancellation after account save creates an account-only Session. A positional prompt starts its first Run only after explicit Provider/model admission and remains interactive. `--continue` resumes the admitted-Workspace Session with the greatest committed Event sequence; discovery is bounded to 65,536 Session heads and fails with an exact-ID resume fallback on overflow or malformed history. `--resume` resumes an exact Session or opens the picker; `--fork` creates a new Session at the source's latest committed Run boundary. Nothing resumes implicitly merely because a directory or TTY matches.

The human setup and headless credential paths are documented in [Account setup](../setup.md).

`exec` is deterministic one-Run automation and creates its own Session by default; appending to an existing Session requires an explicit Session ID. `show` is deterministic read-only inspection: a Session ID selects its direct canonical Events, including any fork reference, while a Run ID selects only that Run's Events after strict parent-Session validation. Text keeps the parent Session metadata; JSONL contains the selected Events. Missing or conflicting Run ownership fails without output. There is no `run` alias.

The beta slash registry is closed and trusted:

```text
/help  /setup  /paste  /status  /sessions  /new  /clear  /resume  /fork  /rename
/compact  /agents  /provider  /models  /model  /permissions  /quit  /exit
```

`/clear` is the familiar alias of `/new`; it never deletes existing history. The new conversation preserves the current next-Run selection, not startup preferences, without inheriting title, messages or derived context. The [Session contract](../../agents/session.md#session-commands) owns this distinction and its unchanged fresh-admission requirement. Idle `/setup` owns protected account selection and is locked during a Run. `/agents` owns agent inspection and next-Run collaboration settings; Arany does not invent a second `/team` spelling. Idle `/models` pages through the selected Provider's bounded catalog, with a local inline model-ID filter and numbered linear pages. It may stage an unreviewed native row with explicit effort, without a mandatory synthetic check; the browser is locked during a Run. `/provider`, `/model`, and collaboration settings mutate only idle Session defaults. A changed Provider clears model/effort; model activation atomically saves both choices. During a Run the latter controls display pinned values. Commands never become Provider input; `//text` escapes a leading slash. `exec` and `show` treat slash, at-sign, and exclamation prefixes literally.

The inline composer previews only compiled slash names and argument schemas. Tab fills a unique command prefix, static Provider/collaboration value, or a selectable model from setup or the last explicit model catalog; effort candidates follow the current reviewed native model or loaded exact custom profile. An ambiguous command-name menu keeps Up/Down focus above the input: Tab or Enter fills that name without execution, while a later Enter uses ordinary admission. Active-Run inspection shares completion; maintenance still retains every submission. Escape preserves draft/caret, and exact names, arguments, multiline or mid-draft edits take precedence over the menu. The catalog cache is scoped to Provider plus the Session's optional account UUID; changing either invalidates its candidates without editing the draft or reading a credential. Same-source redraws and model-only changes retain the cached catalog. Argument placeholders are never draft bytes or submitted input and remain visible until that argument starts; monochrome brackets retain their meaning. Linear/screen-reader input remains canonical and announces literal arguments instead of capturing Tab. Model catalogs and credential sources are not consulted on keystrokes.

Bare idle `/model` and `/models` share a bounded list inside chat above the retained composer. Up/Down focuses a model, Left/Right changes only that row's tentative reasoning, Enter persists both atomically and Escape preserves defaults/draft/caret/reading position. Filtering stays local; each row keeps its effort across focus/filter changes. The panel overlays lower transcript rows without changing History's viewport or anchor. Below 16×8 only resize/close is admitted; small geometry may temporarily clip the multiline preview without changing draft bytes. Linear mode accepts `NUMBER [EFFORT|default]` on numbered pages. Reviewed native rows offer allowed levels plus default, unknown native/ChatGPT rows explicit closed levels, custom rows declared levels plus default. Typed `/model ID [EFFORT|default]` shares activation; changed native/ChatGPT models start at low, the same model retains effort. Selection is staging with no token load, renewed consent or inference. Run admission still enforces current account/consent and exact custom destination evidence. The separate `/effort` command and quick action are retired.

An unknown idle slash command, locally invalid argument or incomplete local objective selection leaves the inline draft and caret in place for correction before any Session or Provider effect. Attached objective submission shares its local model/account/required-effort validation with Provider admission before consuming the draft; it performs no credential, State, Workspace or Provider I/O and grants no capability evidence. The same inline recovery applies to a parser-rejected slash command during a Run; it cannot steer that Run. The compiled-registry parser rejects extra text on no-argument controls before idle or active dispatch, with a typed error containing only the entered compiled name, including aliases; trailing whitespace remains accepted. Selection-dependent argument validation stays with idle CLI admission, while valid active controls keep their pinned inspection or locked behavior. Errors after State access or an attempted effect do not restore a command that could be resubmitted. Canonical screen-reader input reports a local rejection and clears the submitted line instead: that line cannot be edited after Enter, and the next physical command must not join it.

Idle `/rename` validates the existing 1-to-128-byte UTF-8 title bound before consuming its command. An oversized inline title remains correctable at its original caret without State access; linear input starts a fresh physical line instead. Only the successful Session operation commits `SessionRenamed`, and the canonical operation independently enforces the same bound. This does not restore or retry commands after State or Workspace failure.

The attached CLI labels parser, local-argument, input-capacity and invalid-line rejection once through the terminal's existing compiled `Error:` notice convention. Domain validation text stays undecorated; the caller already knows whether the submission was rejected. Capacity rejection leaves inline text/caret intact, and linear segment rejection adds no bytes to the accepted logical draft. Caught typed Session-operation and effort-admission failures use that same error feedback without restoring a consumed command or promising rollback; resume/fork change the local view only after a valid durable result. The private Session-picker result separates unavailable and empty lists from terminal-owner errors: the attached caller reports the former as an error and the latter as ordinary guidance, while drawing/input/cleanup errors propagate to restoration and exit. Pre-Session picker entry still fails on either unavailable or empty lists. Catalog-operation failure receives contextual recoverable error feedback at its completion boundary without committing defaults or consuming a retained task draft; terminal-owner failure and shutdown still propagate separately. A failed or cancelled query does not promise rollback of credential renewal already started by the account owner. The terminal renders a textual error heading with an optional semantic accent, not a color-only distinction. Valid continuation, loading progress, read-only inspection, locked controls, empty submissions and setup guidance retain their separate ordinary-notice behavior. Feedback remains local and never becomes canonical Session state or Provider input.

Attached selected-Provider admission races the Engine's existing sticky cancellation waiter before Engine construction. A private typed pre-Run interruption returns to the same Session and new draft after strict reload, without an accepted objective, Run receipt, automatic retry or rollback claim for delegated account work. Ordinary admission errors retain chat recovery; terminal-owner faults retain bounded shutdown. Recoverable completion restores the canonical reader's actual retained byte count after panel cleanup. Pre-Session positional-prompt validation is unchanged. No second cancellation channel or canonical Event is introduced.

Idle `/provider chatgpt` validates the selected account before any defaults mutation. A typed account-selection failure receives compiled chat error feedback and preserves the current Provider/model/effort/account source; no sign-in, retry or billing change follows automatically. Custom effort preparation and typed arguments likewise handle profile-loading, model mismatch and undeclared-level rejection as compiled chat errors before mutation. Model activation shares typed failure handling with the literal tuple command; current credential and consent admission happens on the actual Run, not during selection. Successful declared custom effort selection is staging, never a grant of exact compatibility evidence or a Provider call. Fatal State admission/persistence/replay and terminal ownership remain separate. This does not diagnose the user's live OAuth identity failure.

The private defaults writer preserves the Engine's typed busy-operation rejection before display conversion. Idle Provider/model/effort/collaboration controls and model-browser activation recover that one failure into chat without saving their attempted selection. Other failures and strict startup/account publication remain separate; the [CLI contract](../../agents/cli.md) owns classification. This adds no retry or optimistic projection and makes no rollback claim for catalog, account or compatibility work already completed.

`--tools` explicitly enables the private grant/Guard path in attached and `exec` modes. `/permissions` inspects requested or pinned authority; it cannot approve or widen it. There is no approval UI, unsandboxed mode, arbitrary network grant or host-shell fallback.

## 5. Session, Run, and collaboration lifecycle

The lifecycle hierarchy is:

```text
Session
├── durable messages, title, fork lineage, defaults, compaction snapshots
└── Runs[] in committed order
    └── Run
        ├── pinned ProviderProfile/model/policies/budgets
        └── AgentRuns[]
            ├── one primary
            └── ordered 0..N direct children
```

A Session has at most one active Run in beta. The Engine holds a private per-Session state lock across each Run, source fork, and manual compaction; a brief private namespace lock coordinates file creation and removal while other Sessions can proceed independently during work. At most 65,536 Session lock files may exist, including crash-stranded pre-commit files. Completed, failed, cancelled, and interrupted Runs remain immutable history. Resume reconstructs the same Session; fork creates a new Session with a single `SessionForked` first Event recording source Session, boundary Run, boundary sequence, and canonical Event-prefix digest. Store replay validates bounded source ancestry; the context compiler selects successful inherited turns only through that boundary. Historical children remain inspectable but are never recreated as live work.

Before each Run, Arany pins one collaboration policy:

```text
single
  maximum active children = 0

auto(max_active_children = N)
  primary may delegate only when independent work is useful

team(max_active_children = N)
  primary must propose a non-empty team decomposition
```

The product default is `auto` with at most three active children. `N` is a numeric input, not a compiled topology, but spawning is always bounded. Effective capacity is the minimum of the Session policy, a process safety ceiling, remaining call/token/byte/time budgets, and Provider concurrency. Children cannot create nested teams in beta.

The semantic Provider outcome is:

- `Finish { summary, result }`;
- `Delegate { children: bounded ordered objectives }`; or
- explicitly enabled primary-only `Tool { call: closed typed operation }`.

In `single`, the primary finishes without delegating. In `auto`, it may finish directly or delegate. In `team`, its first non-Tool outcome must be a nonempty bounded delegation. Every child is still one-call read-only reasoning; after all required children finish, primary synthesis may use the same remaining Tool budget before finishing. Without Tools, a successful team with `k` children uses `k + 2` Provider calls and a direct answer uses one. Enabled Tools add at most 16 shared continuations to that bounded topology; their context is reserved before effects. Reported input usage above 1,000,000 tokens or output usage above the exact request cap fails at each public production adapter, including standalone library calls; Engine validation remains defense in depth. Native OpenAI can retain unknown usage, while exact custom transport requires positive reported usage. No automatic retries or cross-provider fallbacks exist.

Context is isolated per AgentRun. Children receive explicit assignment capsules and narrower-or-equal authority, never a shared mutable prompt. Only the primary writes the final assistant Message.

## 6. Context and compaction

### Local effect ownership

The [coding-base plan](../../planning/effectful-beta-base/README.md) records the 2026-10-04 scope, alternatives, threat model and verification owners. `tools.json` is private host configuration, not repository configuration. Its pinned scope/resource digests produce immutable primary-only one-use intents. The Engine commits each Provider request fact and intent before dispatch, then a correlated observation before inferring again. Uncertain, cancelled or unacknowledged effects stop without retry; public typed replay validates payloads as well as legal transitions. Old journals remain readable without gaining grants.

The Linux Guard verifies namespace/capability/seccomp and effective cgroup limits before payload release, owns the exact invocation and retained cgroup through cleanup, and reports a digest-bound receipt. Native operations use live no-follow handles and digest/create-preconditioned fresh-inode replacements. Commands/MCP instead receive fresh private selected-file snapshots, no network or accounts and bounded memory/PID/CPU/time/scratch; subprocess changes are discarded. Skills are pinned progressive guidance/resources, never permissions. YAML/schema work occurs only inside this killable boundary. MCP supports exactly the 2025-11-25 local stdio tools profile, with bounded negotiation/catalog/schema/framing/calls and no automatic retries. [The tool guide](../tools.md) owns the supported subset and setup, not a claim of arbitrary build/runtime compatibility.

Past effect records enter context/compaction only as bounded untrusted facts, including interrupted attempts and observed failures, never renewed permission or execution. The tool-enabled compiler reserves current observations from the existing context budget. Its source-pressure guard additionally accounts for maximal Tool history so maintained conversation can compact before the unchanged source cap. Read-only children receive none of the Tool catalog or observations.

### Conversation context

Session history is local canonical state. Provider conversation IDs, cache keys, and opaque compaction items are optional accelerators.

The deterministic context compiler selects trusted instructions, the current user Message, bounded recent Session history, valid compaction material, explicit include snapshots, and required child results under the chosen Provider's exact budget. It records what was included and excluded. Provider switching between Runs recompiles from Arany state.

Provider-neutral textual compaction uses the Provider's separate semantic `compact` operation rather than inventing an AgentRun. Accepted conversation outcomes, including unanswered objectives, and an optional prior summary enter that call; the model-authored result remains untrusted derived data. A later `RunStarted` pins the selected snapshot Event and content digest.

Compaction never replaces or deletes canonical history. `/compact` creates a derived snapshot for an exact committed Session prefix and records:

- covered Run/sequence and source digest;
- Provider, model, prompt/schema/compiler versions;
- bounded summary or compatible opaque Provider item;
- byte/token estimates and content digest; and
- completion/failure provenance.

Manual compaction runs only while idle and makes its Provider usage visible. Each new `RunStarted` records actual selected context-content bytes, its compactable Session-history share, and the policy-adjusted byte budget. After a successful attached Run reaches 80% of that budget with at least 10% compactable history, 24 of the 32 selectable history Runs, or the separate source-pressure threshold, Arany prints a durable warning after the confirmed receipt, then attempts automatic compaction for that exact committed boundary using the Run's pinned Provider/model/effort and account source. The source guard reserves two maximal canonical turns below the independent 256-KiB compaction-input cap, accounting for the pre-reply footprint and the previous unchecked turn; canonical Message and image-metadata limits have shared owners. Continuously maintained successful conversations can therefore compact before their source grows beyond the call's bound. This does not recover every unmaintained or failure-heavy prefix; oversized source still rejects before egress. The private CLI passes the committed config rather than mixing its tuple with newer Session defaults; an absent saved account remains the original environment-backed/custom source, while a recorded API or ChatGPT account is re-admitted by identity and current evidence. Replay grants no credential authority. A large immutable Workspace input alone cannot trigger a call that would not relieve pressure. The Engine checks the expected Run ID and prior attempts under the Session lock, so a stale warning or concurrent compaction does not spend another call for that boundary. Compaction composition separates safe operation failures and explicit interruption from terminal-owner errors: committed/admission failures receive labeled chat feedback, interruption warns about uncertain commitment, and terminal failure propagates to restoration and exit. None changes the already confirmed Run outcome or automatically retries. `exec` makes no hidden post-Run Provider call. Failure leaves the Session intact; an incompatible or digest-mismatched snapshot is ignored with a typed failure. Cross-Session Memory remains deferred and distinct.

## 7. Canonical persistence

One strict SQLite Event journal remains product truth. Session, Run, AgentRun, SessionView, and RunView are reduced state rather than duplicate authoritative tables.

```sql
CREATE TABLE events (
    sequence       INTEGER PRIMARY KEY,
    session_id     TEXT NOT NULL,
    run_id         TEXT,
    agent_run_id   TEXT,
    kind           TEXT NOT NULL,
    event_version  INTEGER NOT NULL CHECK (event_version >= 1),
    payload        TEXT NOT NULL CHECK (json_valid(payload)),
    created_at_ms  INTEGER NOT NULL
) STRICT;

CREATE INDEX events_by_session ON events (session_id, sequence);
CREATE INDEX events_by_run ON events (run_id, sequence) WHERE run_id IS NOT NULL;
```

The initial vocabulary is intentionally small:

- `SessionStarted`, `SessionRenamed`, `SessionDefaultChanged`, `SessionForked`, `ContextCompacted`;
- `MessageAccepted`, `MessageCommitted`;
- `RunStarted`, `RunFinished`;
- `AgentSpawned`, `ProviderCallRecorded`, `AgentFinished`; and
- `ToolStarted`, `ToolFinished`.

Tool Events use payload version 1 and correlate the exact Run, primary actor, one-use intent, pinned policy/enforcement digest, Workspace identity and finite effective limits. A successful observation requires a matching Guard receipt. Unknown, duplicated, unmatched, over-budget or mismatched transitions reject; uncertainty/cancellation cannot be followed by successful Agent completion. Public typed replay validates payloads as well as SQL loading. A started attempt without a terminal observation remains a past interrupted/uncertain fact, never an approval or retry instruction.

New `SessionStarted` Events use payload version 2 to pin the admitted Workspace device and inode atomically with the title, including for an empty Session. Version 1 title-only Events remain readable; their Workspace identity is established by the first `RunStarted` if one exists. Replay rejects a later Run or fork whose identity disagrees with the pinned Session. Legacy empty Sessions cannot be selected by Workspace until a Run binds them.

Provider/profile, collaboration policy, limits, instruction digest, and Workspace snapshot facts are fixed in `RunStarted`. Each observed Run call then records its phase, local disposition, bounded response ID, provider-reported token usage, and accepted wire provenance when available before the relevant AgentRun finishes. Compaction records carry the same optional wire provenance. The closed values distinguish Responses `completed` plus a local `store: false` request from Messages `end_turn` with no equivalent request switch; they do not assert remote retention, account-level Zero Data Retention, or a custom endpoint's compliance. A cancelled or timed-out call has unknown usage and wire provenance, not zero; cancelling the Run after a call has returned preserves its observed record and any known usage. Sticky cancellation wins before either successful or failed primary terminal commitment starts, without rewriting earlier child outcomes. Once that commitment starts, it completes or leaves an interrupted prefix on Store failure. Replay checks call order and accepts older journals with no call records or provenance. A logical transition commits before reduction or feedback. A crash leaves a valid prefix; replay labels incomplete work `Interrupted` rather than inventing cancellation or success.

Recognized typed Provider failures retain an optional closed reason for account access, usage limits, temporary usage or service unavailability, stream protocol, response contract, structured outcome contract or local output acceptance. Only records with a reason use `ProviderCallRecorded` payload version 2; generic calls retain version 1, and both replay without a database-schema change. Strict admission rejects a reason inconsistent with its disposition or version. Raw HTTP status, stream code, upstream text/body and request IDs are excluded. Chat feedback and agent details translate the last call of a failed AgentRun into compiled recovery guidance; cancelled siblings and successful calls cannot supply its cause. Linear modes append the same feedback after their confirmed receipt. This metadata does not enter Provider context or traces, prove a billing diagnosis, trigger retry or switch account routes. Compaction keeps its separate existing failure categories.

Idle rename/default changes may follow an interrupted prefix without submitting another objective. Their committed Session-only facts seal the unfinished predecessor, preserve its observations and prevent later transitions under that abandoned Run ID. The [Session reducer contract](../../agents/session.md#persistence-and-proof) owns this recovery rule; lifecycle operations hold the existing per-Session lock to exclude a live Run. No interruption Event or synthetic terminal outcome is added, and fork/compaction still require their existing committed boundary.

Before Run effects or Provider usage, the Store strictly checks the resolved lineage count plus the complete worst-case lifecycle/children/Tool Event envelope in an immediate transaction. Its database byte preflight covers each slot's maximum ordinary payload and bounded SQLite page/index overhead, one larger image Message, and the unchanged per-append floor; the floor alone cannot fund the later receipt/closure writes. Engine retains operation locks for the Session and all ancestors because replay counts their full journals, including suffixes outside a fork boundary. Compaction additionally checks its direct Session's lifetime attempt quota; inherited summaries do not charge that quota. Finish admission measures the exact serialized terminal payload, not only raw text. Event-count admission is stabilized by those locks; the byte allowance is a conservative preflight, not a lease against concurrent independent writers or a physical disk reservation. Disk faults and process death still require interrupted/uncertain recovery. Independent lineages remain concurrent.

The sole connection opens no-follow in a private owner-verified local state directory, uses defensive mode, rollback `DELETE + EXTRA`, `trusted_schema=OFF`, bounded pages, short transactions, and fixed payload limits. Rebuildable indexes may accelerate Session listing; they never become canonical.

## 8. Terminal presentation

Arany owns the full primary-screen viewport without entering alternate screen. It derives conversation from committed Session state and interleaves bounded terminal-local feedback for important notices and errors. The private History owner indexes row counts without another full-transcript copy and materializes only visible rows. Local feedback has distinct labels and does not enter Events, replay, Provider context, or `show`; it survives redraw/reacquisition within the attached Session and clears on Session change. `PageUp`/`PageDown` navigate older content; `Ctrl+F` searches it literally; `Ctrl+L` returns to the live tail. New content preserves an older reading anchor. Ordinary `show` remains the complete deterministic canonical history and export path; emulator scrollback/search of physical output is no longer a complete Session transcript.

```text
committed Session transcript · older rows navigable

primary · working · Comparing provider contracts
─ Ask Arany ─────────────────────────────────────────
> Compare the APIs
  and their limits
────────── Enter keeps draft · Ctrl+O newline ───────
working · openai/model · input 1200/65536 B · session-name
```

An empty idle transcript shows a compact width-aware welcome near the composer until history, feedback, or agent activity takes its place. With no selected Provider, the welcome, empty-input hint, and status show `/setup`; an objective rejected before account/model admission keeps its inline draft and caret. Linear/screen-reader mode gives the same guidance but clears the submitted physical line. Thin neutral rules identify the borderless composer and its `> ` input lead; its empty hint is not editable text. Long input wraps visually into at most four rows around the caret without inserting message bytes; a clipped window adds its visual-row position to the heading. Ctrl+O inserts an explicit logical newline; Up/Down retain logical-line navigation, and Enter keeps its submission behavior. The composer grows upward while its lower rule distinguishes idle submission from active task retention using committed Run state and shows Ctrl+O where it fits; linear mode uses Ctrl+D after text to continue a draft. Idle Ctrl+K selects Models, Agents, Sessions or Setup without consuming the composer. Setup initially has focus with no selected Provider and explicitly enters the existing `/setup` wizard; cancellation preserves the caller's draft. Session switching remains unavailable while a draft is nonempty. Inline `/help` opens a scrollable registry-backed panel and restores the composer and reading position; linear help stays append-only. Configured status prioritizes state/model, adds Provider from 50 columns, and shows the active Run's recorded context-input used/budget bytes from 80 columns. Active `/status` discloses those bytes ahead of optional IDs at narrower widths; the Session title appears only if space remains. These bytes are not a Provider context-window percentage, and a finished Run's footprint is not current draft usage. Important feedback stays in chat; panel closure and immediate submission/cancellation cues use status. Permissions, collaboration, costs, paths, request IDs, telemetry, event sequence, and completed-agent history remain in existing detail surfaces or `show`.

The Composer preserves grapheme boundaries across edits. Ctrl+Left/Right navigate whitespace-delimited chunks and Ctrl+W deletes backward without splitting paths or model IDs at punctuation; explicit newlines and Unicode whitespace remain separators. Removing a separator can merge Unicode clusters or regroup regional indicators, so its shared deletion operation resegments the bounded draft and snaps forward to a surviving boundary. Wrapping only projects that caret; it does not repair draft offsets or alter message bytes. A decoded Shift+Enter shares Ctrl+O's bounded newline action without enabling a keyboard-enhancement mode; Ctrl+O remains necessary when a terminal sends ordinary Enter for both. Wide frames expose both newline shortcuts. Idle empty-draft Ctrl+C gives bounded chat guidance for terminal-emulator copying before the existing two-second second-press exit. None of these actions reads a clipboard or changes active cancellation. The detailed editing invariant belongs to the [terminal rules](../../agents/terminal.md#terminal-behavior).

The inline renderer uses restrained terminal-native cyan for semantic leads and focused rows, yellow for attention, and red for textual errors, without colored backgrounds. `--no-color` or `NO_COLOR` removes those accents, including after reacquisition; wording, `>` selection markers, bold focus, and bracketed slash hints retain meaning. The linear and deterministic renderers remain unstyled. Terminal-theme contrast and native assistive-technology usability still require release review.

Unix inline ownership accepts standard bracketed text paste through a private bounded reader, not Crossterm's unbounded paste buffer. A complete valid block edits the current draft atomically, including while work or catalog loading is in flight; newlines do not submit, and modal selectors ignore it. CR/CRLF becomes LF and canonical tabs remain tabs with a safe visual expansion. The terminal owner finishes initial cursor-query setup before this sole stdin reader starts and disables paste mode before restoring the terminal. The transport and one-shot payload limits belong to [terminal rules](../../agents/terminal.md#terminal-behavior). This framing itself reads no OS clipboard and makes no safe unframed canonical-paste claim.

Explicit Ctrl+V, decoded Ctrl+Shift+V or `/paste` starts one cancellable terminal-owned clipboard worker outside the input reader and Engine. Linux uses checked standard Wayland/local X11 clients, not a GNOME/KDE integration; unavailable transport gives local error feedback without changing the draft. Its bounded pipes/process deadline, cleared environment and kill/reap ownership belong to [clipboard rules](../../agents/clipboard.md). Pickers, find entry, release and suspend discard late results. Canonical delivery waits for a whole pending segment, resynchronizes draft bytes and appends a complete inert `Draft (not sent)` preview. Another Enter is required to submit. Local failure/keyboard/linear owners pass; successful native service and hostile-client lifecycle evidence remain separate.

Explicit Linux clipboard paste admits at most four PNG attachments with 192 KiB aggregate raw bytes. The immutable image value checks complete chunk/CRC structure, dimensions and pixel count without raster decoding, shares canonical Base64 and excludes pixels from Debug and human history. Composer retains attachments across local controls and active work; explicit submission transfers them to Engine, including image-only objectives. `MessageAccepted` v2 uses a bounded 320-KiB payload; text-only v1 and all other 64-KiB Event bounds are unchanged. Closed replay and forks retain exact bytes. Context selects whole turns within image/count/context bounds and carries explicit objective/history-turn origins; children receive only current-objective images. Native Responses, ChatGPT-plan Responses and Messages build actual inline image blocks under the unchanged wire cap, while custom profiles remain text-only. Compaction uses bounded labels/digests and prior assistant interpretations, not pixels; covered images stay canonical but are not silently restored into later requests. Offline owners prove synthetic admission, semantic routing, encoding, replay and presentation, not native clipboard service success or live model vision. The [image rules](../../agents/image.md) and [current plan](../../planning/arany-beta/README.md#requested-safe-text-and-image-paste) own the detailed contract and evidence.

Account setup uses centered, borderless keyboard-selectable rows for non-secret choices, returning to chat after catalog-only automatic model/effort selection; only the API-key field shows a bounded byte count, never key content. At 16–19 columns it keeps focused choices and actions visible, and the linear/screen-reader path accepts visible choice names with echo until hidden key entry. Native setup returns from selected-account catalog discovery to ordinary chat with low effort and no model/effort/cost questions or synthetic inference. ChatGPT setup retains its complete backend-specific account risk consent before sign-in, then also returns from catalog discovery without inference. These presentation facts never enter Engine interfaces or Events.

The activity shelf is conditional:

- idle Session: no agent row;
- one active primary: one row;
- multiple or attention-requiring agents: at most three stable rows;
- overflow: one `+N more` row;
- failed/blocked agents remain until acknowledged or the Run ends.

`/agents` opens primary-first AgentRuns from the active and 16 most recent Runs, plus the next-Run collaboration controls. It shows bounded objective, summary, and result previews, with compiled failure explanations from committed call outcomes; the same pure presentation owner supplies confirmed inline Run feedback. `arany show SESSION_ID` exposes older and complete committed history. The default screen never reserves an empty team dashboard.

Keyboard behavior is complete. Normal transcript/composer mode does not enable terminal mouse reporting. The Session picker and `/agents` inspector enable mouse reporting transiently while their selectable rows are open; hover, click, and wheel operate only visible rows and have exact keyboard equivalents. Close, suspension, error, panic, cancellation, or loss of terminal ownership disables mouse reporting before returning control. Internal history navigation never enables mouse capture.

The terminal never enters alternate screen and never enables focus reporting, clipboard/title OSC, or globally captured mouse. `presentation.rs` is pure over SessionView/RunView; the `terminal` module alone owns keys, mouse, width, focus, layout, redraw, and RAII restoration. Screen-reader, `exec`, and `show` emit no cursor rewriting or control sequences.

Before current submission progress, the terminal shows a local preparing row and the selected next-Run model. It preserves that projection across input and history actions instead of displaying a prior Run as current. This is request-admission feedback, not committed agent activity or a claim that inference started; it never enters Events, Provider context or `show`. Linear mode announces preparation once and keeps the input prompt suppressed. Current committed progress replaces that local state.

During an attached Run, the Engine incrementally reduces each acknowledged Event and publishes the latest `RunView` with its committed sequence through a one-slot coalescing observation handle. The terminal redraws semantic activity rows from that snapshot, never from raw Provider streams; skipped display intermediates do not skip canonical Events. Closed-history replay still marks an unfinished Run interrupted, while the in-process projection retains its active state until a terminal Event commits.

The attached composer remains terminal-local during a Run. Editing and Enter can retain a bounded objective draft, but only a later idle Enter submits it as a new Run. Active slash inspection reads the current committed projection; Session and next-Run default changes stay locked. Manual and automatic compaction borrow the same editable Composer: every Enter retains text, including slash commands, without inspection or queued work. Their busy frame state is explicit rather than inferred from a notice or a fabricated active Run. Optional progress feedback leaves history find/navigation cues visible; resize and suspend preserve the draft/caret/reading anchor. Completion, failure and interruption return the same draft to explicit idle submission. Linear input carries its actual draft byte count through Enter, suspension, and terminal reacquisition so the 8 KiB limit cannot be bypassed. No local draft enters summary context or canonical Events without submission.

The linear screen-reader presentation retains canonical terminal editing and append-only labels. Enter submits a logical draft; Ctrl+D after nonempty input continues it across bounded physical lines, up to the same 8 KiB objective limit as the inline composer. A rejected physical or aggregate overflow is visible and never automatically submits a prefix; earlier accepted draft remains for an explicit retry or clearing. Ctrl+D on an empty draft exits, while Ctrl+C clears a retained draft. Native macOS behavior and screen-reader review remain release gates.

Unix suspend is owned by the attached terminal: SIGTSTP and inline Ctrl+Z release input and terminal modes before the process stops, then reacquire presentation after SIGCONT. The local composer and any pinned Run future remain in memory; the linear reader restores the composer's byte count so its aggregate limit cannot widen after resume.

Unix SIGTERM and SIGHUP pass through the same terminal-owned input path in idle, setup, picker, compaction, and Run states. Private setup composition preserves typed shutdown and presentation failure through the shared attached caller; only recoverable account/Provider errors return chat feedback. An active Run first requests Engine cancellation, releases terminal ownership, and waits for a bounded durable outcome before a committed receipt can be emitted. The released owner keeps its signal listeners alive during committed output and transfers them to the next terminal acquisition, preserving notifications queued during that interval. A second Ctrl+C during cancellation forces the same bounded shutdown; after an expired grace period, replay determines whether cancellation committed or only an interrupted prefix remains.

## 9. Provider profiles and custom endpoints

One ProviderProfile binds protocol family, normalized endpoint/base path, credential reference, model, outcome encoding, privacy claim, and capability evidence. Native profiles are compiled; each admitted model still requires paid model-scoped live proof before a release claim. Custom profiles live only in trusted user configuration outside the Workspace.

Native API selection carries one closed credential value: Provider, key and optional Anthropic API workspace. The API workspace is a tenant/billing scope, not Arany's filesystem Workspace. Environment-backed Anthropic selections read it explicitly; other Providers and saved accounts never inherit it. Setup offers key-scoped access or a visible explicit workspace ID. Key-only accounts keep schema 1; a scoped Anthropic account requires schema 2, while the backend marker and aggregate storage cap stay unchanged. The selected-account loader returns the complete validated credential. Its scope is immutable across catalog pages and all native calls and enters the current exact-evidence fingerprint. It authorizes no new endpoint or arbitrary header. The [beta plan](../../planning/arany-beta/README.md#completed-implementation-explicit-anthropic-api-workspace-scope) records the implementation and evidence scope; synthetic success is not live API compatibility.

Native model effort is selected from a reviewed per-model set or resolved to an explicit reviewed default before Workspace input. The native adapter sends the resolved level explicitly on every Run and compaction request, and `RunStarted` pins it for replay. The finite reviewed table contains exact model/effort metadata; account catalogs may contain many more IDs. Custom capability-evidence v1 binds only its exact model and provider-default reasoning behavior. Version 2 adds a bounded ordered effort list to the exact profile and fingerprint; each declared effort receives a data-free strict finish probe. An admitted explicit choice is sent on every Run and compaction call and pinned in `RunStarted`; omission retains the conformed provider-default behavior. Catalog availability alone does not prove compatibility. Unknown native models may be selected with explicit closed effort without a synthetic check; `resolve_native_effort_for_run` validates their local selection and limits concurrency to one, while the reviewed metadata resolver stays closed. All actual requests still require provider-enforced schema and strict outcomes.

`provider models` performs account-scoped native discovery without Workspace data, lists the configured model of an exact custom profile without egress, or lists all visible models of the selected OS-user ChatGPT account. The native command can select either its named environment key or the current OS-stored API account. ChatGPT listing uses only its consent-matched OAuth token and may rotate a near-expiry token under the account lock; it does not use the native API key or a Session StateRoot. Both catalog transports require one JSON media type before bounded body reading. Idle attached `/models` uses the selected native, custom, or account-pinned ChatGPT catalog and a bounded keyboard-accessible page view. Native rows outside the reviewed table disclose availability without compatibility evidence and require explicit effort, not a check; ChatGPT Run admission instead revalidates selected-account identity and current consent without a mandatory probe, while custom v2 entries show their declared effort choices alongside provider default. `provider check chatgpt` explicitly verifies the selected account's catalog row and runs the data-free three-call probe before saving account-bound model/effort evidence; a later Run independently revalidates the selected account and its consent, not that optional diagnostic. The catalog is never consulted implicitly during a Run. An unreviewed native row can be staged as an idle Session model without a Provider inference call; selecting its effort and admitting its Run require current credential/account authority and strict real-response validation, not model-check evidence.

The admission layer reads a versioned `provider-profiles.json` from the existing private state root through a bounded no-follow file handle. It validates the entire closed profile set, including unique names and a one-origin binding per credential reference, before returning the named profile. `arany provider check PROFILE` runs data-free synthetic probes and stores 24-hour evidence in a bounded auxiliary SQLite table. `custom:NAME` selects the exact checked profile and requires an explicit matching model; the custom adapter re-resolves and compares fresh fingerprint/address evidence before reading its key or any Workspace input. Schema version 3 has a separate bounded table for optional native model diagnostics. The explicit, potentially billable native check confirms account-catalog visibility, then reuses the fixed native adapters for strict synthetic direct, delegation, and compaction probes under remote output caps. Its HMAC fingerprint binds protocol/endpoint, model, effort, version, key, API workspace and source/account identity without storing the key. Only opt-in checked library constructors require this exact evidence. Ordinary environment-backed `exec`, attached saved-account Runs and account setup do not require or manufacture probe evidence; strict real-response validation and current credential/source admission remain intact.

Closed custom protocol families begin with `openai-responses`; additional `openai-chat-completions` or parameterized `anthropic-messages` support must earn the same adapter and security review. Arany never accepts arbitrary headers, shell credential commands, repository profiles, raw key arguments, automatic model discovery, or a generic compatibility plugin.

Before a custom profile receives Workspace data, `arany provider check PROFILE` performs a synthetic bounded conformance sequence with no repository content. The checker probes a direct finish, one-child delegation, and compaction through the strict Responses wire format; v2 additionally probes each declared effort with a strict finish. It validates response model, IDs, usage bounds, and bodies. Evidence binds the Arany/check version, exact endpoint and profile fields including v2 efforts, allowed address set, timestamp, and expiry. After a profile is admitted, a failed recheck leaves no prior evidence for that profile. Each Run admission re-resolves and matches all bound fields and addresses before Workspace disclosure; `RunStarted` records the exact endpoint and evidence fingerprint. The checker and adapter share the same bounded transport and reject raw or decoded exact key reflection. Paid exact-endpoint live proof remains separate.

Receipts use exact support language:

- `native supported`;
- `broker constrained` after a future OpenRouter gate;
- `custom verified`; or
- `custom unverified`, eligible only for `provider check`.

Non-loopback endpoints require HTTPS. Numeric loopback may use explicit HTTP. Redirects, cookies, ambient proxies, DNS/route drift, metadata/link-local/multicast destinations, and credential reuse across origins are rejected. IPv6 domain answers are limited to reviewed allocated ranges; unknown global-unicast space is not assumed routable. “OpenAI-shaped” is never treated as proof of compatibility.

The CLI has two explicit API-key sources: environment references for `exec` and flag-selected attached use, and one protected default native API account for bare new attached use. A versioned record under the user account root pins either the OS keyring or a user-confirmed private-file account. The keyring record contains no key; the file record is plaintext with private-mode, no-follow, owner and link checks. On Linux, the account root resolves from the effective UID's passwd home rather than Session `--state-dir`, `HOME`, or `XDG_STATE_HOME`; its private lock serializes new-version account replacement against the fixed per-user keyring slot. A validated old record from an environment-selected root is copied under both locks, then replaced there with a version-zero tombstone; a conflicting destination fails closed. The keyring helper is killed and reaped after a five-second deadline, but a timed-out OS-store write may still complete after its request was accepted. A synthetic two-process, two-StateRoot replacement passed against this host's Secret Service; other same-UID or privileged processes, old Arany binaries, full product setup, and native macOS behavior remain outside the coordination and secrecy proof. A debug-only test-root override keeps product-process tests off the real user account root and is rejected in optimized builds. Linux uses the cross-desktop [Secret Service API](https://specifications.freedesktop.org/secret-service/latest/ch01.html), not a KDE- or GNOME-specific contract. The [kernel `persistent-keyring`](https://man7.org/linux/man-pages/man7/persistent-keyring.7.html) survives login sessions but expires, and ordinary [user-key payloads live only in kernel memory](https://man7.org/linux/man-pages/man7/keyrings.7.html); it is therefore not a durable post-reboot replacement for saved Provider accounts. A user-confirmed private-file fallback covers Linux hosts without a usable Secret Service, with no encryption-at-rest claim. macOS uses Keychain Services, with native checks after the current Linux goal. Cancelling pre-Session `--setup` creates no Session or Run; cancelling in-Session `/setup` leaves that Session unchanged. A failed save is not a rollback guarantee.

A saved unreviewed model requires explicit effort and current selected-account authority before Workspace input, without mandatory synthetic inference. Session defaults pin only the account UUID; Run and catalog admission re-read the record and reject a changed ID or Provider before using its key. The selected native Provider carries the validated UUID into `RunStarted`; environment-backed, custom, and legacy Runs have no saved-account UUID. Replay validates a present native UUIDv7 as provenance, never credential authority. Keys never enter arguments, Events, output, or telemetry.

A private CLI module constructs and validates new-registration or returning-account ChatGPT OAuth attempts and has an offline-tested, compiled-endpoint code/JWKS redemption path that returns credentials only after signed identity and granted-plan checks. Returning redemption must preserve the selected signed subject. Attached setup invokes new registration or same-account reauthorization only after paging the complete backend-specific plan-usage warning and collecting explicit Back/Accept consent with Back focused by default; it starts a one-shot loopback listener, opens only the compiled HTTPS authorization URL in the system browser, verifies the callback and signed identity, and saves the token under the OS-user account lock. The returning URL omits the optional ID-token hint, so the browser process receives no credential. A selected account can be reused without OAuth, switched to another saved consented account under the account lock, or reauthorized against its pinned client and host. The private refresh exchange checks the issued client and renewed scopes, and the account owner serializes near-expiry exchange and whole-token replacement under the OS-user account lock with a durable pending marker; catalog, explicit synthetic check, and Run admission call that owner. A synthetic ChatGPT record passed this host's Linux Secret Service and shipped helper path; complete native lifecycle and live account behavior remain unproven. `provider logout chatgpt` marks the selected account unavailable before using the private same-origin revocation operation, then removes local tokens and model evidence while retaining registration metadata for reconnect. An empty `200` confirms revocation; failure is reported without claiming remote or keyring cleanup. The official guide directs renewal near access-token expiry, so the account owner uses bounded stored expiry while treating the undocumented `earliest_refresh_at` scheduling semantics as opaque. A private OpenAI module forms streaming-only subscription Run, compaction, and synthetic-check requests and withholds each result until `response.completed` passes strict decoding and local usage bounds. Setup selects a visible subscription model with low effort without inference or additional questions; it prefers the cost-sensitive `gpt-5.6-luna` when visible, otherwise the first catalog row with an unknown-prices notice. Catalog cancellation or failure after a new, switched, or replacement account leaves account-only defaults with a visible retry notice on failure. Failed current-account reuse preserves Session defaults. Existing API-key and custom routes retain their strict remote cap. Anthropic consumer-subscription authentication remains unadmitted. Arany imports no other CLI's token and calls no private ChatGPT inference route. The [decision register](../research/next-step-decision-register.md) records the trade-off and gates.

The ChatGPT authorization path has a user account-root registration record, distinct from saved API accounts and subscription tokens. It atomically stores one UUIDv4 host ID before the first authorization request, and temporarily stores an issued client ID under the account lock before code redemption. On Linux, a validated old registration migrates under both locks to the stable root; a different destination registration fails closed. After risk consent, an incomplete first registration offers Back / Resume before listener/browser acquisition; Resume reuses its issued client for a fresh PKCE attempt, while cancellation preserves the record. Saving a verified account clears that pending client; reopening reconciles a crash after account publication, so another new account can register with the same stable host ID. Returning authorization bypasses the unfinished-registration choice and does not claim the pending slot. This record contains no code, token, or consent receipt and does not itself authorize subscription catalog use or a Run.

Code redemption has a closed private result: plan-enabled credentials or verified identity metadata without tokens. Missing plan permission does not bypass signature, issuer, exact client audience, nonce, lifetime or returning-subject validation. Metadata-only sign-in persists an explicit permission-disabled account with no tokens or model evidence, retaining its registration for explicit `Enable plan` recovery and account-only Session defaults. The account lock covers publication of this blocked state before bounded cleanup of an old keyring token; uncertain deletion never re-enables use. Existing enabled records omit the negative-capability marker and remain compatible; older closed readers reject a disabled record rather than discarding its restriction. Permission repair alone adds OAuth `prompt=consent` with the same selected registration, not a new client or API-billing fallback. Catalog/check/Run admission rejects disabled accounts before token access or Workspace input. Focused corpora and the optimized networkless product journey prove metadata-only persistence/reopen, blocked catalog/check/Run use, safe chat recovery and explicit same-account enablement. Synthetic success does not diagnose a live identity failure or prove native keyring use.

A private ChatGPT account owner can store up to eight distinct signed-subject registrations under the OS-user account root. Each account has a UUIDv7, stable host ID, issued client ID, backend; enabled credentials additionally require exact storage-bound consent. All accounts share the host ID. The preferred keyring backend keeps one bounded token item per client-ID-derived slot and only metadata in a private index; an explicitly accepted private-file backend keeps its token in that checked atomic index. The account lock spans keyring write and metadata publication; an uncertain new-account write never publishes a selection, though an orphan keyring item may remain. Replacing an existing keyring item first persists a pending marker and blocks loads until a verified replacement completes. Reopen rejects duplicate identities, backend or consent drift, missing selected keyring items, and mismatched token subject or host. The same-binary keyring helper concurrently drains its bounded pipes for larger OAuth records, and its availability probe returns no secret. A private renewal owner holds the account lock across selected-account read, near-expiry OAuth refresh, and whole-token replacement. Its durable pending marker blocks later loads after a crash or uncertain save until verified reauthorization; an `Unavailable` exchange preserves the old token and clears that marker, though a lost response after server-side rotation may make the next refresh unusable. The checked atomic index also holds at most 64 expiring account-bound model/effort fingerprints; a new verified sign-in clears that account's records, while token rotation does not. Setup can save, reuse, select, or reauthorize a verified account and pin its UUID/model/low effort in Session defaults after a matching catalog read without inference. Switching validates the target's current consented credential under the OS-user account lock and keeps its UUID with account-only defaults if catalog loading is cancelled. Reauthorization requires the original selected UUID, client, subject, and backend under the lock, preserves its UUID, and clears its model evidence. A consented-account Provider Run exists but lacks live account/keyring proof. Current Run and compaction records explicitly identify `account_consent`, using a domain-separated opaque fingerprint bound to model/effort, signed identity, account, storage consent and compiled admission contract; the owner rechecks its selected snapshot under the account lock after normal renewal without loading diagnostic model evidence. Missing category retains historical `conformance` encoding, while unknown categories/fields fail strict decoding. Neither journal provenance nor optional diagnostics grant current account authority.

A sign-out-pending or disconnected account cannot supply a token to a catalog, check, or Run. The account index retains its identity and backend to permit same-account reconnect, but clears its model checks before revocation egress. Private-file logout atomically clears the token; keyring logout follows the blocked-index write with supervised item deletion, whose failure leaves a visible cleanup warning. An already in-flight Provider request is not retroactively cancelled.

A terminal refresh-token error also disconnects that registration under the account lock, clears model checks and the private-file token, and attempts supervised deletion of the keyring item after publishing the blocked state. Temporary refresh failures leave the token and registration usable for retry; ambiguous storage or deletion failures never re-enable account use.

The private OpenAI subscription transport has a data-free direct/delegate/compaction probe for an exact model/effort. `provider check chatgpt` first clears prior exact evidence, checks visibility under the selected account token, invokes the three bounded streamed calls after `--accept-cost`, and publishes a 24-hour fingerprint only if that same account remains selected with matching consent and registration. Attached setup and effort activation do not invoke this optional checker; the expected account UUID is checked under the account lock before token retrieval or renewal; attached catalog and Run admission use that same early identity guard. Fingerprints use the retained verified ID token and exact consent receipt rather than rotating access/refresh tokens, and live under the OS-user account root. A failed or incomplete probe does not create evidence for its tuple; cancellation after strict completion may leave an already-started atomic publication. The separate `provider models chatgpt` path lists visible slugs and sanitized display names without invoking inference. A Run revalidates selected-account identity and consent before Workspace input; its `RunStarted` and compaction Events preserve the account UUID, opaque fingerprint, and local-only output-bound category, not tokens. A local output cap is not a remote usage cap.

Subscription inference always uses one bounded SSE decoder after its HTTP status, destination, encoding and length guards, independently of the response media label. Requests negotiate `text/event-stream`; missing or misleading labels cannot discard an otherwise valid stream or select a different decoder. Non-SSE JSON/HTML has no fallback. Success still requires typed `response.completed`, the expected model, complete assistant message, strict phase-specific outcome, positive bounded final usage and no selected-token reflection. Native and catalog media admission is unchanged. Both ChatGPT admission and optional checker fingerprints version this acceptance contract; changing it does not require a new model check or alter storage consent.

The shared Responses decoder validates all assistant text parts, distinguishes explicit `commentary` from exactly one final structured outcome, and joins ordered final text parts without inserting or repairing bytes. Missing/null phase preserves legacy single-final-message support. Unknown phases, malformed/refusal blocks, extra finals, late commentary and non-null error/incomplete markers reject. Native, exact-custom and ChatGPT check versions reflect the shared acceptance contract. Subscription development diagnostics preserve received stream counts and distinguish closed final-decoding causes without retaining response content or changing canonical failure reasons.

## 10. OTLP ships last in beta

OTLP is part of the beta milestone but added after the Session/team/provider proof. It is runtime opt-in and cannot change canonical truth or a Run outcome.

Development-only local diagnostics are a separate private owner, not an OTLP extension or an Engine behavior seam. The CLI enables it after ordinary attached Session/`exec` admission; the subscription adapter records compiled failure stages and numeric counters with bounded local stack frames. StateRoot admits the private, fixed-size log with nonblocking locking. No content, credential, upstream text or source path is recorded or exported. Optimized builds leave it disabled, and canonical replay retains its existing coarse failure categories. [The diagnostic guide](../development-diagnostics.md) specifies retention and non-claims.

Each admitted Run is one trace correlated by safe Session, Run, and AgentRun IDs. Its root is created only after `RunStarted` commits; the preceding context compilation is represented by a backdated child span only for that admitted Run. Dynamic bounded spans cover the Run, each AgentRun, each Provider call, and committed transition timing. Manual or automatic compaction is a separate Session operation trace with its Provider call and a marker only after `ContextCompacted` commits. The successful topology is derived from admitted agents and calls; it is not a fixed nine-span shape.

Arany exports trace-only OTLP/HTTP protobuf to an explicit numeric-loopback Collector selected by `--otlp-endpoint`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, or `OTEL_EXPORTER_OTLP_ENDPOINT` in that order. A trace-specific endpoint is exact; the other two append `/v1/traces`. Invalid explicit configuration is rejected before Workspace input. Objectives, Messages, prompts, instructions, summaries, results, paths, file contents, Event payloads, provider bodies, headers, and credentials are excluded by type. Resource attributes are limited to service and SDK identity; Provider identity is a closed category and custom destinations are not exported. Provider-call failures carry only a closed low-cardinality `error.type` (`timeout`, `provider_unavailable`, `provider_rejected`, `invalid_response`, `output_limit`, or `task_panic`); cancellation uses `cancelled`. The Collector owns remote TLS, authentication, vendor routing, buffering, and backend retry. Arany uses a fixed 256-span queue, 64-span batches, a 256 KiB body ceiling, a 500 ms complete-request deadline, a 750 ms shutdown timeout, and no exporter retry. Its private async Reqwest adapter runs on an owned current-thread Tokio runtime inside the bounded SDK batch worker; automatic decompression is disabled. The Linux [trickling-Collector gate](../security/findings/otlp-http-response-deadline.md) verifies that a slow response cannot indefinitely occupy that worker; native macOS behavior remains unverified. Export remains lossy and failure-isolated; SQLite replay is canonical.

## 11. Security boundary

Authority is fixed before project input. Repository text, Session history, provider output, child Messages, custom-profile responses, and replayed Events are untrusted data. They cannot widen Workspace roots, endpoints, credentials, protocol, models, budgets, topology, policy, instruction role, telemetry destination, or capabilities.

On Unix, the Workspace input loader rejects an opened instruction or explicit-include file whose hard-link count is not exactly one. This conservatively excludes static aliases to files outside the Workspace, at the cost of rejecting legitimate multi-linked files inside it. Link-count admission is not a defense against concurrent hard-link churn or bind mounts; those require separate native evidence and policy.

The release boundary includes:

- default read-only Runs; opt-in model-driven files/commands/Skills/local MCP only through the typed private grant/Guard path; no arbitrary fetch, runtime plugin, self-update or unsandboxed fallback; account setup owns its compiled browser/callback route, and only the credential helper accesses the OS store;
- private no-follow SQLite state outside the Workspace, strict data-only replay, aggregate growth admission, and no encryption-at-rest claim;
- one immutable aggregate Run budget for calls, output tokens, bytes, time, memory, disk, `N`, and concurrency;
- only admitted fixed native or exact verified custom Provider requests, with phase-minimal content and origin-bound credentials; standard OS TLS validation may make auxiliary certificate-related connections, never an Arany payload destination;
- inert terminal output, no alternate screen, transient picker-only mouse, and one RAII terminal owner that restores every enabled mode on every exit path; and
- pinned/reviewed dependencies, lockfile, build/proc-macro/native inventory, and forbidden application `unsafe`.

The first local effectful base implements immutable typed effects, deterministic restrict-only Policy and a separately enforcing Guard. Private reviewed configuration supplies upfront grants; no per-effect human approval UI or universal host/same-UID protection is claimed. Native macOS, remote MCP/network disclosure and larger runtimes require separate admission/evidence.

## 12. Evidence-dense proof

The central deterministic test is one Session journey, not dozens of isolated unit tests:

1. create a Session and complete a single-agent Run;
2. exit and resume it in a fresh process;
3. change Provider/profile between Runs;
4. complete one `N`-child Run with controlled completion order and overflow presentation;
5. fork at the committed boundary and prove immutable prefix lineage;
6. exercise compaction failure without changing canonical history;
7. cancel another Run and prove every admitted child terminates; and
8. replay Session and Run views from a closed SQLite connection.

Its `ObservationBundle` joins exit class, exact deterministic stdout/stderr, reopened Events, SessionView, and RunView. Compact tables/corpora own collaboration capacities, parser commands, invalid snapshots, paths, hostile terminal text, custom profile conformance, and fault injection. Ratatui TestBackend owns transcript/composer/status frames. Native Linux/macOS PTY tests own history navigation, pickers, transient mouse capture/restoration, signals, suspension, cancellation stages, and terminal restoration. Each native claimed Provider owns an opt-in paid live test; custom profiles own data-free exact conformance before any Workspace-bearing smoke.

## 13. Distribution decision

Arany uses **Apache License 2.0 plus a project `NOTICE` file**. This standard combination allows free use and modification while requiring redistributed derivatives to preserve applicable attribution and mark changed files. It also includes an explicit patent grant.

It does not force private users or hosted services to show a public “Powered by” badge. Requiring use-time public credit would need a custom lawyer-reviewed license and would no longer be the ordinary standard open-source contract. Do not dual-license with MIT because recipients could bypass the Apache-specific NOTICE and changed-file obligations. The repository's exact `LICENSE` and `NOTICE` credit `fpmirabile`; every release artifact must preserve both files.

## 14. Explicitly deferred

| Trigger | Introduce then |
|---|---|
| Local tool workloads exceed the proven base | Reviewed runtime/cache/artifact adapters, bounded larger profiles and actual workload evidence |
| Remote MCP or network effects are needed | Exact protocol/auth/data-disclosure grants and enforcing transport adapters |
| Children need nested delegation | Bounded recursive supervision and depth budgets; beta remains flat |
| Agents write concurrently | Isolated Workspace views and one integration owner |
| Work needs dependencies/ownership transfer | Assignment DAG, attempts, leases, fencing |
| Knowledge must improve another Session | Scoped Memory plus retrieval/deletion evaluation |
| Event payloads exceed their bound | Content-addressed Artifacts |
| Replay misses a measured budget | Rebuildable snapshots |
| Labelled evaluation proves recent/lexical context insufficient | FTS/vector or hybrid derived retrieval |
| Second Client or detached execution exists | Per-user daemon and versioned local protocol |
| Built-in OpenRouter support is claimed | Exact route, privacy, provenance, strict-output, and no-fallback gates |
| Anthropic explicitly approves third-party subscription auth | Reassess native Claude subscription profile |
| Browser/remote/multi-tenant product exists | New identity, authorization, quota, encryption, retention, threat model |

## 15. Implementation order

1. Write the implementation plan with this Session/team contract, security claims/non-claims, and evidence owners.
2. Create the package with forbidden application `unsafe`, pinned toolchain/lockfile, minimal features, and reviewed supply chain.
3. Implement trusted startup, private state admission, Workspace snapshots, Session identity, Event schema, reducers, deterministic output, and hostile-content handling.
4. Make create/exit/resume/multi-Run/fork and deterministic context compilation pass with the scripted Provider.
5. Implement ordered bounded `0..N` children and prove `single`, `auto`, `team`, aggregate budgets, join, and cancellation.
6. Add a primary-screen committed transcript, fixed-bottom composer/status, conditional activity shelf, keyboard-complete Session/agent pickers, and native history/restoration PTY proof; add transient picker mouse only after restoration gates pass.
7. Add native OpenAI and Anthropic API-key adapters and their offline/live conformance.
8. Add trusted custom `openai-responses` profiles plus data-free `provider check`; do not admit unverified endpoints.
9. Add slash-command Tab completion and persistent argument previews without changing the trusted command boundary.
10. Complete the official ChatGPT-plan account, consent, model/effort admission, and synthetic Engine turns on a locally built Linux executable; preserve independent API-key routes and hand real sign-in, plan-consuming checks, and turns to the user.
11. Pursue Anthropic subscription only after provider authorization or a published supported route. Then run the broader Linux/macOS security, PTY/accessibility, fault, distribution, license, and performance gates, with native macOS on a macOS machine after this goal.
12. Verify the existing opt-in OTLP trace export still meets topology, privacy, bounds, failure-isolation, and shutdown gates after the new routes.

The beta 1 implementation handoff follows step 10's security/offline checks and local executable build. Actual account compatibility and usability remain unverified until the user's final checks; implementation completion does not waive those checks or turn synthetic evidence into live support. Later steps complete the broader product contract without being misreported as beta 1 evidence.
