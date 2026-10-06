# Local coding tools

Arany can continue a primary AgentRun through file operations, commands, portable Skills and local stdio MCP tools. This is an explicit, offline local foundation, not unrestricted host shell access. Ordinary attached startup asks whether you trust the opened folder once a native OpenAI, Anthropic or ChatGPT account is selected, and remembers explicit trust for that folder. `/permissions` revisits the choice. Untrusted folders remain read-only. Explicit process selections and headless Runs can opt into reviewed configuration with `--tools`; this flag adds no paid compatibility check.

```sh
target/debug/arany --tools
target/debug/arany exec --tools --provider openai --model gpt-5.4 "Inspect and fix the selected code"
```

Select an account/model as usual. These commands can consume Provider usage when you submit work. Native OpenAI and the consented ChatGPT adapter advertise strict native functions and require exactly one function call per step, including phase-specific completion/delegation. Anthropic uses strict structured outcomes. Both translate to one semantic Tool proposal at a time; compatibility with a real selected model requires live evidence. Exact custom profiles currently reject `--tools`: their existing data-free conformance covers read-only outcomes, not this changed encoding.

The generated function/outcome schemas offer file reads/search and only catalog-enabled mutations, commands, Skills and MCP operations. Command, Skill and MCP-server identifiers are constrained to configured names. An unparseable or oversized catalog offers only read-only file operations. This restriction guides generation; catalog text supplies no execution authority and the pinned router/Guard independently admit every proposal.

## Folder trust and approvals

The consent screen shows the full folder path, a brief access explanation and vertical Read only / Trust choices. Read only starts focused. Enter confirms; Escape dismisses without a decision; Ctrl+C exits Arany without saving a decision. Consent for a configured new conversation precedes Session creation. Trust is user-owned, shared by conversations, and pinned to the exact absolute path plus filesystem directory identity. Replacing the directory requires trust again; Session replay never supplies authority.

A private version-1 `workspace-permissions.json` in the fixed OS-user account root stores at most 128 entries and 64 KiB using owner-only atomic replacement. Trust generates a version-1 grant for `workspace_paths: ["."]`, typed writes and the pinned resolved system `sh` executable, with no Skills or MCP grants. Standard secret/link/mount exclusions and finite snapshots still apply; generated directories `target`, `node_modules`, `.venv` and `venv` are omitted from root discovery/snapshots. Commands use the existing offline Guard, not the host terminal or home-installed toolchains.

Shift+Tab cycles **Auto edits → Request approvals → Auto approval → Auto edits**, preserving the draft and caret. The current mode appears beside the composer. Auto edits initially allows typed edits and asks for commands/MCP calls; Request approvals asks for every effect. Reads and Skill/catalog discovery still need the pinned grant but no per-action question. Auto approval uses the selected AI to review the exact proposed action against the current user request; additional model usage can apply. Uncertainty, invalid output, failure or an exhausted review budget asks the user. Switching out of Auto while the AI is reviewing also asks the user.

A human approval shows the exact proposed action, with vertical Deny / Allow once choices and Deny initially focused. All pages must be reviewed before accepting. Approval is one-use, never expands the grant, and expires with the immutable intent within one minute. Escape, a dropped reply or timeout denies; Run cancellation prevents dispatch. Denial stops the Run without retry or rollback of earlier completed actions. AI reviews have a 20-second ceiling, a 256-token output cap, no Tools or project instructions/history/includes, and canonical Provider-call usage. They share the primary call budget, preserving final continuation. Headless `--tools` uses its explicit private upfront grant without an interactive approval channel.

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

Use exact, nonoverlapping relative files/directories. The singleton `["."]` selects the whole project under the same exclusions and bounds; mixing `"."` with other roots rejects. List/search and command working directories accept `"."` or an empty path for the project root; mutations still require a named relative path. Absolute paths, traversal, links, mount crossings and special files are not admitted. StateRoot and the fixed account root must not overlap the Workspace or configured runtime resources. `.git`, `.env`/`.env.*`, `.aws`, `.ssh`, `.gnupg`, `.npmrc` and `.pypirc` path components are excluded. This is not a universal secret detector: review the files you select. Tool observations can be sent to the selected Provider and persisted in the private, unencrypted journal.

