# Finding: native release builds can execute an unreviewed `emcc` from `PATH`

Status: **open; release-blocking until controlled build-host provenance is verified**. Severity: **medium** because an attacker must influence the release host's executable search path, but a planted executable can run during compilation. This is a build-host code-execution and artifact-provenance risk, not a Workspace-triggered runtime exploit or evidence that a published Arany binary is affected.

## Evidence and boundary

The locked Linux and both macOS normal/build trees select `libc 0.2.189`. Its [pinned build script](https://github.com/rust-lang/libc/blob/ef0906e20828777175f65caa7e681a0ce33c559a/build.rs#L90-L143) calls `emcc_version_code()` without checking the target first. That function launches `emcc -dumpversion` by fixed name through `Command::new`, which searches the build process's `PATH`. On Windows it uses `emcc.bat`; Arany's selected release targets are Linux and macOS. The resulting `emscripten_old_stat_abi` flag is used only in Emscripten source, so this review does not claim a changed native Linux/macOS ABI. The process invocation itself is the risk: a file at that name may run before the build result is inspected.

The same script can invoke `freebsd-version` through `PATH` when ambient `LIBC_CI` is nonzero and no FreeBSD version override is supplied. The [selected build-script review](../beta-build-script-source-review.md) records this conditional path and the other compiler, wrapper, and native-tool inputs. This finding isolates the unexpected fixed-name probes; it does not claim that blocking them pins `cargo`, `rustc`, C compilers, SDKs, linker flags, or unpacked crate source.

The current [release-build entry point](../../../scripts/release-build.sh) rejects AWS-LC, SQLite, and ICU4X source overrides and checks their fresh build records. It does not constrain `emcc` or `LIBC_CI`. `emcc` was absent from this Linux host's current `PATH` when checked on 2026-09-30; that observation does not attest another builder or a later build.

A bounded local probe compiled the exact cached `libc 0.2.189` `build.rs` with its declared Rust 2021 edition, then ran it with a cleared environment, Linux target fields, and a test-owned executable named `emcc` first on `PATH`. The build script exited successfully, emitted `cargo:rustc-cfg=emscripten_old_stat_abi`, and the test-owned executable wrote its unique marker. This proves that this pinned script launches the PATH-resolved executable under the tested Linux target configuration; it does not exercise Cargo's full release build, native macOS, or a distributed Arany binary. The probe's temporary files were removed after observation.

The protected assets are release-host integrity, source provenance, and the distributed binary. The trust boundary is from ambient release environment into dependency build-script execution. Workspace files, Provider responses, Session Events, and user-selected models do not set this `PATH`.

## Resolution and verification

Before publication, use an approved controlled release environment that verifies executable identities and rejects unexpected fixed-name build probes. A narrow extension to the guarded entry point can refuse an executable `emcc` found on `PATH` and ambient `LIBC_CI` before invoking Cargo. A test should plant a harmless executable under a test-owned directory, put it on `PATH`, and prove the guard exits before that file runs; an additional `LIBC_CI` row should fail before Cargo. Repeat the fresh guarded build and inspect the `libc` build record on Linux and both native macOS targets. Direct Cargo builds bypass the guard, and a clean guard run alone does not establish full compiler/SDK or final-artifact provenance.

Changing the shared release-build script requires user approval under [AGENTS.md](../../../AGENTS.md). The user was asked for that approval separately. Advisory, license/notice, paid Provider, and native macOS gates remain open.
