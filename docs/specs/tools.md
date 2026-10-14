# Local coding tools

This contract owns Tool grants, guarded effects, runtime Skills and local MCP. [Terminal](./terminal.md) owns completion, settings and consent presentation; [execution](./execution.md) owns Run coordination and capacity admission. Editing constraints remain in [Tool rules](../../agents/tools.md) and [security rules](../../agents/security.md).

Arany can continue a primary AgentRun through file operations, commands, portable Skills and local stdio MCP tools. This is an explicit, offline local foundation, not unrestricted host shell access. Every attached startup asks whether you trust the opened folder before account selection or setup, including when no account is configured, unless that exact folder already has remembered trust. This consent flow is the same across operating systems and independent of Provider flags or `--tools`. `/permissions` revisits the choice. Without explicit private Tool configuration, untrusted folders remain read-only. Explicit process selections and headless Runs can opt into reviewed configuration with `--tools`; this flag adds no paid compatibility check.

Folder consent and native Tool capability are independent. On platforms without an implemented enforcer, attached chat remains available without Tools even after trust is remembered. `/permissions` uses the same consent choices; Shift+Tab reports the native restriction without enabling authority. Explicit `--tools` rejects unavailable protection. Trust alone never supplies missing OS protection, and no unconfined Tool executes.

The native macOS profile admits checked file operations, pinned Skill data,
host-native commands and local MCP through a separate trusted Guard worker.
File/Skill dispatch prohibits program execution. Commands and MCP use private
disposable storage and retain ordinary native child launches. Both preserve
approval, GO, offline credential/state exclusion, supervised process ownership,
uncertainty and durable receipts. Native evidence and remaining release checks
belong to the linked evidence owners below.

