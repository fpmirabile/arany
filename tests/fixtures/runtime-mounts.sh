set -eu
source=$1; alias=$2; late=$3; nested=$4; fixture=$5; binary=$6; target=$7
mode=$8
setup_alias() {
  /usr/bin/mount --bind "$source" "$alias"
  /usr/bin/mount --make-shared "$alias"
}
report_identity() {
  local line host_pid namespace_pid temporary
  local -a ids
  temporary="$1.tmp.$BASHPID"
  while IFS= read -r line; do
    case "$line" in
      NSpid:*)
        line=${line#NSpid:}
        read -r -a ids <<< "$line"
        [ "${#ids[@]}" -ge 2 ]
        host_pid=${ids[0]}
        namespace_pid=${ids[${#ids[@]}-1]}
        printf '%s %s\n' "$host_pid" "$namespace_pid" > "$temporary"
        /usr/bin/mv -f -- "$temporary" "$1"
        return
        ;;
    esac
  done < /proc/self/status
  return 1
}
run_worker() {
  local phase=$1
  report_identity "$fixture/worker-$phase.pid"
  for ((i = 0; i < 1000; i++)); do
    [ -e "$fixture/worker-$phase-go" ] && break
    /usr/bin/sleep .01
  done
  [ -e "$fixture/worker-$phase-go" ]
  export ARANY_TEST_RUNTIME_MOUNT_MODE="launch-$phase"
  export ARANY_TEST_RUNTIME_MOUNT_GRANT="$fixture/grant.json"
  export ARANY_TEST_RUNTIME_MOUNT_FIXTURE="$fixture"
  export ARANY_TEST_RUNTIME_MOUNT_ALIAS="$alias"
  export ARANY_TEST_RUNTIME_MOUNT_TARGET="$target"
  export ARANY_TEST_RUNTIME_MOUNT_BINARY="$binary"
  exec "$binary" --exact tools::guard::linux::tests::native_runtime_mount_attestation --nocapture
}
wait_worker_identity() {
  local phase=$1 worker=$2 namespace_pid
  for ((i = 0; i < 1000; i++)); do
    [ -s "$fixture/worker-$phase.pid" ] && break
    /usr/bin/sleep .01
  done
  [ -s "$fixture/worker-$phase.pid" ]
  read -r _ namespace_pid < "$fixture/worker-$phase.pid"
  [ "$namespace_pid" = "$worker" ]
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
if [ "$mode" = topology ]; then
  report_identity "$fixture/init.pid"
  for ((i = 0; i < 1000; i++)); do
    [ -e "$fixture/init-go" ] && break
    /usr/bin/sleep .01
  done
  [ -e "$fixture/init-go" ]
fi
setup_alias
/usr/bin/grep -F "$alias" /proc/self/mountinfo | /usr/bin/grep -Eq ' shared:[0-9]+'
run_worker exercise > "$fixture/worker-exercise.log" 2>&1 & worker=$!
wait_worker_identity exercise "$worker"
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
run_worker reject > "$fixture/worker-reject.log" 2>&1 & worker=$!
wait_worker_identity reject "$worker"
wait "$worker"
/usr/bin/grep -qx '{"stage":"runtime_mounts"}' "$fixture/result"
/usr/bin/printf 'private-source-ok\nlate-mount-isolated\nnested-mount-rejected-at-runtime-stage\n'
