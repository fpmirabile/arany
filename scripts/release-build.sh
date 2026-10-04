#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 0 ]]; then
  printf 'usage: bash scripts/release-build.sh\n' >&2
  exit 2
fi

if ! environment="$(/usr/bin/env)"; then
  printf 'release build refused: unable to inspect environment\n' >&2
  exit 2
fi
while IFS= read -r assignment; do
  name="${assignment%%=*}"
  case "$name" in
    AWS_LC_SYS_*|AWS_LC_RS_*|OPENSSL_DIR*|OPENSSL_INCLUDE_DIR*|OPENSSL_LIB_DIR*|LIBSQLITE3_*|SQLITE3_LIB_DIR*|SQLITE3_INCLUDE_DIR*|SQLITE3_STATIC*|SQLITE_MAX_*|ICU4X_DATA_DIR|LIBC_CI|CC|CC_*|HOST_CC|TARGET_CC|CFLAGS|CFLAGS_*|HOST_CFLAGS|TARGET_CFLAGS)
      printf 'release build refused: ambient build override\n' >&2
      exit 2
      ;;
  esac
done <<< "$environment"
unset environment assignment name

if type -P emcc >/dev/null 2>&1; then
  printf 'release build refused: unreviewed emcc executable on PATH\n' >&2
  exit 2
fi

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$project_root"

host="$(rustc -vV | sed -n 's/^host: //p')"
case "$host" in
  x86_64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
  *) printf 'release build refused: unsupported native host\n' >&2; exit 2 ;;
esac

mkdir -p "$project_root/target"
build_dir="$(mktemp -d "$project_root/target/release-guarded.XXXXXXXX")"
export CARGO_TARGET_DIR="$build_dir"
export AWS_LC_SYS_USE_SYSTEM=0
export LIBSQLITE3_SYS_USE_PKG_CONFIG=0

cargo build --release --locked --offline

require_output() {
  local crate="$1"
  local -a matches
  shopt -s nullglob
  matches=( "$build_dir"/release/build/"$crate"-*/output )
  shopt -u nullglob
  if [[ ${#matches[@]} -ne 1 ]]; then
    printf 'release build refused: expected one %s build record\n' "$crate" >&2
    exit 1
  fi
  printf '%s\n' "${matches[0]}"
}

aws_output="$(require_output aws-lc-sys)"
sqlite_output="$(require_output libsqlite3-sys)"
properties_output="$(require_output icu_properties_data)"
normalizer_output="$(require_output icu_normalizer_data)"

if ! grep -Fq 'Building with: CC' "$aws_output" ||
  ! grep -Eq '^cargo:rustc-link-lib=static=aws_lc_.*_crypto$' "$aws_output"; then
  printf 'release build refused: AWS-LC did not use the reviewed bundled CC path\n' >&2
  exit 1
fi
crypto_path="$(sed -n 's/^cargo:libcrypto_path=//p' "$aws_output")"
if [[ "$crypto_path" != "$build_dir"/* || ! -f "$crypto_path" ]]; then
  printf 'release build refused: AWS-LC archive is outside this build\n' >&2
  exit 1
fi

if ! grep -Fxq 'cargo:rerun-if-changed=sqlite3/sqlite3.c' "$sqlite_output" ||
  ! grep -Fxq 'cargo:rustc-link-lib=static=sqlite3' "$sqlite_output"; then
  printf 'release build refused: SQLite did not use the bundled static path\n' >&2
  exit 1
fi

for output in "$properties_output" "$normalizer_output"; do
  if ! grep -Fxq 'cargo:rerun-if-env-changed=ICU4X_DATA_DIR' "$output" ||
    grep -Fq 'icu4x_custom_data' "$output"; then
    printf 'release build refused: ICU4X data source is not verified\n' >&2
    exit 1
  fi
done

binary="$build_dir/release/arany"
if [[ ! -f "$binary" ]]; then
  printf 'release build refused: Arany binary is missing\n' >&2
  exit 1
fi
case "$host" in
  x86_64-unknown-linux-gnu) linked="$(readelf -d "$binary")" ;;
  *) linked="$(otool -L "$binary")" ;;
esac
if [[ "$linked" =~ libsqlite3|libcrypto|libssl ]]; then
  printf 'release build refused: binary links a system SQLite or TLS library\n' >&2
  exit 1
fi

printf 'Guarded native release build: %s\n' "$binary"
