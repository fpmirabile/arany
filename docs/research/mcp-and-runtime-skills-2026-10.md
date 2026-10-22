# MCP and runtime Skills: protocol and dependency evidence

Source baseline: 2026-10-04. The initial review used primary sources without launching a server, installing dependencies, accessing credentials or running live inference. A later same-day review inspected fetched package sources and the offline dependency graph; it did not execute dependency code.

[Tool spec](../specs/tools.md) owns the advertised profile, admission, resource bounds and lifecycle. This report retains source comparisons and failure modes; the original implementation plan and proposed limits are absorbed there. [Project skill setup](../../agents/skills/README.md) separately owns development Skills.

## Protocol upgrade boundary

The 2026-10-04 review compared [2026-07-28 versioning](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)
and [discovery](https://modelcontextprotocol.io/specification/2026-07-28/server/discover)
with the [2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle).
It found per-request version/capabilities and discovery replacing the legacy
initialization handshake. This dated observation is not a fresh claim about the
latest standard. Any upgrade needs an explicit lifecycle/transport review;
[local MCP](../specs/tools.md#local-mcp) owns Arany's admitted generation.

The [Agent Skills format](https://agentskills.io/specification) review on the
same date distinguished portable progressive resource loading from experimental,
host-dependent `allowed-tools`. Format compatibility supplies no authority.

## SDK source findings

The reviewed published **rmcp 3.5.0** separated client, subprocess, HTTP and OAuth features. Its default transport behavior was not equivalent to Arany's resource or replay guarantees: `AsyncRwTransport::receive` appended `read_until` into a `Vec` without a frame ceiling; the HTTP implementation documented replay of ordinary POSTs after session expiration; parse errors could include response previews. These observations explain independent framing, replay and error-disclosure ownership, rather than a requirement to adopt or fork the SDK. [Release feature graph](https://raw.githubusercontent.com/modelcontextprotocol/rust-sdk/rmcp-v3.5.0/crates/rmcp/Cargo.toml), [async transport](https://raw.githubusercontent.com/modelcontextprotocol/rust-sdk/rmcp-v3.5.0/crates/rmcp/src/transport/async_rw.rs), [HTTP transport](https://raw.githubusercontent.com/modelcontextprotocol/rust-sdk/rmcp-v3.5.0/crates/rmcp/src/transport/streamable_http_client.rs).

## Cached parser and validator source review

The 2026-10-04 inspection covered selected `jsonschema` 0.56.0, `yaml_serde` 0.10.7 and `libyaml-rs` 0.3.0 sources and their then-selected graph. It was not a full transitive unsafe audit, advisory check, Rust-floor compilation or native macOS test. [Manifest](../../Cargo.toml), [lockfile](../../Cargo.lock) and [license policy](../../deny.toml) own current versions/features; source inspection is not a substitute for those checks.

### Schema validation limits

`jsonschema`'s `offline()` used a retriever that always failed external retrieval, and `build()` retained meta-schema validation. Its linear-regex options forwarded approximate per-pattern compiler/cache bounds, not aggregate validator memory or CPU limits. Schema compilation, meta-schema checks and validation remained synchronous; dropping a task or wrapping it in an asynchronous timeout could not establish hard termination. [Pinned options](https://github.com/Stranger6667/jsonschema/blob/1e244c994dd81a1feb7801556a813c6bf2d45dad/crates/jsonschema/src/options.rs), [retriever](https://github.com/Stranger6667/jsonschema/blob/1e244c994dd81a1feb7801556a813c6bf2d45dad/crates/jsonschema/src/retriever.rs), [regex implementation](https://github.com/Stranger6667/jsonschema/blob/1e244c994dd81a1feb7801556a813c6bf2d45dad/crates/jsonschema/src/regex.rs), [compiler](https://github.com/Stranger6667/jsonschema/blob/1e244c994dd81a1feb7801556a813c6bf2d45dad/crates/jsonschema/src/compiler.rs).

No unsafe code was found in the inspected compiler/reference/regex core, but transitive SIMD, allocation, reference-cast and storage code prevented an all-safe-Rust graph claim. Catching unwind panics did not contain aborts or allocation failure. Library error values could carry schema, instance or URI content, explaining why raw dependency errors were unsuitable as public receipts. [Validation errors](https://docs.rs/crate/jsonschema/0.56.0/source/src/error.rs).

### YAML allocation and tag semantics

`yaml_serde::Loader` read an entire reader and built an event vector before invoking the Serde visitor. Visitor bounds therefore controlled decoded output and alias replay, not the earlier parser allocation. The wrapper used raw pointers; `libyaml-rs` included manual allocation and deliberate abort paths. No exploitable defect was established by this inspection, but a visitor or `catch_unwind` alone did not prove containment. [Pinned loader](https://github.com/yaml/yaml-serde/blob/22a7009e918620b523ea06e54ca75275eabe8027/src/loader.rs), [parser wrapper](https://github.com/yaml/yaml-serde/blob/22a7009e918620b523ea06e54ca75275eabe8027/src/libyaml/parser.rs), [allocation/assertions](https://github.com/yaml/libyaml-rs/blob/66d8ae3d348b328ef2159e3dc0bc53f1deb4c3a2/src/lib.rs).

Typed string deserialization ignored scalar tags; `deserialize_any` exposed local tags through `visit_enum` but could ignore nonlocal explicit tags. Consequently a public Serde visitor could not substantiate the original blanket all-tags-rejected claim. Neither inspected parser executed tags, resolved includes or interpolated environment variables. [Pinned deserializer](https://github.com/yaml/yaml-serde/blob/22a7009e918620b523ea06e54ca75275eabe8027/src/de.rs).

### Build-input observations

The top-level parser/validator and `libyaml-rs` declared `build = false`; the latter was transpiled Rust, not a bundled C build. The inspected closure still contained procedural macros and build scripts: Unicode table generation from checked-in data and a reference-cast compiler probe. Their inspected paths did not fetch remote data or choose arbitrary external source directories. This was a narrow build-input observation, not a claim that dependency compilation executes no code. [Unicode build](https://docs.rs/crate/unicode-general-category/1.1.0/source/build.rs), [reference-cast build](https://docs.rs/crate/ref-cast/1.0.27/source/build.rs), [Strum macro source](https://docs.rs/crate/strum_macros/0.28.0/source/src/lib.rs).
