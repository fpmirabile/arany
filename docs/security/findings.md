# Release-blocking security findings

Each section is one release-blocking finding with its status, severity, evidence, attack path, fix and verification. Changing shared build, CI or release infrastructure to resolve one requires user approval under [AGENTS.md](../../AGENTS.md).

## AWS-LC build can select an unreviewed system install

Status: **guarded on one local Linux build; release-blocking until target-wide provenance is verified**. Severity: **medium** because control of the build host or its environment is required, with high impact if an unreviewed cryptographic library enters a release. This is not a Workspace- or Provider-triggered runtime exploit. The release gate cannot pass until the linked cryptographic implementation is tied to reviewed source and build inputs on every target.

### Evidence and boundary

Arany's locked Linux and both macOS feature trees select `aws-lc-rs 1.18.1` and `aws-lc-sys 0.45.0` through Reqwest, Rustls, and Rustls-WebPKI. The pinned [`aws-lc-sys` build script](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/main.rs#L725-L813) selects `AWS_LC_SYS_SYSTEM_DIR` when supplied. Otherwise, unless `AWS_LC_SYS_USE_SYSTEM=0`, it tries [system detection](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/system_detect.rs) before compiling the published native source. Detection considers target-specific or plain `OPENSSL_DIR`, `OPENSSL_INCLUDE_DIR`/`OPENSSL_LIB_DIR`, and then Unix pkg-config candidates. It validates an AWS-LC marker and usable bindings, but those checks do not pin the installation to the reviewed crate source or a release-approved digest. A compatible system installation can therefore change what is linked without changing `Cargo.lock`.

The current Linux release build is **not** shown to have taken that alternate path. Its `aws-lc-sys` build output reports `Building with: CC`, links `static=aws_lc_0_45_0_crypto`, and names an archive under this build's `target/release/build/aws-lc-sys-*/out/` directory. `readelf -d target/release/arany` lists no dynamic `libcrypto` or `libssl`. These observations apply only to this artifact on this host; they do not prove the source-file, compiler-flag, or linker inputs of a future Linux or macOS build.

The trust boundary is from the developer or CI host's installed libraries, pkg-config state, and environment into a dependency build script and then into Arany's TLS cryptography. The protected assets are Provider transport confidentiality and integrity, and reviewable release provenance. Workspace files, model output, selected Provider responses, and Session history do not set these build controls. No affected release artifact or runtime compromise has been demonstrated.

### Resolution and verification

Use an approved controlled release environment that explicitly disables system detection and rejects a contradictory explicit system directory. Review and pin the remaining native compiler, assembler, source-selection, and linker inputs rather than treating one variable as full reproducibility; the source controls below also affect bundled builds. For each release target, inspect the dependency's final build output, linked libraries, native source set, and resulting artifact. Exercise a hostile build environment in an isolated target directory and require the release gate to fail before publication. A lockfile checksum, selected Cargo feature, or this host's static archive alone does not close the finding.

