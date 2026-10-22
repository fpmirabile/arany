# Claude Code MultiEdit: shipped-code evidence

Verified 2026-10-22 by reading official npm distribution files as data. No package was installed, imported or executed. Only selected source/type files were extracted from the versioned tarballs. No upstream code is reproduced here. Locations below are approximate lines in the original minified `package/cli.js`, with symbol names to locate the relevant implementation.

## Sources and removal boundary

- [Official npm 1.0.88 tarball](https://registry.npmjs.org/@anthropic-ai/claude-code/-/claude-code-1.0.88.tgz): `package/cli.js`, MultiEdit prompt `Bn2` around 969–1008; schema `Ux6` and tool `w$` around 1879; single Edit `ZY` around 1872–1879; transformer `kP` around 1815; permission helper `Bg` around 735; write helpers `Ny` around 736 and `rN` around 742. `package/sdk-tools.d.ts` also exposes `FileMultiEditInput`.
- [Official npm 1.0.128 tarball](https://registry.npmjs.org/@anthropic-ai/claude-code/-/claude-code-1.0.128.tgz): prompt `pWA` around 914; schema `Po4` and tool `GE` around 1147; single Edit `DY` around 1140–1146; transformer `JM` around 1044; catalog `iE` around 3337; write helpers `Sv` around 3608 and `fM` around 3614. MultiEdit remains enabled and registered in the normal catalog.
- [Official npm 2.0.0 tarball](https://registry.npmjs.org/@anthropic-ai/claude-code/-/claude-code-2.0.0.tgz): `package/cli.js` and `package/sdk-tools.d.ts` contain no MultiEdit. Edit's strict schema `jk2` around 1090 still takes one `old_string`/`new_string` pair and optional `replace_all`; `FileEditInput` around line 89 confirms the public shape. The internal array transformer `H31` around 1003 survives behind the single-edit wrapper. This establishes removal of the public batch Tool, **not** an array schema folded into Edit.
- [Official npm registry metadata](https://registry.npmjs.org/@anthropic-ai/claude-code): 1.0.128 is the final published 1.x version; published 2025-09-27, followed by 2.0.0 on 2025-09-29. Therefore 1.0.128 is the last release before this removal boundary. Intermediate tarballs 1.0.100 and 1.0.120 also retain MultiEdit. The code establishes what disappeared and when; it does not establish a product rationale for removal.

## Verified behavior

Unless a row distinguishes versions, both 1.0.88 and 1.0.128 have the behavior below. References are to the source locations above.

| Concern | Shipped behavior and evidence |
| --- | --- |
| Input | A strict object with string `file_path` and a nonempty `edits` array. Each strict edit has string `old_string`, string `new_string`, and optional boolean `replace_all`, default false. Neither array schema declares a maximum edit count. The path is described as absolute, but Edit validation also resolves relative paths. Schemas `Ux6` / `Po4`; shared Edit validation. |
| Ordering and intermediate results | `kP` / `JM` iterate in array order, replacing a working string, so the transformation itself uses the previous result. **However**, MultiEdit first invokes single Edit validation separately for every edit against the original file on disk. It also rejects a later nonempty `old_string` (after trimming trailing newlines) if it is a substring of an earlier `new_string`. Thus the prompt's unrestricted sequential claim is broader than the implementation: a later edit that consumes newly introduced text ordinarily fails validation or the substring guard. |
| Matching and uniqueness | Each original-file validation requires a match and rejects multiple nonoverlapping matches unless `replace_all` is true. Those uniqueness checks are not rerun against each intermediate string. The shared matcher accepts exact text first, then normalizes typographic quotation marks; MultiEdit's transformer still uses its supplied strings. This is not a pure exact-byte matching contract. `ZY` / `DY` validation; `it` near 1812 in 1.0.88; `kP` / `JM`. |
| `replace_all` | False replaces the first occurrence; true replaces all occurrences in the current working string. Replacement callbacks preserve replacement text literally rather than expanding JavaScript replacement tokens. An empty replacement can additionally consume the following newline when the searched string lacks a trailing newline. `we2` around 1812–1815 / `keA` around 1041–1044. |
| Equal strings | Every edit with equal old and new strings is rejected by single Edit validation before writing, including two empty strings. The transformer also rejects any edit that leaves the working string unchanged and a batch whose final result equals the original. `ZY` / `DY`; `kP` / `JM`. |
| Empty old string and creation | Single Edit validation permits empty old text for an absent file or an existing whitespace-only file, rejecting a nonempty existing file. The batch transformer treats empty old text as replacement of the entire working string. A one-edit creation is supported. Although the prompt advertises creation followed by ordinary edits, those later edits are still validated against the original absent file and fail. Creating an entirely empty file via equal empty strings is rejected by public validation despite an internal transformer special case. |
| Validation before writing | All individual Edit validations complete before the batch call. The full transformation completes before the target-content write; transformation failure does not write an earlier edit. Settings-file validation also checks proposed output where applicable. The call may perform file-history preparation before transformation. 1.0.128 additionally checks file-read state and modification time again in the call after its before-edit hook; 1.0.88 lacks this second call-time check. `w$` / `GE` validation and call; `ZY` / `DY`. |
| Atomicity | Logical edit failure is all-or-nothing for target contents: one full result is calculated, then one write helper is called. The helper writes a temporary sibling, flushes it, preserves existing mode, then renames it. **On failure it cleans up and falls back to a direct target write.** Consequently the code does not promise all-or-nothing target contents for every I/O failure or crash. `Ny` → `rN` / `Sv` → `fM`. |
| Permission handling | MultiEdit delegates once to Edit's permission check for the shared path, using placeholder old/new strings; it does not independently authorize each edit. It is marked mutating and not concurrency-safe. In 1.0.88, `Bg` checks edit-deny and edit-ask rules before special settings/command and Git-configuration prompts, then accepts eligible paths in `acceptEdits` mode or through an allow rule; otherwise it asks. MultiEdit does not create a distinct permission authority. `w$` / `GE`; `ZY`; `Bg`. |

## Errors

Representative exact messages in 1.0.88: “No changes to make: old_string and new_string are exactly the same.” and “At least one edit is required”. Longer messages are paraphrased below.

Single Edit validation supplies these errors to MultiEdit in 1.0.88 (`ZY`, around 1872–1879), retained in the corresponding 1.0.128 validator (`DY`, around 1140–1146):

| Error code | Meaning of emitted message |
| --- | --- |
| 1 | Old and new strings are identical; there is no change. |
| 2 | Project configuration ignores the directory. |
| 3 | Empty-old creation cannot replace an existing nonempty file. |
| 4 | The file does not exist; an alternative path/current-directory hint can be appended. |
| 5 | Notebook files must use NotebookEdit. |
| 6 | Read the file before writing it. |
| 7 | The file changed after it was read; read it again. |
| 8 | The search string is absent; the message includes that string. |
| 9 | Multiple matches with `replace_all` false; the message includes the count/string and requests either replace-all or unique context. |

The transformer separately reports a later old string contained in an earlier replacement, an edit that cannot change the working string, or a final file identical to the original. The schema reports that at least one edit is required. The 1.0.128 call-time freshness failure asks for another read. Permission denial identifies the path; permission requests identify the requested write or special protected configuration. These are descriptions of the emitted errors, not a proposed Arany error vocabulary.

## Consequences for the Arany plan

- Ordered same-file batches, a nonempty edit list, per-edit `replace_all: false`, equal-string rejection and delegation to existing edit permission machinery have direct Claude Code precedent.
- Unrestricted intermediate-result semantics agree with Claude's prompt but differ from its shipped validator and substring guard. The plan must record the owner's explicit sequential-semantics decision and the independently verified OpenCode precedent instead of claiming full Claude implementation parity.
- Existing-file-only scope and the 64-edit ceiling are owner decisions, not Claude defaults. Claude's public creation behavior and lack of a schema count ceiling must remain visible in the comparison.
- All-or-nothing behavior on edit-validation failure already exists in Claude's implementation. Arany's stronger publication/failure guarantees require its existing Guard, validated changeset and receipt path; Claude's non-atomic fallback cannot supply those guarantees. Do not add a separate generic transaction framework merely to imitate an advertised atomic Tool.

The requested npm-code evidence is conclusive for the behavioral comparison and removal boundary. Removal rationale is not inferable from these artifacts and is not needed to choose Arany's contract.
