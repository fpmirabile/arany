# Finding: ambient ICU4X data directory can replace locked Rust source

Status: **guarded on one local Linux build; release-blocking until target-wide provenance is verified**. Severity: **medium** because build-environment control is required, while the substituted source can alter a security-relevant parser dependency. This is a release-provenance finding, not evidence of a Workspace-triggered runtime exploit or an affected Arany artifact.

## Evidence and boundary

The locked Linux normal/build tree selects `icu_properties_data 2.3.0` and `icu_normalizer_data 2.3.0` through `idna_adapter` → `idna` → `url` → Reqwest. Arany uses `reqwest::Url::parse` for [custom Provider endpoint admission](../../../src/provider/custom.rs) and [destination resolution](../../../src/provider/custom/destination.rs). The same two ICU4X packages are selected in both locked macOS normal/build trees.

Both published [properties](https://docs.rs/crate/icu_properties_data/2.3.0/source/build.rs) and [normalizer](https://docs.rs/crate/icu_normalizer_data/2.3.0/source/build.rs) build scripts set `icu4x_custom_data` when `ICU4X_DATA_DIR` is present. Their respective [source](https://docs.rs/crate/icu_properties_data/2.3.0/source/src/lib.rs) [files](https://docs.rs/crate/icu_normalizer_data/2.3.0/source/src/lib.rs) then use Rust `include!` on `ICU4X_DATA_DIR/mod.rs` instead of the published `data/mod.rs`. ICU4X documents this as a way to [replace compiled data](https://docs.rs/icu_provider_export/latest/icu_provider_export/baked_exporter/index.html). An empty value is still present for the build-script check; the release environment must reject or remove the variable, not merely set it to an empty string. The external `mod.rs` is Rust source outside the crate archive and its lockfile checksum.

The current Linux release build output for both data crates emits only `rerun-if-env-changed=ICU4X_DATA_DIR`, not `rustc-cfg=icu4x_custom_data`. This supports the published-data path for that build. It does not prove future Linux or native macOS artifacts, and the exact effect of any particular custom data on Arany's canonical endpoint checks has not been demonstrated.

The trust boundary is from an ambient developer or CI environment and host filesystem into dependency compilation. The protected assets are reviewed release-source provenance and the URL/IDNA parsing substrate used before Provider egress. Workspace text, Provider responses, and Session history do not set this build variable. No runtime endpoint bypass is claimed.

## Resolution and verification

Fail release builds when `ICU4X_DATA_DIR` is present unless a separately reviewed, digest-pinned custom-data artifact is explicitly approved. Capture both data-crate build outputs and ensure neither emits `icu4x_custom_data`; verify the release artifact was built in the controlled environment. Exercise a hostile set-variable case in an isolated build and require the release gate to fail before publication. A lockfile checksum or this host's default-data build output alone does not close the finding.

Changing shared build or release infrastructure requires user approval under [AGENTS.md](../../../AGENTS.md). The [AWS-LC](./aws-lc-system-autodetection.md) and [SQLite](./sqlite-build-environment-override.md) build-input findings, advisory and license reviews, and native macOS verification remain separate gates.

The approved [release-build entry point](../../../scripts/release-build.sh) rejects `ICU4X_DATA_DIR` even when empty and requires both fresh data-crate build records without `icu4x_custom_data`. Its hostile-variable test and one fresh Linux build pass. Direct Cargo builds bypass this entry point; source-file and macOS provenance remain open.
