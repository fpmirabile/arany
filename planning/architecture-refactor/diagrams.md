# Architecture refactor diagrams

Supporting diagrams for the [architecture refactor plan](./README.md). Measurements behind the first two diagrams are in [evidence.md](./evidence.md). Measured on `origin/main` at `8857765`.

## 1. Today: module map with fan-in

Box label: module, `n / N` = non-test lines / total lines. Solid arrow: `crate::X` references from the source module to the target (count on the arrow). Dashed arrow: `cfg`-selected platform adapter. Red boxes are shallow, teal boxes are deep, purple boxes are edges.

```mermaid
flowchart TB
    subgraph BIN["arany binary (main.rs + cli/)  14,951 / 23,344"]
        CLI["cli/attached  8,867 / 11,176<br/>cli/exec  515 / 1,169<br/>cli/provider  340 / 424"]
        CRED["cli/credentials  1,427 / 2,342<br/>cli/chatgpt  3,669 / 8,100"]
    end

    subgraph LIB["arany library (lib.rs re-exports ~160 items)"]
        direction TB
        TERM["terminal  8,677 / 14,386"]
        PRES["presentation  1,355 / 2,569"]
        ENG["engine  6,544 / 7,434<br/>engine.rs 2,118 · run_loop 3,484<br/>compaction 403 · progress 289 · lifecycle 250"]
        SESS["session  5,193 / 9,158<br/>events 2,121 · reducer 2,233<br/>context 196 · compaction 250 · input 246"]
        STORE["store  5,534 / 6,961<br/>store.rs 601 · journal 371 · replay 537<br/>state 3,794 (build_cache about 2,000)"]
        PROV["provider  7,034 / 14,473<br/>render 260 · conversation 258 · decode 101<br/>openai 1,899 · anthropic 1,036 · custom 963<br/>streaming 648 · native_check 334 · catalog 242"]
        TOOLS["tools  13,207 / 21,574<br/>tools.rs 545 · types 1,161 · fs 1,322 · git 692<br/>config 334 · workspace 425 · skills 391 · mcp 294<br/>toolchains 568 · approval 93"]
        GUARD["tools/guard  7,381 / 12,295<br/>guard.rs 1,738 · admission 1,933<br/>linux 2,117 · macos about 6,000"]
        TEL["telemetry  1,099 / 2,584"]
        DIAG["diagnostics  592"]
        PFS["private_fs  363 / 481"]
    end

    CLI --> TERM
    CLI --> PRES
    CLI --> ENG
    CRED --> STORE
    TERM -->|37| PRES
    TERM -->|26| SESS
    TERM -->|13| PROV
    TERM -->|11| TOOLS
    PRES -->|15| SESS
    PRES -->|14| TOOLS
    ENG -->|153| TOOLS
    ENG -->|80| SESS
    ENG -->|31| PROV
    ENG -->|14| TEL
    ENG -->|10| DIAG
    ENG -->|5| STORE
    SESS -->|60| TOOLS
    SESS -->|43| PROV
    PROV -->|25| TOOLS
    PROV -->|14| SESS
    PROV -->|11| STORE
    TOOLS -->|19| STORE
    TOOLS -->|16| PFS
    TOOLS -->|8| SESS
    TOOLS -->|5| ENG
    STORE -->|16| PFS
    STORE -->|8| SESS
    TOOLS --> GUARD
    GUARD -. linux / macos .-> GUARD

    classDef deep fill:#173b34,color:#fff,stroke:#5eead4,stroke-width:2px;
    classDef shallow fill:#4a2a2a,color:#fff,stroke:#f87171,stroke-width:2px;
    classDef edge fill:#2b2346,color:#fff,stroke:#c4b5fd;
    class SESS,STORE,PFS,PRES,GUARD deep;
    class ENG,TOOLS shallow;
    class CLI,CRED,TERM edge;
```

## 2. Today: runtime flow of one Run

