#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 0 ]]; then
  printf 'usage: bash scripts/release-bundle.sh\n' >&2
  exit 2
fi

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$project_root"

source_result="$(bash scripts/source-archive.sh)"
case "$source_result" in
  'Verified source archive: '*) source_archive="${source_result#Verified source archive: }" ;;
  *) printf 'bundle refused: source archive was not verified\n' >&2; exit 1 ;;
esac
if [[ ! -f "$source_archive" ]]; then
  printf 'bundle refused: verified source archive is missing\n' >&2
  exit 1
fi

host="$(rustc -vV | sed -n 's/^host: //p')"
package="arany-0.1.0-$host-beta-candidate"
stage="$(mktemp -d "$project_root/target/bundle-stage.XXXXXXXX")"
artifact="$(mktemp -d "$project_root/target/bundle-artifact.XXXXXXXX")"
bundle_complete=false
cleanup() {
  rm -rf -- "$stage"
  if [[ "$bundle_complete" != true ]]; then
    rm -rf -- "$artifact"
  fi
}
trap cleanup EXIT
bundle="$stage/$package"
mkdir -p "$bundle/sources"
staged_source="$bundle/sources/arany-0.1.0-source.tar.gz"
cp "$source_archive" "$staged_source"
tar -xzf "$staged_source" -C "$stage"
build_root="$stage/arany-0.1.0"
if [[ ! -f "$build_root/Cargo.lock" || ! -f "$build_root/scripts/release-build.sh" ]]; then
  printf 'bundle refused: verified source archive lacks build inputs\n' >&2
  exit 1
fi
cd "$build_root"
binary_result="$(bash scripts/release-build.sh)"
case "$binary_result" in
  'Guarded native release build: '*) binary="${binary_result#Guarded native release build: }" ;;
  *) printf 'bundle refused: guarded build did not identify its binary\n' >&2; exit 1 ;;
esac
if [[ "$binary" != "$build_root"/target/release-guarded.*/release/arany || ! -f "$binary" ]]; then
  printf 'bundle refused: guarded build output is outside the archived source\n' >&2
  exit 1
fi
cp "$binary" "$bundle/arany"
cp LICENSE NOTICE "$bundle/"

option_checksum=04744f49eae99ab78e0d5c0b603ab218f515ea8cfe5a456d7629ad883a3b6e7d
cargo_cache="${CARGO_HOME:-${HOME:?}/.cargo}/registry/cache"
locked_checksum() {
  awk -v wanted_name="$1" -v wanted_version="$2" '
    function emit() {
      if (name == wanted_name && version == wanted_version &&
          source == "registry+https://github.com/rust-lang/crates.io-index") print checksum
    }
    /^\[\[package\]\]$/ {
      emit()
      name = version = source = checksum = ""
      next
    }
    /^name = "/ { name = $3; gsub(/"/, "", name) }
    /^version = "/ { version = $3; gsub(/"/, "", version) }
    /^source = "/ { source = $3; gsub(/"/, "", source) }
    /^checksum = "/ { checksum = $3; gsub(/"/, "", checksum) }
    END { emit() }
  ' Cargo.lock
}

for target in x86_64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
  cargo tree --locked --offline --target "$target" --edges normal,build \
    --prefix none --format '{p}' > "$stage/tree-$target"
done
awk '
  NF == 0 || $1 == "arany" { next }
  $1 ~ /^[A-Za-z0-9_-]+$/ && $2 ~ /^v[0-9A-Za-z.+-]+$/ {
    print $1, substr($2, 2)
    next
  }
  { print "bundle refused: unexpected Cargo tree row" > "/dev/stderr"; exit 1 }
' "$stage"/tree-* | LC_ALL=C sort -u > "$stage/selected-packages"
selected_count="$(wc -l < "$stage/selected-packages")"
if (( selected_count < 1 || selected_count > 512 )) ||
  ! grep -Fxq 'option-ext 0.2.0' "$stage/selected-packages" ||
  [[ "$(locked_checksum option-ext 0.2.0)" != "$option_checksum" ]]; then
  printf 'bundle refused: selected registry package set or option-ext pin changed\n' >&2
  exit 1
fi

