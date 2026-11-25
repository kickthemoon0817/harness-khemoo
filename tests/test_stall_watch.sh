#!/usr/bin/env bash
# Exercises bin/stall_watch.sh in a scratch harness home. The lock waits are real: a
# holder takes a lock file under the scratch home and a flock(1) process waits on it,
# so lslocks, fuser and /proc report them as they would a build lock.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
pids=()
cleanup() { [ "${#pids[@]}" -gt 0 ] && kill "${pids[@]}" 2>/dev/null; pkill -f -- "$H/fake/rsync"; rm -rf "$H"; }
trap cleanup EXIT
mkdir -p "$H/bin" "$H/state/logs" "$H/fake" "$H/lk" "$H/none"
cp "$here/bin/stall_watch.sh" "$H/bin/"
S="$H/state"; W="$H/bin/stall_watch.sh"
# A stand-in for a copy to a remote host that never finishes.
printf '#!/usr/bin/env bash\nwhile :; do sleep 1; done\n' > "$H/fake/rsync"; chmod +x "$H/fake/rsync"
export HARNESS_HOME="$H" HARNESS_STATE="$S" HARNESS_PATH="$PATH"
export STALL_LOCK_GLOBS="$H/lk/worv-dev-build-remote*.lock $H/lk/worv-fast-set-remote*.lock"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
stalls() { cat "$S/stalls.log" 2>/dev/null; }
count() { stalls | grep -c -- "$1"; }

# The process that took the build lock has exited; the copy it started inherited the
# descriptor and holds the lock alone.
B="$H/lk/worv-dev-build-remote-t.lock"
( exec 8>"$B"; flock 8; "$H/fake/rsync" -a --delete -e "ssh -o BatchMode=yes" /w/tree fake@host:/w/ > /dev/null 2>&1 & echo $! > "$H/orphan.pid" )
pids+=("$(cat "$H/orphan.pid")")
# The suite lock's holder is alive and running a copy.
F="$H/lk/worv-fast-set-remote-t.lock"
bash -c 'exec 8>"$1"; flock 8; "$2" -a /w/evidence fake@host2:/w/ev/; :' _ "$F" "$H/fake/rsync" > /dev/null 2>&1 & pids+=($!)
sleep 0.5
flock "$B" true & pids+=($!)
flock "$F" true & pids+=($!)
flock "$F" true & pids+=($!)
sleep 0.5

STALL_LOCK_MIN=15 "$W"; rc=$?
check "a wait shorter than STALL_LOCK_MIN raises nothing" '[ "$rc" -eq 0 ] && [ "$(count lock_wait)" -eq 0 ]' "$(stalls)"

STALL_LOCK_MIN=0 "$W"
line=$(stalls | grep " STALL lock_wait $B ")
check "a lock wait is raised in the stall line format" '[[ "$line" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z\ STALL\ lock_wait\ $B\ 1\ waiting ]]' "$(stalls)"
check "a holder whose locker died is found through the child holding the descriptor" '[[ "$line" == *"holder pid $(cat "$H/orphan.pid"), up 0 min ("*"rsync -a --delete -e ssh -o BatchMode=yes /w/tree fake@host:/w/)"* ]] && [[ "$line" == *"lslocks names locker pid "*" (dead)"* ]]' "$line"
line=$(stalls | grep " STALL lock_wait $F ")
check "the waiters are counted and a live holder's copy is named" '[[ "$line" == *" 2 waiting (pids "* ]] && [[ "$line" == *"doing: "*"rsync -a /w/evidence fake@host2:/w/ev/"* ]] && [[ "$line" != *"sleep 1"* ]]' "$line"

STALL_LOCK_MIN=0 "$W"
check "the same lock is not raised again within the re-alert window" '[ "$(count lock_wait)" -eq 2 ]' "$(stalls)"
awk -v t="$(( $(date +%s) - 31 * 60 ))" -v b="$B" '$1 == "lock_wait" && $2 == b { $3 = t } { print }' "$S/stall_watch.seen" > "$S/seen.new" && mv "$S/seen.new" "$S/stall_watch.seen"
STALL_LOCK_MIN=0 "$W"
check "a lock still waited on is raised again once the window has passed" '[ "$(count "lock_wait $B ")" -eq 2 ] && [ "$(count "lock_wait $F ")" -eq 1 ]' "$(stalls)"

# A build holds its compile lock while its remote-docker shim waits on a tree lock that
# another shim holds for a copy to a slow host: the alarm follows the tree lock one step.
mkdir -p "$H/lk2" "$H/run/remote-docker-locks"
C="$H/lk2/worv-dev-build-remote-w03.lock"; T="$H/run/remote-docker-locks/0123456789abcdef.lock"
bash -c 'exec 7>"$1"; flock 7; "$2" -a --delete -e "ssh -o BatchMode=yes" /w/tree worv@slow.host:/w/; :' _ "$T" "$H/fake/rsync" > /dev/null 2>&1 & tree_holder=$!; pids+=("$tree_holder")
sleep 0.3
bash -c 'exec 8>"$1"; flock 8; exec 7>"$2"; flock -x 7; :' _ "$C" "$T" > /dev/null 2>&1 & build=$!; pids+=("$build")
sleep 0.3
flock "$C" true & pids+=($!)
sleep 0.5
STALL_LOCK_MIN=0 STALL_LOCK_GLOBS="$H/lk2/*.lock" "$W"
line=$(stalls | grep " STALL lock_wait $C ")
check "a holder blocked on a tree lock names that lock and its holder" '[[ "$line" == *"holder pid $build, up 0 min"* ]] && [[ "$line" == *"; blocked: pid "*" waits on tree lock $T, held by pid $tree_holder, up 0 min ("* ]]' "$line"
check "the tree lock holder's copy is named with its target host and age" '[[ "$line" == *"it is doing: "*"rsync -a --delete -e ssh -o BatchMode=yes /w/tree worv@slow.host:/w/ (0 min)"* ]]' "$line"
STALL_LOCK_MIN=0 STALL_LOCK_GLOBS="$H/lk/*.lock" "$W"
check "a holder that waits on no tree lock names no block" '! stalls | grep " STALL lock_wait $H/lk/" | grep -q "blocked:"' "$(stalls)"