The [SQLite build-environment finding](#ambient-build-variables-can-override-bundled-sqlite), advisory audit, source/license notices, and native macOS verification remain separate release gates.

The approved [release-build entry point](../../scripts/release-build.sh) rejects ambient AWS-LC and OpenSSL source-selection variables, sets `AWS_LC_SYS_USE_SYSTEM=0`, and requires a fresh bundled CC build record, static link directive, in-build archive, and no dynamic `libcrypto`/`libssl`. Its hostile-variable test and one fresh Linux build pass. Direct Cargo builds bypass this entry point; compiler, PATH, SDK, source-file, and macOS provenance remain open.

### Bundled-build source controls

Source review dated 2026-09-30, for `aws-lc-sys 0.45.0`; this is not target-wide
artifact or cryptographic clearance:

- [`prebuilt-nasm`](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/main.rs#L1186-L1199)
  applies only to Windows x86-64, not Arany's Linux/macOS targets. Absent overrides,
  the [binding selector](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/main.rs#L893-L966)
  uses pregenerated universal Linux or universal-prefixed Apple bindings.
- The [CC](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/cc_builder.rs#L373-L411)
  and [CMake](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/cmake_builder.rs#L202-L317)
  builders still accept compiler, flags and toolchain inputs. The `AWS_LC_SYS_`
  controls `CMAKE_BUILDER`,
  `NO_PREGENERATED_SRC`, `NO_ASM`, `SANITIZER`, binding/prefix controls, `STATIC`,
  `SMALL`, `NO_JITTER_ENTROPY` and `INCLUDES` can alter source selection or linkage.
- Apple's [CoreServices probe](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/main.rs#L1096-L1148)
  can disable jitter entropy when headers are absent. A locked graph or one
  Linux static archive cannot establish equivalent macOS native inputs.

## Ambient compiler inputs can alter the bundled AWS-LC build

Status: **open for compiler/toolchain provenance; the known ambient `CC`/`CFLAGS` override path is guarded but has not been verified on macOS**. Severity: **medium** because exploitation requires control of the release process environment or its compiler search path, not a Workspace or Provider input. No affected distributed binary is known.

### Evidence and boundary

[Cargo.lock](../../Cargo.lock) selects published `aws-lc-sys 0.45.0` and its build dependency `cc 1.5.1`; `cargo tree --locked --offline --target x86_64-unknown-linux-gnu --invert aws-lc-sys --edges normal,build` confirms the Linux selection. The pinned AWS-LC [builder selection](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/main.rs#L725-L813) can choose its CC source builder. That builder [creates `cc::Build`](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/cc_builder.rs#L368-L385) and [compiles the bundled native files](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/cc_builder.rs#L607-L740). Arany's [release-build guard](../../scripts/release-build.sh) deliberately requires the `Building with: CC` record and a private static AWS-LC archive, so this is the accepted Linux path, not a hypothetical CMake branch.

The published `cc 1.5.1` [environment contract](https://github.com/rust-lang/cc-rs/blob/d279cdda99af5bf0eec594ee0626218fa91052ab/src/lib.rs#L65-L123) accepts `CC` as a compiler executable and `CFLAGS` as compiler arguments, including target-specific and `HOST_`/`TARGET_` forms. Its [implementation](https://github.com/rust-lang/cc-rs/blob/d279cdda99af5bf0eec594ee0626218fa91052ab/src/lib.rs#L4412-L4468) resolves those forms, and [appends CFLAGS last](https://github.com/rust-lang/cc-rs/blob/d279cdda99af5bf0eec594ee0626218fa91052ab/src/lib.rs#L2217-L2301). The AWS-LC builder itself [acknowledges that ordering](https://github.com/aws/aws-lc-rs/blob/7943223c99d909bc399bdf1b856821bb04f1f3c5/aws-lc-sys/builder/cc_builder.rs#L572-L585). GCC documents that `-include file` [reads a named file before the primary C source](https://gcc.gnu.org/onlinedocs/gcc/Preprocessor-Options.html). Thus an ambient `CFLAGS=-include /path/to/unreviewed.h` can supply additional C input to the selected native compilation, while an ambient `CC` or target-specific variant can select an unreviewed compiler executable. These are source-backed inferences about accepted inputs, not observations of a completed hostile Arany build.

The [guard](../../scripts/release-build.sh) rejects AWS-LC-specific source overrides, a PATH-resolved `emcc`, and inherited `CC`/`CFLAGS` variables, including target/host forms, before invoking Cargo. It reads the process environment through `/usr/bin/env` because this host's Bash 5.3.20 omits an inherited raw-target `CFLAGS_x86_64-unknown-linux-gnu` from `compgen -e`; the pinned `cc` implementation can read that name through `std::env::var_os`. The guard reports a generic error without echoing environment contents. The existing Linux process test proves refusal for plain, raw-target, and `HOST_`/`TARGET_` forms before any build tool runs. This does not prove equivalent native macOS behavior, pin the selected compiler or its non-environment arguments, or attest every preprocessor input. The trust boundary is ambient release-host environment to dependency build script to native archive and distributed binary. Protected assets are release-host integrity and artifact provenance; Workspace content and Session data do not supply these variables.

### Resolution and verification

Before release, decide and enforce a narrow allowlist of native compiler executables, wrappers, flags, and inherited environment for each supported host. The current guard-only change refuses the known `CC`/`CFLAGS` family before Cargo, including raw target names that Bash `compgen -e` omits. A fresh Linux guarded build passed with inherited environment cleared and only system tool paths, `CARGO_HOME`, and `LANG` supplied; the resulting binary SHA-256 is `e18df05e3d7b0e538f556af19c8b31f4399745e97dcf098719f2886b260e0e9f`. A controlled build should also record resolved compiler identity and effective native arguments. Repeat the override and build gates on native macOS rather than extrapolating Linux compiler behavior.

Direct `cargo build` bypasses the release guard. Rejection of these two families alone does not attest `cargo`, `rustc`, linker, archiver, SDK, other native-tool variables, unpacked registry source, or final artifact identity; those remain part of broader build-host provenance. No arbitrary host-code execution from `-include` alone, altered existing binary, or macOS result is claimed here.

## Ambient build variables can override bundled SQLite

Status: **guarded on one local Linux build; release-blocking until target-wide provenance is verified**. Severity: **medium** because control of the build environment is required; this is not a Workspace- or Provider-triggered runtime exploit. A release is blocked when its SQLite source and compile options cannot be tied to reviewed inputs.

### Evidence and boundary

Arany selects `rusqlite/bundled` in [Cargo.toml](../../Cargo.toml). The locked Linux feature tree confirms `libsqlite3-sys 0.38.2` receives `bundled`. Yet the pinned dependency's [build script](https://github.com/rusqlite/rusqlite/blob/v0.40.2/libsqlite3-sys/build.rs#L69-L107) checks `LIBSQLITE3_SYS_USE_PKG_CONFIG` first: any present value other than `0` takes its system-linked path before the `bundled` branch. In the bundled branch, [additional environment variables](https://github.com/rusqlite/rusqlite/blob/v0.40.2/libsqlite3-sys/build.rs#L303-L335) can change SQLite limits or add `-D`/`-U` compiler flags. The source accepts those values as build configuration; the Cargo feature alone does not pin the effective SQLite artifact.

The current Linux release build is **not** shown to have taken the alternate path. Its `libsqlite3-sys` build output names the cached `sqlite3/sqlite3.c` source and emits `cargo:rustc-link-lib=static=sqlite3`; `readelf -d target/release/arany` lists no dynamic `libsqlite3`. These observations apply only to the current artifact on this host, not to later builds or macOS.

The trust boundary is from an ambient developer or CI process environment into a dependency build script and then into the release binary. The protected assets are SQLite-backed canonical history and reviewable artifact provenance. A Workspace file, model response, or selected Provider does not set these build variables. The possible failure is an unreviewed system SQLite library or altered compile options shipping despite a manifest that appears to request bundled SQLite. No affected release artifact or exploit of Arany's runtime Store has been demonstrated.

### Resolution and verification

Run release builds in a controlled environment that rejects or explicitly pins `LIBSQLITE3_SYS_USE_PKG_CONFIG`, `LIBSQLITE3_FLAGS`, and the `SQLITE_MAX_*` overrides. A checked-in Cargo environment setting with `force = true` is one possible mechanism, as described by the [Cargo configuration reference](https://doc.rust-lang.org/cargo/reference/config.html#env); a supervised release-build guard is another.

For each release target, verify the exact dependency build output, linked libraries, and effective SQLite compile options from the final artifact. Exercise a deliberately hostile build environment in an isolated target directory and require the release gate to fail before publication. A green ordinary test suite or a `bundled` feature tree alone does not close this finding. Native macOS verification, advisory review, and license/notice clearance remain separate gates.

The approved [release-build entry point](../../scripts/release-build.sh) rejects ambient SQLite source/flag overrides, sets `LIBSQLITE3_SYS_USE_PKG_CONFIG=0`, and requires a fresh bundled-source record, static link directive, and no dynamic `libsqlite3`. Its hostile-variable test and one fresh Linux build pass. Direct Cargo builds bypass this entry point; compiler, source-file, and macOS provenance remain open.

## Ambient ICU4X data directory can replace locked Rust source

Status: **guarded on one local Linux build; release-blocking until target-wide provenance is verified**. Severity: **medium** because build-environment control is required, while the substituted source can alter a security-relevant parser dependency. This is a release-provenance finding, not evidence of a Workspace-triggered runtime exploit or an affected Arany artifact.

### Evidence and boundary

The locked Linux normal/build tree selects `icu_properties_data 2.3.0` and `icu_normalizer_data 2.3.0` through `idna_adapter` → `idna` → `url` → Reqwest. Arany uses `reqwest::Url::parse` for [custom Provider endpoint admission](../../crates/arany/src/provider/custom.rs) and [destination resolution](../../crates/arany/src/provider/custom/destination.rs). The same two ICU4X packages are selected in both locked macOS normal/build trees.

Both published [properties](https://docs.rs/crate/icu_properties_data/2.3.0/source/build.rs) and [normalizer](https://docs.rs/crate/icu_normalizer_data/2.3.0/source/build.rs) build scripts set `icu4x_custom_data` when `ICU4X_DATA_DIR` is present. Their respective [source](https://docs.rs/crate/icu_properties_data/2.3.0/source/src/lib.rs) [files](https://docs.rs/crate/icu_normalizer_data/2.3.0/source/src/lib.rs) then use Rust `include!` on `ICU4X_DATA_DIR/mod.rs` instead of the published `data/mod.rs`. ICU4X documents this as a way to [replace compiled data](https://docs.rs/icu_provider_export/latest/icu_provider_export/baked_exporter/index.html). An empty value is still present for the build-script check; the release environment must reject or remove the variable, not merely set it to an empty string. The external `mod.rs` is Rust source outside the crate archive and its lockfile checksum.

The current Linux release build output for both data crates emits only `rerun-if-env-changed=ICU4X_DATA_DIR`, not `rustc-cfg=icu4x_custom_data`. This supports the published-data path for that build. It does not prove future Linux or native macOS artifacts, and the exact effect of any particular custom data on Arany's canonical endpoint checks has not been demonstrated.

The trust boundary is from an ambient developer or CI environment and host filesystem into dependency compilation. The protected assets are reviewed release-source provenance and the URL/IDNA parsing substrate used before Provider egress. Workspace text, Provider responses, and Session history do not set this build variable. No runtime endpoint bypass is claimed.

### Resolution and verification

Fail release builds when `ICU4X_DATA_DIR` is present unless a separately reviewed, digest-pinned custom-data artifact is explicitly approved. Capture both data-crate build outputs and ensure neither emits `icu4x_custom_data`; verify the release artifact was built in the controlled environment. Exercise a hostile set-variable case in an isolated build and require the release gate to fail before publication. A lockfile checksum or this host's default-data build output alone does not close the finding.

The [AWS-LC](#aws-lc-build-can-select-an-unreviewed-system-install) and [SQLite](#ambient-build-variables-can-override-bundled-sqlite) build-input findings, advisory and license reviews, and native macOS verification remain separate gates.

The approved [release-build entry point](../../scripts/release-build.sh) rejects `ICU4X_DATA_DIR` even when empty and requires both fresh data-crate build records without `icu4x_custom_data`. Its hostile-variable test and one fresh Linux build pass. Direct Cargo builds bypass this entry point; source-file and macOS provenance remain open.

## Native release builds can execute an unreviewed `emcc` from `PATH`

Status: **guarded on the tested Linux release path; open for macOS and full build-host provenance**. Severity: **medium** because an attacker must influence the release host's executable search path, but a planted executable can run during compilation. This is a build-host code-execution and artifact-provenance risk, not a Workspace-triggered runtime exploit or evidence that a published Arany binary is affected.

### Evidence and boundary

The locked Linux and both macOS normal/build trees select `libc 0.2.189`. Its [pinned build script](https://github.com/rust-lang/libc/blob/ef0906e20828777175f65caa7e681a0ce33c559a/build.rs#L90-L143) calls `emcc_version_code()` without checking the target first. That function launches `emcc -dumpversion` by fixed name through `Command::new`, which searches the build process's `PATH`. On Windows it uses `emcc.bat`; Arany's selected release targets are Linux and macOS. The resulting `emscripten_old_stat_abi` flag is used only in Emscripten source, so this review does not claim a changed native Linux/macOS ABI. The process invocation itself is the risk: a file at that name may run before the build result is inspected.

The same script can invoke `freebsd-version` through `PATH` when ambient `LIBC_CI` is nonzero and no FreeBSD version override is supplied. The selected build-script review recorded this conditional path and the other compiler, wrapper, and native-tool inputs. This finding isolates the unexpected fixed-name probes; it does not claim that blocking them pins `cargo`, `rustc`, C compilers, SDKs, linker flags, or unpacked crate source.

The [release-build entry point](../../scripts/release-build.sh) rejects AWS-LC, SQLite, and ICU4X source overrides, ambient `LIBC_CI` even when empty, and an executable `emcc` found through Bash's `PATH` search before invoking Cargo. It checks fresh build records for the bundled inputs. This does not attest another entry point, builder, or later build.

A bounded local probe compiled the exact cached `libc 0.2.189` `build.rs` with its declared Rust 2021 edition, then ran it with a cleared environment, Linux target fields, and a test-owned executable named `emcc` first on `PATH`. The build script exited successfully, emitted `cargo:rustc-cfg=emscripten_old_stat_abi`, and the test-owned executable wrote its unique marker. This proves that this pinned script launches the PATH-resolved executable under the tested Linux target configuration; it does not exercise Cargo's full release build, native macOS, or a distributed Arany binary. The probe's temporary files were removed after observation.

The protected assets are release-host integrity, source provenance, and the distributed binary. The trust boundary is from ambient release environment into dependency build-script execution. Workspace files, Provider responses, Session Events, and user-selected models do not set this `PATH`.

### Resolution and verification

The user approved the narrow guard. Its product-process test plants harmless test-owned `emcc` and `rustc` executables first on `PATH`, proves the guard exits 2 before either runs, and separately rejects empty `LIBC_CI` before Cargo. A fresh guarded Linux release build passed on 2026-10-01; both `libc` build records contain the `emscripten_old_stat_abi` check declaration but not an emitted setting. Before publication, repeat the build and record review on both native macOS targets, and use a controlled release environment that verifies compiler, SDK, executable, source, and final-artifact identities. Direct Cargo builds bypass this guard; a clean guard run does not establish full host provenance.

Advisory, license/notice, paid Provider, native macOS, and broader build-host provenance gates remain separate.
