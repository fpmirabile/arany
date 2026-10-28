# Git writes and remembered approvals

- Question: how can Arany commit, branch, push and open pull requests with peer-like usability while preserving explicit authority and honest recovery?
- Source-check date: 2026-10-21. Peer evidence is the official Claude Code and Codex CLI documentation as published on 2026-10-21; neither page exposes a stable document revision, so no version can be pinned, and the conclusions should be rechecked if those docs change. This is not a pinned peer release or implementation audit.
- Conclusion: typed local writes use the neutralized-view approach and receipted publication. Host push and PR creation require the separately approved isolation exception. Prefix matching must operate on a closed typed schema rather than authorize arbitrary Git options.
- Scope: evidence, alternatives and conflicts. [AGENTS.md guarantees](../../AGENTS.md#non-negotiable-guarantees), [typed Git writes](../specs/tools.md#typed-git-writes), [human-approved host Git](../specs/tools.md#human-approved-host-git), [typed prefix rules](../specs/tools.md#typed-prefix-rules) and [execution](../specs/execution.md) own the accepted decisions and current contracts. This report preserves rationale, grants no capability and establishes no native or live support.

## Peer evidence

### Claude Code

The [permissions documentation](https://code.claude.com/docs/en/permissions#bash) describes command-text glob matching, with deny before ask before allow. Compound commands are split; an allow must cover each subcommand, while a matching deny or ask applies to any subcommand, including nested commands. Remembering a compound approval saves separate rules. A fixed wrapper list is stripped; arbitrary environment runners are not. Broad Git patterns can also match configuration-bearing options; a restriction written for `git push` does not cover every equivalent spelling. These are documented behaviors, not independently tested parser claims.

The [sandbox documentation](https://code.claude.com/docs/en/sandboxing#protected-paths) protects Git configuration and hooks against writes while permitting other Git metadata writes, including shared metadata for linked worktrees. Content-scoped ask rules still prompt for sandboxed commands. Excluded commands leave filesystem and network isolation but retain the regular permission flow. The same page's [Git-over-SSH section](https://code.claude.com/docs/en/sandboxing#git-over-ssh-fails-with-the-sandbox-on) explicitly says Git network operations work inside the Linux/WSL2 sandbox once the host is allowed; macOS SSH has a proxy limitation and an exclusion workaround. Therefore, “Claude always runs Git writes on the host” is unsupported. No reviewed source establishes inert repository configuration for its host path.

### Codex CLI

The [rules documentation](https://learn.chatgpt.com/docs/agent-configuration/rules) describes exact argv-prefix `prefix_rule` matching and `forbidden > prompt > allow`. An allow permits execution outside the sandbox without prompting. The TUI persists remembered allows in the user rules layer; trusted project layers can also supply rules. Linear shell scripts containing plain words and supported separators may be split; scripts with expansions, redirection or control flow are treated conservatively as opaque wrapper invocations. This is documentation evidence; `execpolicy` implementation was not audited.

The [approval and security documentation](https://learn.chatgpt.com/docs/agent-approvals-security#protected-paths-in-writable-roots) makes `.git` recursively read-only under default `workspace-write`, including a gitdir pointer's resolved target. **Inference:** writes to that metadata require different authority from the default sandbox. This does not prove that every Codex Git write always uses the same host route or that host Git configuration is neutralized.

The shared usability pattern is remembered command authorization with restrictive rules taking precedence. The peers do not establish one universal isolation implementation. Arany's user-owned-only rules and mandatory human approval for typed host network writes are owner constraints, not claims of identical peer policy.

## Git and GitHub configuration risks

The [Git configuration reference](https://git-scm.com/docs/git-config) documents that `include`/`includeIf` imports other configuration, `core.sshCommand` substitutes the SSH command, `url.*.insteadOf` rewrites URLs, and `remote.*.pushurl` overrides push destinations and may name multiple destinations. `credential.helper` can execute a helper or shell command; `gpg.program` chooses an executable for signatures. **Inference:** setting only `-c core.hooksPath=/dev/null` does not neutralize these independent routes. A private configuration view plus explicitly admitted destination and authentication settings has a smaller authority surface than passing arbitrary repository config through.

The [GitHub CLI PR manual](https://cli.github.com/manual/gh_pr_create) says PR creation can also prompt to push or fork; explicit `--head` skips those behaviors. It also documents repository-derived base selection when `--base` is omitted. **Recommendation:** the typed adapter supplies explicit repository, head, base, title and body so approval covers one PR creation rather than undisclosed side effects. The [environment manual](https://cli.github.com/manual/gh_help_environment) lists token precedence, repository/host selection, and executable-bearing editor/browser/pager variables. Authentication and environment admission therefore need a deliberate adapter contract.

## Verification limits

This investigation ran no Linux/macOS native write test, remote push, PR creation, credential access or peer executable experiment. Pending native macOS evidence is listed in [next steps](../../NEXT_STEPS.md#daily-use-first).