```mermaid
sequenceDiagram
    autonumber
    participant CLI as CLI (attached / exec)
    participant E as Engine::run_turn
    participant S as Store (SQLite thread)
    participant L as RunLoop::execute
    participant R as provider::render
    participant P as Provider adapter
    participant D as provider::decode
    participant T as ToolRuntime (Policy)
    participant G as Guard helper
    participant V as Reducer / views

    CLI->>E: RunRequest {objective, workspace, policy, GoalRequest}
    E->>E: select_provider, validate_request, lock Session + fork ancestors
    E->>S: load_view, admit_run_start (13 + chunk slots)
    E->>E: ToolRuntime::admit, WorkspaceInputs, CompiledContext::compile (376 KiB)
    E->>S: GoalStarted/GoalResumed, GoalRunLinked, MessageAccepted, RunStarted, AgentSpawned
    S-->>V: each append re-validated by apply_committed
    loop one step per Provider call
        L->>L: pause at boundary? steering note? (GoalSteeringNoteRecorded)
        L->>S: GoalActiveTimeReserved, GoalProviderCallReserved
        L->>R: ProviderRequest to Conversation {system, tools, messages}
        R->>P: wire request (Messages / Responses)
        P->>D: tool_use / function_call / structured text
        D-->>L: ProviderOutcome: Finish, Delegate, or Tools (one call)
        L->>S: ProviderCallRecorded, GoalActiveTimeSettled
        alt Tools
            L->>T: intent(call) with limits and result capacity
            L->>S: ToolStarted
            L->>T: approval: human mpsc(1) or AI review call (ToolReview phase)
            T->>G: Task, Ready, GO over stdio (systemd-run + bwrap or launchd + Seatbelt)
            G-->>T: observation + GuardReceipt, or Uncertain
            L->>S: ToolFinished (+ ApplyChangeset integration for commands)
            L->>L: push ToolStep; compact_tool_context_if_needed (2/3 of budget)
        else Delegate
            L->>L: run_children: at most 8 children, JoinSet, at most 16 read-only Tool calls each
            L->>S: AgentSpawned, ProviderCallRecorded, ToolStarted/Finished, AgentFinished per child
            L->>L: RootSynthesis call with child_results
        else Finish
            L->>S: AgentFinished, MessageCommitted, RunFinished
        end
    end
    E->>S: GoalFinished {status, report}
    S-->>V: SessionView / RunView / GoalView
    V-->>CLI: RunProgress watch (coalesced RunUpdate) and RunOutcome
    Note over E,V: autonomous Goal: run_inner repeats run_turn with GoalRequest::resume until the Goal stops
    Note over S,V: replay = Store::load_session then SessionView::replay, never re-executes
```

## 3. Today: process and privilege map

```mermaid
flowchart LR
    subgraph HOST["OS user session"]
        subgraph MAIN["arany process (current-thread Tokio)"]
            CLI2["CLI + terminal"]
            ENG2["Engine + RunLoop"]
            SQL["arany-sqlite std thread<br/>one rusqlite connection"]
            TELW["telemetry batch worker<br/>256-span queue"]
            CLI2 --> ENG2 --> SQL
            ENG2 -.-> TELW
        end
        STATE[("StateRoot (private dir, 0700)<br/>events.sqlite3 · locks · tools.json<br/>provider-profiles.json · build-cache/")]
        ACCT[("Account root (passwd home)<br/>account-credentials.json<br/>chatgpt-accounts.json")]
        KEYH["arany --internal-credential-helper<br/>(same binary, supervised)"]
        KEYRING[("OS keyring<br/>Secret Service / Keychain")]
        CLIP["clipboard OS client<br/>(terminal-owned child)"]
        SQL --> STATE
        CLI2 --> KEYH --> KEYRING
        KEYH --> ACCT
        CLI2 --> CLIP
    end

    subgraph LINUX["Linux Guard per intent"]
        SDR["systemd-run --user transient unit"]
        BWR["/usr/bin/unshare + bwrap<br/>seccomp, cgroup limits, no network"]
        GH["arany --internal-tool-guard<br/>helper_main: Task, Ready, GO, observation"]
        PAY["payload: typed file op · pinned command<br/>git read · Skill read · stdio MCP server"]
        SDR --> BWR --> GH --> PAY
    end

    subgraph MAC["macOS Guard per command or MCP Run"]
        LD["launchd job (own coalition)"]
        UNIT["arany --internal-native-tool-unit<br/>unit supervisor: disk images, control channel"]
        SB["Seatbelt worker<br/>file / command / git profiles"]
        PAY2["payload"]
        LD --> UNIT --> SB --> PAY2
    end

    ENG2 -- "stdio Task/Ready/GO" --> SDR
    ENG2 -- "stdio Task/Ready/GO" --> LD
    PAY --> WS[("Workspace (ro bind or private snapshot)<br/>changesets returned, not written")]
    PAY2 --> WS
    GH -. "capacity FIFO, seed storage" .-> STATE
    MCP["MCP stdio servers (at most 4, pinned)"]
    PAY --> MCP
    PAY2 --> MCP

    classDef priv fill:#4a2a2a,color:#fff,stroke:#f87171;
    class SDR,BWR,GH,LD,UNIT,SB priv;
```

