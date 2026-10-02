# Beta minimum Rust version: locked metadata review

Status: **declaration check only; Rust 1.88 build and native-platform behavior unverified**. Reviewed on 2026-10-01 against the current [manifest](../../Cargo.toml), [lockfile](../../Cargo.lock), and offline Cargo metadata. Arany declares `rust-version = "1.88"`; the checked-in [toolchain pin](../../rust-toolchain.toml) is 1.98.1.

For each target below, `cargo tree --locked --offline --target <triple> --edges normal,build --prefix none --format '{p}'` identified the selected package identities. Their `rust_version` fields came from `cargo metadata --locked --offline --filter-platform <triple> --format-version 1`. Counts include Arany, exclude development-only dependencies, and deduplicate repeated tree entries. Both macOS trees select the same package identities.

| Target | Selected packages | Declare `rust-version` | Omit `rust-version` | Declare above 1.88 |
| --- | ---: | ---: | ---: | ---: |
| `x86_64-unknown-linux-gnu` | 269 | 227 | 42 | 0 |
| `x86_64-apple-darwin` | 209 | 169 | 40 | 0 |
| `aarch64-apple-darwin` | 209 | 169 | 40 | 0 |

The 39 undeclared packages common to all three selections are: `ambient-authority 0.0.2`, `cap-fs-ext 4.0.3`, `cap-primitives 4.0.3`, `cap-std 4.0.3`, `castaway 0.2.4`, `compact_str 0.9.1`, `convert_case 0.10.0`, `directories 6.0.0`, `dirs-sys 0.5.0`, `dunce 1.0.5`, `fallible-iterator 0.3.0`, `fallible-streaming-iterator 0.1.9`, `fs-set-times 0.20.3`, `fs_extra 1.3.0`, `httparse 1.10.1`, `ident_case 1.0.1`, `ipnet 2.12.2`, `libsqlite3-sys 0.38.2`, `maybe-owned 0.3.4`, `option-ext 0.2.0`, `rusqlite 0.40.2`, `scopeguard 1.2.0`, `signal-hook 0.3.18`, `signal-hook-mio 0.2.5`, `smallvec 1.16.2`, `stable_deref_trait 1.2.1`, `static_assertions 1.1.0`, `subtle 2.6.1`, `sync_wrapper 1.0.2`, `tower-layer 0.3.3`, `tower-service 0.3.3`, `try-lock 0.2.5`, `unicode-width 0.2.0`, `untrusted 0.9.0`, `utf8_iter 1.0.4`, `utf8parse 0.2.2`, `vcpkg 0.2.15`, `want 0.3.1`, and `zerovec-derive 0.11.6`.

Linux additionally selects three undeclared packages: `async-recursion 1.1.1`, `hex 0.4.3`, and `ordered-stream 0.2.0`. Each macOS target additionally selects `core-foundation-sys 0.8.7`. A missing declaration leaves the package's minimum compiler version unknown; it is not evidence that Rust 1.88 suffices.

This metadata screen finds no declared MSRV conflict in the selected normal/build graphs. It does not prove that these packages or Arany compile with Rust 1.88, nor that the macOS selections build or behave correctly on native hosts. Those claims require an actual Rust 1.88 build and platform-specific verification.
