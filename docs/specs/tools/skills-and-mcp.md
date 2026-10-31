# Tools: Skills and MCP

## Portable Skills

Each `skills` entry contains a name, reviewed discovery description, absolute directory, exact relative file-to-SHA-256 map and an optional `disable_model_invocation` flag (default false):

```json
{
  "name": "review",
  "description": "Review the selected project code",
  "directory": "/absolute/reviewed/skills/review",
  "disable_model_invocation": false,
  "files": {"SKILL.md": "<digest>", "references/checklist.md": "<digest>"}
}
```

Project Skills come only from `.agents/skills/NAME/SKILL.md`, admitted by remembered whole-folder trust or an explicit grant whose roots contain `.agents/skills`; other directories, home Skills and untrusted folder contents supply none. Private entries win over duplicate project names; at most 64 names are admitted. Names, resources and digests are bounded and pinned anew at Run admission; links, special files and excessive discovery reject. Startup and `/permissions` refresh names; installation changes never renew a pinned Run's grants.

The model sees admitted names and bounded previews (literal frontmatter excerpts, descriptions truncated to 96 characters). A Skill flagged `disable_model_invocation` is hidden from the model and refused as not offered if proposed; explicit user invocation still works. A project Skill sets the flag only through literal `disable-model-invocation: true` frontmatter in its pinned `SKILL.md`; a private entry carries the reviewed flag. Resources load lazily.

`SKILL.md` is bounded UTF-8 text; the host never evaluates YAML and returns frontmatter as inert guidance. Identity and authority come only from the admitted name, directory and digest: `allowed-tools`, hooks, shell directives, forks and installation text are guidance, never grants, and no Skill body is a system instruction. Requesting a Skill executes nothing; its scripts run only as ordinary approved commands in the Guard, and Linux exposes admitted Skills read-only to them under `/skills/SKILL_ID`.

