# Beta dependency supply-chain inventory

Status: **current locked inventory and point-in-time advisory/license/source checks; redistribution gate open**. Inventory figures re-measured offline on 2026-10-17 against the current lock (including the workspace crate split and `schemars` 1.2.2) for `x86_64-unknown-linux-gnu`, `x86_64-apple-darwin` and `aarch64-apple-darwin` from the current [manifest](../../Cargo.toml), [lockfile](../../Cargo.lock) and cached published manifests. Counts describe selected dependencies, not source-code clearance, legal advice, native macOS proof or artifact provenance.

## Scope and reproducible evidence

The lockfile contains 463 packages: two workspace crates and 461 crates.io registry packages. Linux normal/build selects 306 identities: the two workspace crates, 34 direct third-party dependencies and 270 transitive third-party dependencies. Including development dependencies selects 307 identities; filtered metadata contains 393, including optional/unselected packages. The lockfile and metadata are therefore not release-artifact inventories by themselves.

Both macOS normal/build trees select the same 254 identities: the two workspace crates, 35 direct third-party dependencies and 217 transitive third-party dependencies. Including development dependencies selects 256 identities; filtered metadata contains 347. The macOS-only identities are `apple-native-keyring-store 1.0.2`, `core-foundation 0.10.1`, `core-foundation-sys 0.8.7`, `security-framework 3.7.0` and `security-framework-sys 2.17.0`. Linux selects 57 identities absent from macOS, including its credential backend and `seccompiler`. These are locked graph observations, not native compilation or execution proof.

Reproduce the graph and feature observations without refreshing the lockfile:

```sh
cargo metadata --locked --offline --filter-platform x86_64-unknown-linux-gnu --format-version 1
cargo tree --locked --offline --target x86_64-unknown-linux-gnu --edges normal,build --prefix none --format '{p}'
cargo tree --locked --offline --target x86_64-unknown-linux-gnu --edges normal,build --duplicates
cargo tree --locked --offline --target x86_64-unknown-linux-gnu --edges features --invert reqwest
cargo metadata --locked --offline --filter-platform x86_64-apple-darwin --format-version 1
cargo metadata --locked --offline --filter-platform aarch64-apple-darwin --format-version 1
cargo tree --locked --offline --target x86_64-apple-darwin --edges normal,build --prefix none --format '{p}'
cargo tree --locked --offline --target aarch64-apple-darwin --edges normal,build --prefix none --format '{p}'
cargo tree --locked --offline --target x86_64-apple-darwin --edges normal,build,dev --prefix none --format '{p}'
cargo tree --locked --offline --target aarch64-apple-darwin --edges normal,build,dev --prefix none --format '{p}'
```

