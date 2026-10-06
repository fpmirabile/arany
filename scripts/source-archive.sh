#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 0 ]]; then
  printf 'usage: bash scripts/source-archive.sh\n' >&2
  exit 2
fi

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
version=0.1.0
package="arany-$version"
cd "$project_root"
mkdir -p "$project_root/target"
export CARGO_TARGET_DIR="$project_root/target"

cargo package --locked --offline --allow-dirty --no-verify

stage="$(mktemp -d "$project_root/target/source-stage.XXXXXXXX")"
verify="$(mktemp -d "$project_root/target/source-verify.XXXXXXXX")"
artifact="$(mktemp -d "$project_root/target/source-artifact.XXXXXXXX")"
trap 'rm -rf -- "$stage" "$verify"' EXIT

tar -xzf "$project_root/target/package/$package.crate" -C "$stage"
cp "$project_root/Cargo.toml" "$stage/$package/Cargo.toml"
cp "$project_root/Cargo.lock" "$stage/$package/Cargo.lock"

archive="$artifact/$package-source.tar.gz"
tar -czf "$archive" -C "$stage" "$package"
tar -xzf "$archive" -C "$verify"
extracted="$verify/$package"
cmp "$project_root/Cargo.toml" "$extracted/Cargo.toml"
cmp "$project_root/Cargo.lock" "$extracted/Cargo.lock"
(
  cd "$extracted"
  selected_terminal="$(cargo tree --offline --locked -p crossterm --depth 0)"
  if [[ "$selected_terminal" != 'crossterm v0.29.0' ]]; then
    printf 'source archive refused: published Crossterm was not selected\n' >&2
    exit 1
  fi
  for target in x86_64-apple-darwin aarch64-apple-darwin; do
    selected_tls="$(cargo tree --offline --locked --target "$target" -p rustls-platform-verifier --depth 0)"
    if [[ "$selected_tls" != 'rustls-platform-verifier v0.7.1' ]]; then
      printf 'source archive refused: published macOS TLS verifier was not selected for %s\n' "$target" >&2
      exit 1
    fi
  done
  CARGO_TARGET_DIR="$verify/target" cargo check --offline --locked
)

printf 'Verified source archive: %s\n' "$archive"
