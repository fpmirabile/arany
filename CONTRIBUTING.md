# Contributing to Arany

Bug reports, focused fixes, documentation improvements and native platform
testing are welcome. You do not need to understand the whole harness to help.

## Choose a contribution

For a bug, start with a small reproduction and the behavior you expected. For
a feature or a substantial redesign, open an issue describing the problem and
proposed outcome before investing in a large implementation. Small fixes and
documentation corrections can go straight to a pull request.

Check existing issues and pull requests for related work. The
[documentation map](./docs/README.md) locates product contracts and architecture;
[next steps](./NEXT_STEPS.md) lists known gaps and pending verification.

## Get started

1. Fork the repository, clone your fork and create a focused branch from `main`.
2. Follow the [development setup](./docs/development.md#get-started) to install
   prerequisites and run the project.
3. Find the behavior you want to change in the relevant
   [product spec](./docs/specs/README.md) and inspect nearby code and tests.

Keep repository content in English. Prefer a small, complete change that reuses
existing code. Consider Linux, macOS and Windows when platform behavior is
involved, and preserve security, accessibility and resource limits. Update the
owning contract when intentionally changing observable behavior.

## Verification

Run the applicable [development checks](./docs/development.md#verification).
Documentation changes need prose and link review; they do not require rebuilding
Rust. For code changes, cover the affected behavior and meaningful failure path
through the existing tests where possible.

Tell reviewers which checks you ran and on which OS. If a check needs a service,
account or platform you do not have, describe the missing verification. An
ignored test or a successful compile does not establish native behavior.

## Issues

Use the [bug report](./.github/ISSUE_TEMPLATE/bug_report.md) or
[improvement/task](./.github/ISSUE_TEMPLATE/improvement.md) template. Include the
steps to reproduce, expected and actual behavior, and relevant OS, terminal and
Arany version. A small example is more useful than an entire conversation log.
Remove credentials and private task data from commands, logs and screenshots.
Use `unknown` when you cannot establish a fact.

## Pull requests

Open a pull request against `fpmirabile/arany:main` using the
[PR template](./.github/pull_request_template.md). Explain the problem, resulting
behavior and verification, and link the related issue when there is one. Keep
each PR focused so reviewers can assess it independently.

Respond to review feedback and resolve conversations before merging. The
[CI checks](./.github/workflows/ci.yml) must pass; a maintainer may need to approve
CI for a first-time contributor. Report limitations rather than claiming checks
you could not run.

If you use a coding agent, have it start with [AGENTS.md](./AGENTS.md). Review its
changes and verification before submitting, just as you would your own work.
