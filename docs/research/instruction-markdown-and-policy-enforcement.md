# Instruction Markdown and deterministic policy enforcement

**Status:** research and architectural recommendation  
**Research date:** 2026-09-28  
**Question:** How should the harness discover and compose repository instruction Markdown, prefer `AGENTS.md` with `CLAUDE.md` as a fallback, and turn selected security declarations into deterministic enforcement without treating arbitrary prose as policy?

## Reading guide

This report labels claims deliberately:

- **Fact**: directly supported by a cited primary source such as official product documentation or a first-party specification.
- **Inference**: derived from facts, but not guaranteed by a cited product.
- **Recommendation**: the proposed contract for this repository.
- **Gate**: behavior that must pass implementation and adversarial tests before the project may claim it as a guarantee.

The word **instruction** means model-facing guidance. The word **policy** means typed input to a deterministic authorization decision. They may share a Markdown container, but they are not the same mechanism.

## Executive conclusion

`AGENTS.md` and `CLAUDE.md` are useful interoperability formats, but their free-form text is prompt context. It cannot guarantee that an effect is blocked. Anthropic says this explicitly: its instruction files are context rather than enforced configuration and recommends hooks or managed settings for blocking actions. GitHub likewise warns that AI may not follow custom instructions identically every time. [[Claude Code memory](https://code.claude.com/docs/en/memory)] [[GitHub Copilot response customization](https://docs.github.com/en/copilot/concepts/prompting/response-customization)] **Fact**

The requested filename behavior is feasible and already has a documented precedent. OpenCode's main documentation selects project `AGENTS.md` and uses `CLAUDE.md` only when `AGENTS.md` is absent. Codex can be configured similarly by adding `CLAUDE.md` to `project_doc_fallback_filenames`, although that is not Codex's default. [[OpenCode rules](https://opencode.ai/docs/rules/)] [[Codex `AGENTS.md`](https://learn.chatgpt.com/docs/agent-configuration/agents-md)] **Fact**

**Recommendation:** define the harness contract as follows:

1. The client supplies an explicit workspace-root handle and initial working directory.
2. For every directory from workspace root to the applicable target directory, the resolver selects exactly one file: `AGENTS.md` when it exists; otherwise `CLAUDE.md`; otherwise none. It never implicitly loads both in one directory.
3. The selected free-form Markdown is injected as provenance-labelled model guidance in root-to-leaf order.
4. Only an exact, versioned fenced block named `harness-policy` is parsed as policy. Natural-language phrases such as “never access the database” remain guidance and may trigger a lint suggestion, but never become authority through an LLM or classifier.
5. Repository policy is **restrict-only**. It may change `allow` to `ask` or `deny`, reduce resource scopes, and lower limits. It can never grant a capability or widen an admin, user, or run profile.
6. Policy from every applicable scope is merged by intersection. Text order and “override” language cannot erase a security restriction.
7. The resolver produces an immutable, content-addressed `InstructionSnapshot`. The deterministic policy compiler consumes its typed restrictions; the separate guard enforces the compiled result at every effect boundary.
8. Missing files are normal. Ambiguous paths, unreadable selected files, invalid policy blocks, unsupported policy versions, budget exhaustion, path races, or unavailable enforcement cause effectful execution to fail closed.

The module should therefore be **inside the core architecture but not inside the trusted OS enforcer**. An in-process instruction resolver owns discovery, scope, imports, parsing, and provenance. Domain types represent restrictions. The policy compiler intersects them with trusted profiles. The separate guard accepts only the compiled policy digest and enforces it. Markdown parsing does not belong in the privileged guard process.

## 1. What the ecosystem actually standardizes

### 1.1 `AGENTS.md` is an open Markdown convention, not an authorization schema

The Agentic AI Foundation's `AGENTS.md` site describes the file as a README-like place for agent context, says there are no required fields, and recommends nested files whose closest scope takes precedence. It also says explicit user chat prompts override file instructions. [[`AGENTS.md` open format](https://agents.md/)] **Fact**

**Inference:** interoperability currently covers a predictable filename and human-readable guidance, not a common parser, merge algebra, trust model, or security policy language. A harness that wants deterministic guarantees must define those parts itself and version them.

### 1.2 OpenAI Codex

Codex builds its instruction chain once per run or TUI session. At global scope it selects `AGENTS.override.md` before `AGENTS.md`. At project scope it walks from the project root to the current working directory and selects at most one file per directory in this order: `AGENTS.override.md`, `AGENTS.md`, then configured fallback names. It concatenates selected files root to leaf, with closer guidance later. Empty files are skipped. [[Codex `AGENTS.md`](https://learn.chatgpt.com/docs/agent-configuration/agents-md)] **Fact**

Codex's documented default combined project-guidance limit is 32 KiB, configured with `project_doc_max_bytes`. The advanced-configuration page describes the same knob as the amount read from each `AGENTS.md`, so the official wording is inconsistent about aggregate versus per-file semantics. [[Codex `AGENTS.md`](https://learn.chatgpt.com/docs/agent-configuration/agents-md)] [[Codex advanced configuration](https://learn.chatgpt.com/docs/config-file/config-advanced)] **Fact**

**Gate:** do not claim byte-for-byte Codex compatibility without pinning a Codex version and testing its actual truncation behavior.

Codex supports arbitrary fallback filenames. Configuring:

```toml
project_doc_fallback_filenames = ["CLAUDE.md"]
```

makes its per-directory search prefer `AGENTS.md` and then try `CLAUDE.md`. Codex does not document Claude-style `@path` expansion for `AGENTS.md`. [[Codex `AGENTS.md`](https://learn.chatgpt.com/docs/agent-configuration/agents-md)] **Fact**

Codex separates guidance from controls. Its `.rules` files make argv-prefix decisions for commands requested outside the sandbox and select the most restrictive matching result (`forbidden` over `prompt` over `allow`). Project-local rules load only from a trusted project configuration layer. The OS sandbox and approval policy form separate protection layers. [[Codex rules](https://learn.chatgpt.com/docs/agent-configuration/rules)] [[Codex configuration basics](https://learn.chatgpt.com/docs/config-file/config-basic)] [[Codex approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security)] **Fact**

**Inference:** Codex does not treat free-form `AGENTS.md` text as an effect-level security boundary.

### 1.3 Anthropic Claude Code

Claude Code loads managed, user, project, and local instruction sources. For `CLAUDE.md`, it loads files from the filesystem root through the current working directory at launch and discovers descendant files when it reads inside those directories. Files are concatenated rather than structurally merged; a closer file appears later, but Anthropic warns that conflicting instructions may be followed arbitrarily. [[Claude Code memory](https://code.claude.com/docs/en/memory)] **Fact**

Current Claude Code also supports `AGENTS.md`. Its default is the **inverse** of the policy requested for this harness: if a project `CLAUDE.md`, `.claude/CLAUDE.md`, or `CLAUDE.local.md` exists in the current directory or an ancestor, Claude reads those and suppresses direct `AGENTS.md` loading. It reads `AGENTS.md` only when those project/local Claude files are absent, unless the user configures both formats. Direct `AGENTS.md` support requires Claude Code 2.1.277 or later. [[Claude Code memory: `AGENTS.md`](https://code.claude.com/docs/en/memory#agentsmd)] **Fact**

Claude imports `@path` references outside code spans and fenced code blocks. Relative imports resolve from the containing file; absolute imports are accepted; recursive imports stop after four hops. Project imports that resolve outside the working directory require approval the first time. [[Claude Code memory: imports](https://code.claude.com/docs/en/memory#import-additional-files)] **Fact**

Claude recommends keeping a `CLAUDE.md` below 200 lines for adherence. A file up to 4 MiB loads in full and a larger file is skipped. The 200-line/25-KiB startup limit belongs to auto-memory's `MEMORY.md`, not `CLAUDE.md`. [[Claude Code memory](https://code.claude.com/docs/en/memory)] **Fact**

Most importantly, Anthropic states that `CLAUDE.md` and auto-memory are context, not enforced configuration. Its managed-settings guidance assigns tool/path blocking to `permissions.deny` and isolation to `sandbox.enabled`; `CLAUDE.md` remains behavioral guidance. [[Claude Code memory](https://code.claude.com/docs/en/memory)] **Fact**

### 1.4 OpenCode

OpenCode's main rules documentation provides the closest precedent for the requested fallback. Project `AGENTS.md` applies to its directory and descendants, global `~/.config/opencode/AGENTS.md` applies across projects, project `CLAUDE.md` is used when no project `AGENTS.md` exists, and global `~/.claude/CLAUDE.md` is used when no OpenCode global file exists. If both names exist, only `AGENTS.md` is used. [[OpenCode rules](https://opencode.ai/docs/rules/)] **Fact**

That documentation does not automatically expand `@` references in `AGENTS.md`; additional local globs and remote URLs come from `opencode.json`. Remote instructions have a five-second fetch timeout. [[OpenCode rules](https://opencode.ai/docs/rules/)] **Fact**

OpenCode V2 documents different semantics: it recognizes only `AGENTS.md`, does not fall back to `CLAUDE.md`, discovers descendant instructions just in time, and adds changes to ambient instruction files to session history before the next prompt. [[OpenCode V2 instructions](https://opencode.ai/v2/docs/instructions)] **Fact**

**Inference:** instruction discovery changes across harness versions. Compatibility should be an explicit versioned profile, not undocumented behavior that silently changes with dependencies.

### 1.5 Gemini CLI

Gemini CLI loads a global context file, workspace and ancestor files, and just-in-time descendant context when a tool accesses a path, bounded by a trusted root. It can configure one or multiple context filenames, including `AGENTS.md`. [[Gemini CLI project context](https://geminicli.com/docs/cli/gemini-md/)] **Fact**

Gemini's import processor supports relative and absolute `@file` paths, detects cycles, checks paths against allowed directories, ignores import syntax inside code regions, and defaults to five levels of import depth. [[Gemini CLI memory import processor](https://geminicli.com/docs/reference/memport/)] **Fact**

Gemini also has a separate TOML policy engine with `allow`, `deny`, and `ask_user` decisions. It evaluates tool calls outside the context-file mechanism. The current documentation warns that its workspace policy tier is not functional, which is another reason to verify implementation rather than infer enforcement from file presence. [[Gemini CLI policy engine](https://geminicli.com/docs/reference/policy-engine/)] **Fact**

### 1.6 GitHub Copilot

GitHub documents repository-wide, path-specific, and agent instruction files. Multiple `AGENTS.md` files can exist in a repository, and the nearest file takes precedence; a single root `CLAUDE.md` or `GEMINI.md` is an alternative on the documented GitHub surface. [[GitHub repository custom instructions](https://docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/add-custom-instructions/add-repository-instructions)] **Fact**

GitHub explicitly says custom instructions are subject to nondeterministic model behavior. [[GitHub Copilot response customization](https://docs.github.com/en/copilot/concepts/prompting/response-customization)] **Fact**

### 1.7 Comparison

| Harness | Initial discovery | Descendant scope | Alternate filename behavior | Imports | Security status |
|---|---|---|---|---|---|
| Codex | Global, then project root to CWD | Not beyond launch CWD in the documented initial chain | Configurable per-directory fallback; `CLAUDE.md` is possible but not default | No `@` import contract documented | Guidance; sandbox, approvals, and `.rules` are separate |
| Claude Code | Managed/user, then ancestors to CWD | Lazy on file read | By default, project Claude files suppress `AGENTS.md`; configurable modes exist | Native `@path`, four hops, external-import approval | Explicitly context, not enforcement |
| OpenCode main | Local traversal plus global | Project directory scope | `AGENTS.md` first, `CLAUDE.md` fallback | Config-driven extras; no automatic `@` parsing | Included in model context |
| OpenCode V2 | Global and workspace chain | Lazy on exploration | `AGENTS.md` only | Config instruction array not active in V2 | Privileged model instruction values |
| Gemini CLI | Global and workspace/ancestors | Just in time to trusted root | Configurable filename list | Native `@file`, cycle/path/depth checks | Separate TOML policy engine |
| GitHub Copilot | Product-surface dependent | Nearest `AGENTS.md`; path-specific instruction files | Root alternatives documented | Surface-dependent | Explicitly nondeterministic guidance |

**Recommendation:** imitate concepts, not undocumented quirks. Use OpenCode's filename preference, Codex's one-file-per-directory chain, Gemini/OpenCode's just-in-time descendant awareness, Claude/Gemini's bounded import lessons, and the vendors' consistent separation between guidance and enforcement.

## 2. Proposed instruction-discovery specification

### 2.1 Inputs and trust root

Every run receives:

```text
WorkspaceRootHandle
InitialDirectoryHandle
InstructionProfileVersion
TrustedGlobalConfigRoot (optional)
InstructionLimits
```

**Recommendation:** the client must select and authorize the workspace root. The resolver must not independently walk to the filesystem root or guess authority from a `.git` directory. Git markers may help a client suggest a root, but they do not grant filesystem authority.

The initial directory must be the workspace root or a descendant verified by handle-relative traversal. A mutating or effectful run without a valid root fails. A read-only conversation may continue without repository instructions only if the UI states that no workspace policy is active.

### 2.2 Exact per-directory selection

For each directory `D` in the path from workspace root through the applicable scope directory:

```text
if exact entry D/AGENTS.md exists:
    select D/AGENTS.md
else if exact entry D/CLAUDE.md exists:
    select D/CLAUDE.md
else:
    select nothing for D
```

This is **existence fallback**, not “first non-empty file.” An empty `AGENTS.md` still exists and shadows `CLAUDE.md`. This follows the user's requested wording exactly and avoids different fallback results based on content decoding. The diagnostic surface should warn about an empty selected file and a shadowed fallback.

Only exact ASCII filenames participate. On case-insensitive filesystems, ambiguous aliases or a directory entry whose preserved spelling is not exact cause `AmbiguousInstructionName`; the resolver does not guess.

The resolver does not support `AGENTS.override.md`, `AGENTS.local.md`, `.claude/CLAUDE.md`, or other compatibility names in the base `harness-v1` profile. A future named compatibility profile may add them without changing this contract.

If `AGENTS.md` exists but is unreadable, oversized, not a regular file, or invalid UTF-8, the resolver fails. It must not silently fall back to `CLAUDE.md`, because “invalid” is not “absent” and fallback would make a shadowing attack possible.

### 2.3 Scope and order

Selected guidance is ordered broad to specific:

```text
trusted global guidance, if enabled
workspace-root selected file
each selected ancestor file
target-directory selected file
```

Later text is more specific for model guidance, but this order does not create a security override. The prompt assembler wraps every source in a typed envelope containing source ID, normalized workspace-relative path, scope, and digest. It must not concatenate content into an unlabelled blob.

For the initial model turn, the target is the initial working directory. Before a later tool targets another path, the resolver computes the chain from workspace root to that target's containing directory **before the target is read or modified**. Descendant instructions discovered by that trusted lookup may then be added to the next model turn.

For policy, just-in-time model delivery is insufficient. The policy compiler resolves applicable restrictions before authorizing the effect:

- `fs.read`, `fs.write`, and artifact effects use the canonical target path's directory scope.
- `process.exec` uses its canonical working-directory scope plus every declared filesystem resource scope.
- `tool.invoke` and `mcp.invoke` use the tool's registered owner scope and the target-resource scope when present.
- `db.*`, `net.*`, `secret.*`, and `memory.*` use the run scope and the scope of the process or tool that requested them.
- An effect spanning several scopes receives the intersection of all applicable policies.

A native process with broad directory access can cross descendant scopes without another harness tool call. Therefore the harness may claim nested-policy enforcement for that process only if it either:

1. eagerly discovers all instruction scopes inside every granted filesystem subtree and compiles the exclusions into the sandbox, or
2. grants only exact files/directories whose policy chains were already resolved.

If the platform backend cannot enforce the resulting holes and subtrees, strict mode refuses the process grant. **Gate**

### 2.4 Snapshot timing

Discovery produces an immutable snapshot before the first model request and a new snapshot generation before any newly scoped effect. Each snapshot contains:

```text
snapshot_version
workspace_identity
ordered selected sources
source file identities and content digests
expanded guidance graph
compiled restriction inputs
limits and parser versions
selection and shadowing diagnostics
snapshot_digest
```

An edit does not silently mutate the policy for an already running process. Explicit reload or newly scoped discovery creates a new snapshot and a new grant generation. Existing descendants retain the restrictions installed when they launched and may only be narrowed, never widened in place.

## 3. Import specification

Cross-product import behavior is inconsistent: Claude and Gemini implement `@path`; Codex does not document it; OpenCode's main docs say it does not parse such references automatically. **Fact**

**Recommendation:** support a small, optional import grammar for guidance, not for policy:

```text
@./relative/path.md
@../shared/path.md
```

An import is recognized only when the trimmed line consists solely of `@` followed by an unquoted relative path. Imports inside code spans, fenced code blocks, HTML comments, headings, lists, or prose are literal text. Paths containing control characters, backslashes on Unix, platform device prefixes, URL schemes, `~`, or absolute roots are rejected.

Project instruction files may import only regular UTF-8 files that remain under the workspace root. Trusted global instructions may import only within the configured global instruction root. Imports across those roots require a separate user-managed allowlist and become a new snapshot; a repository can never request interactive permission to import arbitrary host files during unattended execution.

Imports expand inline for model guidance and retain their own provenance. Imported files do **not** activate `harness-policy` blocks in version 1. Authoritative repository restrictions must be visible directly in a selected `AGENTS.md` or fallback `CLAUDE.md`. This prevents a harmless-looking instruction file from hiding policy activation behind a transitive prose reference.

Recommended hard limits, configurable only by trusted admin or user configuration:

| Limit | Default | Failure |
|---|---:|---|
| Selected instruction file | 64 KiB | reject selected scope |
| Imported file | 64 KiB | reject snapshot |
| Aggregate expanded guidance | 256 KiB | reject snapshot; no silent truncation |
| Selected files | 32 | reject snapshot |
| Imported files | 32 | reject snapshot |
| Import depth | 4 | reject snapshot |
| Path components | 256 | reject path |
| Markdown nesting parsed for fences | 64 | reject file |

Cycles are detected by stable file identity and also by normalized path. Re-reading the same imported file deduplicates its content but records every import edge for diagnostics. Missing imports are errors, not comments inserted into the prompt.

No remote URL imports exist in version 1. Fetching remote instructions introduces network policy, cache freshness, identity, integrity, availability, and prompt-injection concerns that are unrelated to local instruction discovery.

## 4. Typed policy blocks inside Markdown

### 4.1 Why a typed block is required

The sentence “never access the database” is underspecified. It does not identify which databases, whether network is otherwise allowed, whether MCP tools count, whether credentials may be read, or whether an existing connection may be inherited. A model can interpret the sentence, but its interpretation cannot be the authorization decision.

**Recommendation:** the resolver recognizes at most one exact fenced block per selected file:

````markdown
```harness-policy
version = 1
mode = "restrict-only"

[[rule]]
decision = "deny"
effect = "db.connect"
resource = "db://**"

[[rule]]
decision = "deny"
effect = "secret.use"
resource = "secret-tag://database"

[[rule]]
decision = "deny"
effect = "net.connect"
resource = "net://**"

[[rule]]
decision = "deny"
effect = "ipc.connect"
resource = "unix://**"

[[rule]]
decision = "deny"
effect = "mcp.invoke"
resource = "mcp://*/database/**"

[[rule]]
decision = "deny"
effect = "memory.direct"
resource = "memory://**"

[limits]
max_processes = 24
max_output_bytes = 8388608
```
````

The example closes several database paths, but it is not by itself the complete “no DB” guarantee. The platform profile must also remove inherited database handles, database credentials, Unix/abstract sockets, container-daemon sockets, and network routes, as described in [the deterministic-protection report](./deterministic-harness-protection.md).

### 4.2 Version 1 grammar

The block body is strict TOML decoded into a closed schema:

- `version` must equal `1`.
- `mode` must equal `restrict-only` for repository sources.
- A `rule` has exactly `decision`, `effect`, `resource`, and optional `reason`.
- Repository `decision` accepts only `deny` and `ask`. It never accepts `allow`.
- `effect` is a member of the harness's versioned effect catalog.
- `resource` is parsed by the selector type registered for that effect. Version 1 has component wildcards only and no regular expressions.
- `limits` accepts only named bounded integer fields defined by the schema.
- Unknown keys, duplicate keys, duplicate blocks, invalid effect/resource pairs, numeric overflow, unsupported versions, and unterminated `harness-policy` fences are errors.
- Environment interpolation, command substitution, anchors, aliases, templates, conditionals, and executable expressions do not exist.
- Markdown `@` imports do not contribute policy blocks.

The parser returns normalized Policy IR; it never asks a model what a field means. A formatter can produce a canonical representation whose digest is stable across whitespace and comments.

### 4.3 Merge algebra

Policy provenance tiers are:

```text
managed/admin hard constraints
          intersection
user hard constraints and selected protection profile
          intersection
root-to-leaf repository restrictions
          intersection
run request and approval grant
```

For decisions, define the restriction lattice:

```text
deny < ask < allow
```

The effective result is the greatest restriction, equivalently the minimum in that ordering. Resource sets intersect. Numeric maxima take the minimum. Boolean permissions use logical AND. A missing repository rule is the identity element and cannot reopen an upstream deny.

Root and leaf policy blocks are therefore order-independent for security. A leaf may narrow a root rule but cannot override it. A user approval may move an `ask` request within the already authorized maximum, but cannot bypass a `deny` from any active source.

Repository policy is untrusted input but restrict-only. A malicious repository can make itself inconvenient or impossible to operate on by denying effects, but it cannot acquire host authority. The user may start a separately labelled run that ignores repository policy only through an explicit trusted profile decision made before execution; the UI and receipt must then state that repository guarantees are disabled.

### 4.4 Guidance and policy are two outputs

One selected Markdown source produces two independent values:

```text
InstructionDocument
├── ModelGuidance       # prose, headings, examples, imported guidance
└── PolicyRestrictions  # parsed only from exact typed block
```

The fenced policy block should be removed from ordinary model guidance and replaced with a short generated summary such as “Repository policy denies all network and database effects.” This prevents the model from treating policy syntax as something it can negotiate or rewrite in a tool argument.

A linter may detect phrases such as “never access production” and suggest an equivalent typed rule. That result is advisory until a human accepts a concrete block. Jev or another model may help generate or explain the block, but it cannot activate, grant, or widen policy.

## 5. Secure file resolution and TOCTOU behavior

Path-string validation followed by an ordinary reopen is insufficient because a path component can be replaced between check and use. **Recommendation:** resolve and read instruction files through directory handles rooted at the authorized workspace.

On Linux, an implementation can use `openat2` with `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS`, and `RESOLVE_NO_MAGICLINKS` for handle-relative traversal. The Linux man page documents these flags as restrictions on path resolution. [[Linux `openat2(2)`](https://man7.org/linux/man-pages/man2/openat2.2.html)] **Fact** Equivalent platform adapters must demonstrate the same property before claiming conformance.

Required behavior:

1. Open and pin the workspace-root directory handle after authorization.
2. Enumerate each path component relative to the pinned parent handle.
3. Refuse symlinks, junctions, reparse points, magic links, sockets, devices, FIFOs, and directories where a regular instruction file is expected.
4. Open the selected file once with no-follow semantics; derive metadata and content from that handle rather than reopening its path.
5. Enforce the byte bound during reading, before allocation grows beyond the limit.
6. Record stable file identity, size, modification metadata, and a cryptographic content digest.
7. Revalidate the parent/child relationship before issuing the snapshot. If the platform cannot do so safely, fail closed.
8. Pass bytes or immutable normalized IR forward, never an unchecked path for the guard to reopen.

Hard links are not treated as symlinks. Their shared identity is recorded, and the immutable content snapshot prevents a later modification from changing the current decision. A policy requiring exclusive link count may be added in a higher-assurance profile, but it is not portable enough for the base contract.

If an instruction file changes after snapshot creation:

- the current Policy IR and launched descendants do not widen;
- a new model turn receives a visible stale-snapshot diagnostic;
- the next effect that needs the changed scope either uses an explicit new snapshot generation or is denied;
- changing a policy never mutates an already issued grant silently.

## 6. Failure semantics and diagnostics

### 6.1 Fail-closed matrix

| Condition | Model-only conversation | Effectful operation |
|---|---|---|
| Neither filename exists | Continue with broader guidance and baseline policy | Continue only within baseline policy |
| `AGENTS.md` and `CLAUDE.md` both exist | Select `AGENTS.md`; report shadowed Claude file | Same |
| Selected `AGENTS.md` is empty | Load empty source; report warning; do not fall back | Same |
| Selected file unreadable or not regular | Continue only if user explicitly chooses no-workspace read-only mode | Deny |
| Guidance exceeds configured budget | Report exact source and limit | Deny; never silently omit policy-bearing files |
| Ordinary Markdown is malformed | Preserve as text when safely bounded | Policy unaffected unless the policy fence is ambiguous |
| Typed block invalid or version unsupported | Show parse diagnostic | Deny all effectful operations in its scope |
| Import missing, cyclic, escaping root, or over limit | Reject snapshot | Deny effects using the snapshot |
| File changes during resolution | Retry once with a fresh generation | Deny after bounded retry |
| Backend cannot enforce compiled restriction | Conversation may continue | `ProtectionUnavailable`; do not degrade |

### 6.2 User-visible manifest

The CLI, web client, and protocol should expose the same structured manifest:

```text
scope: packages/payments
selected: packages/payments/AGENTS.md
fallback_checked: false
shadowed: packages/payments/CLAUDE.md
source_digest: sha256:...
guidance_bytes: 4812
policy: valid harness-policy v1
restrictions: 6 rules, 2 limits
imports: 2 guidance-only files
snapshot: sha256:...
```

Diagnostics use stable codes such as:

- `instruction.selected.agents`
- `instruction.selected.claude_fallback`
- `instruction.shadowed.claude`
- `instruction.invalid.not_regular`
- `instruction.import.outside_root`
- `instruction.budget.exceeded`
- `policy.block.duplicate`
- `policy.schema.unknown_field`
- `policy.effect.unsupported`
- `policy.enforcement.unavailable`

Every deny or approval prompt cites the restriction's source path, scope, line range, normalized rule ID, and snapshot digest. Logs contain digests and reason codes by default, not raw instruction content.

## 7. Module placement

**Recommendation:** treat instruction support as one feature with responsibilities split along existing trust boundaries:

```text
client supplies workspace root and target
                    │
                    ▼
        instruction resolver (engine adapter)
 discovery · secure reads · imports · provenance
                    │
          InstructionSnapshot
              ┌─────┴─────┐
              ▼           ▼
       prompt context   policy compiler
       model guidance   pure intersection
                          │
                    CompiledPolicy
                          │
                          ▼
                  harness-guard process
             final validation and enforcement
```

Suggested ownership:

| Concern | Owner | Rationale |
|---|---|---|
| `InstructionScope`, source identity, normalized restriction types | Domain | Stable semantics without filesystem or Markdown dependencies |
| Discovery timing and active target scopes | Engine | Part of run/tool orchestration |
| Secure filesystem loading, Markdown/import parsing, provenance | `instructions` module/adaptor | Changeable format and OS I/O hidden behind one deep interface |
| Restriction intersection and validation | Policy module | Pure, deterministic, fuzzable logic |
| Prompt rendering | Provider/context adapter | Provider-specific role and token behavior stays outside domain |
| Effect mediation and OS restrictions | Separate guard | Different privilege, lifecycle, and platform dependencies |

Start `instructions` as an in-process private module, not a new service and not necessarily a separate crate. Promote it to a crate only when dependency control, reuse by multiple binaries, compile locality, or an independently versioned API justifies the boundary.

The guard must never parse Markdown or follow imports. Its input is closed Policy IR plus the snapshot digest, grant, and enforcement requirements. This keeps the privileged trusted computing base small.

## 8. Validation plan

### 8.1 Discovery contract tests

Use table-driven tests covering:

- no file, only `AGENTS.md`, only `CLAUDE.md`, and both names in one directory;
- an empty `AGENTS.md` shadowing a non-empty `CLAUDE.md`;
- root `AGENTS.md` plus nested `CLAUDE.md` fallback;
- root `CLAUDE.md` plus nested `AGENTS.md`;
- exact case, case aliases, non-UTF-8 names, and case-insensitive filesystems;
- workspace root equal to CWD and CWD several levels below root;
- attempts to traverse above the workspace;
- multi-target operations whose scopes differ;
- deterministic source order and snapshot digest.

### 8.2 Import tests

- relative sibling and parent imports that stay in the root;
- absolute, home-relative, URL, network, and device paths rejected;
- cycles by path and by file identity;
- duplicate imports and stable deduplication;
- depth, file-count, per-file, and aggregate-byte boundaries at `limit - 1`, `limit`, and `limit + 1`;
- import-like text inside inline code, fences, comments, lists, and prose remains literal;
- policy fences inside imported guidance are ignored with a diagnostic;
- missing or unreadable import fails the snapshot.

### 8.3 Policy parser and algebra tests

- golden parse/canonicalization/digest fixtures;
- unknown and duplicate fields, unsupported versions, overflow, invalid UTF-8, and unterminated fences;
- property test: merge is associative, commutative, and idempotent;
- property test: adding a repository restriction never widens the effective capability set;
- property test: reordering repository sources never changes effective security policy;
- approval cannot cross any deny or maximum scope;
- no repository syntax can produce an `allow` grant;
- fuzz Markdown fence detection, TOML decoding, selectors, and diagnostics.

### 8.4 Filesystem adversarial tests

- selected file is a symlink, intermediate directory symlink, junction/reparse point, FIFO, socket, or device;
- symlink target swaps between lookup and read;
- parent directory renamed and replaced during discovery;
- file replaced after metadata read but before hashing;
- hard-linked file modified after snapshot creation;
- extremely deep paths and long components;
- concurrent edits produce either one coherent snapshot or a bounded failure, never mixed content;
- platform adapter reports unsupported no-follow semantics and strict mode refuses.

### 8.5 End-to-end enforcement tests

Create a root `AGENTS.md` with a valid strict no-database block and verify that all of these fail before the effect occurs:

- a native database client;
- an arbitrary script implementing the database protocol;
- a connection through a Unix or abstract socket;
- an MCP database tool;
- a tool that reads a database credential;
- use of an inherited connection or descriptor;
- a container or Docker socket used to reach the database;
- direct access to the harness memory database;
- a child and grandchild process attempting the same effects.

Then add a nested `CLAUDE.md` fallback with stricter limits and verify it applies only where no same-directory `AGENTS.md` exists. Add a same-directory `AGENTS.md` and verify the Claude file becomes shadowed. Verify that neither a leaf policy nor a user approval can widen the root no-database restriction.

### 8.6 Compatibility fixtures

Maintain explicit fixtures for:

- `harness-v1` exact behavior described here;
- optional `codex-compatible-v1` discovery;
- optional `claude-compatible-v1` import behavior;
- optional `opencode-compatible-v1` behavior.

Compatibility profiles change discovery and prompt assembly only. They do not change the restrict-only policy algebra or the guard's enforcement requirements.

## 9. Recommended implementation sequence

1. Define `InstructionScope`, `InstructionSource`, `InstructionManifest`, `InstructionSnapshot`, and closed Policy IR in the domain vocabulary.
2. Implement the pure filename-selection and root-to-target chain logic against an abstract directory-handle interface.
3. Implement one Linux secure loader and adversarial path tests.
4. Add bounded guidance imports without policy activation.
5. Add the strict `harness-policy` v1 parser, canonical formatter, and merge property tests.
6. Feed normalized guidance to the prompt assembler with source envelopes.
7. Feed compiled restrictions and snapshot digest to the guard; reject unsupported enforcement.
8. Ship `harness instructions explain <target>` and machine-readable protocol diagnostics before enabling policy blocks by default.
9. Run the complete no-database conformance suite.
10. Add macOS and Windows loaders only after they pass the same path and race tests.

## 10. Final recommendation

Adopt `AGENTS.md` as the canonical repository instruction filename and `CLAUDE.md` as its exact per-directory fallback. This gives the project a portable default, preserves useful Claude-oriented repositories, and matches the user's desired mental model.

Do not market Markdown prose as enforcement. Make the distinction visible in the product:

```text
Markdown prose                 → model guidance
typed harness-policy fence     → deterministic restriction input
compiled policy + grant        → guard decision
OS/tool/MCP/memory adapters     → effect enforcement
attestation + receipt           → evidence of what was effective
```

This design lets a repository state operational conventions naturally while also expressing auditable, machine-checkable restrictions in the same discoverable file. The important security property is not that the model “understood” the Markdown. It is that every relevant effect is mediated by a guard using a closed policy compiled from a reproducible, provenance-rich snapshot.

## Primary sources

- [OpenAI: Custom instructions with `AGENTS.md`](https://learn.chatgpt.com/docs/agent-configuration/agents-md)
- [OpenAI: Advanced configuration](https://learn.chatgpt.com/docs/config-file/config-advanced)
- [OpenAI: Configuration basics](https://learn.chatgpt.com/docs/config-file/config-basic)
- [OpenAI: Rules](https://learn.chatgpt.com/docs/agent-configuration/rules)
- [OpenAI: Agent approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security)
- [Anthropic: How Claude remembers your project](https://code.claude.com/docs/en/memory)
- [OpenCode: Rules](https://opencode.ai/docs/rules/)
- [OpenCode V2: Instructions](https://opencode.ai/v2/docs/instructions)
- [Gemini CLI: Project context](https://geminicli.com/docs/cli/gemini-md/)
- [Gemini CLI: Memory import processor](https://geminicli.com/docs/reference/memport/)
- [Gemini CLI: Policy engine](https://geminicli.com/docs/reference/policy-engine/)
- [GitHub: Adding repository custom instructions](https://docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/add-custom-instructions/add-repository-instructions)
- [GitHub: Response customization](https://docs.github.com/en/copilot/concepts/prompting/response-customization)
- [`AGENTS.md` open format](https://agents.md/)
- [Linux man-pages: `openat2(2)`](https://man7.org/linux/man-pages/man2/openat2.2.html)
