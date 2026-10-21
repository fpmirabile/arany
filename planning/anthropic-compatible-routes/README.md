# Anthropic-compatible custom routes

## Metadata

- Status: `Draft`
- Owner: Fernando Mirabile
- Last updated: `2026-10-21`

## Outcome and scope

One feature: a user-selected `custom:NAME` profile can declare protocol
`anthropic-messages` and an explicit base URL, pass data-free conformance, and
run through Arany's existing Anthropic Messages adapter. This extends the
harness's admitted Provider destinations without changing execution policy.
Ship recipes for DeepSeek API keys, Kimi platform API keys and Kimi Code
subscription console keys, and GLM standard API keys.

Kimi Code is included with Arany's honest User-Agent. GLM Coding Plan benefits
are excluded unless Z.ai approves Arany; DeepSeek has no documented subscription
route. Do not reuse vendor CLI OAuth clients. Saved vendor accounts, new native
Provider identities, Chat Completions, Responses compatibility work, and custom
Workspace Tool/image enablement are outside this feature. Reusing Messages
`tool_use` for delegation does not grant Workspace Tools.

## Research

[DeepSeek, Kimi and GLM research](../../docs/research/deepseek-kimi-glm-providers-2026-10.md)
owns the dated vendor endpoints, models, authorization terms, peer source
comparison, alternatives and unverified live behavior. Its conclusion replaces
the input investigation's separate Chat/Responses adapter proposals.

The following Arany facts were checked with `rg` against this worktree; line
citations refer to the planning baseline, not proposed implementation:

| Verified fact | Source |
|---|---|
| Custom configuration accepts document version 1, only `openai-responses` / `json_schema`, and a canonical endpoint ending `/responses`; credential references are `ARANY_PROVIDER_*`. | [custom.rs:53](../../crates/arany/src/provider/custom.rs), [custom.rs:75](../../crates/arany/src/provider/custom.rs), [custom.rs:111](../../crates/arany/src/provider/custom.rs), [custom.rs:208](../../crates/arany/src/provider/custom.rs), [custom.rs:218](../../crates/arany/src/provider/custom.rs) |
| Custom transport currently imports OpenAI wire functions; its evidence fingerprint hardcodes the protocol and encoding. | [custom/transport.rs:5](../../crates/arany/src/provider/custom/transport.rs), [custom/check.rs:196](../../crates/arany/src/provider/custom/check.rs) |
| Custom admission resolves the destination and compares evidence before reading its environment key; invocation rejects Workspace Tools and images. | [custom/adapter.rs:16](../../crates/arany/src/provider/custom/adapter.rs), [custom/adapter.rs:76](../../crates/arany/src/provider/custom/adapter.rs) |
| The native Anthropic transport fixes `/v1/messages`, sends Bearer auth and `anthropic-version`, and separately supports an Anthropic workspace header. | [anthropic.rs:15](../../crates/arany/src/provider/anthropic.rs), [anthropic.rs:196](../../crates/arany/src/provider/anthropic.rs) |
| Native Messages already has streaming assembly, request/tool-use encoding, opaque thinking handling and usage decoding. Run outcomes use text or native functions; compaction still requests structured JSON via `output_config`. | [anthropic/wire.rs:20](../../crates/arany/src/provider/anthropic/wire.rs), [anthropic/wire.rs:420](../../crates/arany/src/provider/anthropic/wire.rs), [anthropic/wire.rs:551](../../crates/arany/src/provider/anthropic/wire.rs), [anthropic/wire.rs:594](../../crates/arany/src/provider/anthropic/wire.rs), [anthropic/wire.rs:614](../../crates/arany/src/provider/anthropic/wire.rs) |
| Checks clear old evidence, run direct/delegate/compaction probes plus each declared effort, and only publish complete success; the aggregate deadline is 180 seconds. | [custom/check.rs:12](../../crates/arany/src/provider/custom/check.rs), [custom/check.rs:31](../../crates/arany/src/provider/custom/check.rs), [custom/check.rs:73](../../crates/arany/src/provider/custom/check.rs) |
| Saved-account CLI access is native-only; custom routes cannot use a saved native account. | [cli/provider.rs:133](../../crates/arany-cli/src/cli/provider.rs), [cli/provider/select.rs:84](../../crates/arany-cli/src/cli/provider/select.rs) |
| `store::migrate` has schema/Event registries, but no JSON-record registry. `ProviderProfiles` is a bounded 64 KiB read-only record (`pending: None`), and ordinary reads do not migrate it. | [store/migrate.rs:31](../../crates/arany/src/store/migrate.rs), [store/state.rs:115](../../crates/arany/src/store/state.rs), [store/state.rs:160](../../crates/arany/src/store/state.rs) |

