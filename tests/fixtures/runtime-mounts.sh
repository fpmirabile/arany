set -eu
source=$1; alias=$2; late=$3; nested=$4; fixture=$5; binary=$6; target=$7
mode=$8
setup_alias() {
  /usr/bin/mount --bind "$source" "$alias"
  /usr/bin/mount --make-shared "$alias"
}
report_identity() {
  local line host_pid namespace_pid
  local -a ids
  while IFS= read -r line; do
    case "$line" in
      NSpid:*)
        line=${line#NSpid:}
        read -r -a ids <<< "$line"
        [ "${#ids[@]}" -ge 2 ]
        host_pid=${ids[0]}
        namespace_pid=${ids[${#ids[@]}-1]}
        printf '%s %s\n' "$host_pid" "$namespace_pid" > "$1"
        return
        ;;
    esac
  done < /proc/self/status
  return 1
}
run_worker() {
  ARANY_TEST_RUNTIME_MOUNT_MODE="launch-$1" \
  ARANY_TEST_RUNTIME_MOUNT_GRANT="$fixture/grant.json" \
  ARANY_TEST_RUNTIME_MOUNT_FIXTURE="$fixture" \
  ARANY_TEST_RUNTIME_MOUNT_ALIAS="$alias" \
  ARANY_TEST_RUNTIME_MOUNT_TARGET="$target" \
  ARANY_TEST_RUNTIME_MOUNT_BINARY="$binary" \
    "$binary" --exact tools::guard::linux::tests::native_runtime_mount_attestation --nocapture
}
cleanup() { /usr/bin/umount --recursive "$alias" >/dev/null 2>&1 || true; }
trap cleanup EXIT
if [ "$mode" = timeout-child ]; then
  report_identity "$fixture/held.pid"
  exec /usr/bin/sleep 60
fi
if [ "$mode" = timeout ]; then
  /usr/bin/setsid --wait /usr/bin/bash "$0" \
    "$source" "$alias" "$late" "$nested" "$fixture" "$binary" "$target" timeout-child &
  held=$!
  for ((i = 0; i < 1000; i++)); do
    [ -s "$fixture/held.pid" ] && break
    /usr/bin/sleep .01
  done
  [ -s "$fixture/held.pid" ]
  report_identity "$fixture/init.pid"
  printf 'ready\n' > "$fixture/timeout-ready"
  wait "$held"
  exit 0
fi
setup_alias
/usr/bin/grep -F "$alias" /proc/self/mountinfo | /usr/bin/grep -Eq ' shared:[0-9]+'
run_worker exercise > "$fixture/worker-exercise.log" 2>&1 & worker=$!
for ((i = 0; i < 1000; i++)); do
  [ -e "$fixture/ready" ] && break
  if ! kill -0 "$worker" 2>/dev/null; then wait "$worker"; exit 1; fi
  /usr/bin/sleep .01
done
[ -e "$fixture/ready" ]
/usr/bin/mount --bind "$late" "$alias/late"
/usr/bin/touch "$fixture/go"
wait "$worker"
/usr/bin/grep -qx PASS "$fixture/result"
cleanup
/usr/bin/rm -f "$fixture/result" "$fixture/ready" "$fixture/go"
setup_alias
/usr/bin/mount --bind "$nested" "$alias/nested"
run_worker reject > "$fixture/worker-reject.log" 2>&1
/usr/bin/grep -qx '{"stage":"runtime_mounts"}' "$fixture/result"
/usr/bin/printf 'private-source-ok\nlate-mount-isolated\nnested-mount-rejected-at-runtime-stage\n'
