# Local coding tools

Arany can continue a primary AgentRun through file operations, commands, portable Skills and local stdio MCP tools. This is an explicit, offline local foundation, not unrestricted host shell access. The normal launch remains read-only. Interactive and headless Runs opt in with `--tools`; no paid compatibility check is added by this flag.

```sh
target/debug/arany --tools
target/debug/arany exec --tools --provider openai --model gpt-5.4 "Inspect and fix the selected code"
```

Select an account/model as usual. These commands can consume Provider usage when you submit work. Native OpenAI, Anthropic API and the consented ChatGPT adapter encode one strict semantic Tool proposal at a time; live tool-capable model compatibility remains unverified. Exact custom profiles currently reject `--tools`: their existing data-free conformance covers read-only outcomes, not this changed encoding.

## Private grants

Put an owner-only `tools.json` beside the admitted Session database, outside the Workspace. Its directory must be private (`0700`) and the file must be regular, single-link, no-follow, current-user-owned and `0600`. `--state-dir` selects this Session/configuration root; it does not move your saved account. Permissions are not encryption or protection from another process running as the same user.

A minimal file-only configuration is:

```json
{
  "version": 1,
  "workspace_paths": ["src", "Cargo.toml", "Cargo.lock"],
  "write": true,
  "commands": [],
  "skills": [],
  "mcp": []
}
```

Use exact, nonoverlapping relative files/directories. `"."`, absolute paths, traversal, links, mount crossings and special files are not admitted. StateRoot and the fixed account root must not overlap the Workspace or configured runtime resources. `.git`, `.env`/`.env.*`, `.aws`, `.ssh`, `.gnupg`, `.npmrc` and `.pypirc` path components are excluded. This is not a universal secret detector: review the files you select. Tool observations can be sent to the selected Provider and persisted in the private, unencrypted journal.

The Linux profile exposes the system `/usr` tree read-only as its declared runtime, including compiler/library data. Workspace, StateRoot and account roots must not overlap that tree: an otherwise private file inside a runtime mount would still be visible to a same-user payload. This base does not authorize mounting the user's home or a package cache.

The offline profile denies socket creation, including TCP, UDP and pathname/abstract Unix sockets, plus connect/bind/listen and destination-bearing sends. A private network namespace alone would not exclude a pathname socket in a visible runtime tree. Anonymous IPC socket pairs and pipes remain usable but cannot rebind or send to named destinations; descriptor/message passing with `sendmsg`/`sendmmsg` is unsupported. This is not a network-capable test profile.

JSON has a closed versioned schema: all six top-level fields are required; unknown and duplicate fields reject. There is no repository discovery, configuration interpolation, automatic installation or hidden executable lookup. Configuration is pinned for a Run; editing it does not cancel an already admitted Run. Changed pinned executable or resource contents reject dispatch rather than being silently trusted.

## Commands

Each `commands` entry declares a reviewed executable and its SHA-256, whether it is an interpreter, and any additional pinned input files:

```json
{
  "name": "bash",
  "executable": "/usr/bin/bash",
  "sha256": "REPLACE_WITH_THE_64_CHARACTER_LOWERCASE_SHA256",
  "interpreter": true,
  "inputs": []
}
```

The placeholder above is deliberately invalid. Resolve a system executable's real path and calculate its digest yourself, for example with `readlink -f /usr/bin/bash` and `sha256sum /usr/bin/bash`. Arany requires a no-follow, regular, single-link ELF input; a symlink such as `/usr/bin/python3` must be replaced with its resolved executable path. Updating an executable requires reviewing and changing its pin.

Additional inputs use `{"path":"/absolute/reviewed/file","destination":"relative/name","sha256":"<digest>"}`. They appear read-only under `/inputs/PROGRAM_ID/relative/name`. Executables are copied to `/programs/PROGRAM_ID`. Paths and digests are checked again for each fresh invocation. Descriptions and the `interpreter` declaration do not prove that arbitrary arguments or a script are safe; allowing a shell/interpreter authorizes its behavior inside the same OS-enforced boundary, including descendants using the read-only `/usr` runtime.

