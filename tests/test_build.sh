#!/usr/bin/env bash
# Exercises bin/build.sh in a scratch harness home: a fake build_host.sh whose answers each case
# names, a scratch worktree whose tools/dev/iter.sh takes the lane's lock the way the real one does
# (`flock -n` on WORV_DEV_BUILD_LOCK, then a blocking `flock <fd>`, then its pid into the lock file),
# and real flock holders on scratch lock files.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
T=$(mktemp -d); pids=()
# Ends every process this suite started and everything under them, so a case that fails leaves
# nothing waiting on a lock the next case uses.
reap() {
  local all
  [ "${#pids[@]}" -gt 0 ] || return 0
  all=$(ps -e -o pid=,ppid= | awk -v roots=" ${pids[*]} " '
    { parent[$1] = $2 }
    END { for (p in parent) { q = p; while (q > 1) { if (index(roots, " " q " ")) { print p; break } q = parent[q] } } }')
  kill $all 2>/dev/null; pids=(); sleep 0.2
}
cleanup() { reap; rm -rf "$T"; }
trap cleanup EXIT
H="$T/h"; W="$T/w"; F="$T/fake"; LK="$T/lk"
mkdir -p "$H/bin" "$H/state" "$W/tools/dev" "$F/py" "$LK"
cp "$here/bin/build.sh" "$H/bin/"
cat > "$H/bin/build_host.sh" <<'B'
#!/usr/bin/env bash
echo "${1:-normal}" >> "$FAKE/build_host.calls"
lane() {
  case "$1" in
    local) echo "export DOCKER_HOST=unix:///var/run/docker.sock WORV_DEV_BUILD_LOCK=$LK/worv-dev-build.lock WORV_BUILDER_IMAGE=img WORV_LOCAL_LANE=1" ;;
    *) echo "export DOCKER_HOST=ssh://u@$1 WORV_DEV_BUILD_LOCK=$LK/worv-dev-build-remote-$1.lock WORV_BUILDER_IMAGE=img WORV_LOCAL_LANE=" ;;
  esac
}
if [ "${1:-}" = --free-only ]; then [ -s "$FAKE/free" ] || exit 1; lane "$(cat "$FAKE/free")"; exit 0; fi
lane "$(cat "$FAKE/normal")"
B
cat > "$W/tools/dev/iter.sh" <<'I'
#!/usr/bin/env bash
[ "${1:-}" = build ] || exit 2; shift
echo "start $$ host=$DOCKER_HOST lock=${WORV_DEV_BUILD_LOCK##*/} n=$# args=$*" >> "$FAKE/iter.calls"
lock=${WORV_DEV_BUILD_LOCK:-/tmp/worv-dev-build.lock}
exec {fd}>>"$lock"
if ! flock -n "$fd"; then
  echo "iter.sh build: waiting for the dev build that holds $lock" >&2
  if [ -n "${FAKE_SLOW_RELEASE:-}" ]; then
    # A flock that has the lock but has not yet returned, as in the instant after the kernel grants it.
    cp "$FAKE/hold.py" "$FAKE/py/$fd"; (cd "$FAKE/py" && exec -a flock python3 "$fd")
  else
    flock "$fd"
  fi