Editing/evidence rules: [security](../../agents/security.md#security-pass-triggers),
[testing](../../agents/testing.md), [spec workflow](../../docs/specs/README.md),
and [runtime architecture](../../docs/architecture/system-overview.md#2-runtime-architecture).
The existing [automatic state migrations plan, slice 4](../state-migrations/README.md#vertical-slices)
owns the missing JSON migration mechanism. This plan depends on that slice;
it does not introduce a second migration framework.

## Contract and scenarios

- Owning specs: [Provider custom profiles](../../docs/specs/provider.md#custom-destinations-and-routing),
  [structured outcomes](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding),
  [conformance commands](../../docs/specs/provider.md#commands), and
  [saved API keys](../../docs/specs/credentials.md#account-scope-and-identity).
  S1 documents the new protocol, schema/migration, authentication and evidence
  binding. Credentials explicitly distinguishes custom environment keys from
  native saved accounts, including subscription-issued Kimi console keys.
- Successful path: migrate the private profile document, admit the selected
  destination, read only its key, run `arany provider check custom:NAME`, and
  select the exact model for a normal Run. Direct answer, one-child delegation
  and compaction retain the existing semantic contract, journal ordering,
  usage accounting and safe presentation. There is no hidden discovery call.
- Reject unknown protocols/fields, mixed protocol/encoding fields, noncanonical
  or unsafe URLs, credential-origin conflicts, model drift, unsupported effort,
  absent/stale evidence, and changed DNS before Workspace disclosure. A failed
  recheck removes admission; protocol/base-path/auth changes require a new check.
- Reject malformed/truncated/multiple tool calls, bad stop reasons, missing
  required usage, reflected credentials, over-bound streams and invalid strict
  compaction. No JSON repair, alternate wire fallback or synthesized usage.
  Cancellation drops buffered text, stops further dispatch and preserves unknown
  remote usage. Reopen/show/replay never dispatch a Provider request.
- Evidence owners: extend the [custom process journey (custom.rs:3)](../../crates/arany-cli/tests/session_run/custom.rs),
  [custom checker corpus](../../crates/arany/src/provider/custom/check.rs),
  [Anthropic wire corpus](../../crates/arany/src/provider/anthropic/wire/tests.rs),
  and [routing journeys](../../crates/arany/tests/engine_run/routing.rs).
  Synthetic fixtures prove the admitted contract, not vendor acceptance. The
  owner runs billable live checks with personal keys; pending evidence remains
  in [NEXT_STEPS](../../NEXT_STEPS.md#daily-use-first).

## Chosen direction

- Simplest sufficient solution after Ponytail: extend the existing closed
  custom protocol dispatch and share the native Messages encoder, stream
  decoder, tool-use handling and usage logic. Keep the admitted custom HTTP
  client and custom identity/provenance; parameterizing native transport must
  not bypass custom admission or impose native Claude model validation.
  Add no vendor Provider enum variants, dependencies, crates or plugin system.
- Proposed profile document version 2 uses protocol-specific closed entries:
  Responses retains `endpoint` and `outcome_encoding: json_schema`; Messages
  requires `base_url` and `outcome_encoding: native_tools`. Both retain explicit
  model, credential reference, privacy, output cap and declared efforts. Messages
  also requires closed `auth: bearer | x-api-key`, bound to evidence. For
  Messages, remove at most one terminal slash and append `/v1/messages` to the
  admitted base path, rather than URL-joining a leading slash that discards it.
  Reject an already suffixed Messages endpoint and mixed `endpoint`/`base_url`.
  Bind protocol, base URL, derived endpoint, encoding and any typed wire options
  into evidence; advance the check contract so old evidence cannot authorize it.
- Reuse native Bearer auth or the narrowly typed `x-api-key` alternative:
  DeepSeek uses its documented `x-api-key`, Kimi platform uses Bearer, Kimi Code
  uses `x-api-key`, and GLM uses Bearer. Send a compiled
  `User-Agent: arany/<version>` on check, Run and compaction requests. No arbitrary
  headers, borrowed client identity or ambient Anthropic workspace header.
  A demonstrated vendor wire difference earns only a closed typed option at
  the adapter edge, bound to evidence and tested in its slice. No generic
  vendor parameter bag or silent endpoint-dependent behavior.
- An empty effort list requests vendor-default behavior without sending
  Anthropic's native Claude effort default. Declared efforts must have an
  evidenced wire mapping and pass their probes. Preserve the strict compaction
  contract and decoder; a vendor that cannot satisfy it remains unadmitted.
  DeepSeek explicitly lacks documented `output_config.format` support; resolve
  the open compaction question before treating S1 as implementation-ready.
- Ownership: custom admission owns normalized destination, credential reference
  and evidence; the adapter owns external wire translation and transient opaque
  turns; Engine/reducer retain semantic validation, budgets and durable facts.
  The existing in-process Provider seam has concrete Responses and Messages
  adapters. External JSON/HTTP types stay behind it. Failures retain typed
  status/stream distinctions; cancellation and deadline handling remain shared.
- Linux/macOS: common Rust/network behavior, no new native facility. Platform
  TLS trust and private-file admission keep their existing owners. Native
  support needs platform evidence; this documentation change establishes none.

## Security and resource model

Triggered review: new Provider destinations, credential use, cost surface and
persisted format. Assets are selected keys, Workspace/context data, evidence
and journal integrity. Authority comes only from private user configuration,
current conformance and existing grants. Profile strings, DNS, vendor responses,
repositories and model text are attacker-controlled across their respective
boundaries; none can select or widen a route, credential, grant or child authority.

Normalize and admit the complete base path and derived Messages URL before
credential access or Workspace loading. Retain HTTPS outside numeric loopback,
platform certificate/name verification, public-address checks, DNS pinning and
re-resolution. Refuse redirects, proxies, URL credentials/query/fragment,
ambiguous paths and destination drift. Isolate environment references by origin;
recipes use different references for every vendor and Kimi billing route, even
if the user could obtain identical key bytes. Never load saved native keys as
custom credentials or forward a native Anthropic workspace header.

Only the admitted request carries its selected secret. Keep reflection checks
before preview and across decoded responses; no keys in repo files, examples,
fixtures, Events, diagnostics, telemetry or PR artifacts. Kimi's User-Agent is
an honest identifier, not authorization; never reuse kimi-cli's OAuth client ID.
Z.ai's technical endpoint compatibility does not authorize Coding Plan benefits.
Neither TLS nor a conformance pass proves ZDR, general DLP or vendor terms compliance.

Retain these bounds, verified in [provider.rs:58](../../crates/arany/src/provider.rs),
[provider.rs:179](../../crates/arany/src/provider.rs),
[provider.rs:249](../../crates/arany/src/provider.rs),
[custom/destination.rs:8](../../crates/arany/src/provider/custom/destination.rs),
[custom/transport.rs:234](../../crates/arany/src/provider/custom/transport.rs),
[custom/transport.rs:31](../../crates/arany/src/provider/custom/transport.rs),
[custom.rs:139](../../crates/arany/src/provider/custom.rs),
[custom.rs:218](../../crates/arany/src/provider/custom.rs),
and [streaming.rs:151](../../crates/arany/src/provider/streaming.rs):

- 32 profiles / 64 KiB document, 512-byte URL and 256-byte path (apply to base
  and derived URL), 512-byte key, eight admitted DNS addresses / five-second
  lookup, ten-second connection and 120-second custom call deadline.
- 512 KiB request, 1 MiB uncompressed response/stream, 8,192 SSE events,
  262,144 JSON nodes / depth 64; one-slot bounded preview, no unbounded queue.
- Custom output cap 4,096 tokens; positive labelled input/output usage and valid
  cache subsets, with absent optional cache fields remaining unknown. Missing
  required counters reject. Check aggregate 180 seconds, no retries, bounded
  probes, evidence expires after 24 hours ([custom/check.rs:12](../../crates/arany/src/provider/custom/check.rs)).
- Concurrency one, as required by the [custom contract](../../docs/specs/provider.md#custom-destinations-and-routing);
  normal calls share Goal charging/deadlines and the
  [bounded retry contract](../../docs/specs/provider.md#usage-provenance-and-reliability).
  A 429 without `Retry-After` uses the existing bounded backoff, never a vendor
  retry loop. Overflow, cancellation or timeout ends the call without accepting
  partial output. Provider work creates no new Guard/descendant authority;
  Workspace effects remain subject to existing per-effect isolation.

## Risks and mitigations

- Additive protocol values still change persisted format under AGENTS rule 5 →
  bump the profile document to version 2, ship the registered v1 → v2 transform
  and fixture via the JSON-record migration path in `store::migrate`, and keep
  only the v2 ordinary decoder. Preserve existing Responses semantics; do not
  reset user settings or add a compatibility reader. The JSON slice must first
  provide safe replacement for this currently read-only record kind.
- Vendor “Anthropic compatible” does not establish strict compaction, effort,
  tool-use or usage fidelity → per-slice fixtures and exact live checks; omit
  unsupported effort choices and leave failing routes unadmitted. Do not repair
  responses or loosen a shared decoder to make a recipe pass.
- Subscription/API keys can select different billing on the same host →
  separate named profiles and credential references. S3 requires a newly issued
  **Standard API Key**, excluding legacy Compatible keys that can consume plan
  quota; see the [research](../../docs/research/deepseek-kimi-glm-providers-2026-10.md).

## Vertical slices

1. **S1 — Messages custom protocol and DeepSeek API recipe (one PR).** Depends
   on automatic-state-migrations slice 4, not merely the journal registry.
   Deliver v2 schema and migration → base-URL/DNS/credential admission →
   protocol-specific conformance → shared Messages adapter → Engine/journal →
   ordinary Run/show, with a DeepSeek recipe in `docs/setup.md` linked from the
   Provider contract. Base URL `https://api.deepseek.com/anthropic`; exact model
   profiles for `deepseek-flash` and `deepseek-v4-pro`, without Claude aliasing.
   Update provider.md custom profiles/conformance and credentials.md's custom
   key boundary. Tests extend the existing owners with migration success,
   interruption/invalid-version preservation, stale cross-protocol evidence,
   path preservation, credential canaries, direct answer, one child, compaction
   and no-dispatch replay. Pin vendor deltas: thinking blocks/signatures and
   tool-use continuation, ignored parallel-call suppression and cache controls,
   unsupported thinking budget fields, missing optional cache usage versus missing required counters,
   SSE keep-alive comments, and 429 without `Retry-After`. Test strict compaction
   `output_config` rejection and omission of native-only effort defaults;
   DeepSeek supports only `output_config.effort`, so the unchanged compaction
   request is a known obstacle. A supported equally strict representation needs
   primary-source evidence and a plan update before implementation; absent that,
   ship the protocol with DeepSeek marked unavailable. DeepSeek's long queue
   cannot extend the 120-second local bound. A usable slice includes the migrated
   existing Responses path as a regression case and owner live-check procedure.
2. **S2 — Kimi API and Kimi Code recipes (one PR, after S1).** Separate profiles
   and keys: platform base `https://api.moonshot.ai/anthropic` with `kimi-k3`
   (other documented platform models only after exact checks); subscription base
   `https://api.kimi.com/coding/` with the plan console key and a tier-visible
   model (`k3`, `k3-256k`, `kimi-for-coding` or
   `kimi-for-coding-highspeed`). Each recipe crosses profile/check → admitted
   Messages request → Run/delegation/compaction → journal/show, and updates
   setup plus the Provider/credential scenarios. Assert `arany/<version>` on
   every request, no kimi-cli identity/OAuth reuse, and no cross-billing key
   fallback. Pin thinking/reasoning field and signature handling on the
   Messages wire, strict compaction, optional versus required usage, and 429
   without a retry header. Out-of-tier 401 refuses without retry or API billing
   fallback. Offline fixtures establish the same path for both bases; owner
   live-checks each with its own key. Any necessary typed Messages delta ships
   with these tests in this PR, preserving native Anthropic regression evidence.
3. **S3 — GLM standard API-key recipe (one PR, after S2).** Base URL
   `https://api.z.ai/api/anthropic`; Standard API Key and exact `glm-5.3` /
   `glm-5.3-flash` profiles. Cross profile/check → admitted adapter →
   Run/delegation/compaction → journal/show, with setup and Provider/credential
   scenario updates. The [subscription terms](https://docs.z.ai/legal-agreement/subscription-terms)
   and [supported-tool restriction](https://docs.z.ai/devpack/tool/others)
   exclude GLM Coding Plan use by Arany; explicitly omit that recipe and ZCode
   OAuth. Revisit only with Z.ai approval. Tests pin Standard-key route isolation,
   Messages thinking/reasoning and continuation shapes, unsupported native
   effort/structured-output fields, missing optional cache usage versus absent
   required totals, bounded quota/business errors and 429 without `Retry-After`.
   Chat `reasoning_content` is not evidence of a Messages field: only documented
   Messages shapes earn adapter changes. Owner live-checks the exact standard
   billing route; failure leaves it unadmitted rather than using plan quota.

## Open questions

- **DeepSeek strict compaction — owner: Fernando Mirabile / S1 implementer.**
  The documented Messages endpoint supports only `output_config.effort`, while
  Arany requests a provider-enforced JSON schema for compaction (research and
  `anthropic/wire.rs:551` above). Is there a documented, equally strict Messages
  representation usable by the shared adapter? Resolve from primary sources
  and exact conformance before admitting DeepSeek; tool selection plus repaired
  JSON is not a substitute. If none exists, skip DeepSeek as requested and
  retain an explicitly unavailable candidate recipe. This is a material Plan
  gate, so the plan remains Draft. Kimi/GLM exact wire fidelity remains an
  explicit per-route conformance/live evidence gate, not an assumed capability.

## Exit criteria

- Each slice ships one independently usable PR with its owning spec/scenario
  updates, recipe and deterministic checks at the existing evidence owners.
  S1 includes the registered profile migration/fixture and guards; no ordinary
  legacy reader survives. Native Anthropic and custom Responses behavior stays
  covered. Local implementation verification follows the development guide;
  full code-change CI passes on Linux/macOS. This planning task runs no cargo.
- Observe success for direct answer, exactly one child and strict compaction;
  rejection for invalid destination, cross-route credential attempts, stale
  evidence, malformed/missing usage and over-bound/truncated streams; cancellation
  and reopened strict replay cause no additional dispatch or credential leak.
- Owner-run, potentially billable `arany provider check custom:NAME` passes for
  each advertised exact DeepSeek, Kimi platform, Kimi Code and GLM model profile,
  using the owner's own keys outside the repository. Record safe outcome and
  unresolved live/platform claims in NEXT_STEPS, never credentials or raw bodies.
  A pending check does not block synthetic implementation; it does block a live
  support claim. Failed routes are skipped until they meet the unchanged contract.
- No GLM subscription or borrowed OAuth identity is implemented. Kimi Code uses
  its console key with honest identity. Review dated terms again before enabling
  subscription use. Close the plan under the [planning lifecycle](../README.md#lifecycle),
  retaining unresolved owner checks in NEXT_STEPS and trimming absorbed research.