Commands receive an explicit argument array, a relative working directory, cleared environment, private `HOME`/temporary/Cargo directories, no terminal input, and bounded UTF-8 stdout/stderr. They use a fresh writable snapshot of only selected project paths. Empty directories and executable bits are preserved without set-ID bits. They see prior typed edits, but **their own file changes are discarded**. A compiler can write a binary under `/scratch` and execute it in the same command. No artifact auto-import, shared build cache, package download or background job survives a call. The current limits do not establish support for every large Rust build or language toolchain.

## Portable Skills

Each `skills` entry contains an explicit name, reviewed discovery description, absolute directory and exact relative file-to-SHA-256 map:

```json
{
  "name": "review",
  "description": "Review the selected project code",
  "directory": "/absolute/reviewed/skills/review",
  "files": {"SKILL.md": "<digest>", "references/checklist.md": "<digest>"}
}
```

The model initially sees only configured names/descriptions, then can request the pinned `SKILL.md` and listed resources lazily. `SKILL.md` needs YAML frontmatter with the matching string `name` and a nonempty string `description`, followed by the Markdown body. Quoted/folded scalars and CRLF are supported; ambiguous/duplicate or excessive metadata rejects. Local executable YAML tags reject; global YAML tags may be ignored inertly. Behavioral fields such as `allowed-tools`, hooks, shell directives, forks and installation instructions are guidance only, never grants. Skill scripts require an ordinary approved command/interpreter; requesting a Skill executes none of its body.

YAML parsing occurs only inside the killable Guard, not the credential-owning Engine. Skills are mounted read-only under `/skills/SKILL_ID`; unlisted resources cannot be loaded by the Skill tool.

## Local MCP

Each `mcp` entry pins a server executable, immutable input files, fixed startup arguments and allowed tool names:

```json
{
  "name": "local",
  "program": {
    "name": "python-mcp",
    "executable": "/absolute/resolved/python-executable",
    "sha256": "<digest>",
    "interpreter": true,
    "inputs": [{"path":"/absolute/reviewed/server.py","destination":"server.py","sha256":"<digest>"}]
  },
  "args": ["/inputs/python-mcp/server.py"],
  "tools": ["echo"]
}
```

The configured server has the same offline snapshot, runtime and descendant restrictions as commands, with no Provider/account credentials. This subset is intended for local data/computation tools. Servers needing external APIs, credentials, writable host state or package installation are not supported by this base.

Arany implements the **2025-11-25 stdio tools profile**: `initialize`, exact version/capability negotiation, `notifications/initialized`, bounded `tools/list` pagination, schema validation, one `tools/call`, then disconnect. It is not the newer 2026-07-28 stateless profile or an HTTP/OAuth adapter. Server-to-client ping works; sampling, roots and elicitation are unavailable and receive method-not-found responses. Notifications cannot trigger discovery, refresh permission or extend deadlines.

Discovery returns a digest of each admitted complete tool definition. A later call lists again and requires the selected digest; drift rejects before the call. Arguments must be an object encoded as a JSON string, satisfying the admitted input schema. Schemas use the supported Draft 2020-12 or Draft 7 subset, bounded linear regexes and offline compilation; references, `$id`, remote retrieval and unsupported dialects reject. Calls have no hidden retry. Output admits bounded text, inert resource links and legacy object `structuredContent`; a declared output schema is enforced for successful results. Links are not fetched. Embedded resources, images and audio are rejected. `isError` is recorded as an observed failure, not success.

## Limits and failure behavior