# gpu_idle reads the probe table and the allocator log.
iso() { date -u -d "-$1 min" +%FT%TZ; }
{
  printf 'time\thost\tconnect_s\tupload_mb_s\tload1\tnproc\tgpu_util_pct\tgpu_mem_used_mb\tstatus\n'
  for m in 40 28 19 10 1; do
    printf '%s\tidle@a\t0.2\t80\t0.1\t16\t%s\t900\tok\n' "$(iso "$m")" "$([ "$m" = 40 ] && echo 55 || echo 0)"
    printf '%s\tbusy@b\t0.2\t80\t9.0\t32\t%s\t9000\tok\n' "$(iso "$m")" "$([ "$m" = 10 ] && echo 97 || echo 0)"
    printf '%s\tdown@d\t-\t-\t-\t-\t-\t-\tdown\n' "$(iso "$m")"
    printf '%s\tcompiling@e\t0.2\t80\t14.0\t16\t0\t30\tok\n' "$(iso "$m")"
  done
  printf '%s\tnew@c\t0.2\t80\t0.1\t16\t0\t500\tok\n' "$(iso 2)"
} > "$S/host_health.tsv"
rm -f "$S/stalls.log" "$S/stall_watch.seen"
echo "20261125T100000Z five=0% week=28% allowed=10 busy=4 claimable=0 -> spawning 0" > "$S/logs/allocator.log"
STALL_LOCK_GLOBS="$H/none/*.lock" "$W"
check "an idle card with no work waiting raises nothing" '[ "$(count gpu_idle)" -eq 0 ]' "$(stalls)"
{ echo "20261125T110000Z five=0% week=28% allowed=10 busy=4 claimable=2 -> spawning 1"
  echo "20261125T110500Z slot3 claude-opus-5-5 hit its usage limit; cooling 1800s"; } >> "$S/logs/allocator.log"
STALL_LOCK_GLOBS="$H/none/*.lock" "$W"
check "a card idle on every probe of the window while work is claimable is raised" '[ "$(count "STALL gpu_idle idle@a card 0 % and load under a quarter of the cores on all 4 probes")" -eq 1 ] && stalls | grep -q "claimable=2"' "$(stalls)"
check "a card busy on one probe, one that did not answer, one probed only lately, or a host compiling is not" '[ "$(count gpu_idle)" -eq 1 ] && [ "$(count "STALL gpu_idle compiling@e")" -eq 0 ]' "$(stalls)"
echo "20261125T111000Z five=0% week=28% allowed=10 busy=4 claimable=0 -> spawning 0" >> "$S/logs/allocator.log"
rm -f "$S/stall_watch.seen"
STALL_LOCK_MIN=15 "$W"
check "a lock waiter counts as work waiting for an idle card" '[ "$(count "STALL gpu_idle idle@a ")" -eq 2 ] && stalls | tail -1 | grep -q "while 3 lock waiter(s)"' "$(stalls)"
printf 'idle@a demoted 2 0 1764000000\n' > "$S/host_health.streaks"
rm -f "$S/stall_watch.seen"
STALL_LOCK_MIN=15 "$W"
check "a card on a host the probe moved last raises no gpu_idle" '[ "$(count "STALL gpu_idle idle@a ")" -eq 2 ]' "$(stalls)"
rm -f "$S/host_health.streaks"

# host_degraded reads bin/host_health.sh's streaks.
rm -f "$S/stalls.log" "$S/stall_watch.seen"
printf 'busy@b ok 0 5 0\nidle@a demoted 2 0 1764000000\n' > "$S/host_health.streaks"
STALL_LOCK_GLOBS="$H/none/*.lock" "$W"
check "a demoted host is raised with its demotion time and last probe" '[ "$(count "STALL host_degraded idle@a moved last at 2025-11-24T16:00:00Z, 2 bad probes in a row now; last probe: ok, upload 80 MB/s")" -eq 1 ] && [ "$(count host_degraded)" -eq 1 ]' "$(stalls)"
awk -v t="$(( $(date +%s) - 3600 ))" '$1 == "host_degraded" { $3 = t } { print }' "$S/stall_watch.seen" > "$S/seen.new" && mv "$S/seen.new" "$S/stall_watch.seen"
STALL_LOCK_GLOBS="$H/none/*.lock" "$W"
check "the same demotion is raised once, however long it lasts" '[ "$(count host_degraded)" -eq 1 ]' "$(stalls)"
printf 'idle@a demoted 2 0 1764003600\n' > "$S/host_health.streaks"
STALL_LOCK_GLOBS="$H/none/*.lock" "$W"
check "a later demotion of the same host is raised again" '[ "$(count host_degraded)" -eq 2 ]' "$(stalls)"

HARNESS_STATE=/proc/no-such-state "$W"; rc=$?
check "a state directory it cannot write still exits 0" '[ "$rc" -eq 0 ]' "rc=$rc"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
