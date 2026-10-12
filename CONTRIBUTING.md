# Contributing to Arany

Bug reports, focused fixes, documentation improvements and native platform testing are welcome.

## Get started

1. Fork the repository and create a focused branch from `main`.
2. Install Rustup and native build tools, including a C compiler. The repository selects its [pinned toolchain](./rust-toolchain.toml).
3. From the checkout, run:

```sh
cargo fetch --locked
cargo run --locked
```

Keep repository content in English. For behavior changes, update the relevant [product spec](./docs/specs/README.md). The [documentation map](./docs/README.md) helps you find it.

## Verification

For documentation-only changes, review the text, local links and anchors; no Rust build is needed. For development skill changes, follow the [skill workflow](./agents/skills/README.md#update-a-local-skill).

For code changes, run the applicable checks before submitting:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline
cargo test --workspace --locked --offline --release
```

Linux terminal tests need Bubblewrap and util-linux `script`, with user and network namespaces enabled. On macOS, set `TMPDIR=/private/tmp` when running tests.

The [CI workflow](./.github/workflows/ci.yml) checks Linux and macOS. See [native test and CI reproduction notes](./agents/references/testing-details.md#local-verification-and-ci) for checks ignored locally and platform-specific setup.

## Issues

Use the [bug report](./.github/ISSUE_TEMPLATE/bug_report.md) or [improvement/task](./.github/ISSUE_TEMPLATE/improvement.md) template, including when creating issues through the CLI or API. Each template includes guidance for humans and agents. Keep the report in English and TL;DR: lead with the point, aim for 1–3 short bullets per section, and link or collapse longer evidence. Mark missing facts as `unknown`; sanitize commands, logs and screenshots before sharing.

## Pull requests

Open a pull request against `fpmirabile/arany:main`. Explain the problem, what changed and how you verified it, including checks you could not run. Resolve review conversations and pass the required checks before merging. First-time contributors may need a maintainer to approve CI.

Use the [PR template](./.github/pull_request_template.md), including when publishing through the CLI or API. Follow the same brief writing style as issues, report only checks actually run, and use `N/A` with a short reason for sections that do not apply.

If you are an AI agent, read [AGENTS.md](./AGENTS.md) before making changes.
