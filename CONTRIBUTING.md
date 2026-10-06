# Contributing to Arany

Contributions are welcome. Fork the repository, create a focused branch from `main`, and open a pull request against `fpmirabile/arany:main`. Explain the problem, the resulting behavior, and how you verified it.

Read [AGENTS.md](./AGENTS.md) and the relevant [documentation](./docs/README.md) before changing code. Repository content must be in English. Changes to product behavior must follow the [spec workflow](./docs/specs/README.md).

## Verification

The [CI workflow](./.github/workflows/ci.yml) owns its Rust version and check commands. It checks formatting, rejects Clippy warnings across all targets, and runs the default test suite in both debug and release profiles. Run the equivalent checks locally before submitting:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline
cargo test --workspace --locked --offline --release
```

Run `cargo fetch --locked` first when the locked dependencies are not already available. Linux terminal fixtures need Bubblewrap and util-linux `script`, with user and network namespaces enabled. CI provisions these prerequisites on disposable GitHub-hosted runners. Ignored native, manual, performance, and live Provider gates remain separate; default CI uses no account credentials or paid calls. See [testing rules](./agents/testing.md) for evidence requirements.

## Pull requests

Changes reach `main` through pull requests. Resolve review conversations and pass the required checks before merging. External contributors do not need repository write access. GitHub may require a maintainer to approve CI for a first-time contributor; fork pull requests receive no repository secrets and use a read-only token.
