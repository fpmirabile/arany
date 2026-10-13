# Development setup and checks

These commands and prerequisites are shared by human contributors and coding
agents. Contribution guidance lives in [CONTRIBUTING.md](../CONTRIBUTING.md);
agent instructions start at [AGENTS.md](../AGENTS.md).

## Get started

Install Rustup and native build tools, including a C compiler. The repository
selects its [pinned Rust toolchain](../rust-toolchain.toml). From the checkout:

```sh
cargo fetch --locked
cargo run --locked
```

Fetching dependencies prepares the cache for the offline checks below. Account
setup and submitted tasks follow the [setup guide](./setup.md); model calls can
consume API or plan usage. The default tests need no real Provider account.

## Verification

For documentation-only changes, review prose, local links and anchors; no Rust
build is needed. Development skill changes also follow the
[skill source workflow](../agents/skills/README.md#update-a-local-skill).

For code changes, run the applicable checks:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline
cargo test --workspace --locked --offline --release
```

Linux terminal tests need Bubblewrap and util-linux `script`, with user and
network namespaces enabled. On macOS, set `TMPDIR=/private/tmp` for test
invocations; the ordinary temporary-path aliases fail private-state admission.

The [CI workflow](../.github/workflows/ci.yml) owns automated job selection.
[Native test and CI reproduction notes](../agents/references/testing-details.md#local-verification-and-ci)
explain additional prerequisites, fixture isolation and activation of checks
ignored locally. Default local tests are not the complete native or release
suite. Live Provider checks require separate deliberate usage authorization;
an ignored check is not passing evidence.
