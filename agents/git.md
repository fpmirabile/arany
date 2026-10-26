# Git

Load before running a git command or proposing a git step in a plan.

## Authorization

- The user's standing instruction authorizes inspecting repository state and creating or switching to a dedicated branch for every new feature, before editing and without asking again.
- Other git operations require an explicit user request or an approved plan containing the step; that authorization does not carry into later plans or tasks.

## Forbidden

- Never force-push, including `--force-with-lease`.
- Never push directly to `main`; merge through a pull request.
- Never bypass the `main` ruleset (for example `gh pr merge --admin`): merge only after required checks and approvals pass. The admin bypass is reserved for the owner, by hand.
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
- When restoring a Git blob through a command wrapper that may compact output, use raw passthrough and verify the resulting blob hash against the source object before continuing. Display output is not file content.

## Conflicts

- Read both sides and reconstruct the intended result; never accept one side blindly.
- For code outside the current change, prefer the current `main` version and reapply the task-specific edit.
- For generated files and lockfiles, keep the authoritative input, rerun its generator, and review the regenerated result.
- Stop and ask when ownership or the correct resolution is unclear.

## Review before a PR

- Read the complete diff after the last edit; remove debug output, dead code, commented-out blocks and untracked TODOs, and confirm every changed line belongs to the approved task.
- Run the [repository checks](../docs/development.md#verification) after the final edit; [testing rules](./testing.md) own evidence selection and reporting.

## Pull requests

- When publishing is authorized under the rules above, open a PR once the branch has a reviewable unit; do not merge it unless the user asks.
- Use a short, imperative, scoped title without a trailing period.
- Use the [PR template](../.github/pull_request_template.md) for every PR, including CLI or API PRs; the publishing tool does not apply it. `Notes` is required when a [security-pass trigger](./security.md#security-pass-triggers) fires; passing tests are not a safety claim.
- For issues created through the CLI or API, use the [bug report](../.github/ISSUE_TEMPLATE/bug_report.md) or [improvement/task](../.github/ISSUE_TEMPLATE/improvement.md) template.
- Report skipped or blocked checks; do not claim checks that were not run.
- From a plan, fill the template's `Slices` line under [slice delivery](../planning/PLANNING.md#slice-delivery).
- After pushing, watch the PR's CI without waiting indefinitely.
- Stacked PRs are fine: base the branch on its parent, open the PR against that parent, and when GitHub retargets it after the parent merges, push or re-run the workflow so CI covers the retargeted merge.

## Rollback

- Revert merged work with `git revert` and a new PR.
- Preserve local work when correcting an unpushed mistake. Use a destructive reset only with explicit approval and a verified recovery path.