The [terminal completion contract](../terminal/commands.md#interactive-commands-and-completion) owns slash invocation. Before the first Provider call, an explicit invocation commits one ordinary `Skill { name, resource: null }` intent and its observation through the admitted Skill path; replay admits this Engine origin only for the accepted Skill, after definition parsing, once per Run. The observation enters that turn as `invoked_skill (untrusted)` data under the [64 KiB](../limits.md#max_tool_result_bytes) Tool result ceiling, context accounting and the credential-reflection check. Later Runs never replay the body; the accepted message records the Skill name separately from the verbatim objective. A failed read ends the Run with `tool_stopped` before Provider egress.

A root `skills-lock.json` without a `.agents/skills` directory produces a notice to run `npx --yes skills@1.7.0 experimental_install` (the [Skills 1.7.0 installer](https://github.com/vercel-labs/skills/blob/v1.7.0/src/install.ts)). Arany checks only whether the directory exists, never reads the lock and never runs the installer.

Skill text is read in process from the at most [64 Skill](../limits.md#max_skills) directories opened at admission; one that cannot be opened fails only its own reads for that Run. Each read serves only a listed resource matching its pinned digest, never following links or opening set-ID files. Text over the intent's result allowance is shortened at a UTF-8 boundary with the shared `…[N bytes, L lines omitted; B bytes, T lines total]…` marker, so `Succeeded` means verified, bounded text, not complete text; the marker is untrusted, since Skill content can contain it. A Goal plan names Skill resources exactly (`resource: null` means `SKILL.md`) and only narrows admitted files; a matching successful Skill receipt may satisfy a Goal success criterion.

Guard failures before dispatch report the closed failure stage and check, say when the cause is unknown, and give a recovery step: namespace-bootstrap failures point to user-namespace and Bubblewrap support, native-manager failures to the user service manager, and only native-protection and startup failures to the Guard executable and prerequisites. Project-selection timeouts, vanished inputs and conflicts are named at the selection boundary; the same errors from payload startup, capture or typed mutation keep their own stage. `/agents` renders the stage from a typed fact. Recovery text asks the user to resend the message, assuming no resumable Goal; only development builds mention `development.log`. No raw native error text, path or secret is shown, and nothing retries automatically.

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

The server gets the offline snapshot, runtime and descendant restrictions of commands and no Provider or account credentials; servers needing external APIs, credentials, writable host state or package installation are unsupported.

Arany implements the **2025-11-25 tools profile** (not the 2026-07-28 stateless profile) over stdio and, for [remote servers](#remote-mcp-and-oauth), streamable HTTP: `initialize`, exact version and capability negotiation, `notifications/initialized`, bounded `tools/list` pagination, schema validation, one `tools/call`, then disconnect. Server-to-client ping works; sampling, roots and elicitation get method-not-found. Notifications cannot trigger discovery, refresh permission or extend deadlines.

Discovery returns a digest of each admitted complete tool definition; a later call lists again and rejects before calling if the selected digest drifted. Arguments must be an object encoded as a JSON string that satisfies the admitted input schema. Schemas use the supported Draft 2020-12 or Draft 7 subset, bounded linear regexes and offline compilation; references, `$id`, remote retrieval and unsupported dialects reject. Calls never retry. Output admits bounded text, inert resource links (never fetched) and legacy object `structuredContent`; a declared output schema is enforced for successful results. Embedded resources, images and audio reject. `isError` is recorded as an observed failure.

### Remote MCP and OAuth

An `mcp` entry with `url` instead of `program` is a remote server: `{"name": "docs", "url": "https://mcp.example.com/mcp", "auth": "oauth", "tools": ["search"]}`. Exactly one of `program` and `url` is present, and a remote entry has no `args`. The URL follows the [`web_fetch`](./commands-and-cache.md#web-fetch-and-web-search) rules (HTTPS on the default port, no credentials or fragment; numeric loopback may use HTTP and any port). `auth` is absent (no credential) or `oauth`. Remote servers are Linux-only (configuration fails closed elsewhere) and primary-only: researchers neither see nor can call them (a guessed name is denied before any approval), and the parent reads no saved token for a call without the proxy class. The catalog row names the streamable HTTPS transport and the `arany mcp login` action.

A remote call runs in the Guard like `web_fetch` (`limits.network = proxy`), reaching only the server's host through the egress proxy, independent of the `network` grant. Framing is the stdio profile's bounded JSON-RPC over streamable HTTP: one `POST` per message with `Accept: application/json, text/event-stream` and `MCP-Protocol-Version`, the `Mcp-Session-Id` from `initialize` echoed, `202` for acknowledged notifications and replies, and responses as a JSON frame or an event stream of `data:` frames serviced as they arrive (a server may await its `ping` answer before the result). A response is at most [`MAX_MCP_RESPONSE_BYTES`](../limits.md#max_mcp_response_bytes) and 16 frames, each within the stdio frame bound; compressed, redirected and other content types fail the call. A frame whose decoded text contains the bearer token fails the call before capture, so the token never reaches Events, spills or the Provider. Once `tools/call` is sent, the outcome is [Uncertain](../execution/events-and-outcomes.md#effect-outcomes) (transport error, refused reply to a server `ping`, timeout, oversized, invalid or unrelated frame) unless the server refused with `401`/`403` or returned a well-formed JSON-RPC 2.0 response with the call's request ID. Local catalog, schema, digest and result bounds apply, and output stays untrusted. Server requests other than `ping` get method-not-found. There is no GET stream, resumption or session `DELETE`.

`arany mcp login <server>` authorizes an `auth: "oauth"` server; `arany mcp logout <server>` revokes and removes it. Login resolves protected-resource metadata (RFC 9728), then authorization server metadata (RFC 8414), falling back to the server origin. It requires an `issuer` equal to the discovered address, HTTPS (or numeric loopback) endpoints and PKCE `S256`. It registers a public client dynamically (RFC 7591; a server without a registration endpoint is unsupported), prints the authorization URL, accepts one code on a one-shot `127.0.0.1` callback (exact `Host`, path and `state`; at most eight connections and 180 seconds) and exchanges it with the PKCE verifier and the server URL as `resource` (RFC 8707). Every login request uses the [Provider destination policy](../provider/routing-and-usage.md#configuration-and-egress), follows no redirect, reads at most [`MAX_METADATA_BYTES`](../limits.md#max_metadata_bytes) and is never made for a model call.

The saved token never enters an intent, Event, digest or observation. A token within a minute of expiry renews through the saved refresh token:

- The renewal belongs to the triggering MCP effect: that intent commits first, and the renewal runs inside the action deadline.
- The grant moves from `ready` to `renewing` before the request; a logout or new login in between wins over the renewal result.
- A new token or rotated refresh token replaces the grant; a definite client refusal (4xx other than 408/429) marks it `rejected`; anything else (lost reply, timeout, 5xx, invalid reply, failed publication, cancellation, crash) leaves it `renewing`, so a refresh token is never submitted twice and only a new login clears it.
- An unconfirmed renewal makes the call [Uncertain](../execution/events-and-outcomes.md#effect-outcomes); later calls see the blocked grant and record `unauthorized` without a request.

A token is sent only to the URL its record names. A missing, expired-without-refresh, failed-to-renew or `401`/`403`-rejected token ends the call with ``{"server": "<server>", "error": "unauthorized", "action": "run `arany mcp login <server>` …"}`` and observation `Failed`; `mcp_list` and `mcp_call` never retry. A token is at most [`MAX_ACCESS_TOKEN_BYTES`](../limits.md#max_access_token_bytes); the credentials contract owns its [storage](../credentials.md#remote-mcp-tokens). Logout sends one revocation request when advertised, removes the local token either way and warns when revocation is unconfirmed. Replay never contacts the network. An allowed server receives whatever the model sends, the proxy cannot see TLS content, and no scopes are requested.

## Extension and transport admission

The MCP profiles are [local stdio](#local-mcp) and [remote streamable HTTP](#remote-mcp-and-oauth), as a client behind the egress proxy. No generic listener or runtime self-update is admitted; a stdio server and network-capable extensions are deferred. The only other listeners are the per-command [egress proxy](./commands-and-cache.md#network-grants-and-egress-proxy) with, on Linux, its in-namespace loopback bridge, and the one-shot [ChatGPT OAuth callback](../chatgpt.md#authorization-transaction) and [MCP login callback](#remote-mcp-and-oauth), which are the only listener exceptions to the rules below.

A new listener needs a named feature and a reviewed protocol, and must validate exact `Host` and `Origin`, authenticate and authorize each session, admit exact content types, bound bodies and sessions, reject DNS-rebinding, CSRF and cross-site requests, and encrypt traffic leaving loopback; loopback routing is not authentication. PTY, update, package-install, approval and other high-authority operations require a narrow authenticated protocol and explicit capability.

MCP servers, plugins, Skills and updates require reviewed provenance and an immutable version or digest; no update endpoint may select arbitrary packages or installation/lifecycle scripts. MCP credentials bind server, issuer endpoints and audience (the server URL as `resource`), never Provider or ambient credentials; authorization is per server, Tool, resource and operation. Manifests and results stay untrusted. Network grants keep the [Provider destination and disclosure constraints](../provider/routing-and-usage.md#configuration-and-egress).