The Linux profile exposes the system `/usr` tree read-only as its declared runtime, including compiler/library data. Workspace, StateRoot and account roots must not overlap that tree: an otherwise private file inside a runtime mount would still be visible to a same-user payload. This base does not authorize mounting the user's home or a package cache.

The offline profile denies socket creation, including TCP, UDP and pathname/abstract Unix sockets, plus connect/bind/listen and destination-bearing sends. A private network namespace alone would not exclude a pathname socket in a visible runtime tree. Anonymous IPC socket pairs and pipes remain usable but cannot rebind or send to named destinations; descriptor/message passing with `sendmsg`/`sendmmsg` is unsupported. This is not a network-capable test profile.

JSON has a closed versioned schema: all six top-level fields are required; unknown and duplicate fields reject. Private configuration has no repository discovery, interpolation, automatic installation or hidden executable lookup. Folder trust explicitly pins the resolved system shell rather than looking up arbitrary commands on PATH. Configuration is pinned for a Run; editing it does not cancel an already admitted Run. Changed pinned executable or resource contents reject dispatch rather than being silently trusted.

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

With explicit `--tools`, the inline `/` completion menu includes configured runtime Skills alongside local commands, labeled `Skill` and `Cmd`. Tab or the first Enter inserts `$NAME ` as an ordinary task draft; a later Enter sends it. The selected model loads the pinned `SKILL.md` through the Skill Tool before following its guidance. Exact command names keep command behavior, even when a Skill shares the name; choose the Skill from a shorter prefix. Installed development-agent Skills are not automatically granted. Folder trust alone grants no Skills. Restart after changing the configured names; Run admission still revalidates the actual configuration and resources.

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

Search matches literal UTF-8 file content with one-based line numbers, within either a named regular file or a directory under selected roots. Whole-project folder trust preserves this same behavior for named files. Search does not supply edit preconditions; use the include's SHA-256 or a successful Read. Existing no-follow checks, protected paths and scan/input/result bounds apply to both forms.

The pinned Tool catalog supplies the host clock in Unix milliseconds with an explicit UTC basis, so a current-date task does not depend on historical dates or a shell command. It grants no authority.

