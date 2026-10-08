# Contributing to Arany

Contributions are welcome. Fork the repository, create a focused branch from `main`, and open a pull request against `fpmirabile/arany:main`. Explain the problem, the resulting behavior, and how you verified it.

Read [AGENTS.md](./AGENTS.md) and the relevant [documentation](./docs/README.md) before changing code. Repository content must be in English. Changes to product behavior must follow the [spec workflow](./docs/specs/README.md).

Apply the [product and implementation priorities](./AGENTS.md#product-and-implementation-priorities): preserve Arany's harness responsibilities, design for OS parity, and use Ponytail for every implementation and code review. Install the [project development skills](./agents/skills/README.md#install-for-a-harness) before coding; their checked-in sources and generated installations have different roles.

## Verification

Documentation-only changes need prose, local-link and anchor review. For skill changes, also validate frontmatter, references and installation as described in the [skill workflow](./agents/skills/README.md#update-a-local-skill). These changes do not require rebuilding or testing the Rust executable.

Use Rustup with the repository's [pinned toolchain](./rust-toolchain.toml). The [CI workflow](./.github/workflows/ci.yml) checks formatting and Clippy on Linux and runs native default tests in debug and release on Linux and macOS. Run the applicable checks locally before submitting:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline
cargo test --workspace --locked --offline --release
```

Run `cargo fetch --locked` first when the locked dependencies are not already available. Linux terminal fixtures need Bubblewrap and util-linux `script`, with user and network namespaces enabled. On macOS, set `TMPDIR=/private/tmp` when running tests so private-state fixtures use a physical path rather than the rejected `/tmp` or `/var` symlink aliases. CI provisions its prerequisites on disposable GitHub-hosted runners.

Default suites exercise only the scenarios enabled for their native OS. Ignored native, manual, performance, and live Provider gates remain separate; default CI uses no account credentials or paid calls. Passing compilation or an OS-specific subset does not prove platform parity. See [testing rules](./agents/testing.md) for evidence requirements.

## Pull requests

Changes reach `main` through pull requests. Resolve review conversations and pass the required checks before merging. External contributors do not need repository write access. GitHub may require a maintainer to approve CI for a first-time contributor; fork pull requests receive no repository secrets and use a read-only token.

[Dependabot configuration](./.github/dependabot.yml) owns the version-update schedule and open-PR limits for Cargo and GitHub Actions. Review proposed dependency changes and pass the required checks before merging.