| Resource | Current ceiling / behavior |
|---|---|
| Private configuration | 64 KiB; 32 selected roots, 8 commands, 32 Skills, 4 MCP servers |
| Project snapshot | 32 MiB; 2,048 file/empty-directory entries; 8,192 scanned directory entries; depth 16 |
| File operation | 1 MiB input file; reads page at most 4,096 UTF-8 bytes; writes/edit strings at most 8 KiB |
| Runtime resources | 96 MiB aggregate copied executables/inputs/Skills; executable 32 MiB, extra input 1 MiB, Skill file 16 KiB |
| Processes | 512 MiB aggregate memory, no swap, group OOM termination, 64 processes/threads; 25% of one CPU |
| Private storage | Separate 64 MiB scratch and 64 MiB writable project tmpfs; no persistent generic-command changes |
| Time | Run 300 seconds; each unit at most 60 seconds, command capture 50 seconds, MCP dialogue 40 seconds; finite startup/cleanup and parent deadlines |
| Tool loop | 16 proposals/effects shared across primary planning/synthesis; model-step receipt ceiling 32; children retain their existing maximum of 8 one-call reasoning tasks |
| Tool context | 64 KiB catalog plus observations reserved before dispatch; result ceiling 16 KiB, narrowed to remaining space including JSON escaping |
| MCP | 128 KiB frame; 4 pages, 128 tools and 256 KiB catalog; 64 incoming server messages; bounded schema depth/nodes/regex; no reference fetch |

List/search return at most 64 rows and explicitly indicate possible truncation. Oversized requests/results otherwise reject whole records. This is a finite base, not automatic truncation of arbitrary files or unlimited model context. Provider requests retain their own byte/token/deadline and route restrictions; ChatGPT still has a local-only output-acceptance cap, not a remote plan-usage guarantee.

Native file writes use digest/create preconditions, no-follow handles and a fresh inode plus atomic replacement, never in-place truncation of an aliased inode. Parent directories must already exist; use `mkdir` explicitly. Symlinks, hard links, special files and stale preconditions reject. This does not provide a transaction against concurrent host edits, a rollback guarantee or protection from a hostile same-user/root process.

Before dispatch the Engine commits an immutable `ToolStarted` intent. Before inferring again it commits the correlated bounded `ToolFinished` observation. An interrupted or uncertain effect stops the loop without automatic retry; inspect `arany show --output text SESSION_ID` or the private JSONL export and the actual files before intentionally retrying. Replay, resume, fork and compaction carry past tool facts only as untrusted context and never execute an old intent or renew permission. JSONL is an explicit sensitive-data export.

## Linux enforcement and verification

The first enforcer requires installed non-set-ID, root-owned `/usr/bin/bwrap`, `/usr/bin/systemd-run`, `/usr/bin/systemctl` and `/usr/bin/env`, working unprivileged user namespaces, a systemd user manager and cgroup v2 with effective memory/swap/PID/CPU limits and owned cgroup termination. This is a kernel/native-capability contract, not a GNOME/KDE dependency. Other Linux init systems need a separately enforcing adapter; Arany does not pretend the present implementation works without these facilities. Unsupported architecture or missing enforcement fails closed, with no unsandboxed fallback. Native macOS containment is not implemented or claimed.

The Guard verifies isolated mount/network/PID/user/IPC/UTS namespaces, dropped capabilities, no-new-privileges, inherited syscall filters and effective limits before releasing a one-use intent. It owns the exact transient unit, invocation and retained cgroup identity, including double-fork/setsid descendants, and verifies termination before accepting completion. Normal transient-unit garbage collection is handled using the retained group's empty/removed state. The compiled seccomp deny profile is defense in depth, not a universal syscall allowlist. Linux x86-64 is exercised here; ARM64's profile is source-supported but needs a native runner.

Byte/entry bounds on synchronous local snapshot preparation do not guarantee a finite response from stalled host storage or an uninterruptible kernel operation. Process deadlines and cleanup bound ordinary payload execution; they are not hard-real-time or kernel-failure guarantees. Native file preconditions do not make a multi-file transaction or exclude concurrent hostile host mutation.

The native synthetic corpus exercises real guarded edits/build/execution, Skill resources, MCP, hostile paths/frames/schemas, quotas, cancellation, authority exhaustion, read-only children, compaction and closed-journal replay. Default tests cover strict adapter wire/configuration/replay without launching effects. Reproduce with:

```sh
cargo test --locked --offline --all-targets
cargo test --locked --offline --test session_run tools:: -- --ignored --nocapture
```

Real model tool loops, user account trials, native macOS, native ARM64 and manual terminal/accessibility checks remain the final user checklist, not successful evidence from these synthetic tests. See [next steps](../NEXT_STEPS.md) and the [implementation plan](../planning/effectful-beta-base/README.md).
