# Git

Load before running a git command or proposing a git step in a plan.

## Authorization

- The user's standing instruction authorizes inspecting repository state and creating or switching to a dedicated branch for every new feature. Do this before editing, without asking again.
- Other git operations require an explicit user request or an approved plan containing the step; that authorization does not carry into later plans or tasks.

## Forbidden

- Never force-push, including `--force-with-lease`.
- Never push directly to `main`; merge through a pull request.
- Never skip hooks or signing unless the user explicitly authorizes it for a specific commit.
- Never use destructive resets or delete a branch containing unmerged work without explicit approval and a verified recovery path.

## Branches

- Create one branch per logical task: `feat/<short-desc>`, `fix/<short-desc>`, `chore/<short-desc>`, or `docs/<short-desc>`.
- Use kebab-case and branch from the latest `origin/main`; fetching that reference is covered by the standing branch instruction.
- Do not mix unrelated work into an existing branch.
- Preserve pre-existing staged and unstaged work. Do not stash, reset or switch away from it implicitly. Use an isolated checkout when practical; a bounded in-place documentation edit may remain unstaged for the user to separate, without committing unrelated work.

## Commits

- Use short, imperative English messages: `<type>(<scope>): <change>`.
- Keep one concern per commit when practical.
- Stage explicit paths. Do not use `git add .` or `git add -A`.
- Never commit `.env`, ignored files, credentials, private prompts, raw conversation data, or generated local review artifacts.
- Do not amend a pushed commit; add a new commit.

## Updating from main

- On a pushed branch, fetch and merge `origin/main`.
- Rebase only while the branch is strictly local. When uncertain, merge.

## Conflicts

- Read both sides and reconstruct the intended result; never accept one side blindly.
- For code outside the current change, prefer the current `main` version and reapply the task-specific edit.
- For generated files and lockfiles, keep the authoritative input, rerun its generator, and review the regenerated result.
- Stop and ask when ownership or the correct resolution is unclear.

## Review before a PR

- Read the complete diff after the last edit.
- Remove debug output, dead code, commented-out blocks, and untracked TODOs.
- Run the applicable [repository checks](../CONTRIBUTING.md#verification) after the final edit; documentation-only work follows its document and skill checks.
- For Rust, default to `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` once the workspace exists, unless repository commands replace them.
- Confirm every changed line belongs to the approved task.
- Confirm durable learnings were written to the authoritative documentation.

## Pull requests

- When publishing is authorized under the rules above, open a PR once the branch has a reviewable unit; do not merge it unless the user asks.
- Use a short, imperative, scoped title without a trailing period.
- The body contains `Summary`, `Verification`, `Security` when triggered, and `Documentation`.
- Report skipped or blocked checks. Do not claim checks that were not run.
- After pushing, report the current CI state without waiting indefinitely.

## Rollback

- Revert merged work with `git revert` and a new PR.
- Preserve local work when correcting an unpushed mistake. Use a destructive reset only with explicit approval and a verified recovery path.
