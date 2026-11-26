#!/usr/bin/env bash
# Exercises bin/build_host.sh with real flock holders and waiters on scratch lock files and a
# fake lease.sh, so lslocks reports them as it would a compile lock.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
T=$(mktemp -d); pids=()
cleanup() { [ "${#pids[@]}" -gt 0 ] && kill "${pids[@]}" 2>/dev/null; rm -rf "$T"; }
trap cleanup EXIT
mkdir -p "$T/h/bin" "$T/h/state/locks" "$T/lk"
cp "$here/bin/build_host.sh" "$T/h/bin/"
printf '%s\n' "a@one" "b@two" > "$T/h/state/locks/harness-first-remote-only"
cat > "$T/h/bin/lease.sh" <<'L'
#!/usr/bin/env bash
echo "remote card: a@one (slots 4-5)"
echo "remote card: b@two (slots 6-7)"
[ -n "${HOLD:-}" ] && echo "slot $HOLD HELD 2490 1 t domain=80 remote-all${HOLD_MARK:-} render (alive)"
exit 0
L
chmod +x "$T/h/bin/lease.sh"
export HARNESS_HOME="$T/h" HARNESS_STATE="$T/h/state" BUILD_LOCK_DIR="$T/lk" BUILD_LOCK_NAMES="a@one=one b@two=two"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
B="bash $T/h/bin/build_host.sh"
hold() { flock "$1" sleep 300 & pids+=($!); sleep 0.3; }
wait_on() { flock "$1" true & pids+=($!); sleep 0.3; }

out=$($B)
check "both locks free: the first host" '[ "$out" = "export DOCKER_HOST=ssh://a@one WORV_DEV_BUILD_LOCK=$T/lk/worv-dev-build-remote-one.lock" ]' "$out"
hold "$T/lk/worv-dev-build-remote-one.lock"
out=$($B)
check "the first host compiling: the second host, whose lock is free" '[ "$out" = "export DOCKER_HOST=ssh://b@two WORV_DEV_BUILD_LOCK=$T/lk/worv-dev-build-remote-two.lock" ]' "$out"
out=$(HOLD=6 $B)
check "the free host held whole by a cost read: left out" '[[ "$out" == *"ssh://a@one "* ]]' "$out"
out=$(HOLD=6 HOLD_MARK=" no-cost" $B)
check "a no-cost hold does not keep builds off its host" '[[ "$out" == *"ssh://b@two "* ]]' "$out"
hold "$T/lk/worv-dev-build-remote-two.lock"; wait_on "$T/lk/worv-dev-build-remote-one.lock"; wait_on "$T/lk/worv-dev-build-remote-one.lock"; wait_on "$T/lk/worv-dev-build-remote-two.lock"
out=$($B)
check "every lock taken: the host with the fewest waiters" '[[ "$out" == *"ssh://b@two "* ]]' "$out"
out=$(HOLD=4 $B)
check "with one host held whole, the other even when it has more waiters" '[[ "$out" == *"ssh://b@two "* ]]' "$out"
out=$(BUILD_LOCK_NAMES="" $B)
check "a host without a name gets its lock from its address" '[[ "$out" == *"worv-dev-build-remote-two.lock" ]]' "$out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
