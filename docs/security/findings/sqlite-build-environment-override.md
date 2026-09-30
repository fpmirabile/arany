# Finding: ambient build variables can override bundled SQLite

Status: **guarded on one local Linux build; release-blocking until target-wide provenance is verified**. Severity: **medium** because control of the build environment is required; this is not a Workspace- or Provider-triggered runtime exploit. A release is blocked when its SQLite source and compile options cannot be tied to reviewed inputs.

## Evidence and boundary

Arany selects `rusqlite/bundled` in [Cargo.toml](../../../Cargo.toml). The locked Linux feature tree confirms `libsqlite3-sys 0.38.2` receives `bundled`. Yet the pinned dependency's [build script](https://github.com/rusqlite/rusqlite/blob/v0.40.2/libsqlite3-sys/build.rs#L69-L107) checks `LIBSQLITE3_SYS_USE_PKG_CONFIG` first: any present value other than `0` takes its system-linked path before the `bundled` branch. In the bundled branch, [additional environment variables](https://github.com/rusqlite/rusqlite/blob/v0.40.2/libsqlite3-sys/build.rs#L303-L335) can change SQLite limits or add `-D`/`-U` compiler flags. The source accepts those values as build configuration; the Cargo feature alone does not pin the effective SQLite artifact.

The current Linux release build is **not** shown to have taken the alternate path. Its `libsqlite3-sys` build output names the cached `sqlite3/sqlite3.c` source and emits `cargo:rustc-link-lib=static=sqlite3`; `readelf -d target/release/arany` lists no dynamic `libsqlite3`. These observations apply only to the current artifact on this host, not to later builds or macOS.

The trust boundary is from an ambient developer or CI process environment into a dependency build script and then into the release binary. The protected assets are SQLite-backed canonical history and reviewable artifact provenance. A Workspace file, model response, or selected Provider does not set these build variables. The possible failure is an unreviewed system SQLite library or altered compile options shipping despite a manifest that appears to request bundled SQLite. No affected release artifact or exploit of Arany's runtime Store has been demonstrated.

## Resolution and verification

Run release builds in a controlled environment that rejects or explicitly pins `LIBSQLITE3_SYS_USE_PKG_CONFIG`, `LIBSQLITE3_FLAGS`, and the `SQLITE_MAX_*` overrides. A checked-in Cargo environment setting with `force = true` is one possible mechanism, as described by the [Cargo configuration reference](https://doc.rust-lang.org/cargo/reference/config.html#env); a supervised release-build guard is another. Changing shared build configuration requires user approval under [AGENTS.md](../../../AGENTS.md).

For each release target, verify the exact dependency build output, linked libraries, and effective SQLite compile options from the final artifact. Exercise a deliberately hostile build environment in an isolated target directory and require the release gate to fail before publication. A green ordinary test suite or a `bundled` feature tree alone does not close this finding. Native macOS verification, advisory review, and license/notice clearance remain separate gates.

The approved [release-build entry point](../../../scripts/release-build.sh) rejects ambient SQLite source/flag overrides, sets `LIBSQLITE3_SYS_USE_PKG_CONFIG=0`, and requires a fresh bundled-source record, static link directive, and no dynamic `libsqlite3`. Its hostile-variable test and one fresh Linux build pass. Direct Cargo builds bypass this entry point; compiler, source-file, and macOS provenance remain open.
