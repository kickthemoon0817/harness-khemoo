#!/usr/bin/env bash
# Exercises bin/build_host.sh with real flock holders and waiters on scratch lock files and a
# fake lease.sh, so lslocks reports them as it would a compile lock.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
T=$(mktemp -d); pids=()
cleanup() { [ "${#pids[@]}" -gt 0 ] && kill "${pids[@]}" 2>/dev/null; rm -rf "$T"; }
trap cleanup EXIT
mkdir -p "$T/h/bin" "$T/h/state/locks" "$T/lk"
cp "$here/bin/build_host.sh" "$here/bin/cost_wait_hosts.sh" "$T/h/bin/"
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
hold() { ( exec 9>"$1"; flock 9; exec sleep 300 ) > /dev/null 2>&1 & pids+=($!); sleep 0.3; }
wait_on() { flock "$1" true > /dev/null 2>&1 & pids+=($!); sleep 0.3; }

out=$($B)
check "both locks free: the first host" '[ "$out" = "export DOCKER_HOST=ssh://a@one WORV_DEV_BUILD_LOCK=$T/lk/worv-dev-build-remote-one.lock WORV_BUILDER_IMAGE=worv-builder:isaac6" ]' "$out"
hold "$T/lk/worv-dev-build-remote-one.lock"
out=$($B)
check "the first host compiling: the second host, whose lock is free" '[ "$out" = "export DOCKER_HOST=ssh://b@two WORV_DEV_BUILD_LOCK=$T/lk/worv-dev-build-remote-two.lock WORV_BUILDER_IMAGE=worv-builder:isaac6" ]' "$out"
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
check "a host without a name gets its lock from its address" '[[ "$out" == *"worv-dev-build-remote-two.lock "* ]]' "$out"


# A waiting cost read keeps compiles off the card it waits for.
kill "${pids[@]}" 2>/dev/null; pids=(); sleep 0.3
echo "b@two $(date +%s)" > "$T/h/state/locks/cost-wait.2464"
hold "$T/lk/worv-dev-build-remote-one.lock"
out=$($B)
check "a cost read waiting for a host keeps compiles off it, even behind a taken lock" '[[ "$out" == *"ssh://a@one "* ]]' "$out"
echo "b@two $(( $(date +%s) - 600 ))" > "$T/h/state/locks/cost-wait.2464"
out=$($B)
check "a wait marker not refreshed for COST_WAIT_FRESH_S is ignored" '[[ "$out" == *"ssh://b@two "* ]]' "$out"
kill "${pids[@]}" 2>/dev/null; pids=(); sleep 0.3
{ printf 'time\thost\tconnect_s\tupload_mb_s\tload1\tnproc\tgpu_util_pct\tgpu_mem_used_mb\tstatus\n'
  printf 't\ta@one\t0.2\t80\t2.0\t16\t0\t20\tok\n'; printf 't\tb@two\t0.2\t80\t20.0\t24\t0\t20\tok\n'; } > "$T/h/state/host_health.tsv"
echo "any $(date +%s)" > "$T/h/state/locks/cost-wait.2502"; rm -f "$T/h/state/locks/cost-wait.2464"
out=$($B)
check "a wait for any card keeps the quietest unheld host free of compiles" '[[ "$out" == *"ssh://b@two "* ]]' "$out"
echo "a@one $(date +%s)" > "$T/h/state/locks/cost-wait.2464"; echo "b@two $(date +%s)" > "$T/h/state/locks/cost-wait.2502"
out=$($B)
check "with every host waited for, they are all candidates again" '[[ "$out" == *"ssh://a@one "* ]]' "$out"
rm -f "$T/h/state/locks/cost-wait."*


# The local CPU lane, switched on by state/local-cpu-lane.
kill "${pids[@]}" 2>/dev/null; pids=(); sleep 0.3; rm -f "$T/h/state/locks/cost-wait."*
echo "worv-builder:isaac6-manure" > "$T/h/state/local-cpu-lane"
out=$($B)
check "with the local lane on and a remote lane free, the remote lane still comes first" '[[ "$out" == *"ssh://a@one "*"WORV_BUILDER_IMAGE=worv-builder:isaac6" ]]' "$out"
hold "$T/lk/worv-dev-build-remote-one.lock"; hold "$T/lk/worv-dev-build-remote-two.lock"
out=$($B)
check "both remote lanes compiling: the local lane, its own lock and image" '[ "$out" = "export DOCKER_HOST=unix:///var/run/docker.sock WORV_DEV_BUILD_LOCK=$T/lk/worv-dev-build.lock WORV_BUILDER_IMAGE=worv-builder:isaac6-manure" ]' "$out"
kill "${pids[@]}" 2>/dev/null; pids=(); sleep 0.3
echo "a@one $(date +%s)" > "$T/h/state/locks/cost-wait.2464"; echo "b@two $(date +%s)" > "$T/h/state/locks/cost-wait.2502"
out=$($B)
check "every remote host waited for: the local lane, not a loaded remote one" '[[ "$out" == *"unix:///var/run/docker.sock"* ]]' "$out"
rm -f "$T/h/state/locks/cost-wait."*
hold "$T/lk/worv-dev-build.lock"; hold "$T/lk/worv-dev-build-remote-one.lock"; hold "$T/lk/worv-dev-build-remote-two.lock"; wait_on "$T/lk/worv-dev-build-remote-one.lock"; wait_on "$T/lk/worv-dev-build-remote-two.lock"
out=$($B)
check "every lane taken: the local lane when it has the fewest waiters" '[[ "$out" == *"unix:///var/run/docker.sock"* ]]' "$out"
out=$(BUILD_HOST_REMOTE_ONLY=1 $B)
check "BUILD_HOST_REMOTE_ONLY leaves the local lane out" '[[ "$out" == *"ssh://"* ]]' "$out"
rm -f "$T/h/state/local-cpu-lane"
out=$($B)
check "with the lane switched off, no compile goes local" '[[ "$out" == *"ssh://"* ]]' "$out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
