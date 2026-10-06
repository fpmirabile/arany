# Arany

Arany is a Rust-first, CLI-only agent harness. It keeps Sessions and Runs in a local SQLite journal, supports bounded direct read-only child agents, and makes Provider and model selection explicit. Opt-in local coding tools add guarded file operations, bounded offline commands, portable Skills and stdio MCP to the primary agent. The beta is still in development; the [beta plan](./planning/arany-beta/README.md) and [coding-base plan](./planning/effectful-beta-base/README.md) track scoped evidence, and [next steps](./NEXT_STEPS.md) separates final user-owned checks from beta 2 and broader release work.

## Try the current build

Build from the locked sources, then inspect the CLI:

```sh
cargo build --locked
target/debug/arany --help
```

`cargo run --locked` rebuilds and runs the current development sources. Development chat and `exec` keep a bounded, private `development.log` in their Session State directory for subscription failures, with closed cause categories and local stack frames but no tokens, prompts or raw responses. Optimized `--release` builds leave it disabled. See [development diagnostics](./docs/development-diagnostics.md) for location, privacy and retention.

The current repository build uses one local, pinned repair for Crossterm terminal hangup and the published `rustls-platform-verifier` for system TLS trust. `bash scripts/source-archive.sh` creates and verifies a separate source archive that retains the Crossterm repair; ordinary `cargo package` removes local patches and is not an installation path for that repair. The guarded native release build is `bash scripts/release-build.sh`. Arany restricts its own request destinations and which content and credentials it sends; operating-system TLS validation may make separate certificate-related connections. Neither build command by itself clears the remaining beta release gates.

In a terminal, `arany` opens the normal chat input even without a default account. Submitting work then points you to `/setup`, where you can save an OpenAI or Anthropic API key or connect a ChatGPT account. Native API setup reads its catalog and selects a low-effort default without model/effort/cost questions or inference. ChatGPT setup reads its visible catalog and returns to chat with `gpt-5.6-luna` when available, otherwise its first visible model, and low effort. It starts no inference; compatibility checks are optional diagnostics, not Run prerequisites. Setup prefers the operating system's credential store; if no keyring is available and none is pinned, it can save to a private but unencrypted file after an explicit warning and confirmation. `arany --setup` opens setup before creating a new Session. See the [setup guide](./docs/setup.md) for account limitations, model checks, and cancellation behavior.

In the inline composer, `Ctrl+O` inserts a newline; Shift+Enter also works when your terminal distinguishes it from Enter. Ctrl+Left/Right move by word, and Ctrl+W deletes the previous word. Arrow keys select slash-name suggestions; Tab or Enter fills the focused name without running it, and a later Enter submits. `Ctrl+K` opens Models, Agents, Resume or Setup without discarding your draft. `/model` opens a list above that input: Up/Down changes the model, Left/Right its reasoning, Enter selects both and Escape returns without changes. Screen-reader mode accepts a numbered model with an optional effort, such as `1 high`. Use `/resume` to choose a conversation by readable title, with creation/activity dates and its model/access route. Empty conversations inherit your available default; configured conversations retain their selection. Use `/help` for the full command list. Important notices and errors remain readable in the chat; this local feedback lasts only within the attached Session. Idle Ctrl+C clears a draft; on empty input it explains Ctrl+Shift+C copying before a second Ctrl+C within two seconds exits. The screen-reader mode keeps labeled, append-only output and literal slash commands.

Framed emulator text paste stays editable and never submits automatically. On Linux, explicit Ctrl+V or `/paste` reads text or PNG through standard Wayland/local X11 clients, not a GNOME/KDE integration. Image drafts admit at most four PNGs totaling 192 KiB; Enter can send images alone or with text. Human history shows metadata, while private canonical history and explicit JSONL export retain image bytes. Compaction uses text and image metadata, not pixels. Native clipboard service success and real model vision still need verification; see the setup guide for limits and privacy.

For a headless Run, provide the selected API key through its named environment variable using your secret manager, then choose the Provider and model explicitly:

```sh
target/debug/arany provider models openai
target/debug/arany exec --provider openai --model gpt-5.4 "Your task"
```

`exec` can incur Provider charges. Model listing is availability, not proof that every listed model can run. Native unreviewed models require explicit effort, not a synthetic check; actual responses still undergo strict validation. `provider check` is an optional, potentially billable diagnostic for native models, while exact custom destinations require their separate data-free conformance before Workspace disclosure. Use `arany provider check --help` for its bounds. `arany show --output text SESSION_ID` reads a saved Session. The Workspace defaults to the current directory; use `--workspace` to select another, and keep `--state-dir` outside it.

For coding work, add `--tools` in chat or `exec` and create a private, reviewed `tools.json` outside the Workspace. The [tool guide](./docs/tools.md) covers file-only setup, executable/resource pins, Skills, MCP and limits. Only the primary receives effect authority. Linux enforcement uses installed Bubblewrap, the systemd user manager and effective kernel namespace/cgroup/syscall controls; missing enforcement rejects without an unsandboxed fallback. Commands/MCP work on a selected-file copy without network or account credentials; their changes are discarded, while typed writes/edits integrate into the project. Native macOS enforcement and tool-capable custom-profile conformance are not implemented.

The currently wired Run routes are native OpenAI and Anthropic API keys, exact custom read-only profiles after data-free conformance, and a consented ChatGPT-plan account/model/effort. ChatGPT Runs have no API-key billing fallback and only a local output-acceptance limit, not a remote generation or plan-usage cap. This path passes synthetic tests but has not completed a verified live Run. The new tool loop is verified with synthetic Providers and actual isolated Linux processes, not live models. Arany has no daemon. Session history is not encrypted at rest, and native macOS, paid live Provider, accessibility, and supply-chain release gates remain open. Do not treat the current build as a verified beta release.

Architecture and security boundaries: [system overview](./docs/architecture/system-overview.md) · [account setup](./docs/setup.md) · [local tools](./docs/tools.md) · [dependency inventory](./docs/security/beta-dependency-inventory.md). License: [Apache-2.0](./LICENSE) with [NOTICE](./NOTICE).

Linux terminal test fixtures use util-linux `script` and Bubblewrap with enabled user/network namespaces to isolate effective-user preferences in both build profiles.

For development, start with [the documentation map](./docs/README.md) and [AGENTS.md](./AGENTS.md). [Product specs](./docs/specs/README.md) define agreed observable behavior and point to its evidence owners; [planning](./planning/README.md) holds proposed changes and execution work.
