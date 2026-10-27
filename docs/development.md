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
CI scans for bounds that should link to the
[generated limits table](specs/limits.md); run `python3 scripts/spec-bounds.py`
locally after changing a spec.

For code changes, run formatting, Clippy and the tests that cover what you
touched:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --locked --offline <filter>
```

Choose `<filter>` from the owning corpus or journey of the changed code, for
example `-p arany --test engine_run goals::`, `-p arany-cli --test session_run tools::` or
`-p arany --lib store::` for a module's own `tests.rs`; add `--release` when the change
touches an optimized-profile gate. On both Linux and macOS, [CI](../.github/workflows/ci.yml)
runs the complete workspace suite in release and the unit, binary and
`session_run` tests in debug. Release owns the shared Engine and Guard integration
suites and release-script tests. Debug retains development diagnostics, account
fixtures, Keychain and terminal journeys that release excludes or exercises
differently, plus unit assertions. The same [test action](../.github/actions/ci-tests/action.yml)
owns standard and focused lanes. Watch the complete CI run and fix a red result
before handoff. A new integration target with debug-only coverage must also be
included in the debug selection.

Both native debug lanes run production-only Clippy in debug and release without
`test-support`: selecting `--lib --bins` excludes the dev-dependencies that enable
it. These passes retain CI's `RUSTFLAGS` to share dependency fingerprints and
caches; `arany_ci` only affects tests. The debug pass also rejects `unwrap()` while
allowing `expect()` for proven invariants, and covers the same configuration as a
debug production `cargo check`:

```sh
cargo clippy --workspace --lib --bins --release --locked --offline -- -D warnings
cargo clippy --workspace --lib --bins --locked --offline -- -D warnings -D clippy::unwrap_used
```

Only the Linux debug lane also compiles all release targets with all features,
replacing CI's `RUSTFLAGS` to cover test alternatives without `arany_ci` while
denying warnings. This extra dependency rebuild is omitted on macOS to limit
runner cost:

```sh
RUSTFLAGS='-D warnings' cargo check --workspace --all-targets --all-features --release --locked --offline
```

Run the full suite locally only when CI is unavailable or the change is
cross-cutting:

```sh
cargo test --workspace --locked --offline
cargo test --workspace --locked --offline --release
```

The Linux hygiene job also checks unused dependencies with `cargo-machete 0.9.1`,
spelling with `typos-cli 1.35.8` and TOML formatting with `taplo-cli 0.10.0`.
Install those pinned versions with `cargo install --locked --version <version> <package>`
and run:

```sh
cargo machete
typos
taplo fmt --check
```

Use `taplo fmt` to apply TOML formatting. Keep spelling exceptions narrow in
`.typos.toml`; unused-dependency ignores require a reviewed reason in the manifest.
Hygiene runs on documentation changes too, independently of the Rust change detector.

CI marks the Rust jobs `skipped` without allocating their runners only when every
changed path is on the [detector's non-code allowlist](../scripts/ci-changes.py).
Code, tests, fixtures, embedded assets, dependencies, build settings, scripts and
Actions definitions run the checks; unknown paths, an empty diff, an unavailable
base or failed detection also run them. Renames include both old and new paths.
Pull requests on any base compare the merge result with its first parent; pushes
to `main` compare the complete pushed range with the event's `before` commit,
including code changes followed by documentation commits.

The workflow stays enabled, and the five required check names remain present:
`Clippy`, `Linux Tests (debug)`, `Linux Tests (release)`, `macOS Tests (debug)` and
`macOS Tests (release)`. They are separate jobs so skipping does not leave an
unexpanded matrix name in place of a required check. A job-level skip satisfies
GitHub's required-check policy; skipping the whole workflow with path filters can
leave checks pending. Manual dispatch always runs the standard lanes; its inputs
enable the additional native lanes and history measurement. The optional Linux
ARM64 lane retains the complete suite in both profiles. When changing this policy,
audit Rust includes and file reads before widening the allowlist, then run:

```sh
python3 scripts/test-ci-changes.py
actionlint .github/workflows/ci.yml .github/workflows/ci-focused.yml
```

Linux terminal tests need Bubblewrap and util-linux `script`, with user and
network namespaces enabled. On macOS, set `TMPDIR=/private/tmp` for test
invocations; the ordinary temporary-path aliases fail private-state admission.

[Native test and CI reproduction notes](../agents/references/testing-details.md#local-verification-and-ci)
explain additional prerequisites, fixture isolation and activation of checks
ignored locally. A targeted or default local run is not the complete native or release suite; it does not establish platform support. Live Provider checks require separate deliberate usage
authorization; an ignored check is not passing evidence.
