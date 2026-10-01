# Finding: native release builds can execute an unreviewed `emcc` from `PATH`

Status: **guarded on the tested Linux release path; open for macOS and full build-host provenance**. Severity: **medium** because an attacker must influence the release host's executable search path, but a planted executable can run during compilation. This is a build-host code-execution and artifact-provenance risk, not a Workspace-triggered runtime exploit or evidence that a published Arany binary is affected.

## Evidence and boundary

The locked Linux and both macOS normal/build trees select `libc 0.2.189`. Its [pinned build script](https://github.com/rust-lang/libc/blob/ef0906e20828777175f65caa7e681a0ce33c559a/build.rs#L90-L143) calls `emcc_version_code()` without checking the target first. That function launches `emcc -dumpversion` by fixed name through `Command::new`, which searches the build process's `PATH`. On Windows it uses `emcc.bat`; Arany's selected release targets are Linux and macOS. The resulting `emscripten_old_stat_abi` flag is used only in Emscripten source, so this review does not claim a changed native Linux/macOS ABI. The process invocation itself is the risk: a file at that name may run before the build result is inspected.

The same script can invoke `freebsd-version` through `PATH` when ambient `LIBC_CI` is nonzero and no FreeBSD version override is supplied. The [selected build-script review](../beta-build-script-source-review.md) records this conditional path and the other compiler, wrapper, and native-tool inputs. This finding isolates the unexpected fixed-name probes; it does not claim that blocking them pins `cargo`, `rustc`, C compilers, SDKs, linker flags, or unpacked crate source.

The [release-build entry point](../../../scripts/release-build.sh) rejects AWS-LC, SQLite, and ICU4X source overrides, ambient `LIBC_CI` even when empty, and an executable `emcc` found through Bash's `PATH` search before invoking Cargo. It checks fresh build records for the bundled inputs. This does not attest another entry point, builder, or later build.

A bounded local probe compiled the exact cached `libc 0.2.189` `build.rs` with its declared Rust 2021 edition, then ran it with a cleared environment, Linux target fields, and a test-owned executable named `emcc` first on `PATH`. The build script exited successfully, emitted `cargo:rustc-cfg=emscripten_old_stat_abi`, and the test-owned executable wrote its unique marker. This proves that this pinned script launches the PATH-resolved executable under the tested Linux target configuration; it does not exercise Cargo's full release build, native macOS, or a distributed Arany binary. The probe's temporary files were removed after observation.

The protected assets are release-host integrity, source provenance, and the distributed binary. The trust boundary is from ambient release environment into dependency build-script execution. Workspace files, Provider responses, Session Events, and user-selected models do not set this `PATH`.

## Resolution and verification

The user approved the narrow guard. Its product-process test plants harmless test-owned `emcc` and `rustc` executables first on `PATH`, proves the guard exits 2 before either runs, and separately rejects empty `LIBC_CI` before Cargo. A fresh guarded Linux release build passed on 2026-10-01; both `libc` build records contain the `emscripten_old_stat_abi` check declaration but not an emitted setting. Before publication, repeat the build and record review on both native macOS targets, and use a controlled release environment that verifies compiler, SDK, executable, source, and final-artifact identities. Direct Cargo builds bypass this guard; a clean guard run does not establish full host provenance.

Advisory, license/notice, paid Provider, native macOS, and broader build-host provenance gates remain separate.