## 4. Today: data model, Events by aggregate

```mermaid
flowchart LR
    subgraph SESSION_EV["Session aggregate (Session-scoped)"]
        SS["SessionStarted<br/>SessionRenamed<br/>SessionDefaultChanged<br/>SessionForked<br/>ContextCompacted"]
    end
    subgraph GOAL_EV["Goal aggregate (goal_id, run_id)"]
        GS["GoalStarted · GoalRunLinked · GoalResumed<br/>GoalPlanContractRecorded · GoalFinished"]
        GB["GoalProviderCallReserved<br/>GoalActiveTimeReserved · GoalActiveTimeSettled<br/>GoalCompactionCallReserved · GoalContextCompacted<br/>GoalSteeringNoteRecorded"]
    end
    subgraph RUN_EV["Run aggregate (run_id)"]
        RS["MessageAccepted · RunStarted<br/>MessageCommitted · RunFinished"]
    end
    subgraph AGENT_EV["AgentRun aggregate (run_id, agent_run_id)"]
        AS["AgentSpawned · ProviderCallRecorded<br/>AssistantDisplayRecorded · AgentFinished"]
        TS["ToolStarted {EffectIntent}<br/>ToolFinished {ToolObservation + GuardReceipt}"]
    end

    SS --> SV
    GS --> GV
    GB --> GV
    RS --> RV
    AS --> AV
    TS --> TV

    subgraph VIEWS["Reduced views (SessionView::apply_committed)"]
        SV["SessionView<br/>title, defaults, lineage, workspace_identity<br/>runs: list of RunView · goals: list of GoalView · compactions"]
        GV["GoalView<br/>budget, phase, status, plan_contract<br/>active_ms, reserved_active_ms, provider_calls<br/>known_tokens, report, run_ids"]
        RV["RunView<br/>objective, config: RunConfig, status<br/>agents: list of AgentView · tools: list of ToolView<br/>display, steering_notes, compaction summary"]
        AV["AgentView<br/>role, ordinal, grant, provider_calls<br/>tools (child research), status, summary/result"]
        TV["ToolView<br/>intent, observation, review_call_index"]
        SV --> GV
        SV --> RV
        RV --> AV
        RV --> TV
        AV --> TV
    end

    VIEWS --> PM["PresentationModel / render_exec / goal_status<br/>(pure, width-clipped)"]
    VIEWS --> CC["CompiledContext::compile<br/>(history of HistoryTurn, images, summary)"]
    VIEWS --> RP["RunProgress watch (RunUpdate {sequence, run, goal})"]
```

## 5. Target: crates, modules and dependency direction (after PR14)

Every box is named for what it owns. The `arany` library keeps its name so test imports stay valid; `arany-journal` is conditional on roadmap item 8.

