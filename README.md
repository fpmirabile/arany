# Arany

Arany is a Rust-first, CLI-only agent harness. It keeps Sessions and Runs in a local SQLite journal, supports bounded direct read-only child agents, and makes Provider and model selection explicit. The beta is still in development; the [implementation plan](./planning/arany-beta/README.md) tracks its evidence, and [next steps](./NEXT_STEPS.md) separates the final user-owned beta 1 checks from beta 2 and broader release work.

## Try the current build

Build from the locked sources, then inspect the CLI:

```sh
cargo build --locked
target/debug/arany --help
```

The current repository build uses one local, pinned repair for Crossterm terminal hangup and the published `rustls-platform-verifier` for system TLS trust. `bash scripts/source-archive.sh` creates and verifies a separate source archive that retains the Crossterm repair; ordinary `cargo package` removes local patches and is not an installation path for that repair. The guarded native release build is `bash scripts/release-build.sh`. Arany restricts its own request destinations and which content and credentials it sends; operating-system TLS validation may make separate certificate-related connections. Neither build command by itself clears the remaining beta release gates.

In a terminal, `arany` opens the normal chat input even without a default account. Submitting work then points you to `/setup`, where you can choose an OpenAI or Anthropic API key, a model, and an effort, or connect a ChatGPT account. ChatGPT setup then lists visible models, asks for an explicit effort, and offers a separate plan-consuming compatibility check before a Run. Setup prefers the operating system's credential store; if no keyring is available and none is pinned, it can save to a private but unencrypted file after an explicit warning and confirmation. `arany --setup` opens setup before creating a new Session. See the [setup guide](./docs/setup.md) for account limitations, model checks, and cancellation behavior.

In the inline composer, `Ctrl+O` inserts a newline, Tab completes supported slash commands, and `Ctrl+K` opens Models, Agents, Sessions, or Setup without discarding your draft. Use `/help` for the full command list. Important notices and errors remain readable in the chat; this local feedback lasts only within the attached Session. The screen-reader mode keeps labeled, append-only output and literal slash commands.

For a headless Run, provide the selected API key through its named environment variable using your secret manager, then choose the Provider and model explicitly:

```sh
target/debug/arany provider models openai
target/debug/arany exec --provider openai --model gpt-5.4 "Your task"
```

`exec` can incur Provider charges. Model listing is availability, not proof that every listed model can run; an unreviewed model needs a separate, explicitly accepted, potentially billable `provider check`. Use `arany provider check --help` for its bounds. `arany show --output text SESSION_ID` reads a saved Session. The Workspace defaults to the current directory; use `--workspace` to select another, and keep `--state-dir` outside it.

The currently wired Run routes are native OpenAI and Anthropic API keys, exact custom profiles after data-free conformance, and a checked ChatGPT-plan account/model/effort. ChatGPT Runs have no API-key billing fallback and only a local output-acceptance limit, not a remote generation or plan-usage cap. This path passes synthetic tests but has not completed a real account login or live Run. Arany does not ship effectful tools or a daemon. Session history is not encrypted at rest, and native macOS, paid live Provider, accessibility, and supply-chain release gates remain open. Do not treat the current build as a verified beta release.

Architecture and security boundaries: [system overview](./docs/architecture/system-overview.md) · [account setup](./docs/setup.md) · [dependency inventory](./docs/security/beta-dependency-inventory.md). License: [Apache-2.0](./LICENSE) with [NOTICE](./NOTICE).
