# Arany

A fast multi-agent coding harness for the terminal, built so you can trust what its agents did.

Arany aims to do the hard thinking about teams for you: on a big task it plans the work and splits it across agents automatically. Every agent works inside a verifiable cage, so you can check that everything went as intended and that no agent went rogue.

## Why Arany

- **Teams without the choreography.** You describe the task; Arany plans it and divides the work across agents, instead of making you design the team.
- **Safe AI, checkable afterwards.** Agents run inside a cage you can verify, and agents can only narrow authority, never widen it.
  - **Durable journal and replay.** Every Provider call, Tool intent and effect is saved locally before the next step. Resume, fork and replay never re-run an effect.
  - **Honest uncertainty.** If the outcome of an action is unknown, Arany says so, stops there and never retries it for you.
  - **Sandboxed Guard with receipts.** Edits, commands, Skill scripts and MCP calls run in the Guard under an explicit grant and publish only as validated changes with receipts.
  - **Budgeted Goals.** Budgets, pause, steering and cancellation survive restarts and crashes.
  - **Hostile input stays data.** Repository files, Tool output, MCP metadata and Skill text are never treated as instructions.
- **Your Provider, your terminal.** Choose OpenAI or Anthropic API keys, or a ChatGPT account, and work on Linux or macOS (Windows users run the Linux build under WSL2). Output is accessible and composes with scripts.

## Status

Development beta; Linux and macOS have native Tool enforcement, but platform verification and release checks remain incomplete. Teams today are read-only researcher children: a primary agent can delegate bounded read-only work. Editing teams are in progress; see [next steps](./NEXT_STEPS.md) for the order and for remaining gaps. The [planning index](./planning/README.md#active-plans) locates proposed team features.

## Get started

### Install from source

You need Git, Rustup and native build tools, including a C compiler. Rustup selects the toolchain pinned in [rust-toolchain.toml](./rust-toolchain.toml). From a terminal:

```sh
git clone https://github.com/fpmirabile/arany.git
cd arany
cargo install --path crates/arany-cli --locked
arany --help
```

This installs the current source build into Cargo's binary directory; keep that directory on your `PATH`. To try it from the checkout without installing:

```sh
cargo run --locked
```

Release packaging has a separate [build and distribution workflow](./docs/security/beta-dependency-inventory.md#release-checks-still-required).

### Use Arany

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

## Learn more

- [Documentation map](./docs/README.md): find the owner for a topic.
- [System overview](./docs/architecture/system-overview.md): architecture and data flow.
- [Product specs](./docs/specs/README.md): behavior and acceptance scenarios.
- [Next steps](./NEXT_STEPS.md): current gaps and upcoming work.
- [Contribution guide](./CONTRIBUTING.md): bug reports, fixes, documentation and native platform testing are welcome. Coding agents start with [AGENTS.md](./AGENTS.md); both use the shared [development setup and checks](./docs/development.md) and the [development skill setup](./agents/skills/README.md#install-for-a-harness).

## License

[Apache-2.0](./LICENSE), with [NOTICE](./NOTICE).