fi
printf 'pid %s, %s, %s\n' "$$" "$PWD" "$*" > "$lock"
sleep "${FAKE_COMPILE_S:-1}"
exec {fd}>&-
echo "compiled $$" >> "$FAKE/iter.calls"
exit "${FAKE_RC:-0}"
I
cat > "$F/hold.py" <<'P'
import fcntl, os, sys, time
fcntl.flock(int(os.path.basename(sys.argv[0])), fcntl.LOCK_EX)
open(os.path.join(os.environ["FAKE"], "granted"), "w").close()
time.sleep(float(os.environ.get("FAKE_HOLD_S", "4")))
P
chmod +x "$H/bin/build.sh" "$H/bin/build_host.sh" "$W/tools/dev/iter.sh"
export HARNESS_HOME="$H" HARNESS_STATE="$H/state" FAKE="$F" LK BUILD_REBALANCE_S=1
unset DOCKER_HOST WORV_DEV_BUILD_LOCK FAKE_RC FAKE_COMPILE_S FAKE_SLOW_RELEASE
LOG="$H/state/logs/build.log"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
reset() { reap; rm -f "$F"/iter.calls "$F"/build_host.calls "$F"/free "$F"/granted "$F"/py/* "$LOG" "$T"/out "$T"/err "$T"/rc; echo "${1:-local}" > "$F/normal"; }
hold() { ( exec 9>"$LK/$1"; flock 9; exec sleep "$2" ) > /dev/null 2>&1 & pids+=($!); HOLDER=$!; sleep 0.3; }
# build [env…] -- [args…]: runs build.sh in the background from the worktree; BUILD is its pid.
build() {
  local e=(); while [ "$1" != -- ]; do e+=("$1"); shift; done; shift
  ( cd "$W" && env "${e[@]}" "$H/bin/build.sh" "$@" > "$T/out" 2> "$T/err"; echo $? > "$T/rc" ) & BUILD=$!; pids+=($BUILD)
}
await() { local i; for i in $(seq 1 "$((${1:-20} * 10))"); do [ -s "$T/rc" ] && return 0; sleep 0.1; done; return 1; }
row() { grep " build start=" "$LOG" 2>/dev/null | tail -1; }
starts() { grep -c '^start ' "$F/iter.calls" 2>/dev/null; }

# A waiter moves once another lane frees.
reset local; hold worv-dev-build.lock 30; HL=$HOLDER
build -- --test worv.env.manure
sleep 2.5; echo two > "$F/free"
await 15
check "a build waiting for a busy lane moves to the lane build_host.sh finds free, and compiles there" \
  '[ "$(cat "$T/rc")" = 0 ] && [ "$(starts)" = 2 ] && grep -q "^start .* lock=worv-dev-build.lock " "$F/iter.calls" && grep -q "^start .* host=ssh://u@two lock=worv-dev-build-remote-two.lock n=2 args=--test worv.env.manure$" "$F/iter.calls" && [ "$(grep -c "^compiled " "$F/iter.calls")" = 1 ]' \
  "rc=$(cat "$T/rc" 2>/dev/null) $(cat "$F/iter.calls" 2>/dev/null)"
check "the move is said on stderr" 'grep -qE "^build.sh: moved from local to two after [0-9]+ s waiting$" "$T/err"' "$(cat "$T/err")"
first=$(awk '/^start /{print $2; exit}' "$F/iter.calls")
check "the waiter it left is gone, and the lane's holder is untouched" '! kill -0 "$first" 2>/dev/null && kill -0 "$HL"' "first=$first"
check "build.log has the move and the build's row" \
  'grep -qE "Z build.sh: moved from local to two after [0-9]+ s waiting \($W\)$" "$LOG" && row | grep -qE "Z build start=[0-9T:-]+Z lane=two wait_s=[2-9] compile_s=[0-9]+ exit=0 moves=1 device=0 cwd=$W args=--test worv.env.manure$"' \
  "$(cat "$LOG" 2>/dev/null)"
kill "$HL" 2>/dev/null

# A build that holds its lock is never moved.
reset one; echo two > "$F/free"
build FAKE_COMPILE_S=4 -- worv.env.manure
await 15
check "a build compiling on its lane stays there though another lane is free" \
  '[ "$(cat "$T/rc")" = 0 ] && [ "$(starts)" = 1 ] && grep -q "^compiled " "$F/iter.calls" && [ ! -s "$T/err" ] && ! grep -q "^--free-only" "$F/build_host.calls"' \
  "rc=$(cat "$T/rc" 2>/dev/null) $(cat "$F/iter.calls" "$T/err" "$F/build_host.calls" 2>/dev/null)"
check "its row times the wait and the compile" 'row | grep -qE " lane=one wait_s=[0-2] compile_s=[3-6] exit=0 moves=0 "' "$(row)"
reset one; hold worv-dev-build-remote-one.lock 2
build FAKE_SLOW_RELEASE=1 FAKE_HOLD_S=4 -- worv.env.manure
for i in $(seq 1 50); do [ -e "$F/granted" ] && break; sleep 0.1; done
echo two > "$F/free"
await 15
check "a waiting flock the lock was granted to, though it has not returned yet, is never stopped" \
  '[ "$(cat "$T/rc")" = 0 ] && [ -e "$F/granted" ] && [ "$(starts)" = 1 ] && grep -q "^compiled " "$F/iter.calls" && ! grep -q moved "$T/err"' \
  "rc=$(cat "$T/rc" 2>/dev/null) $(cat "$F/iter.calls" "$T/err" 2>/dev/null)"

# Only a lane build_host.sh would give, free now.
reset local; hold worv-dev-build.lock 4
build -- worv.env.manure
sleep 1; echo two > "$F/normal"
await 15
check "a build does not move to a lane build_host.sh leaves out, though its lock is free" \
  '[ "$(cat "$T/rc")" = 0 ] && [ "$(starts)" = 1 ] && grep -q "^start .* lock=worv-dev-build.lock " "$F/iter.calls" && grep -q "^--free-only" "$F/build_host.calls" && ! grep -q moved "$T/err"' \
  "rc=$(cat "$T/rc" 2>/dev/null) $(cat "$F/iter.calls" "$T/err" 2>/dev/null)"
check "its row counts the wait" 'row | grep -qE " lane=local wait_s=[3-9] compile_s=[0-9]+ exit=0 moves=0 "' "$(row)"

# Exit codes, arguments, device builds.
reset one
build FAKE_RC=3 -- --test "a b" c
await 10
check "iter.sh's exit code passes through" '[ "$(cat "$T/rc")" = 3 ] && row | grep -q " exit=3 "' "rc=$(cat "$T/rc" 2>/dev/null) $(row)"
check "the arguments reach iter.sh as given" 'grep -q "^start .* n=3 args=--test a b c$" "$F/iter.calls"' "$(cat "$F/iter.calls")"
reset two; echo local > "$F/free"; hold worv-dev-build-remote-one.lock 3
build DOCKER_HOST=ssh://lease@card WORV_DEV_BUILD_LOCK="$LK/worv-dev-build-remote-one.lock" -- --test --device worv.env.manure
await 15
check "a device build runs on the caller's export line, untouched and never moved" \
  '[ "$(cat "$T/rc")" = 0 ] && [ ! -e "$F/build_host.calls" ] && [ "$(starts)" = 1 ] && grep -q "^start .* host=ssh://lease@card lock=worv-dev-build-remote-one.lock n=3 args=--test --device worv.env.manure$" "$F/iter.calls"' \
  "rc=$(cat "$T/rc" 2>/dev/null) $(cat "$F/iter.calls" "$F/build_host.calls" 2>/dev/null)"
check "its row says it was a device build" 'row | grep -qE " lane=one wait_s=[2-9] compile_s=[0-9]+ exit=0 moves=0 device=1 "' "$(row)"

# Signals and the caller's timeout.
reset local; hold worv-dev-build.lock 30
build -- worv.env.manure
sleep 1.5; it=$(awk '/^start /{print $2; exit}' "$F/iter.calls")
kill -TERM "$(pgrep -f "$H/bin/build.sh" | head -1)"
await 5
check "a signal sent to build.sh alone ends its iter.sh too" '[ "$(cat "$T/rc")" = 143 ] && ! kill -0 "$it" 2>/dev/null && row | grep -q " exit=143 "' "rc=$(cat "$T/rc" 2>/dev/null) it=$it $(row)"
reset local; hold worv-dev-build.lock 30
( cd "$W" && timeout -s KILL 2 "$H/bin/build.sh" worv.env.manure > /dev/null 2>&1; echo $? > "$T/rc" ) 2> /dev/null & pids+=($!)
await 10; sleep 0.3; it=$(awk '/^start /{print $2; exit}' "$F/iter.calls")
check "a caller's timeout reaches iter.sh as it would a bare iter.sh, waiting included" '[ "$(cat "$T/rc")" = 137 ] && [ -n "$it" ] && ! kill -0 "$it" 2>/dev/null' "rc=$(cat "$T/rc" 2>/dev/null) it=$it"
reap

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