Both encodings retain closed semantic Tool proposals. OpenAI/ChatGPT coding requests use [strict native functions](https://developers.openai.com/api/docs/guides/function-calling), required selection and no parallel calls; the adapter supplies paired current action/results without provider-side storage. Finish/Delegate are explicit strict submission functions, not executable Workspace operations. The decoder rejects final text, duplicate or incomplete calls, unknown function names and malformed arguments without switching encodings. Read-only requests, children and compaction keep their structured-text encoding. Field descriptions supply operational details such as Read paging and non-empty Edit matching; [OpenAI's Structured Outputs guide](https://developers.openai.com/api/docs/guides/structured-outputs?api-mode=responses) documents descriptions inside strict schemas. Descriptions guide generation, while local typed validation and the Guard enforce the limits; they are not permission or a guarantee that every live model proposes valid arguments.

The primary completes file actions through correlated Tool continuations in both planning and synthesis. A read needed for an edit is a Tool proposal in the same Run, not a final promise or a request for the user to supply a result. Delegated children reason only over supplied data; the primary obtains needed file evidence and performs edits itself. Previous Tool observations remain available to the primary after delegation. `@` file references add bounded include snapshots labeled with their escaped relative path and SHA-256 of the actual file bytes; metadata counts toward the context budget. Trailing-slash directory references identify where to inspect without reading a directory as a file or granting additional access.

Native adapters present the bounded catalog and JSON Tool outputs as structured task data, with ordered call/disposition/output observations rather than internal journal intents and receipts. Each observation carries a workspace-effect fact: inspection and disposable commands have none, attested successful file mutations are applied, uncertain/cancelled mutations remain uncertain, and other mutations are not confirmed. The full intent, limits and Guard receipt remain canonical. Plain or malformed-JSON output stays an untrusted string; no result or catalog is promoted to instructions or authority. Only inspecting a file does not produce an edit receipt; an authorized requested change still requires an Edit or Write proposal before completion. This representation improves the information supplied to the model, not a guarantee of live task completion.

Native file writes use digest/create preconditions, no-follow handles and a fresh inode plus atomic replacement, never in-place truncation of an aliased inode. Parent directories must already exist; use `mkdir` explicitly. Symlinks, hard links, special files and stale preconditions reject. This does not provide a transaction against concurrent host edits, a rollback guarantee or protection from a hostile same-user/root process.

Guard preparation, bootstrap or attestation failures before the GO dispatch gate record Failed without a receipt and stop the Run without another model call; strict replay rejects continuation or a successful answer after that failure: no operation was dispatched. Beginning the GO write makes subsequent missing completion conservatively uncertain, including a partial write. No unconfined fallback exists. Historical failures are previous-Run facts, not pending current Tools; a fresh request may inspect current files and propose a newly authorized edit with fresh preconditions. It never reuses a previous intent or approval.

Before review or dispatch the Engine commits an immutable `ToolStarted` intent. Before inferring again it commits the correlated bounded `ToolFinished` observation. An interrupted or uncertain effect stops the loop without automatic retry; inspect `arany show --output text SESSION_ID` or the private JSONL export and the actual files before intentionally retrying. Replay, resume, fork and compaction carry past tool facts only as untrusted context and never execute an old intent or renew permission. JSONL is an explicit sensitive-data export.

Run admission requires enough resolved Event slots for its entire bounded topology and all configured Tool continuations, before Provider usage or Tool preparation. The Session and its fork ancestors stay locked throughout, so cooperating operations cannot consume that logical headroom. Near the history quota, start a new Session; this admission does not reserve physical disk or eliminate crash/disk-fault uncertainty after dispatch.

## Linux enforcement and verification

The first enforcer requires installed non-set-ID, root-owned `/usr/bin/bwrap`, `/usr/bin/systemd-run`, `/usr/bin/systemctl` and `/usr/bin/env`, working unprivileged user namespaces, a systemd user manager and cgroup v2 with effective memory/swap/PID/CPU limits and owned cgroup termination. This is a kernel/native-capability contract, not a GNOME/KDE dependency. Other Linux init systems need a separately enforcing adapter; Arany does not pretend the present implementation works without these facilities. Unsupported architecture or missing enforcement fails closed, with no unsandboxed fallback. Native macOS containment is not implemented or claimed.

The Guard verifies isolated mount/network/PID/user/IPC/UTS namespaces, dropped capabilities, no-new-privileges, inherited syscall filters and effective limits before releasing a one-use intent. It owns the exact transient unit, invocation and retained cgroup identity, including double-fork/setsid descendants, and verifies termination before accepting completion. Normal transient-unit garbage collection is handled using the retained group's empty/removed state. The compiled seccomp deny profile is defense in depth, not a universal syscall allowlist. Linux x86-64 is exercised here; ARM64's profile is source-supported but needs a native runner.

Byte/entry bounds on synchronous local snapshot preparation do not guarantee a finite response from stalled host storage or an uninterruptible kernel operation. Process deadlines and cleanup bound ordinary payload execution; they are not hard-real-time or kernel-failure guarantees. Native file preconditions do not make a multi-file transaction or exclude concurrent hostile host mutation.

The native synthetic corpus exercises real guarded edits/build/execution, Skill resources, MCP, hostile paths/frames/schemas, quotas, cancellation, authority exhaustion, read-only children, compaction and closed-journal replay. The existing primary team journey reopens a failed bootstrap Session, searches a regular file under whole-project trust, then reads/edits after delegation with Auto edits; the OOM owner preserves post-dispatch uncertainty and then completes fresh read/edit in the same resumed Session without replaying its old command. The path corpus rejects file Search on symlinks, hard links, FIFOs and over-depth paths under whole-project grants. Default tests cover strict adapter wire/configuration/replay and schema exclusion of unconfigured or unnamed operations without launching effects. The approval journey covers real root edits/commands, approve/deny/dropped replies/cancellation, AI approval and human fallback after uncertain/malformed/failed review, with known usage and closed replay. The isolated PTY consent owner covers vertical navigation, explicit trust, Ctrl+C without an implicit choice and terminal restoration in inline and linear modes. Reproduce with:

```sh
cargo test --locked --offline --all-targets
cargo test --locked --offline --test session_run tools:: -- --ignored --nocapture
```

Real model tool loops, user account trials, native macOS, native ARM64 and manual terminal/accessibility checks remain the final user checklist, not successful evidence from these synthetic tests. See [next steps](../NEXT_STEPS.md) and the [implementation plan](../planning/effectful-beta-base/README.md).
