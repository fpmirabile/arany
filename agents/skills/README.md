# Project skills

Maintain Arany's development skill sources here: `rust-clean-code` for Rust, `arany-terminal-design` for terminal interaction, and `clean-architecture-review` for architecture reviews. Apply each only when its task fits. [Arany's implementation priorities](../../AGENTS.md#product-and-implementation-priorities) require Ponytail for every implementation and code review; the local skills complement it.

`agents/skills/` contains editable, checked-in sources. `.agents/skills/` contains generated, gitignored installations discovered by Codex and other compatible agents; agent-specific directories can link to them. These are different directories, not alternative spellings. [`skills-lock.json`](../../skills-lock.json) is the single manifest, with portable local source paths. These skills guide development agents; they do not enable runtime Skills or Tool permissions in Arany.

## Install for a harness

Run from the repository root with Node.js/npm available. Use the reviewed Skills CLI version below. Disable telemetry for the invoking shell:

```sh
export DISABLE_TELEMETRY=1
```

In PowerShell, use `$env:DISABLE_TELEMETRY = "1"` instead. Then install the three local skills and pinned Ponytail:

```sh
npx --yes skills@1.7.0 add ./agents/skills --agent codex claude-code --yes
npx --yes skills@1.7.0 add https://github.com/DietrichGebert/ponytail/tree/e3ba2aa6f1e6f0bc4d69eb09c9f0d0a93af56156/skills/ponytail --skill ponytail --agent codex claude-code --yes
npx --yes skills@1.7.0 list
```

This creates `.agents/skills/`, links the selected skills under `.claude/skills/`, and refreshes their lock entries without changing source files. Select only `codex` or another supported agent as needed; use `--copy` where symlinks are unavailable. Confirm that all four skills appear and `.agents/skills/ponytail/SKILL.md` can be opened. If a running agent does not discover them, start a fresh session; the explicit Ponytail path remains readable in the current session.

On a fresh checkout, a lock entry does not mean its skill is installed. Restore a missing required skill before implementation; if installation is blocked, report the missing skill instead of claiming it was applied. Do not create a placeholder skill or silently fetch a different revision.

## Restore the lockfile

The manifest includes the three local skills, pinned Ponytail, Matt Pocock's adopted development workflows and `find-skills`. The scoped setup above installs the four core skills only. To restore the complete project set, set the telemetry variable above and run:

```sh
npx --yes skills@1.7.0 experimental_install
```

In this version, `install` aliases `add` and requires a source. `experimental_install` restores into shared `.agents/skills` without selecting agent-specific links. Use the scoped setup above when agent-specific links are needed. Existing Matt Pocock and `find-skills` entries retain their recorded sources and hashes but have no immutable `ref`; restoring them can resolve newer upstream content. Review and pin a revision when updating an external skill; a stored hash alone is not a revision pin.

Matt Pocock's skills are part of the project's adopted workflow. Preserve adopted manifest entries during setup and cleanup: absence from a generated installation directory does not establish that a skill is unused.

## Update a local skill

Edit its source under `agents/skills`, then reinstall only that skill, for example:

```sh
npx --yes skills@1.7.0 add ./agents/skills --skill rust-clean-code --agent codex claude-code --yes
```

Review source changes and generated lock entries together. Never maintain project changes only in ignored installed copies. Preserve the Rust skill's license and attribution. Keep descriptions specific to the task and load supporting references only when needed, following the [Agent Skills guidance](https://agentskills.io/skill-creation/best-practices).

When checking a move or installation, validate skill frontmatter and relative references, compare source and installed files, and verify restoration from a relocated checkout using its local lock entries. Changes to these instructions do not require rebuilding the Rust executable.

## Ponytail

[Ponytail](https://github.com/DietrichGebert/ponytail) is an external skill pinned by commit, path and hash in `skills-lock.json`. The setup command installs only the skill, not its upstream plugin or lifecycle hooks. Review source changes before updating the pinned revision and command. Keep Arany-specific application rules in [AGENTS.md](../../AGENTS.md#product-and-implementation-priorities), rather than modifying the ignored upstream installation.
