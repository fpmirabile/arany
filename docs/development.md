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

For code changes, run formatting, Clippy and the tests that cover what you
touched:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --locked --offline <filter>
```

Choose `<filter>` from the owning corpus or journey of the changed code, for
example `--test engine_run goals::`, `--test session_run tools::` or
`--lib store::` for a module's own `tests.rs`; add `--release` when the change
touches an optimized-profile gate. The complete workspace suite in both
profiles runs in [CI](../.github/workflows/ci.yml) on every pull request,
whatever its base branch; watch that run and fix a red result before handoff.
Run the full suite locally only when CI is unavailable or the change is
cross-cutting:

```sh
cargo test --workspace --locked --offline
cargo test --workspace --locked --offline --release
```

Linux terminal tests need Bubblewrap and util-linux `script`, with user and
network namespaces enabled. On macOS, set `TMPDIR=/private/tmp` for test
invocations; the ordinary temporary-path aliases fail private-state admission.

[Native test and CI reproduction notes](../agents/references/testing-details.md#local-verification-and-ci)
explain additional prerequisites, fixture isolation and activation of checks
ignored locally. A targeted or default local run is not the complete native or release suite; it does not establish platform support. Live Provider checks require separate deliberate usage
authorization; an ignored check is not passing evidence.