source_bytes=0
license_bytes=0
license_count=0
copy_selected_package() {
  local name="$1" version="$2" checksum archive candidate member relative leaf output size root_texts=0
  checksum="$(locked_checksum "$name" "$version")"
  if [[ ! "$checksum" =~ ^[0-9a-f]{64}$ ]]; then
    printf 'bundle refused: %s %s has no locked checksum\n' "$name" "$version" >&2
    exit 1
  fi
  archive=
  shopt -s nullglob
  for candidate in "$cargo_cache"/*/"$name-$version.crate"; do
    if [[ "$(shasum -a 256 "$candidate" | cut -d ' ' -f 1)" == "$checksum" ]]; then
      archive="$candidate"
      break
    fi
  done
  shopt -u nullglob
  if [[ -z "$archive" ]]; then
    printf 'bundle refused: verified %s %s source is not cached\n' "$name" "$version" >&2
    exit 1
  fi
  size="$(wc -c < "$archive")"
  (( source_bytes += size ))
  if (( size > 16777216 || source_bytes > 268435456 )); then
    printf 'bundle refused: selected registry source exceeds the bundle limit\n' >&2
    exit 1
  fi
  mkdir -p "$bundle/third-party/$name-$version"
  cp "$archive" "$bundle/sources/$name-$version.crate"
  printf '%s %s %s\n' "$name" "$version" "$checksum" >> "$bundle/SELECTED-PACKAGES.txt"
  tar -tzf "$archive" > "$stage/members"
  while IFS= read -r member; do
    if [[ "$member" != "$name-$version/"* ]]; then
      printf 'bundle refused: unexpected member in %s %s\n' "$name" "$version" >&2
      exit 1
    fi
    relative="${member#"$name-$version/"}"
    leaf="${relative##*/}"
    case "${leaf^^}" in
      LICENSE*|LICENCE*|COPYING*|COPYRIGHT*|NOTICE*) ;;
      *) continue ;;
    esac
    if [[ ! "$relative" =~ ^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*$ || "$relative" =~ (^|/)\.\.?(/|$) ]]; then
      printf 'bundle refused: unsafe package notice name in %s %s\n' "$name" "$version" >&2
      exit 1
    fi
    output="$bundle/third-party/$name-$version/$relative"
    mkdir -p "${output%/*}"
    if ! tar -xOzf "$archive" "$member" | head -c 1048577 > "$output"; then
      printf 'bundle refused: package notice extraction failed for %s %s\n' "$name" "$version" >&2
      exit 1
    fi
    size="$(wc -c < "$output")"
    (( license_bytes += size ))
    (( license_count += 1 ))
    if (( size == 0 || size > 1048576 || license_bytes > 33554432 )); then
      printf 'bundle refused: selected package notices exceed the bundle limit\n' >&2
      exit 1
    fi
    if [[ "$relative" == */* ]]; then
      printf '%s %s %s\n' "$name" "$version" "$relative" >> "$stage/nested-notices"
    else
      (( root_texts += 1 ))
    fi
  done < "$stage/members"
  if (( root_texts == 0 )); then
    printf '%s %s\n' "$name" "$version" >> "$stage/missing-root-license"
  fi
  if [[ "$name" == ident_case ]]; then
    tar -xOzf "$archive" "$name-$version/src/lib.rs" \
      > "$bundle/third-party/$name-$version/copyright-source.rs"
  fi
}
while read -r name version; do
  copy_selected_package "$name" "$version"
done < "$stage/selected-packages"
if ! cmp -s "$stage/missing-root-license" <(printf '%s\n' \
  'opentelemetry 0.33.0' \
  'opentelemetry-http 0.33.0' \
  'opentelemetry-otlp 0.33.0' \
  'opentelemetry-proto 0.33.0' \
  'opentelemetry_sdk 0.33.0'); then
  printf 'bundle refused: packages without root license texts changed\n' >&2
  exit 1
fi
cp "$stage/missing-root-license" "$bundle/MISSING-ROOT-LICENSES.txt"
LC_ALL=C sort "$stage/nested-notices" > "$bundle/SELECTED-NESTED-NOTICES.txt"
if ! cmp -s "$bundle/SELECTED-NESTED-NOTICES.txt" <(printf '%s\n' \
  'aws-lc-sys 0.45.0 aws-lc/LICENSE' \
  'aws-lc-sys 0.45.0 aws-lc/third_party/fiat/LICENSE' \
  'libsqlite3-sys 0.38.2 sqlcipher/LICENSE' \
  'tracing-core 0.1.36 src/spin/LICENSE'); then
  printf 'bundle refused: selected nested package notices changed\n' >&2
  exit 1
fi
if (( license_count > 1024 )); then
  printf 'bundle refused: too many selected package notices\n' >&2
  exit 1
fi
for spec in \
  'fallible-iterator 0.3.0 LICENSE-MIT' \
  'fallible-streaming-iterator 0.1.9 LICENSE-MIT' \
  'ident_case 1.0.1 LICENSE' \
  'vcpkg 0.2.15 LICENSE-MIT' \
  'signal-hook 0.3.18 LICENSE-MIT' \
  'rustls-platform-verifier 0.7.1 LICENSE-MIT' \
  'aws-lc-rs 1.18.1 LICENSE' \
  'aws-lc-sys 0.45.0 LICENSE' \
  'libsqlite3-sys 0.38.2 LICENSE'; do
  read -r name version license_file <<< "$spec"
  if [[ ! -s "$bundle/third-party/$name-$version/$license_file" ]]; then
    printf 'bundle refused: reviewed %s %s notice is missing\n' "$name" "$version" >&2
    exit 1
  fi
done
apache_license="$bundle/third-party/rustls-platform-verifier-0.7.1/LICENSE-APACHE"
if [[ "$(shasum -a 256 "$apache_license" | cut -d ' ' -f 1)" != c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 ]]; then
  printf 'bundle refused: pinned OpenTelemetry upstream license text changed\n' >&2
  exit 1
fi
for name in opentelemetry opentelemetry-http opentelemetry-otlp opentelemetry-proto opentelemetry_sdk; do
  vcs_info="$(tar -xOzf "$bundle/sources/$name-0.33.0.crate" "$name-0.33.0/.cargo_vcs_info.json")"
  if ! grep -Fq '"sha1": "19833847cab86c8464c1dfb6d28b1de9c0b50038"' <<< "$vcs_info"; then
    printf 'bundle refused: %s 0.33.0 upstream revision changed\n' "$name" >&2
    exit 1
  fi
  cp "$apache_license" "$bundle/third-party/$name-0.33.0/UPSTREAM-LICENSE-APACHE"
done
cat > "$bundle/third-party/aws-lc-rs-1.18.1/NOTICE" <<'EOF'
AWS Libcrypto for Rust
Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.

Based on ring
Copyright Brian Smith
EOF
if [[ "$(shasum -a 256 "$bundle/third-party/aws-lc-rs-1.18.1/NOTICE" | cut -d ' ' -f 1)" != 38959e06d4e48e85ff548e145c944ea8094d227d8ce7fb8cdba0de478de128a5 ]]; then
  printf 'bundle refused: pinned AWS-LC repository NOTICE changed\n' >&2
  exit 1
fi
if [[ "$host" == x86_64-unknown-linux-gnu ]]; then
  if [[ "$(rustc -V)" != 'rustc 1.98.1 (48a229cea 2026-09-01) (Arch Linux rust 1:1.98.1-2)' ]]; then
    printf 'bundle refused: Linux Rust standard-library notices are unreviewed for this toolchain\n' >&2
    exit 1
  fi
  mkdir -p "$bundle/third-party/rust-stdlib-1.98.1"
  for spec in \
    '/usr/share/licenses/rust/COPYRIGHT-library.html.rustc e865f7bd5018634399d00f92f049ccfa84a4c1d60c2483d1a81eb63a5038d659' \
    '/usr/share/doc/rustc/licenses/MIT.txt b85dcd3e453d05982552c52b5fc9e0bdd6d23c6f8e844b984a88af32570b0cc0' \
    '/usr/share/doc/rustc/licenses/Apache-2.0.txt 074e6e32c86a4c0ef8b3ed25b721ca23aca83df277cd88106ef7177c354615ff' \
    '/usr/share/doc/rustc/licenses/Unicode-3.0.txt f5062c9a188d81dfe66b56db4182dcf9e4b17c0d9b0d311a8e20b3a1b075c443' \
    '/usr/share/doc/rustc/licenses/BSD-2-Clause.txt f32fb3b417a194167cfad068223fc975ba96c5960513a10f66a3c28720aec1df'; do
    read -r notice_path expected_checksum <<< "$spec"
    if [[ ! -f "$notice_path" || "$(shasum -a 256 "$notice_path" | cut -d ' ' -f 1)" != "$expected_checksum" ]]; then
      printf 'bundle refused: selected Rust standard-library notice changed\n' >&2
      exit 1
    fi
    cp "$notice_path" "$bundle/third-party/rust-stdlib-1.98.1/${notice_path##*/}"
  done
fi
cat > "$bundle/THIRD-PARTY-NOTICES.txt" <<'EOF'
This is a beta release candidate, not a redistribution-approved artifact.
The complete third-party and native-code notice review remains open.

SELECTED-PACKAGES.txt records the Linux and macOS normal/build registry union
and each source archive's locked checksum. Every selected registry source archive
is in sources/; its packaged license/copyright texts are in third-party/ at
their original relative paths. SELECTED-NESTED-NOTICES.txt identifies the
four currently selected nested texts; a changed set blocks candidate creation.
The five OpenTelemetry 0.33.0 packages in MISSING-ROOT-LICENSES.txt declare
Apache-2.0 but contain no root license text. Their packaged VCS records pin
the same upstream revision, whose root LICENSE matches the pinned SHA-256 of
each packaged UPSTREAM-LICENSE-APACHE. Embedded file-level notices still
require release review.

option-ext 0.2.0 is included under MPL-2.0. Its exact published source is
sources/option-ext-0.2.0.crate; its license is in
third-party/option-ext-0.2.0/LICENSE.txt. Arany does not modify its source.

Crossterm 0.29.0 is the unmodified published crate under MIT. Its license
is in third-party/crossterm-0.29.0/LICENSE; its checksum-matched source
archive is sources/crossterm-0.29.0.crate.

rustls-platform-verifier 0.7.1 is the unmodified published crate under
MIT OR Apache-2.0. Its checksum-matched source archive is in sources/ and
both packaged license texts are in third-party/rustls-platform-verifier-0.7.1.

The published fallible-iterator 0.3.0, fallible-streaming-iterator 0.1.9,
ident_case 1.0.1, vcpkg 0.2.15, and signal-hook 0.3.18 crates declare a
historical MIT/Apache-2.0 choice. This candidate includes their exact
published source archives and packaged MIT license texts. ident_case's
copyright header is preserved in third-party/ident_case-1.0.1/copyright-source.rs.
This MIT branch is provisional pending release-owner review.

The exact published aws-lc-rs 1.18.1, aws-lc-sys 0.45.0, and
libsqlite3-sys 0.38.2 source archives are in sources/. Their packaged
license texts are under third-party/; the aws-lc-sys directory also includes
its nested AWS-LC and Fiat Cryptography license texts. Optional SQLCipher and
no-std spin license texts are included from their published source archives,
but inclusion does not mean either code is linked. The matching pinned
AWS-LC repository NOTICE is in third-party/aws-lc-rs-1.18.1/NOTICE. The
libsqlite3-sys archive contains optional SQLCipher source, but this
candidate's guarded build selects bundled SQLite. These files do not
establish a complete binary-specific attribution.

Do not redistribute this candidate until the remaining third-party, bundled
native-code, Rust standard-library, and target-specific notices are reviewed.
EOF
if [[ "$host" == x86_64-unknown-linux-gnu ]]; then
  cat >> "$bundle/THIRD-PARTY-NOTICES.txt" <<'EOF'

The Linux candidate includes the selected Rust 1.98.1 standard-library
copyright inventory and accompanying license texts under
third-party/rust-stdlib-1.98.1. These host-package files have exact pinned
hashes, but inclusion does not prove which library/runtime code is linked or
complete the binary's attribution review.
EOF
fi

(
  cd "$bundle"
  shasum -a 256 arany LICENSE NOTICE THIRD-PARTY-NOTICES.txt \
    SELECTED-PACKAGES.txt SELECTED-NESTED-NOTICES.txt MISSING-ROOT-LICENSES.txt \
    sources/* > SHA256SUMS
  find third-party -type f -print | LC_ALL=C sort | xargs shasum -a 256 >> SHA256SUMS
)
archive="$artifact/$package.tar.gz"
tar -czf "$archive" -C "$stage" "$package"
tar -tzf "$archive" > /dev/null
bundle_complete=true
printf 'Candidate bundle (redistribution gate open): %s\n' "$archive"
shasum -a 256 "$archive"
