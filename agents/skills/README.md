# Project skills

Maintain Arany's own development skills here. `rust-clean-code` guides Rust engineering; `arany-terminal-design` guides terminal design and interaction. These files are the editable sources. `.agents/skills/` and agent-specific skill directories are generated, gitignored installations.

The repository's single manifest is [`skills-lock.json`](../../skills-lock.json). Local entries use portable paths under `./agents/skills`; external entries retain their existing sources. Skills guide the development agent and do not register commands or tools in the Arany executable.

## Install for a harness

Run from the repository root. The following commands use the verified Skills CLI version and disable its telemetry:

```bash
DISABLE_TELEMETRY=1 npx --yes skills@1.7.0 add ./agents/skills --agent codex --yes
```

Select another supported harness with `--agent`, or provide multiple targets:

```bash
DISABLE_TELEMETRY=1 npx --yes skills@1.7.0 add ./agents/skills --agent codex claude-code --yes
```

This installs both local skills and regenerates their content hashes in the existing lockfile. Codex uses `.agents/skills`; the second target receives links under its own skill directory. The source folders remain unchanged. Keep the Rust skill's license and attribution with its references.

## Restore the lockfile

```bash
DISABLE_TELEMETRY=1 npx --yes skills@1.7.0 experimental_install
```

In this CLI version, `install` is an alias for `add` and requires a source; `experimental_install` restores the lockfile. Restoration installs into the shared `.agents/skills` directory; it does not select agent-specific destinations. Run the targeted local installation above when those links are needed. Restoration also fetches the external skills recorded in the lockfile: it is not an offline or immutable-revision guarantee for those existing entries.

## Update a local skill

Edit its source under `agents/skills`, then rerun the targeted installation to refresh the installed copies and generated lock hashes. Review the source changes and corresponding lock entries together. Never treat edits to the ignored installed copies as durable repository changes.

When checking a move or installation, validate skill frontmatter and relative references, compare source and installed files, and verify restoration from a relocated checkout using its local lock entries. Changes to these instructions do not require rebuilding the Rust executable.
