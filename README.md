# Arany

Arany is a Rust-first, CLI-only agent harness. It keeps Sessions and Runs in a local SQLite journal, supports bounded direct read-only child agents, and makes Provider and model selection explicit. The beta is still in development; the [implementation plan](./planning/arany-beta/README.md) tracks the remaining release gates.

## Try the current build

Build from the locked sources, then inspect the CLI:

```sh
cargo build --locked
target/debug/arany --help
```

The current repository build uses local, pinned repairs for Crossterm terminal hangup and the macOS TLS verifier's certificate-network-fetch policy. `bash scripts/source-archive.sh` creates and verifies a separate source archive that retains both; ordinary `cargo package` removes local patches and is not an installation path for either repair. The guarded native release build is `bash scripts/release-build.sh`. Neither command by itself clears the remaining beta release gates.

In a terminal, `arany` opens account setup when no default API account exists. It asks for an OpenAI or Anthropic API key, a model, and an effort, then prefers the operating system's credential store. If no keyring is available and none is pinned, it can save to a private but unencrypted file after an explicit warning and confirmation. `arany --setup` deliberately replaces the default account. See the [setup guide](./docs/setup.md) for saved-account limitations, model checks, and cancellation behavior.

In the inline composer, `Ctrl+O` inserts a newline, Tab completes supported slash commands, and `Ctrl+K` opens quick actions without discarding your draft. Use `/help` for the full command list. The screen-reader mode keeps labeled, append-only output and literal slash commands.

For a headless Run, provide the selected API key through its named environment variable using your secret manager, then choose the Provider and model explicitly:

```sh
target/debug/arany provider models openai
target/debug/arany exec --provider openai --model gpt-5.4 "Your task"
```

`exec` can incur Provider charges. Model listing is availability, not proof that every listed model can run; an unreviewed model needs a separate, explicitly accepted, potentially billable `provider check`. Use `arany provider check --help` for its bounds. `arany show --output text SESSION_ID` reads a saved Session. The Workspace defaults to the current directory; use `--workspace` to select another, and keep `--state-dir` outside it.

The currently supported beta routes are native OpenAI and Anthropic API keys and exact custom profiles after data-free conformance. ChatGPT-plan sign-in is displayed in setup but is not usable yet; it never silently falls back to API-key billing. Arany does not ship effectful tools or a daemon. Session history is not encrypted at rest, and native macOS, paid live Provider, accessibility, and supply-chain release gates remain open. Do not treat the current build as a verified beta release.

Architecture and security boundaries: [system overview](./docs/architecture/system-overview.md) · [account setup](./docs/setup.md) · [dependency inventory](./docs/security/beta-dependency-inventory.md). License: [Apache-2.0](./LICENSE) with [NOTICE](./NOTICE).