Counts deduplicate Cargo tree identities and intersect them with locked metadata target kinds, licenses and declared minimum versions. Direct dependencies are the union of selected normal/build edges from both workspace crates. On 2026-10-17, 348 of 463 packages (including both workspace crates' `1.88`) declare a `rust-version`, none above 1.88; 115 omit it. Among selected third-party normal/build identities, 50 Linux and 47 macOS packages omit a minimum. The pinned host toolchain is 1.98.1 and the package minimum remains 1.88. This is only a manifest screen: an actual Rust 1.88 build remains unverified, as does native macOS behavior.

## Build-time and native authority

The selected Linux normal/build graph has 37 third-party custom-build and 26 procedural-macro packages; each macOS graph has 35 and 20. `jsonschema-value 0.58.6` adds one custom-build package on each target, reviewed in the [Tool delta](./local-tool-dependency-review.md); the earlier macOS figure (33) was already stale, 34 before the bump. These are compiler-host execution surfaces. The three active Cargo `links` declarations remain `aws-lc-rs 1.18.1`, `aws-lc-sys 0.45.0` and `libsqlite3-sys 0.38.2`. Bundled SQLite and AWS-LC still require the [SQLite](./findings/sqlite-build-environment-override.md) and [AWS-LC](./findings/aws-lc-system-autodetection.md) findings; ICU4X retains its [external Rust-source override](./findings/icu4x-external-data-source.md). The [local Tool dependency review](./local-tool-dependency-review.md) covers the added parser/filter choices and selected entrypoint delta without clearing their entire transitive code.

The macOS-only packages add no Cargo custom-build or `links` declarations, but CoreFoundation/Security framework FFI and native trust/key-store behavior remain required review/evidence. Dependency selection alone does not establish native Tool support; [Tools](../specs/tools.md#macos-enforcement-and-compatibility) owns enforcement and compatibility.

The macOS TLS decision accepts published `rustls-platform-verifier 0.7.1` and standard OS trust evaluation, which may make auxiliary certificate-related connections ([Apple trust-fetch documentation](https://developer.apple.com/documentation/security/sectrustsetnetworkfetchallowed%28_%3A_%3A%29)). A local network-fetch override was rejected because it changes chain building without proving the absence of other OS traffic. Arany restricts its own request destinations and payloads; it does not claim machine-wide Provider-only connections. Native macOS TLS behavior and separation of OS certificate traffic from Arany payloads remain unverified.

The selected keyring path uses an encrypted D-Bus session but cannot establish Arany-only access to an unlocked item, as the [credential contract](../specs/credentials.md#limits-and-record-admission) requires users to acknowledge. On 2026-10-01, a native same-user `/usr/bin/secret-tool` client retrieved a synthetic Arany record from this host's unlocked KDE `ksecretd` collection; the test deleted it afterward without touching the fixed default slot. The [Secret Service locking contract](https://specifications.freedesktop.org/secret-service/latest/unlocking.html) permits shared access to unlocked items; encrypted D-Bus transfer does not establish client authorization. A private-bus KDE attempt required a creation prompt and did not complete save/reuse, so it supplies no isolated-store success evidence. This is a release decision for saved subscription tokens and a disclosure limit for the already active API-key account, not evidence about locked items or every Linux backend.

The [macOS credential source review](./beta-macos-credential-source-review.md) traces the selected `keyring/v1` → User Keychain → Security Framework path and its fixed-slot/redaction boundary. It does not clear native Keychain behavior, Security Framework FFI, ACLs, prompts, or artifact provenance.

The [binary](../../crates/arany-cli/src/main.rs) forbids application `unsafe_code`; the [library](../../crates/arany/src/lib.rs) denies it with narrowly scoped exceptions for the native Guard and [private filesystem ACL adapter](../../crates/arany/src/private_fs/acl.rs). Each exception requires a safe interface, documented invariants and dedicated native review/tests. Transitive unsafe, generated code, FFI, build scripts and native C are not covered. Source-scoped entrypoint work remains in the [build review](./beta-build-script-source-review.md), [macro review](./beta-proc-macro-source-review.md) and [Tool delta](./local-tool-dependency-review.md). YAML parsing and MCP schema compilation/validation run only inside the killable Guard because their internal allocation/unsafe boundaries are not cleared by a byte cap. These reviews are not blanket dependency or artifact clearance.

The [procedural-macro review](./beta-proc-macro-source-review.md) retains its prior selected entrypoint evidence; `ref-cast-impl 1.0.27` adds a generated reference-cast unsafe surface described in the [Tool delta](./local-tool-dependency-review.md). Existing `proc-macro-crate` host-command, `enumflags2_derive`, `futures-macro`, `yoke-derive` and `zerovec-derive` limits remain there and in their linked scoped reviews. Source inspection or a diagnostic expansion is not proof of every release expansion, compiler-host executable, native behavior or artifact.

A separate [bounded handwritten `zerovec` review](./beta-zerovec-handwritten-unsafe-review.md) follows the two selected Yokeable implementations and the two-field `MultiFieldsULE` path into their immediate safety contracts. It found no threshold-crossing issue in those source paths; nested codecs, exact expansion, whole-crate soundness, native behavior, and artifact provenance remain open.

The [build-script source review](./beta-build-script-source-review.md) retains its inspected baseline; the Tool delta adds `ahash`, `ref-cast` and `unicode-general-category` on Linux, and also selects the previously reviewed `num-traits` path on macOS. The [guarded release script](../../scripts/release-build.sh) still rejects known AWS-LC, SQLite, ICU4X, libc and compiler override families; it does not pin every compiler/SDK/PATH/source input or establish macOS provenance. Crossterm 0.29.0 now selects its unmodified registry archive. No local replacement remains; the [source archive](../../scripts/source-archive.sh) checks registry selection and the [bundle](../../scripts/release-bundle.sh) carries its checksum-matched source and MIT text through the ordinary third-party path. Only Crossterm source/checksum selection changed in the lockfile; package versions and selected identities are unchanged.

Feature unification remains material: telemetry uses the private async Reqwest adapter without `blocking`; SQLite uses bundled source; direct minimal features do not remove preexisting transitive SIMD/unsafe or OS crypto. The Tool base adds `tokio/process`, `jsonschema 0.58.6` and `yaml_serde 0.10.7` with default features disabled, plus Linux-only `seccompiler 0.5.0` without JSON/filter-file deserialization. JSON Schema retrieval is offline and reference resolution is rejected at admission; YAML only enables `std`. Current selected normal/build duplicate-version names are `getrandom`, `hashbrown`, `io-lifetimes` and `syn`; optional lockfile entries are not active duplicates. Development features remain distinct from the release graph.

### Serve schema dependency

`schemars = "=1.2.2"` is selected only by `arany-cli`, with default features
disabled and only `derive` and `std` enabled; `preserve_order` stays disabled.
The harness library has no new dependency. It generates schemas for compiled
wire types, never for repository-provided code or configuration. The existing
`jsonschema` package also validates protocol examples as a CLI dev dependency.

The locked additions are MIT `schemars 1.2.2` and
`schemars_derive 1.2.2`, plus MIT OR Apache-2.0 `dyn-clone 1.0.20` and
`serde_derive_internals 0.30.0`; existing `ref-cast 1.0.27` and its macro remain
selected. Published manifests declare no build script for those four additions;
`cargo tree --locked --offline -p schemars --edges build` has no direct build
dependencies. The normal/build tree retains the existing `ref-cast` build and
macro authority described above. `schemars_derive` is new compiler-host macro
authority; its source forbids unsafe and the inspected derive/parser entrypoints
have no filesystem, environment or process-command calls. This scoped review
is not a release artifact, advisory or whole-transitive-source clearance.

Crossterm remains the unmodified published `0.29.0` crate. Its Unix backend's
zero-read/permanent-error handling is tracked by [upstream issue #1110](https://github.com/crossterm-rs/crossterm/issues/1110)
and [repair PR #1116](https://github.com/crossterm-rs/crossterm/pull/1116).
The 2026-10-06 Arany investigation led to owned input polling and restoration
that clears the backend without a cursor query; initial acquisition still uses
Crossterm. This decision favors upstream maintenance over a vendored repair;
it does not establish arbitrary acquisition-fault or native macOS behavior.
[Native terminal checks](../../NEXT_STEPS.md#beta-2-and-broader-release) remain open.

## License and advisory status

All 304 selected Linux third-party identities declare nonempty license metadata, with 22 distinct raw expressions. The five historical slash-form declarations retain their [pinned MIT-alternative source review](./beta-legacy-slash-license-source-research.md). `option-ext 0.2.0` still declares `MPL-2.0`; native compound expressions and Unicode notices remain separate distribution obligations. The new filter/parser metadata fits the existing reviewed allowlist without broadening it. Metadata is not proof of a chosen license branch, linked-code notices or legal compliance.

`option-ext` enters only through `arany → directories 6.0.0 → dirs-sys 0.5.0 → option-ext 0.2.0` in the current locked graph. The sole application use of `directories` is `StateRoot::default_path`. Its [pinned Linux path](https://docs.rs/directories/6.0.0/src/directories/lin.rs.html) selects an absolute `XDG_STATE_HOME` or the home directory's `.local/state`, then appends `arany`; its [macOS project path](https://docs.rs/directories/6.0.0/directories/struct.ProjectDirs.html) is `Library/Application Support/dev.Arany.arany`. Removing the dependency would eliminate this license branch but must preserve both locations, the home-directory fallback, and rejection when no safe absolute path is available. The standard-library [`home_dir` contract](https://doc.rust-lang.org/std/env/fn.home_dir.html) is a candidate, but [Rust's release notes](https://doc.rust-lang.org/stable/releases.html#version-1900-2025-09-18) place its empty-`HOME` fallback fix after Arany's declared Rust 1.88; a replacement cannot assume exact parity there. Native macOS behavior remains unverified. Keeping the branch is not automatically prohibited by MPL: [Mozilla's official FAQ](https://www.mozilla.org/en-US/MPL/2.0/FAQ/) says an unmodified MPL component in a larger executable requires informing recipients how to obtain that component's source. The root `NOTICE` remains Arany-only; the bundle script's generated `THIRD-PARTY-NOTICES.txt` now names the exact source archive path and locked checksum, but the updated bundle has not been built or reviewed as an artifact. The user approved a version-specific cargo-deny metadata exception on 2026-10-02; it does not complete the release obligation or imply legal clearance.

All 252 selected macOS third-party identities also declare nonempty license metadata, with 21 distinct expressions. The macOS-only packages declare `MIT OR Apache-2.0`; that does not clear Apple-framework behavior or redistribution obligations.

`cargo-audit 0.22.2` and `cargo-deny 0.20.2` remain complementary. On 2026-10-04, `cargo audit -D warnings --db /tmp/arany-effectful-advisories-20261004` fetched 1,290 RustSec advisories, updated the index, scanned all 458 locked packages and exited zero without warnings. `cargo deny --locked --offline check` passed advisories, bans, licenses and sources with nonfatal duplicate-version warnings, without editing [deny.toml](../../deny.toml); that lane uses its cached feed rather than the private fresh audit feed. Both local results predate the rustls 0.23.46 lock change; the fresh Release policy run on main `ed0ec69` passed for the then-current lock ([run 38805626883](https://github.com/fpmirabile/arany/actions/runs/38805626883)), a point-in-time observation rather than a standing guarantee. The version-specific `option-ext` exception remains metadata policy only. Neither result certifies redistributed notices/source bundles, platform behavior or future advisories.

## Release checks still required

1. Require a passing [release-policy run](../../agents/references/testing-details.md#release-policy-ci) whose job summary names the release commit's `Cargo.lock` SHA-256. The workflow runs on every pull request and every push to `main` and repeats strict `cargo-audit -D warnings` and the selected-target `cargo-deny` checks against freshly fetched private advisory databases; the deny check runs offline against its own fetched database, never a cached feed. An unavailable feed or policy finding fails the run. It does not close source/binary notice, legal, native/compiler or MSRV requirements.
2. Keep the `option-ext` `MPL-2.0` metadata exception scoped to version 0.2.0 and verify its source path instruction in a generated bundle; approve or revise the evidenced MIT alternatives for the five slash-form packages and include their exact notices; review compound/native expressions; and verify bundled SQLite/AWS-LC notices and the distributed `LICENSE`/`NOTICE` contents. Run the updated exact root/nested material checks and rerun the license/source policy on all three release targets.
3. Extend the selected-entrypoint reviews through transitive build/macro behavior, exact expansions, native source/FFI, enabled features, duplicate versions, and transitive `unsafe` against the locked versions. Include the Linux Secret Service graph and macOS CoreFoundation/Security framework bindings, then verify platform behavior on a native macOS runner. All-target metadata resolution is available offline, but it is not source or artifact review.
4. Build release artifacts through the [guarded entry point](../../scripts/release-build.sh) from the reviewed lockfile and pinned toolchain in a controlled environment, close the [SQLite](./findings/sqlite-build-environment-override.md), [AWS-LC](./findings/aws-lc-system-autodetection.md), [ICU4X](./findings/icu4x-external-data-source.md), [libc fixed-name executable](./findings/libc-build-path-executable.md), and [native compiler-input](./findings/native-compiler-environment-inputs.md) findings on each native target, then verify remaining compiler/PATH/SDK provenance and publish artifact checksums/signatures. Re-run the inventory after every dependency, feature, target, or lockfile change.
5. Build and test the locked graph with Rust 1.88 before claiming support for the declared minimum. The selected-target metadata review does not replace that compiler gate.

The authority model and required supply-chain gate are defined in [Security](../../agents/security.md). The [incident and standards research](../research/harness-security-lessons-and-controls.md#supply-chain-and-native-code) retains their source rationale.
