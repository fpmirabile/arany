# Local Tool dependency review

**Status:** selected dependency/entrypoint review and local synthetic evidence; not whole-dependency or redistribution clearance

The [Tool contract](../specs/tools.md) uses Guard-only YAML parsing, offline MCP JSON Schema validation and a fixed Linux syscall filter, following [MCP/Skill research](../research/mcp-and-runtime-skills-2026-10.md) and [Guard research](../research/deterministic-harness-protection.md#linux-launch-and-syscall-rationale). [Cargo.toml](../../Cargo.toml) and [Cargo.lock](../../Cargo.lock) remain the version/feature/checksum authority.

## Direct choices and runtime ownership

| Selected addition | Features and declared license | Boundary and limits |
|---|---|---|
| `jsonschema 0.58.6` | Default features disabled; `MIT`; declared Rust 1.85 | MCP JSON Schema compilation/validation runs only in the killable Guard. The default HTTP/file resolvers (`resolve-http`, `resolve-file`), TLS (`tls-aws-lc-rs`) and IDNA (`idna`) features are not enabled by this dependency. Arany additionally requests offline validation and rejects schema references, `$id` and unsupported dialects before compilation. Schema input/depth/nodes and linear-regex size are bounded. |
| `yaml_serde 0.10.7` | Default features disabled, `std` enabled; `MIT OR Apache-2.0`; declared Rust 1.82 | Skill metadata parsing runs only in the Guard. Its selected `libyaml-rs 0.3.0` parser has internal allocation/unsafe boundaries that occur before Arany's visitor budgets. A byte cap and successful fixture are not parser soundness or internal-allocation proof. No frontmatter value grants execution. |
| Linux `seccompiler 0.5.0` | Default features disabled; JSON/Serde filter deserialization absent; `Apache-2.0 OR BSD-3-Clause`; no declared minimum Rust version | Existing `libc` is its sole mandatory dependency. Arany compiles only a fixed bounded native filter, explicitly handles wrong architecture/x32, and uses the dependency's safe installer before threads/payload. Dependency FFI/unsafe remains outside the application's `forbid(unsafe_code)`. The deny profile supplements, not replaces, namespaces/mounts/cgroups. |
| Existing locked `regex 1.13.1`, now direct | Default `std`, `perf` and Unicode features; `MIT OR Apache-2.0`; declared Rust 1.65; no new package version | Permission-rule and hook-matcher regexes compile and match in the Engine process, unlike MCP JSON Schema validation in the Guard. Compilation caps are 128 KiB compiled size, 32 nesting levels and a 32 KiB DFA cache per regex. The bounded [permission/hook configuration contract](../specs/tools.md#limits-and-acceptance-scenarios) owns aggregate rule/group counts and input/subject limits. Lookaround/backreferences reject; this does not execute hook commands or make regex matching an enforcement boundary. |
| Existing `tokio 1.53.2` | Adds `process` to the existing minimal feature selection | Bounded async stdio/process supervision; not an authority or containment interface. Exact unit/cgroup ownership remains in the Guard. |

Inspection used cached published sources at the locked versions, not separately fetched Git repositories. Public source anchors: [JSON Schema manifest](https://docs.rs/crate/jsonschema/0.58.6/source/Cargo.toml), [YAML manifest](https://docs.rs/crate/yaml_serde/0.10.7/source/Cargo.toml), [filter manifest](https://docs.rs/crate/seccompiler/0.5.0/source/Cargo.toml) and [safe installer](https://docs.rs/seccompiler/0.5.0/seccompiler/fn.apply_filter.html). Disabling defaults is not proof that the entire unified graph is network-free, safe Rust, or cleared for release. Provider/credential network behavior remains separately scoped.

## Selected compiler-host delta

The locked normal/build graph selects 37 custom-build packages on Linux and 35 on either macOS architecture. This review covers these additional Linux entrypoints:

- `ahash 0.8.12/build.rs`: fixed target/compiler-version feature probes and emitted cfg directives; the selected `version_check` helper is still a compiler-host dependency, not attested compiler identity.
- `ref-cast 1.0.27/build.rs`: `RUSTC --version`, fixed capability directives and `OUT_DIR/private.rs` generated from a fixed template and Cargo's package patch version.
- `unicode-general-category 1.1.0/build.rs`: packaged `unicode-general-category-1.1.0/src/tables.rs` data transformed into `OUT_DIR/category.rs`; no direct process, network or external source substitution in that script.
- `jsonschema-value 0.58.6/build.rs`: its only statement is `pyo3_build_config::use_pyo3_cfgs()` under the `pyo3` feature, which Arany does not select; the optional `pyo3-build-config` build dependency is therefore absent from the selected graph, so the script compiles and runs as an empty `main`. Its `unsafe` bindings are confined to the `pyo3` and `magnus` modules, which are behind unselected `cfg` gates and not compiled.

`jsonschema 0.58.6` itself has no `build.rs`, and its only `unsafe` text is in doc examples.

macOS also selects the previously inspected `num-traits 0.2.19` compiler probe. These observations do not clear transitive build dependencies, generated output, compiler/wrapper/PATH integrity, or a native macOS artifact. The [build review](./beta-build-script-source-review.md) retains the earlier entrypoint evidence.

The selected procedural-macro totals are 25 Linux and 19 macOS; this review covers the added `ref-cast-impl 1.0.27`. It enters through `jsonschema → referencing → fluent-uri → ref-cast`. Source inspection identified generated unsafe reference casts and the representation/field checks around their emitters; the complete macro implementation and exact release-feature expansion are not cleared. Selected `fluent-uri 0.4.1` sources use `RefCastCustom`/`ref_cast_custom` on transparent `Scheme(str)` and `EStr` with `PhantomData` plus `str`. This identifies invariant/expansion review targets, not proof of soundness. [Published macro source](https://docs.rs/crate/ref-cast-impl/1.0.27/source/src/lib.rs), [selected URI types](https://docs.rs/crate/fluent-uri/0.4.1/source/src/component.rs). Existing macro findings remain in the [macro review](./beta-proc-macro-source-review.md).

## Native macOS dependency selection

The locked `nix` dependency is selected on Unix: production enables user lookup; tests also enable safe FIFO and PTY wrappers. Its cached published manifest declares MIT, a Rust minimum below Arany's and the existing `cfg_aliases` build dependency. The full build script emits fixed platform aliases without process execution, network access or generated source.

The selected safe `User::from_uid` wrapper calls `getpwuid_r`, sizes its initial buffer from the OS with a fixed fallback and caps retry growth at 1 MiB; lookup failure or an absent user propagates to account-path rejection. The safe `mkfifo` wrapper is confined to synthetic private-state rejection fixtures. The test-only safe `openpty` wrapper initializes both descriptors, checks the native return code and transfers successful handles into owned descriptors; the macOS attached fixture passes no custom termios or size and owns its directly spawned test process through bounded capture and kill/reap cleanup. It uses no `forkpty`. Its only test-side unsafe is one `pre_exec` hook ([`own_terminal`](../../crates/arany-cli/tests/session_run/active_terminal_macos.rs)) that calls `setsid` and `TIOCSCTTY` on the inherited PTY descriptor 0 so the child owns the terminal. Between fork and exec it allocates nothing, takes no locks and calls only those two async-signal-safe syscalls, returning the OS error on failure. Product code contains no such hook. The other wrappers contain dependency FFI/unsafe, not application unsafe. This scoped review and the [native account/process owners](../../crates/arany-cli/tests/session_run.rs) in `macOS Tests (debug)` do not establish transitive soundness or release provenance. Fresh advisory and dependency-policy evidence belongs to `Fresh dependency policy` in the [release-policy workflow](../../.github/workflows/release-policy.yml), required for the release lockfile.

`core-foundation =0.10.1` (with `link`) and `security-framework =3.7.0` are direct macOS dependencies at the exact versions already selected transitively by `apple-native-keyring-store` and `rustls-platform-verifier`, so they add no graph identity. Both and their `-sys` crates are `MIT OR Apache-2.0`, have no build script and declare no Cargo `links`. The `link` feature links the OS-owned CoreFoundation framework. Direct use is limited to typed binary-plist serialization of the Guard job dictionary and code-signing metadata reads in the [Guard adapter](../../crates/arany/src/tools/guard/macos/ffi.rs), plus generic-password Keychain item calls in the [credential helper](../../crates/arany-cli/src/accounts/credentials/keyring_helper/macos.rs). The release provenance and advisory gates above still apply.

## Enforcement and evidence owners

The [Tool contract](../specs/tools.md#acceptance-scenarios-and-evidence-owners)
owns current enforcement and scenario routing. Linux group-OOM termination
follows the [kernel cgroup contract](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory)
to prevent a partially killed workload from continuing; native quota evidence
stays with the existing Tool journeys. The [Provider contract](../specs/provider.md#usage-provenance-and-reliability)
owns decoded MCP credential exclusion. Dependency selection and source inspection
here establish neither native support nor whole-transitive safety. The
[dependency inventory](./beta-dependency-inventory.md#release-checks-still-required)
and [NEXT_STEPS](../../NEXT_STEPS.md) own the remaining release and live gates.
