# Arany

A Rust-first, CLI-only agent harness for durable conversations and controlled coding work.

Arany provides the runtime around a language model: it manages context, coordinates agent work, controls access to tools, and saves what happened. You work in a terminal, choose your Provider and model, and return to the same conversation across process restarts.

## Goal and principles

The goal is a dependable harness that can grow without replacing its Engine or tying your workflow to one Provider or operating system.

- **Durable work.** Keep conversations and execution history locally, with explicit resume, fork and recovery.
- **User control.** Make model selection, permissions and execution limits explicit. Tools operate within granted capabilities.
- **OS parity.** Aim for equivalent behavior on Linux, macOS and Windows, with narrow native integrations where necessary.
- **A useful terminal.** Support interactive conversations, accessible plain output and commands that compose with scripts.
- **Simple engineering.** Build in Rust, reuse existing capabilities and apply Ponytail to every implementation. Add abstractions when a real requirement earns them.

## How it works

A conversation is a **Session**. Each submitted task starts a bounded **Run**, led by one primary agent. The primary can reason directly, use explicitly enabled tools, or delegate bounded read-only work to child agents. Arany records execution in a local SQLite journal so you can inspect results and continue the conversation later.

The current implementation includes OpenAI and Anthropic API-key routes, consented ChatGPT-plan access, and verified custom read-only profiles. Optional coding tools provide guarded file edits, offline commands, runtime Skills and local MCP integrations.

**Status:** development beta. Linux and macOS have native Tool enforcement; platform verification and release checks remain incomplete. Windows parity is a design goal, not a current support claim. See [next steps](./NEXT_STEPS.md) for remaining work.

## Install from source

You need Git, Rustup and native build tools, including a C compiler. Rustup selects the toolchain pinned in [rust-toolchain.toml](./rust-toolchain.toml). From a terminal:

```sh
git clone https://github.com/fpmirabile/arany.git
cd arany
cargo install --path . --locked
arany --help
```

This installs the current source build into Cargo's binary directory; keep that directory on your `PATH`. To try it from the checkout without installing:

```sh
cargo run --locked
```

Release packaging has a separate [build and distribution workflow](./docs/security/beta-dependency-inventory.md#release-checks-still-required).

## Use Arany

Start `arany` in the project you want to work with. Open `/setup` to connect an OpenAI or Anthropic API account, or a ChatGPT account through its consent flow, then submit a task.

| Command | Purpose |
|---|---|
| `/setup` | Connect or manage an account |
| `/model` | Choose a model and reasoning effort |
| `/resume` | Reopen a saved conversation |
| `/new` | Start an empty conversation |
| `/help` | See available commands |

For a headless task, provide `OPENAI_API_KEY` through your environment, list your available models, and replace `MODEL_ID` below with your chosen model:

```sh
arany provider models openai
arany exec --provider openai --model MODEL_ID --effort low "Explain the trade-offs of this design"
arany show --output text SESSION_ID
```

Replace `SESSION_ID` with a saved Session ID. Model calls use your selected API account or subscription. The [setup guide](./docs/setup.md) covers credentials, account choices and model selection.

To enable file edits, commands, runtime Skills or MCP, follow the [local Tool setup](./docs/tools.md). Tool access requires explicit permission and available native enforcement.

## Develop

Start with the [contribution guide](./CONTRIBUTING.md) to find work, prepare a change and submit it for review. Coding agents start with [AGENTS.md](./AGENTS.md). Both use the shared [development setup and checks](./docs/development.md).

### Install development skills

Follow the [development skill setup](./agents/skills/README.md#install-for-a-harness)
for Ponytail, Rust Clean Code, Arany Terminal Design and Clean Architecture Review.
The same guide owns [restoration of the complete adopted set](./agents/skills/README.md#restore-the-lockfile)
and [source updates](./agents/skills/README.md#update-a-local-skill). Installation
uses Node.js/npm; the Arany executable itself is Rust. Development skills do not
enable runtime Tools.

### Run and check changes

Use the [development guide](./docs/development.md) for setup, verification commands
and native prerequisites. Documentation-only edits need prose and link review.

## Contribute

Bug reports, focused fixes, documentation improvements and native platform testing are welcome.

1. Describe the problem or proposed behavior; include the OS and terminal for platform-specific issues.
2. Fork the repository and create a focused branch from `main`.
3. Implement the change, run the applicable checks and update the owning documentation.
4. Open a pull request explaining what changed, why, and how you verified it.

Use English for repository content. Prefer the simplest complete change while preserving safety, accessibility and verification. The [contribution guide](./CONTRIBUTING.md) covers the full workflow.

## Documentation

- [Documentation map](./docs/README.md): find the owner for a topic.
- [System overview](./docs/architecture/system-overview.md): architecture and data flow.
- [Product specs](./docs/specs/README.md): behavior and acceptance scenarios.
- [Next steps](./NEXT_STEPS.md): current gaps and upcoming work.

## License

[Apache-2.0](./LICENSE), with [NOTICE](./NOTICE).