```mermaid
flowchart TB
    subgraph CLI["crate arany-cli (bin `arany`)"]
        direction TB
        ATT["cli/attached: interactive flows (setup, pickers, submit, controls)"]
        EXEC["cli/exec: one-shot runs, text/JSONL output"]
        TERMI["terminal: Ratatui screens, composer, history, clipboard"]
        PRESI["presentation: pure projections of views to rows and lines"]
        ACCT["accounts: ChatGPT OAuth, saved API accounts, keyring helper<br/>(today cli/chatgpt + cli/credentials)"]
        SERVE["serve: JSON-RPC stdio server (roadmap item 8, not in this plan)"]
    end

    subgraph ENGINE["crate arany (library: the harness)"]
        direction TB
        ENGINE_MOD["engine: Engine::open / submit, RunHandle, goal admission, reports<br/>engine/ledger: GoalLedger over GoalView, Action::max_events<br/>engine/step: one AgentStep executor for primary, researcher, (later) worker<br/>engine/run_loop: Provider call and step loop<br/>engine/children: delegation waves<br/>engine/compaction and engine/lifecycle"]
        JOURNALTAP["journal: Journal::append + bounded broadcast tap<br/>observers: progress, telemetry, diagnostics (each a Receiver)"]
        PROVADAPT["provider: Provider trait + identity, render, decode, conversation,<br/>adapters openai / anthropic / custom / chatgpt, streaming, catalog, checks"]
        TOOLSRT["tools: ToolRuntime (admit, intent, execute), config (grants),<br/>catalog (OperationFacts table, schemas, model prose),<br/>dispatch (one operation to family router),<br/>fs · git · mcp · skills · workspace · toolchains · approval"]
        GUARD["tools/guard: hard privilege seam: helper main, admission FIFO,<br/>linux (systemd + bwrap) · macos (launchd + Seatbelt + unit)"]
        STORESTATE["store: Store thread (SQLite), state (StateRoot records, locks, build cache)"]
        TEL["telemetry: OTLP exporter fed by the journal tap"]
        DIAG["diagnostics: bounded private failure log"]
        PFS["private_fs: private-directory admission and ACL checks"]
    end

    subgraph JOURNAL["crate arany-journal (vocabulary: no process, no network)"]
        direction TB
        EVENTS["events: Event enum, payload kinds and per-kind versions"]
        REDUCER["reducer: SessionView::apply_committed by aggregate:<br/>session · goal · run · agent · tool"]
        VIEWS["views: SessionView, GoalView, RunView, AgentView, ToolView"]
        VOCAB["vocabulary: ToolCall + OperationFacts, EffectIntent, ToolObservation,<br/>Attestation, ProviderIdentity, Usage, ImageAttachment"]
        REPLAY["replay: strict cold replay, Event bounds, Cancellation primitive"]
    end

    CLI --> ENGINE
    CLI --> JOURNAL
    ENGINE --> JOURNAL
    ENGINE_MOD --> JOURNALTAP --> STORESTATE
    ENGINE_MOD --> PROVADAPT & TOOLSRT
    TOOLSRT --> GUARD
    JOURNALTAP -.-> TEL & DIAG
```

## 6. Ordering: PR dependency graph

Critical path in bold: PR1, PR6, PR7, PR10, PR11, PR12, PR13. Edges read "must land before".

```mermaid
flowchart LR
    PR1[PR1 cycle cut S] --> PR6[PR6 facts table M] --> PR7[PR7 in-process reads M]
    PR5[PR5 per-kind versions S] --> PR7
    PR2[PR2 reducer by aggregate S] --> PR8[PR8 GoalLedger M]
    PR3[PR3 Provider identity S] --> PR8
    PR7 --> PR10[PR10 execute_step M]
    PR8 --> PR10
    PR8 --> PR9[PR9 ledger diet M]
    PR5 --> PR9
    PR10 --> PR11[PR11 journal tap M]
    PR9 -.-> PR11
    PR11 --> PR12[PR12 long-lived Engine M]
    PR12 --> PR13[PR13 workspace split M]
    PR13 --> PR14[PR14 journal crate S]
    PR4[PR4 StateRoot records S]
    classDef crit fill:#4a2a2a,color:#fff,stroke:#f87171,stroke-width:2px;
    class PR1,PR6,PR7,PR10,PR11,PR12,PR13 crit;
```
