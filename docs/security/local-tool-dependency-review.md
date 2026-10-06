# Local Tool dependency review

**Date:** 2026-10-04  
**Status:** selected dependency/entrypoint review and local synthetic evidence; not whole-dependency or redistribution clearance

The guarded implementation of the [Tool contract](../specs/tools.md) uses YAML metadata parsing, offline MCP JSON Schema validation and a fixed Linux syscall filter. The choices follow [MCP/Skill research](../research/mcp-and-runtime-skills-2026-10.md) and [Guard research](../research/effect-guard-platform-base-2026-10.md), rather than introducing a second harness, plugin framework or downloaded policy. [Cargo.toml](../../Cargo.toml) and [Cargo.lock](../../Cargo.lock) remain the version/feature/checksum authority.

## Direct choices and runtime ownership

| Selected addition | Features and declared license | Boundary and limits |
|---|---|---|
| `jsonschema 0.56.0` | Default features disabled; `MIT`; declared Rust 1.85 | Compilation/validation runs only in the killable Guard. The default HTTP/file resolvers, TLS and IDNA features are not enabled by this dependency. Arany additionally requests offline validation and rejects schema references, `$id` and unsupported dialects before compilation. Schema input/depth/nodes and linear-regex size are bounded. |
| `yaml_serde 0.10.7` | Default features disabled, `std` enabled; `MIT OR Apache-2.0`; declared Rust 1.82 | Skill metadata parsing runs only in the Guard. Its selected `libyaml-rs 0.3.0` parser has internal allocation/unsafe boundaries that occur before Arany's visitor budgets. A byte cap and successful fixture are not parser soundness or internal-allocation proof. No frontmatter value grants execution. |
| Linux `seccompiler 0.5.0` | Default features disabled; JSON/Serde filter deserialization absent; `Apache-2.0 OR BSD-3-Clause`; no declared minimum Rust version | Existing `libc` is its sole mandatory dependency. Arany compiles only a fixed bounded native filter, explicitly handles wrong architecture/x32, and uses the dependency's safe installer before threads/payload. Dependency FFI/unsafe remains outside the application's `forbid(unsafe_code)`. The deny profile supplements, not replaces, namespaces/mounts/cgroups. |
| Existing `tokio 1.53.1` | Adds `process` to the existing minimal feature selection | Bounded async stdio/process supervision; not an authority or containment interface. Exact unit/cgroup ownership remains in the Guard. |

The package manifests inspected were cached published sources matching the locked versions, not separately fetched Git repositories. Public source anchors: [JSON Schema manifest](https://docs.rs/crate/jsonschema/0.56.0/source/Cargo.toml), [YAML manifest](https://docs.rs/crate/yaml_serde/0.10.7/source/Cargo.toml), [filter manifest](https://docs.rs/crate/seccompiler/0.5.0/source/Cargo.toml) and [safe installer](https://docs.rs/seccompiler/0.5.0/seccompiler/fn.apply_filter.html). Disabling defaults is not proof that the entire unified graph is network-free, safe Rust, or cleared for release. Provider/credential network behavior remains separately scoped.

## Selected compiler-host delta

The current normal/build graph contains 36 custom-build packages on Linux and 33 on either macOS architecture, versus the prior reviewed 33/29 baseline. The additional Linux entrypoints inspected in full are:

- `ahash 0.8.12/build.rs`: fixed target/compiler-version feature probes and emitted cfg directives; the selected `version_check` helper is still a compiler-host dependency, not attested compiler identity.
- `ref-cast 1.0.27/build.rs`: `RUSTC --version`, fixed capability directives and `OUT_DIR/private.rs` generated from a fixed template and Cargo's package patch version.
- `unicode-general-category 1.1.0/build.rs`: packaged `src/tables.rs` data transformed into `OUT_DIR/category.rs`; no direct process, network or external source substitution in that script.

macOS also newly selects the previously inspected `num-traits 0.2.19` compiler probe. These observations do not clear transitive build dependencies, generated output, compiler/wrapper/PATH integrity, or a native macOS artifact. The [build review](./beta-build-script-source-review.md) retains the earlier entrypoint evidence.

The only new selected procedural-macro package is `ref-cast-impl 1.0.27`, bringing the totals to 25 Linux and 19 macOS. It enters through `jsonschema → referencing → fluent-uri → ref-cast`. Source inspection identified generated unsafe reference casts and the representation/field checks around their emitters; the complete macro implementation and exact release-feature expansion are not cleared. Selected `fluent-uri 0.4.1` sources use `RefCastCustom`/`ref_cast_custom` on transparent `Scheme(str)` and `EStr` with `PhantomData` plus `str`. This identifies invariant/expansion review targets, not proof of soundness. [Published macro source](https://docs.rs/crate/ref-cast-impl/1.0.27/source/src/lib.rs), [selected URI types](https://docs.rs/crate/fluent-uri/0.4.1/source/src/component.rs). Existing macro findings remain in the [macro review](./beta-proc-macro-source-review.md).

## Effective enforcement and evidence

The native Linux corpus verifies memory, swap, process and CPU controls before payload release. `OOMPolicy=kill` is the selected service-manager setting; the Guard independently verifies `memory.oom.group=1`, not merely a requested property. The [kernel's group-OOM contract](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory) prevents partial workload continuation; the installed `systemd.service(5)` documents that `OOMPolicy=kill` sets this control. A real memory-exhaustion payload terminates the owned group and becomes an uncertain, non-retried Run. Native enforcement is not protection from privileged or hostile same-user host actors.

The final isolation pass also excludes Workspace/state/account roots from the broad read-only `/usr` runtime mount and denies socket creation, connect/bind/listen and destination-bearing sends. Anonymous socket-pair IPC remains usable but cannot reconnect or send to named host transports; message/descriptor passing is unsupported. The real inherited-filter owner checks both permitted anonymous IPC and denied socket/destination operations. The native mutation owner verifies that successful receipts stay within the minimum 128-byte result grant even for long paths, with the resource retained in its correlated intent.

The existing native credential-reflection owners were extended with encoded MCP argument values, arrays and keys. Both rejected assertions failed before the correction and pass afterward. Reflection now inspects the bounded argument JSON's semantic keys/values before disclosure or persistence, in addition to decoded Tool fields. This detects the selected credential in those known encodings; it is not universal declassification, taint tracking or detection of arbitrary transformations.

On 2026-10-04 all seven activated real Guard integration owners passed in both debug and release, including command descendants, isolation canaries, quotas, MCP hostile protocol/schema/content, Skills, shared Tool budgets, read-only children, compaction and closed replay. Default wire/configuration/reducer owners also passed; they are not live Provider evidence. Current scenario owners and compatibility boundaries are in the [Tool contract](../specs/tools.md#acceptance-scenarios-and-evidence-owners); activation prerequisites and test lanes are in [testing rules](../../agents/testing.md).

The 458-package lockfile passed fresh `cargo-audit -D warnings` and cached-feed offline `cargo-deny` advisories/bans/licenses/sources without widening `deny.toml`. Full debug/release tests and strict lint passed on Rust 1.98.1. These results do not prove Rust 1.88, native macOS/ARM64, live tool-capable models, complete transitive unsafe review, or redistribution notices/source provenance. The [current inventory](./beta-dependency-inventory.md) owns those pending release gates.