The complete harness requires equivalent granted file operations, commands, Skills and local MCP across supported operating systems, with the same permission, isolation, cancellation and durable-result contract. Resource guarantees differ explicitly by native profile as defined below. [Next steps](../../NEXT_STEPS.md#beta-2-and-broader-release) records unperformed native, platform, live and human checks; implemented code alone does not establish platform support.

```sh
target/debug/arany --tools
target/debug/arany exec --tools --provider openai --model gpt-5.4 "Inspect and fix the selected code"
```

Select an account/model as usual. These commands can consume Provider usage when you submit work. Native OpenAI and the consented ChatGPT adapter advertise strict native functions and require exactly one function call per step, including phase-specific completion/delegation. Anthropic uses strict structured outcomes. Both translate to one semantic Tool proposal at a time; compatibility with a real selected model requires live evidence. Exact custom profiles currently reject `--tools`: their existing data-free conformance covers read-only outcomes, not this changed encoding.

The generated function/outcome schemas offer file reads/search and only catalog-enabled mutations, commands, Skills and MCP operations. Command, Skill and MCP-server identifiers are constrained to configured names. An unparseable or oversized catalog offers only read-only file operations. This restriction guides generation; catalog text supplies no execution authority and the pinned router/Guard independently admit every proposal.

Before a Goal plan is recorded, only bounded Workspace List/Read/Search, an already admitted Skill read, and the configured bounded MCP catalog listing are available. MCP catalog listing is read-only discovery; an actual MCP tool invocation requires a Goal plan even when catalog metadata says `readOnlyHint`. Commands require a plan regardless of their script's intended behavior. A plan may accompany a prelude call and is always validated and journaled before dispatch; read-only Goal tasks can finish without inventing an effect scope. The [Goal contract](./goals.md#initial-plan-contract-and-success-receipt) owns the exact timing and replay rules.

## Folder trust and approvals

The consent screen shows the full folder path, a brief access explanation and vertical Read only / Trust choices. Read only starts focused. With explicit `--tools`, the first choice instead says Use explicit config and explains that skipping folder trust retains that private configuration; neither choice expands its roots or capabilities. When folder trust is selected, the same screen presents metadata-only detected toolchain and host-cache candidates as unchecked choices. Discovery runs for initial consent or when `/permissions` opens the selector; remembered trusted startup revalidates only saved grant pins and does not enumerate optional home-managed candidates. Thus an unrelated over-limit manager cannot block prompt-free chat, while an explicit selector may report its bounded discovery failure. The user may enable exact Rust, Node or Python toolchain directories; host registry caches require a separate explicit selection and disclose that the exact cache may contain private package names, source, registry-response data or embedded credential URLs. Cache contents are not loaded automatically into Provider context. A command or local MCP Tool can read and emit selected cache data; bounded Tool output is journaled and can enter a later Provider request. Consent explicitly warns about this disclosure path. Package-manager configuration and credential files outside the selected cache are not detected, read or mounted. Enter confirms; Escape dismisses without a decision; Ctrl+C exits Arany without saving a decision or entering account setup. `/permissions` rediscovers choices, shows saved selections, preserves them when Trust is confirmed unchanged, and permits explicit add/remove. New candidates start unchecked. Consent precedes saved-account lookup, catalog or credential access, Session discovery and creation, and applies to new, resumed and forked entry. Choosing read only, using explicit configuration without trust, or dismissing does not remember trust, so the next process asks again. Unavailable, malformed or unsafe trust metadata stops startup before account or Session access. Trust is user-owned, shared by conversations, and pinned to the exact absolute path plus filesystem directory identity. Replacing the directory requires trust again; Session replay never supplies authority. A remembered grant applies only to native Providers and platforms that admit Tools; custom Providers stay read-only. Explicit private `--tools` configuration takes precedence over remembered trust and retains its narrower roots and capabilities.

A private version-2 `workspace-permissions.json` in the fixed OS-user account root stores at most 128 entries and 64 KiB using owner-only atomic replacement. Unsupported prior versions reject without changing the record. Trust generates a version-1 Tool grant for `workspace_paths: ["."]`, typed writes, discovered project Skills and a pinned resolved system `sh` executable on Linux/macOS. Its optional runtime selections bind each exact path, profile kind and directory device/inode to that folder entry. MCP requires explicit private configuration. Standard secret/link/mount exclusions and finite snapshots still apply; generated directories `target`, `node_modules`, `.venv` and `venv` are omitted from root discovery/snapshots. Commands use the existing offline Guard, not the host terminal, home as a whole, package-manager configuration or credentials.

Runtime discovery inspects only bounded directory entries and executable-file metadata under `~/.rustup/toolchains`, the known per-version Node installation roots for nvm, fnm, Volta and asdf, and `~/.pyenv/versions`. It never executes a discovered binary or reads project/tool-manager configuration. Candidates require a numeric `major.minor.patch` version directory label; channel aliases such as `stable` and `nightly` are excluded. The recorded label is the manager's exact directory name, not an executed or independently verified binary version. Python candidates require a version-specific regular executable or a `python`/`python3` alias whose single relative target names a version-specific regular executable in the same `bin` directory; arbitrary symlink traversal is rejected. A selected Rust, Node or Python directory is mounted read-only as an exact runtime subtree, and its `bin` directory is added to the command `PATH`. Folder trust does not select every installed version implicitly. The exact version label, absolute path and directory device/inode are bound into the Tool policy and rechecked at Run/Guard admission and before Ready/GO; drift rejects before dispatch. Toolchain code runs with the command's ordinary offline native resource profile and its descendants remain owned by the existing Guard.

Optional host caches are distinct, read-only grants: Cargo's exact `registry/cache`, `registry/src` and `registry/index` subtrees; npm's exact `_cacache` directory; and pip's exact `http-v2` or `wheels` directory. They are never inferred from a toolchain selection and no parent cache directory is mounted. The grant authorizes a process to read the selected cache data, which may include private package names, source, registry-response metadata or embedded credential URLs. Arany does not load cache contents automatically; a command or local MCP Tool can emit selected data through its bounded output, which is journaled and can enter a later Provider request. Separate package-manager configuration and credential files, environment values and parent directories remain unavailable. These host caches do not become Arany's writable persistent build cache and cache data supplies no authority.

Shift+Tab cycles **Auto edits → Request approvals → Auto approval → Auto edits**, preserving the draft and caret. The current mode appears beside the composer and follows the selected Provider's native capability. Selecting a supported native Provider after trusting an unconfigured conversation restores the remembered mode; changing between supported selections retains the current mode. Auto edits initially allows typed edits and asks for commands/MCP calls; Request approvals asks for every effect. Reads and Skill/catalog discovery still need the pinned grant but no per-action question. Auto approval uses the selected AI to review the exact proposed action against the current user request; additional model usage can apply. Uncertainty, invalid output, failure or an exhausted review budget asks the user. Switching out of Auto while the AI is reviewing also asks the user.

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

The Linux profile exposes the system `/usr` tree read-only as its declared runtime, including compiler/library data. Workspace, StateRoot and account roots must not overlap that tree: an otherwise private file inside a runtime mount would still be visible to a same-user payload. This base does not authorize mounting the user's home or any unselected cache.

The offline profiles deny network and named Unix socket creation, connect/bind/listen and destination-bearing sends. A private network namespace alone would not exclude a pathname socket in a visible runtime tree. Anonymous pipes remain usable. Linux also permits anonymous IPC socket pairs without rebinding or sending to named destinations; macOS can create an anonymous pair but denies its socket writes, so payloads cannot depend on socket-based wakeups. Descriptor/message passing with `sendmsg`/`sendmmsg` is unsupported. This is not a network-capable test profile.

JSON has a closed versioned schema: all six top-level fields are required; unknown and duplicate fields reject. Private configuration has no interpolation, automatic installation or hidden executable lookup. Project Skill discovery stays within its selected Workspace roots and cannot widen them. Where commands are supported, folder trust explicitly pins the resolved system shell rather than looking up arbitrary commands on PATH. Configuration is pinned for a Run; editing it does not cancel an already admitted Run. Changed pinned executable or resource contents reject dispatch rather than being silently trusted.

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

The placeholder above is deliberately invalid. Resolve a system executable's real path and calculate its digest yourself, for example with `readlink -f /usr/bin/bash` and `sha256sum /usr/bin/bash` on Linux, or `shasum -a 256 /bin/sh` on macOS. Arany requires a no-follow, regular, single-link native executable: ELF on Linux or Mach-O on macOS. A symlink must be replaced with its resolved executable path. Updating an executable requires reviewing and changing its pin.

Additional inputs use `{"path":"/absolute/reviewed/file","destination":"relative/name","sha256":"<digest>"}`. They appear read-only under `/inputs/PROGRAM_ID/relative/name`. Executables are copied to `/programs/PROGRAM_ID`, except checked immutable macOS system programs, which retain their original paths. Paths and digests are checked again for each fresh invocation. Descriptions and the `interpreter` declaration do not prove that arbitrary arguments or a script are safe; allowing a shell/interpreter authorizes its behavior inside the same OS-enforced boundary, including descendants using the admitted native runtime.

Commands receive an explicit argument array, a relative working directory, cleared environment, private `HOME`/temporary/Cargo directories, no terminal input, and bounded UTF-8 stdout/stderr. Selected toolchains and optional host caches are read-only exact subtrees; selected toolchain `bin` paths augment the fixed system `PATH`. npm and pip cache variables point inside the Guard's private scratch root, with an explicitly selected host cache exposed only at its exact nested subtree. They use a fresh writable snapshot of only selected project paths. Empty directories and executable bits are preserved without set-ID bits. Each snapshot includes prior typed edits and integrated command changes. A compiler can write a binary under `/scratch` and execute it in the same command. Scratch writes and build outputs do not survive calls. Selected host package caches remain read-only host data; Arany has no writable persistent per-Workspace build cache yet (A3). Background jobs do not survive a call. The current limits do not establish support for every large Rust, Node or Python build.

### Command changesets

After command completion and positive descendant termination, Guard compares the private snapshot with its admitted input. It returns a sorted changeset of created, modified and deleted regular files, including digest-bound before/after bytes and executable bits. Binary contents use canonical base64 in the typed record. An ordinary nonzero command exit may still return changes; command exit status and publication are separate outcomes. A failed capture, timeout, cancellation or unconfirmed process cleanup publishes nothing.

The complete snapshot is scanned without following links or reading excluded paths. Links (including hard links), special files, set-ID bits, excluded or invalid paths, mount crossings, unsupported file/directory conversions and overflow reject the whole changeset. Directory creation is supported only as parents of created files; empty-directory changes are not integrated. The current changeset ceiling is 64 files, 8 KiB aggregate decoded before/after content, and the existing serialized argument/result capacity. This bounded base precedes the flexible-limits slice.

Engine records the command observation before creating a separate internal, one-use integration intent bound to that exact command receipt. Models cannot request integration directly. The integration goes through the existing typed-mutation Guard; it never adopts subprocess-created host paths. Publication requires the pinned write grant and all paths inside its selected roots. Approval cannot widen that grant. Current Auto edits accepts in-grant integration after approving the command; Request approvals shows the complete sanitized before/after changes and asks separately; Auto approval reviews the concrete integration and asks a human on uncertainty. Headless execution uses its explicit upfront grant. The later modes/plan-contract slice owns approval of effects outside declared plan scope.

Before publishing, Guard validates the whole batch, Workspace identity, every before digest/executable bit or absent-file precondition, and parent directory types through no-follow handles. Conflict, denial or overflow at this stage leaves every file unchanged. Publication uses fresh regular inodes and checked deletions, then synchronized directory receipts. This is not an atomic multi-file transaction: failure or concurrent drift after publication starts records Uncertain and stops the Run without automatic retry. Cancellation while approval waits leaves the Workspace unchanged; cancellation during publication may leave recorded uncertain effects. Closed replay retains both outcomes and never repeats the command, approval or integration. Only a successful integration receipt proves Workspace publication; a successful command alone does not.

Commands reserve two effect slots and enough context/receipt capacity before dispatch. Empty changesets need no integration. When capacity cannot admit both stages, the command does not dispatch. Integration shares the original finite Run budget and uses fresh authorization; it never renews the command's approval.

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

Arany automatically discovers project Skills only from `.agents/skills/NAME/SKILL.md`. Remembered whole-folder trust admits this guidance; an explicit private grant admits discovery only when its selected roots contain `.agents/skills`. Other harness directories, `agents/skills`, and home/global Skills are not automatic sources. Explicit private entries take precedence over duplicate project names. Names, regular resources and digests are bounded and pinned anew at Run admission; links, special files and excessive discovery reject.

The model initially sees admitted names and bounded discovery previews, then can request the pinned `SKILL.md` and listed resources lazily. Project previews are literal frontmatter excerpts, not host-side YAML parsing; private descriptions are reviewed configuration. Catalog descriptions are truncated to 96 characters; resource digests stay in canonical admission and Tool receipts rather than consuming discovery tokens. `SKILL.md` needs YAML frontmatter with the matching string `name` and a nonempty string `description`, followed by the Markdown body. Quoted/folded scalars and CRLF are supported; ambiguous/duplicate or excessive metadata rejects. Local executable YAML tags reject; global YAML tags may be ignored inertly. Behavioral fields such as `allowed-tools`, hooks, shell directives, forks and installation instructions are guidance only, never grants. Skill scripts require an ordinary approved command/interpreter; requesting a Skill executes none of its body.

Attached completion exposes admitted Skill names according to the [terminal completion contract](./terminal.md#interactive-commands-and-completion), including its tabs/settings and unsent `$NAME` insertion. Untrusted folder contents grant no Skill access. Startup and `/permissions` refresh names; Run admission revalidates configuration and resources. At most 64 names are admitted; changing installation contents during a pinned Run never renews a resource grant.

At attached startup, a root regular `skills-lock.json` with no `.agents/skills` directory produces a notice to run `npx --yes skills@1.7.0 experimental_install` in the project. The [Skills 1.7.0 installer](https://github.com/vercel-labs/skills/blob/v1.7.0/src/install.ts) restores the project lock through that command; `install` aliases `add` and requires a source. Arany inspects installation state, not lock contents or historical command execution. An existing Skill directory, including an empty one, does not trigger that missing-directory notice. Arany never runs the installer automatically.

YAML parsing occurs only inside the killable Guard, not the credential-owning Engine. Linux mounts Skills read-only under `/skills/SKILL_ID`; the macOS file worker opens only the requested admitted resource through checked handles and validates its pinned digest before dispatch and use. Unlisted resources cannot be loaded by the Skill tool. A Goal plan names each Skill resource exactly (`resource: null` means `SKILL.md`) and that list narrows the Run's already admitted Skill names and files; it never grants discovery or additional resources. A matching successful Skill receipt may satisfy the Goal's typed success criterion.

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
| Private configuration | 64 KiB; 32 selected roots, 8 commands, 64 Skills, 4 MCP servers |
| Project snapshot | 32 MiB; 2,048 file/empty-directory entries; 8,192 scanned directory entries; depth 16 |
| File operation | 1 MiB input file; reads page at most 4,096 UTF-8 bytes; writes/edit strings at most 8 KiB |
| Runtime resources | 96 MiB aggregate copied executables/inputs/Skills; executable 32 MiB, extra input 1 MiB, Skill file 16 KiB |
| Selected runtime grants | At most 9 exact roots per folder, one each for Rust, Node, Python and six known cache subtrees; versioned toolchain labels, exact paths and directory device/inode are policy-bound and checked before dispatch; host caches are read-only and individually opt-in |
| Linux processes | 512 MiB aggregate memory, no swap, group OOM termination, 64 processes/threads; 25% of one CPU |
| macOS processes | Supervised aggregate physical-footprint threshold 512 MiB, counting compressed memory whether held in RAM or swapped out, processes/threads threshold 64 and CPU-time threshold 15 seconds per process; nominal 10-ms sampling. No aggregate CPU rate quota. The non-spawning file/Skill worker additionally enforces immutable CPU expiry; commands/MCP retain ordinary signal behavior and use external supervision. |
| Private command storage | Separate 64 MiB scratch and 64 MiB writable project filesystems, including metadata: tmpfs on Linux, fixed-capacity native images on macOS. Commands return bounded validated changesets; MCP writes are discarded. Typed mutations publish directly into the admitted Workspace. |
| Command changeset | 64 regular files; 8 KiB aggregate decoded before/after content; existing 16 KiB serialized arguments and reserved result/context bounds. Whole-record overflow rejects before publication. |
| Time | Run 300 seconds; each unit at most 60 seconds, command capture 50 seconds, MCP dialogue 40 seconds; finite startup/cleanup and parent deadlines. On macOS, worker preparation before Ready has its own 30-second bound, and a timeout there rejects at the handshake stage before dispatch |
| Tool loop | 16 effects shared across primary planning/synthesis, including internal command integration; model-step receipt ceiling 32; children retain their existing maximum of 8 one-call reasoning tasks |
| Skill discovery | 256 top-level entries; 64 Skills; 32 resources per Skill; 8,192 resource entries; depth 16; 32 MiB read; merged grant at most 64 KiB; overflow rejects |
| Tool context | 16 KiB catalog; 64 KiB catalog plus observations reserved before dispatch; result ceiling 16 KiB, narrowed to remaining space including JSON escaping |
| MCP | 128 KiB frame; 4 pages, 128 tools and 256 KiB catalog; 64 incoming server messages; bounded schema depth/nodes/regex; no reference fetch |

List/search return at most 64 rows and explicitly indicate possible truncation. Oversized requests/results otherwise reject whole records. This is a finite base, not automatic truncation of arbitrary files or unlimited model context. Provider requests retain their own byte/token/deadline and route restrictions; ChatGPT still has a local-only output-acceptance cap, not a remote plan-usage guarantee.

The accepted macOS resource profile terminates its entire owned unit when an
observation exceeds a threshold or the resource observer fails. Its memory/task/CPU
thresholds are supervised rather than kernel ceilings: scheduling delays and
work between samples can exceed them or exhaust host resources before
termination. Swap prohibition and kernel group-OOM enforcement are not claimed.
The CPU threshold applies independently to each process and does not bound the
whole tree's cumulative CPU time or instantaneous rate. Wall-time, output,
private-storage, offline isolation and complete descendant termination remain
required. Commands and MCP execute native macOS binaries on the host and retain
ordinary child-launch compatibility, including `posix_spawn`. A fork/exec-only
profile does not satisfy this contract.

Normalized intents and Guard receipts explicitly identify `linux_kernel` or
`macos_supervised` resource semantics, and the enforcement digest binds the
native enforcement policy: profile templates, admission rules and their actual
resource constants. Per-invocation paths and the fresh catalogue of eligible
immutable system executables instantiate that policy; selected grants and
configured executable/input pins are bound by the intent's policy digest and
resource identities. The discriminator is mandatory in every intent;
missing or unknown profiles and receipts for another resource profile reject. Acceptance of the resource contract does not enable
dispatch before native isolation, supervision and cleanup are attested.

Setting Darwin's `RLIMIT_CPU` soft and hard values to 15 seconds alone does not
replace CPU supervision: a process can ignore `SIGXCPU` and exceed
that time. The macOS file worker resets and unblocks that signal, then prohibits
handler, mask, signal-wait and limit changes, plus the supported process-launch
variants, before dispatch. This restriction applies only to trusted file/Skill
code. Commands and MCP retain signal compatibility and use external CPU
supervision.

The minimum admitted result budget is 128 bytes. Mutation and failure receipts must fit it, retaining resource identity through the correlated intent rather than repeating an arbitrarily long path. An applied mutation with a lost completion stays uncertain; receipt overflow cannot turn it into an ordinary retryable failure.

Search matches literal UTF-8 file content with one-based line numbers, within either a named regular file or a directory under selected roots. Whole-project folder trust preserves this same behavior for named files. Search does not supply edit preconditions; use the include's SHA-256 or a successful Read. Existing no-follow checks, protected paths and scan/input/result bounds apply to both forms.

The pinned Tool catalog supplies the host clock in Unix milliseconds with an explicit UTC basis, so a current-date task does not depend on historical dates or a shell command. It grants no authority.

The [execution contract](./execution.md#accepted-outcomes) owns strict native-function/structured-text selection and rejection. Tool field descriptions explain Read paging and non-empty Edit matching, while typed validation and the Guard enforce limits. Descriptions are generation guidance, not permission or a live-model completion guarantee.

The primary completes file actions through correlated Tool continuations in both planning and synthesis. A read needed for an edit is a Tool proposal in the same Run, not a final promise or a request for the user to supply a result. Delegated children reason only over supplied data; the primary obtains needed file evidence and performs edits itself. Previous Tool observations remain available to the primary after delegation. `@` file references add bounded include snapshots labeled with their escaped relative path and SHA-256 of the actual file bytes; metadata counts toward the context budget. Trailing-slash directory references identify where to inspect without reading a directory as a file or granting additional access.

Native adapters present the bounded catalog and JSON Tool outputs as structured task data, with ordered call/disposition/output observations rather than internal journal intents and receipts. Each observation carries a workspace-effect fact: inspection and disposable commands have none, attested successful file mutations are applied, uncertain/cancelled mutations remain uncertain, and other mutations are not confirmed. On the native Responses function route, each current observation is carried once in its correlated result item; the user task-data envelope carries the catalog without a second observation list. Historical Tool context remains distinct untrusted data. The full intent, limits and Guard receipt remain canonical. Plain or malformed-JSON output stays an untrusted string; no result or catalog is promoted to instructions or authority. Only inspecting a file does not produce an edit receipt; an authorized requested change still requires an Edit or Write proposal before completion. This representation improves the information supplied to the model, not a guarantee of live task completion.

Native file writes use digest/create preconditions, no-follow handles and a fresh inode plus atomic replacement, never in-place truncation of an aliased inode. Parent directories must already exist; use `mkdir` explicitly. Symlinks, hard links, special files and stale preconditions reject. This does not provide a transaction against concurrent host edits, a rollback guarantee or protection from a hostile same-user/root process.

Guard preparation, bootstrap or attestation failures before the GO dispatch gate record Failed without a receipt and stop the Run without another model call; strict replay rejects continuation or a successful answer after that failure: no operation was dispatched. Beginning the GO write makes subsequent missing completion conservatively uncertain, including a partial write. No unconfined fallback exists. Historical failures are previous-Run facts, not pending current Tools; a fresh request may inspect current files and propose a newly authorized edit with fresh preconditions. It never reuses a previous intent or approval.

The internal Guard task is one newline-delimited JSON frame with at most 96 KiB
before its delimiter. Missing delimiters, oversized frames and malformed tasks
reject before Ready or effects; the exact capacity remains admissible. Both
worker and supervisor reject trailing buffered input, including a valid GO
coalesced with the task, before Ready. GO remains a separate one-use exchange.

Before review or dispatch the Engine commits an immutable `ToolStarted` intent. Before inferring again it commits the correlated bounded `ToolFinished` observation. An interrupted or uncertain effect stops the loop without automatic retry; inspect `arany show --output text SESSION_ID` or the private JSONL export and the actual files before intentionally retrying. Replay, resume, fork and compaction carry past tool facts only as untrusted context and never execute an old intent or renew permission. JSONL is an explicit sensitive-data export.

The [execution admission contract](./execution.md#admission-persistence-and-recovery) owns complete-topology Event headroom, ancestor locking, compaction/fork quotas and pre-disclosure rejection. Logical headroom does not reserve physical disk or eliminate crash/disk-fault uncertainty after dispatch.

## macOS enforcement and compatibility

Native Tool dispatch requires an available Background launchd user domain,
a distinct kernel resource coalition, a trusted bounded supervisor and a
separate attested Seatbelt worker. The private control directory must be outside
the Workspace; overlapping roots reject before dispatch. The supervisor's
reported launchd PID and authenticated local peer must identify the same
process. The controller validates resources before GO and accepts completion
only after the owned coalition is empty and the one-use job is removed or
positively reported absent. Automatic job retirement alone is insufficient
descendant cleanup. Missing enforcement has no unconfined fallback.

Before Tools are offered, admission validates the immutable `launchctl`, `env`
and `hdiutil` executables, kernel coalition observation and the effective user's
launchd domain. The domain probe discards output, has a three-second deadline
and allows one second for kill/reap after failure. Per-unit authentication,
distinct coalition ownership, storage preparation and installed enforcement
remain mandatory before GO; startup availability is not dispatch attestation.

Supervision uses kernel process identity and cumulative CPU accounting across
exec. Reaped or replaced members retire after identity revalidation, including
when resource sampling fails during exit; missing observation of a live member
still rejects. Exact-incarnation signaling must not affect a reused PID.

The private-storage primitive allocates separate owner-only, writable native
filesystems with fixed 64-MiB capacities. Killing the unit supervisor must leave
the storage available for clean detachment by its controller. Normal completion,
partial setup, cancellation and unwind require positively established process
termination before detachment. Unexpected mount targets reject without following
them. Unfinished attachment or failed cleanup retains the private control root
and rejects completion rather than recursively deleting through a live or
uncertain mount. Each unit's owner-only root is `/private/tmp/arany-u-<uuid>`.
Each live unit creates and locks its root's `unit.lock` in one exclusive open;
the kernel releases the lock when the controller exits, including on a crash.
Before launching its job, the controller durably records the root's device/inode
and macOS boot identity. The authenticated supervisor durably records its
process/coalition identity before receiving a task; no payload or attachment
may precede this record.

Before creating a new unit, admission scans at most 4096 directory entries and
attempts recovery of at most eight unlocked roots owned by the effective uid,
within one 30-second recovery deadline. Recovery takes an exclusive lock and
validates private no-follow records and root identity before any cleanup. Live
controllers and competing recovery owners are excluded. Within the recorded
boot, recovery removes the exact one-use job and positively empties its recorded
coalition using current audit-token identities. After a reboot it never signals
persisted process/coalition identities or removes a job from the later boot.

Storage cleanup requires the pinned backing image inode, OS disk-image inventory
and exact kernel device/mount correspondence. Unexpected or nested mount targets
reject before detachment; even an attached but unmounted owned image must be
detached. An interrupted attachment marker is cleared only after positive
absence after detaching the positively identified interrupted image, or evidence
that its recorded boot has ended. Empty inventory alone cannot settle an
unfinished attachment from the current boot. The marker identifies its image
and boot. Recovery durably records positive observation before detachment so
cancellation after detach cannot strand a settled marker. Root deletion additionally requires absence of
all mounts beneath the checked root. Cancellation or failure leaves recoverable
records in place and never retries an old Tool intent.

Unlocked roots without valid ownership records, including older retained roots,
remain untouched and count toward shared capacity, together with live/recovering
roots. Valid records are selected before the eight recovery-attempt budget is
spent. Unreadable or
over-limit listings and recovery deadline expiry reject as unavailable native
protection without removal advice. A filename prefix alone never authorizes
cleanup; there is no stored-format migration or compatibility fallback. Counting
all owned roots conservatively includes roots interrupted before lock publication.

Commands and MCP receive a fresh checked snapshot of the selected Workspace,
including prior typed edits, and a separate read-only tree of pinned runtime
resources. Writable project/scratch mounts prohibit set-ID behavior. Reviewed
system executables must be immutable, root-owned, non-set-ID and unentitled;
privileged process/credential/coalition changes remain denied even after exec.
When a grant enables commands or MCP, admission checks the bounded system
executable catalogue once per Run. Each process intent and receipt binds that
catalogue to the compiled enforcement policy. Workers revalidate its no-follow
file identities, ownership, permissions, size and modification/change timestamps
before Ready; changed or oversized catalogues reject before dispatch without
refresh or retry. The catalogue is transient trusted-controller data and shares
the existing Task byte cap. File/Skill workers carry no catalogue. Privileged OS
mutation remains outside the current-user attacker boundary.
The runtime excludes host account and Session roots, network destinations and
Mach services. Root or mismatched real/effective identities reject process
dispatch. Ordinary unprivileged `fork`, `exec` and `posix_spawn` remain available.
Unselected host paths have no general metadata grant. Directory ancestors
needed for traversal grant metadata without listings, including `/private/tmp`
and the unit control root; admitted runtime directories remain readable.
Commands additionally read the literal filesystem root because the native
loader opens it during launch. This permits listing top-level names, without
granting access beneath them or listing shared temporary/account directories.
Exact runtime metadata also covers native shell selection and system path
aliases; it grants no subtree access to host account or temporary storage.

macOS uses private physical paths rather than a Linux mount namespace. The
command's relative working directory is inside its project snapshot. Standalone
argv paths rooted at `/workspace`, `/scratch`, `/inputs`, `/skills` or `/programs`
map into private storage; copied-program aliases apply to copied executables.
Shell strings are not rewritten. Use `ARANY_TOOL_WORKSPACE`, `ARANY_TOOL_INPUTS`
and `ARANY_TOOL_SKILLS`, or project-relative paths, inside scripts. `HOME` and
`TMPDIR` point to private scratch. Checked system binaries execute at their
original paths because native relocation is not universally supported.

## Shared native capacity and settings

Linux and macOS coordinate Guard capacity across processes and Session roots
using the effective user's fixed account root. The owner-only file
`guard/limits.json` is created on first native admission with:

```json
{"version":1,"units":8,"queued":32}
```

Edit that private file to change capacity; preserve mode `0600` and the private
directory. On macOS the fixed account root is
`~/Library/Application Support/dev.Arany.arany`; on Linux it is
`~/.local/state/arany`, resolved from the effective user's account rather than
environment-selected Session storage. `units` accepts 1–64 and `queued` accepts
1–1024. Unknown fields/versions, unsafe files and invalid ranges reject admission.
Every admission rereads settings. Lowering a limit never kills admitted units or
registered waiters; no new admission exceeds the selected limit. Existing usage
may therefore temporarily exceed a newly lowered value.

Every Guard unit, including file-only units, reserves one slot before native
launch. Live, recovering and retained units count. At defaults, newly admitted
macOS units reserve at most 1 GiB of backing images in aggregate (two 64 MiB images
per unit); the configured ceiling is `units × 128 MiB`. Existing excess roots
from a previous implementation are retained and prevent further admission.
Linux's per-unit mount/cgroup limits remain unchanged. Its slot also owns the
host seed until checked deletion. Each seed contains at most 32 MiB of selected
Workspace data and 96 MiB of runtime data, bounding newly admitted retained
payload at `units × 128 MiB` (1 GiB by default). This is a logical payload bound,
not a physical filesystem quota; directory, file-count and record bounds apply
separately. Historical unrecorded seeds require the maintenance procedure below.

When capacity is occupied, the approved Tool remains pending in its current Run.
Up to `queued` Tools register in one shared FIFO, ordered by registration under
the namespace lock. Newcomers cannot overtake registered waiters. Cancellation,
original intent expiry and process death remove the participant's position.
Overflow rejects before effects. Waiting never renews an intent, approval or Run
budget and is not a durable task queue; restart never resumes old permissions.
There is one pending Tool per Run. The [terminal contract](./terminal.md#conversation-progress-and-history)
owns the temporary queued indication. Native recovery retains its
30-second deadline within the original intent lifetime; local filesystem bounds
remain cooperative rather than hard-real-time guarantees.

### Linux host seed ownership

Every new host seed must retain its reservation from creation until checked
deletion. Normal completion leaves no seed or stale reservation. Following
controller death, subsequent Tool admission recovers the owned storage before
releasing capacity, without replaying its effect. Failed deletion, identity
uncertainty and ambiguous native launch retain the charge. A new boot settles
old processes but does not establish file deletion. Recovery resets a positively
identified failed native unit before retirement. A missing or not-loaded unit
cannot settle an interrupted launch request; such a reservation can exhaust
capacity until positive native evidence or a later boot permits recovery.

Linux reservations use version 2; macOS reservations remain version 1. An
immutable locked reservation owns one bounded version-1 seed record and at most
one publication temporary. The planned location is committed before exclusive
creation, and the checked directory identity before copying payload. A dead
preparation without a committed directory identity permits only empty-directory
removal. The launch-requested phase precedes launcher invocation; positive native
termination precedes recursive deletion. Record updates never replace the locked
lease inode. Metadata retires only after synchronized seed absence. Deletion
rejects links, mounts, foreign ownership, unsafe permissions and replacements;
incomplete cleanup retains its slot and prevents successful completion.

Historical unrecorded roots require an explicit maintenance procedure with
Arany stopped, positive native inactivity and authorization of exact inventoried
objects. Reboot before restarting Arany is the fallback for uncertain inactivity.
Deletion revalidates object identity; a name prefix never authorizes removal.
Normal admission neither migrates nor automatically deletes unsupported records.
Historical cleanup completion applies to the selected host/account inventory;
follow the [one-time maintenance procedure](../tools.md#historical-linux-seed-maintenance).

## Linux enforcement and compatibility

The first enforcer requires installed non-set-ID, root-owned `/usr/bin/bwrap`, `/usr/bin/unshare`, `/usr/bin/systemd-run`, `/usr/bin/systemctl` and `/usr/bin/env`, working unprivileged user namespaces, a systemd user manager and cgroup v2 with effective memory/swap/PID/CPU limits and owned cgroup termination. `systemd-run` must support `--expand-environment=no`; installation alone does not establish compatible launch capabilities. A fixed `/usr/bin/unshare --user --map-current-user --mount --propagation private` bootstrap runs before the unchanged Bubblewrap command, without `--keep-caps`: util-linux establishes private propagation before exec, while retained capabilities make Bubblewrap reject a non-root real UID ([unshare source](https://github.com/util-linux/util-linux/blob/v2.39.3/sys-utils/unshare.c), [Bubblewrap privilege admission](https://github.com/containers/bubblewrap/blob/main/bubblewrap.c)). Before Ready and again after GO, the helper parses its bounded namespace-local `/proc/self/mountinfo` and rejects ambiguous roots, stacked or missing runtime aliases, propagation metadata, and any descendant mount beneath a selected alias. This prevents a later host mount from propagating into an admitted runtime. This is a kernel/native-capability contract, not a GNOME/KDE dependency. Other Linux init systems need a separately enforcing adapter; Arany does not pretend the present implementation works without these facilities. Unsupported architecture or missing enforcement fails closed, with no unsandboxed fallback. macOS uses the separate native profiles described above.

The Guard verifies isolated mount/network/PID/user/IPC/UTS namespaces, dropped capabilities, no-new-privileges, inherited syscall filters and effective limits before releasing a one-use intent. It owns the exact transient unit, invocation and retained cgroup identity, including double-fork/setsid descendants, and verifies termination before accepting completion. Normal transient-unit garbage collection is handled using the retained group's empty/removed state. The compiled seccomp deny profile is defense in depth, not a universal syscall allowlist. ARM64's profile is source-supported; native runner evidence is required before claiming platform support.

Byte/entry bounds on synchronous local snapshot preparation do not guarantee a finite response from stalled host storage or an uninterruptible kernel operation. Process deadlines and cleanup bound ordinary payload execution; they are not hard-real-time or kernel-failure guarantees. Native file preconditions do not make a multi-file transaction or exclude concurrent hostile host mutation.

## Acceptance scenarios and evidence owners

| Scenario | Required observation | Existing owner |
|---|---|---|
| Shared native capacity, settings and FIFO | Simultaneous first creation succeeds; eight default units and 32 registered waiters remain bounded across processes; overflow rejects before launch; cancellation, expiry and dead heads advance FIFO without newcomer overtaking. Valid settings change admission and invalid ranges fail closed; a queued macOS head can recover capacity abandoned during its wait. | [Shared admission corpus](../../src/tools/guard/admission.rs) |
| Retained Linux failed native unit | The exact failed transient job has no payload/cgroup; recovery resets its failure before reclaiming capacity. Unknown startup is retained. | [Native retained-unit owner](../../src/tools/guard/linux.rs) |
| Linux host seed lifecycle | Normal cleanup and safe next-admission recovery remove the seed before capacity release. Real controller SIGKILL, preparation/cleanup interruption, previous-boot files and competing recovery retain exclusive ownership; unsafe deletion and uncertain launch keep capacity charged without Tool replay. | [Admission corpus](../../src/tools/guard/admission.rs), [native Linux recovery owner](../../src/tools/guard/linux.rs), [Tool journeys](../../tests/session_run/tools.rs) |
| Historical Linux seed cleanup | Under the maintenance procedure, exact approved inactive roots and obsolete reservations are removed; replacements, live leases, foreign roots, settings and unrelated data remain untouched. Partial cleanup is reported; final inventory proves the selected objects absent. | [Admission maintenance corpus](../../src/tools/guard/admission.rs), [Tool guide procedure](../tools.md#historical-linux-seed-maintenance) |
| Folder admission before account setup | Every attached entry resolves folder access first, including without an account and with explicit flags. Ctrl+C enters no account flow and creates no Session. Read-only/dismissal asks again next time; remembered trust skips the prompt only for the same directory identity. macOS retains ordinary chat after consent and admits granted Tools only after native protection attests. Explicit `--tools` discloses retained private-config authority at the choice. An initially unconfigured trusted conversation restores its displayed approval mode when a supported native Provider is selected. | [Startup consent journey](../../tests/session_run/startup_trust.rs), [native macOS attached composition](../../src/cli/attached/run.rs) |
| Runtime-profile consent and pinning | Detected toolchains are metadata-only candidates; none is enabled by folder trust alone. Initial consent and explicit `/permissions` selection discover candidates; remembered trusted startup does not enumerate unrelated optional manager directories. Bounded discovery failure is local to the chooser. The user explicitly selects exact Rust, Node or Python roots on the consent screen. Optional Cargo/npm/pip cache subtrees require separate opt-in disclosure, never include configs/credentials or parent cache directories, and remain read-only. `/permissions` can revisit/add/remove selections; stale roots require explicit removal or identity re-admission. A private-cache canary proves there is no automatic load and that explicitly requested output can reach the observation, Event and a later Provider request. Identity drift rejects process dispatch; remembered profiles do not add host runtime authority to typed file/Skill profiles. Unsupported trust-record versions reject before payload dispatch. | [Workspace trust corpus](../../src/tools/workspace.rs), [startup consent journey](../../tests/session_run/startup_trust.rs), [command/cache canary journey](../../tests/session_run/tools/native_exec.rs), [Linux profile/mount policy](../../src/tools/guard/linux.rs), [native macOS command profile](../../src/tools/guard/macos/unit.rs), [Guard profile digest and identity checks](../../src/tools/guard.rs) |
| Runtime mount topology | Linux launches the exact production env/unshare/Bubblewrap prefix from a non-root mapped UID without retained capabilities. It attests a normal private runtime alias before GO and again after a late mount on the shared source, which remains absent. A pre-existing nested mount rejects at the explicit runtime-mount bootstrap stage. The actual topology capture binds pidfds for the namespace init and both exact-filter workers before releasing their gates; success, nonzero status, output failure and deadline failure all wait for every bound process to exit before assertions or fixture cleanup. Any missing identity or exit retains the fixture. A separate forced outer timeout proves the kernel-owned namespace init and held descendant exited through pidfds bound to their self-reported host identities before cleanup. macOS discovers a mounted image beneath a selected runtime root through its native mount table and rejects before Ready/GO; an ordinary selected-runtime command reaches the post-Seatbelt attestation. | [Linux native mount fixture](../../src/tools/guard/linux.rs), [native macOS command journey](../../tests/session_run/tools_macos.rs), [Guard stage and attestation](../../src/tools/guard.rs) |
| Native macOS attached capability | Remembered trust offers file/Skill Tools, the pinned native shell and approval controls while ordinary/resumed greetings retain history without effect Events; stale executable pins reject before inference and preserve the committed prefix | [Native macOS attached composition](../../src/cli/attached/run.rs) |
| Private grant and project discovery | Only selected roots and `.agents/skills` contribute pinned guidance; private names take precedence; unsafe paths and overflow reject | [Tool configuration and discovery corpus](../../src/tools/tests.rs), [native Skill journey](../../tests/session_run/tools.rs) |
| Lock present without installed project Skills | Startup advises `npx --yes skills@1.7.0 experimental_install` without reading lock contents or running an installer; an existing empty directory produces no notice | [Discovery corpus](../../src/tools/tests.rs), [attached notice journey](../../tests/session_run/slash_completion.rs) |
| File Read/Search and digest-bound Edit/Write | Named regular files work under whole-project trust; aliases, special files, stale preconditions and excess bounds reject; exact mutation matches receipt | [Workspace corpus](../../src/tools/tests.rs), [native Tool journeys](../../tests/session_run/tools.rs) |
| Command changeset publication and fresh snapshots | Created, modified and deleted files publish only after a matching integration receipt; a subsequent command sees the integrated bytes/deletions, including executable bits and binary contents | [Workspace corpus](../../src/tools/tests.rs), [native Linux journey](../../tests/session_run/tools.rs), [native macOS journey](../../tests/session_run/tools_macos.rs) |
| Changeset rejection and overflow | Links, special/set-ID/excluded paths, unsupported type conversions, grant denial and byte/count overflow reject the entire set; no sibling file publishes | [Workspace corpus](../../src/tools/tests.rs), [native Tool journeys](../../tests/session_run/tools.rs), [native macOS journey](../../tests/session_run/tools_macos.rs) |
| Changeset approval, conflict and cancellation | Denied or cancelled integration approvals leave all files unchanged; changing an expected file during approval rejects all sibling changes. Closed replay retains both stages and cannot repeat effects | [Native approval journeys](../../tests/session_run/tools.rs), [native macOS journey](../../tests/session_run/tools_macos.rs), [Shared Tool replay corpus](../../tests/session_run/tools/replay.rs) |
| Command/MCP isolation and protocol rejection | Snapshot isolation, network denial, owned descendants and limits hold; malformed frames, schemas, drift and unsupported content reject without fallback. MCP writes remain disposable | [Native Tool journeys](../../tests/session_run/tools.rs), [MCP fixture](../../tests/fixtures/mcp-tools.py) |
| Native macOS file/Skill Guard | Read, Search, List, Mkdir, create/replace Write, Edit and pinned Skill data execute through the real worker; stale create/edit and linked-file access reject; exact file bytes, receipts and closed replay agree | [Native file journey](../../tests/session_run/tools_macos.rs), [Workspace corpus](../../src/tools/tests.rs) |
| Native macOS profile isolation and CPU expiry | Unselected host metadata/data, ancestor listings, network/process launch and CPU-control changes reject under the compiled file profile; an activated owned worker terminates through the immutable CPU limit | [Native profile corpus](../../src/tools/guard/macos.rs) |
| Guard task framing | Truncated, malformed and oversized task frames reject before Ready and effects; the exact-capacity valid frame reaches Ready but still requires GO | [Native refusal corpus](../../tests/session_run/tools_macos.rs) |
| Native macOS owned unit and supervised resources | Escaped orphan descendants remain owned; stale audit tokens cannot signal another incarnation; aggregate memory/task excess and per-process CPU excess with ignored SIGXCPU terminate the complete unit; cleanup confirms the coalition empty and the job absent | [Activated native unit owner](../../src/tools/guard/macos/unit.rs) |
| Native macOS private-storage lifecycle | Both private filesystems enforce their 64-MiB capacities through actual `ENOSPC`; storage survives supervisor termination for clean detachment; partial setup and unwind clean owned mounts; unsafe targets remain untouched and uncertain cleanup retains its control root | [Activated native unit owner](../../src/tools/guard/macos/unit.rs) |
| Native macOS crash recovery | Abrupt controller death during real attachment releases its lock; admission terminates escaped descendants and the live attachment worker, removes the job, detaches positively owned images and permits another unit. Real recovery-detach cancellation retains records/ownership and succeeds on retry, including an interrupted-attachment marker. Empty inventory without prior observation retains uncertain roots. Live/foreign roots, malformed/missing records, identity drift and unexpected mounts remain untouched; invalid records do not consume valid-candidate attempts; cross-boot recovery never signals stale identities | [Activated native unit owner and refusal corpus](../../src/tools/guard/macos/unit.rs), [disk inventory corpus](../../src/tools/guard/macos/storage.rs) |
| Native macOS command/MCP lifecycle | Ordinary native child launches, including bursts of short-lived children, succeed with current selected snapshots and validated command changesets; MCP changes remain disposable; unselected host metadata, ancestor listings, network/signals/privilege changes reject; commands retain signal compatibility without inheriting the file-worker CPU limit; bounded protocol/output failures retain receipts; approval cancellation and missing post-GO completion retain closed replay without retry | [Native Engine journey](../../tests/session_run/tools_macos.rs), [activated native unit owner](../../src/tools/guard/macos/unit.rs) |
| Approval, denial and uncertain review | Only a granted immutable action can dispatch once; failed/uncertain AI review asks a human; denial/cancellation stops and closed replay agrees | [Native approval journeys](../../tests/session_run/tools.rs), [approval policy](../../src/tools/approval.rs) |
| Native macOS catalogue reuse | Repeated commands/MCP in one admitted Run reuse the signature-checked catalogue; stale metadata, catalogue tampering and bound overflow reject before dispatch; privileged system programs remain excluded | [Native catalogue corpus](../../src/tools/guard/macos.rs), [native refusal and Engine journey](../../tests/session_run/tools_macos.rs) |
| Uncertainty, cancellation, receipt bounds and fresh resumed work | Missing post-GO completion remains uncertain; bounded receipts persist; fresh inspection/edit does not execute old intents | [Native Tool journeys](../../tests/session_run/tools.rs), [Tool corpus](../../src/tools/tests.rs), [replay](../../src/store/replay.rs) |
| Slash Skill insertion and file mentions | Browsing loads no guidance or effects; selecting stages a task; file includes carry path/digest and directory references grant no authority | [Terminal contract](./terminal.md), [slash journey](../../tests/session_run/slash_completion.rs), [Engine Run corpus](../../tests/engine_run.rs) |

CI runs the debug-only fixture journeys in the debug macOS profile; the release profile omits the debug-only account-root and Keychain fixtures. The activated unit owner and CPU-expiry gate run under `--cfg arany_ci` in both profiles. Native Linux and macOS owners exercise their actual enforcers with synthetic Providers; pure wire/configuration/replay corpora do not launch effects. Live model/account loops, optimized macOS Engine/account isolation, Linux ARM64 and human terminal/accessibility checks need separate evidence. [Next steps](../../NEXT_STEPS.md) owns open checks; completed outcomes belong in the handoff and Git/CI history; [dependency review](../security/local-tool-dependency-review.md) owns scoped supply-chain evidence. Activation and test-lane requirements remain in [testing rules](../../agents/testing.md).
