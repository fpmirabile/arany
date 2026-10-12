# macOS system catalogue profile rules

Investigated 2026-10-12. This note assesses a narrower Seatbelt profile encoding,
not a performance result or a new enforcement contract.

## Primary-source evidence

Apple's installed profiles on macOS 26.6.2, build 25G83, use `require-any` with
multiple `literal` filters:

- `/System/Library/Sandbox/Profiles/application.sb`, lines 132–148, places a list
  of exact device paths inside `require-any`, then inside `require-not` and
  `require-all` for an `allow file*` rule.
- `/System/Library/Sandbox/Profiles/com.apple.storereceiptinstaller.sb`, lines
  16–35, places several exact file paths inside `require-any` within an
  `allow file-read* file-write*` rule.
- `/System/Library/Sandbox/Profiles/com.apple.searchdiagnose.sb`, lines 24–29,
  scopes `process-exec` through a `with-filter` whose `require-any` contains a
  single executable `literal`. Its separate `no-sandbox` modifier is unrelated
  and must not be copied.

These are local Apple-shipped source artifacts, not third-party descriptions.
The inspected files demonstrate composition syntax; they do not promise a
stable public DSL or establish Arany's runtime behavior.

Apple's first-party [WebKit WebProcess profile](https://raw.githubusercontent.com/WebKit/WebKit/main/Source/WebKit/WebProcess/com.apple.WebProcess.sb.in)
uses `require-any` to combine path filters under `require-all` for `file-read*`
(lines 86–94 in the inspected raw source). Its macOS `with-filter` macro gathers
a rule's existing filters and wraps their union in `require-all` with the extra
filter (lines 914–950). This supports the interpretation that `require-any`
expresses a union. It does not prove that different encodings compile to
identical policies or that one compiles faster. The linked `main` source is
mutable; the ranges identify the inspected version.

Apple's [App Sandbox documentation](https://developer.apple.com/documentation/security/app-sandbox)
describes resource restrictions through entitlements. It is not a specification
for custom Seatbelt `require-any` grammar or compiler complexity.

## Candidate and decision constraints

For a nonempty admitted catalogue, the candidate is one rule:

```scheme
(allow file-read* file-map-executable process-exec
  (require-any (literal "/bin/example") (literal "/usr/bin/example")))
```

Inference: unioning exactly the same path predicates for the same operation set
should preserve the intended catalogue allow set. This is a representation
change only: retain every independently emitted deny rule and its placement.
Do not substitute directory `subpath`, prefixes, regexes, or broader operations.

The [current macOS Guard](../../src/tools/guard/macos.rs) owns admission and
profile construction. Keep its no-follow directory admission, root ownership,
regular-file/link/mode checks, pinned device/inode/time stamps, signature and
entitlement checks, quoted paths, catalogue cap and profile byte cap. Keep
private-capability deny topology and required-path handling. Reject an empty or
invalid catalogue; never emit an unrestricted allow after removing its filters.
No cache or new FFI is necessary to investigate this encoding.

The sources justify a focused experiment, not a fix claim. Before adopting it,
use native evidence to establish admitted execution and meaningful denial cases
through the existing Guard security owner, then compare the same fixture's
profile/Ready stages. A shorter profile or a passing timeout alone cannot prove
enforcement equivalence or a compiler bottleneck. Linux and Windows behavior
are outside this macOS-only representation change.
