# macOS Guard host metadata and ancestor listings

Status: corrected in source; native regression owners are the
[file profile](../../../crates/arany/src/tools/guard/macos.rs) and
[process unit](../../../crates/arany/src/tools/guard/macos/unit.rs).
Severity: medium privacy exposure; release-blocking for the stated offline
selected-root isolation contract.

The Seatbelt templates granted unrestricted `file-read-metadata`, and generated
ancestor rules granted `file-read-data` on directory ancestors. An authorized
command or MCP server could inspect unselected host metadata and enumerate
`/private/tmp`, including other units' directory names. File content access was
still restricted; this finding does not establish arbitrary host content reads.

The attacker controls an admitted command or MCP payload. Authority originates
in private user grants and the immutable Tool intent, not payload text. Assets
include unselected Workspace/account-path metadata and other units' names.
The boundary is the separate Seatbelt worker and its inherited descendant
profile, not the trusted launchd supervisor or the terminal process.
The command profile retains a literal `/` directory read for the native loader;
top-level names remain visible, without access beneath them. Removing this
exception aborts ordinary native launch; it is not a general ancestor grant.

The [native adapter](../../../crates/arany/src/tools/guard/macos.rs) grants metadata only to
the fixed runtime traversal roots and exact required ancestors. Ancestors grant
no directory data. Selected Workspace, private snapshot and admitted runtime
rules retain their existing scopes and bounds. Resource constants and profile
templates enter the policy fingerprint; per-invocation grants remain bound by
the normalized intent. No network, credential or process authority is widened.

Absolute Tool filesystem admission uses native search-only ancestor handles on
macOS, then a readable selected directory. Readable ancestor handles would fail
under these rules and tempt a return to directory-listing grants. Component
no-follow checks remain in force; no unsafe filesystem wrapper is added.
Payload/MCP preparation receives the control root validated before profile
installation; post-sandbox getcwd can require hidden ancestor directory reads.

The existing file-profile and activated unit owners now assert denied metadata
for an unselected file and denied ancestor/control-root listings alongside
successful admitted I/O and ordinary command/MCP launch. Check results belong in
the implementation handoff and CI; [next steps](../../../NEXT_STEPS.md) owns open
platform and recovery verification.
This does not claim protection from a privileged host process or automatic
recovery of an uncertain mounted root after a crash.
