# Guarded public-registry fetch

## Metadata

- Status: `Draft` (owner freeze remains in force)
- Owner: Fernando Mirabile
- Last updated: 2026-10-22

## Outcome and current boundary

Let an agent retrieve a dependency without giving project code general network
access. This is the proposed A6 feature; current [commands and build cache](../../docs/specs/tools.md)
remain offline. [NEXT_STEPS](../../NEXT_STEPS.md#frozen-areas) owns the freeze.

## Proposed direction

Use two phases: a fixed admitted toolchain fetch command reaches public registries
through a Guard-owned proxy and writes into the existing cache; manifest/lockfile
changes return through validated changesets. Build/test then runs offline.
Candidates are `cargo fetch`, `npm install --ignore-scripts` and
`pip download --only-binary=:all:`; their exact safety and supported workflows
need primary-source verification before admission. Project scripts never run
with network access and private registries/tokens are excluded.

The proxy admits HTTPS CONNECT only to a user-owned global allowlist, resolves
DNS itself and rejects loopback/private/link-local addresses. Proposed defaults
cover crates.io, npm and PyPI; exact endpoints and redirect/DNS policy need
research. Bound bytes, connections, DNS work and total time per fetch; overflow
refuses. Ask/Accept edits require approval; any future Auto path needs the
accepted permission policy, never a Supervisor grant.

Linux would route a private network namespace only to the proxy; macOS would
allow only the proxy port through Seatbelt. These are future enforcement designs,
not current capabilities. Unsupported enforcement fails closed. Credentials
never enter the proxy/payload. Run the [network security pass](../../agents/security.md)
before changing Tools/Guard boundaries.

## Slice, scenarios and evidence

After lifting the freeze, admit one registry/toolchain end to end: typed grant,
Policy, journaled intent, confined fetch, bounded cache/changeset receipt and
visible result. Tools owns the contract; existing Guard/command and Engine/replay
corpora must cover fetch followed by offline build, SSRF/host refusal, install
script refusal, quota/time overflow, cancellation and unknown cleanup without
retry. Repeat natively on Linux and macOS before support claims.

## Open questions and exit

Owner: Fernando Mirabile. Resolve exact registry hosts, fetch executables/flags,
proxy protocol/process ownership, numeric limits, DNS rebinding defense and
cache publication/recovery before implementation. Research Claude Code sandbox
and Codex network-access mechanisms under [PLANNING.md](../PLANNING.md).
Close only after accepted success/refusal/recovery behavior moves to Tools and
unperformed host checks move to NEXT_STEPS.
