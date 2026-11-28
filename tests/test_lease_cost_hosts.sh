#!/usr/bin/env bash
# Exercises bin/lease.sh's cost-read host limit in a scratch harness home with two remote cards, a
# room check that always answers ok, a fake docker and ssh with nothing running, sleeping processes
# as holders that are no tick (a run queue), and `claude -p` processes with claims as live ticks.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
PIDS=()
trap 'kill "${PIDS[@]}" 2>/dev/null; rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/state/locks" "$H/cache/cache-slot2" "$H/cache/cache-slot3" "$H/fakebin"
cp "$here/bin/lease.sh" "$here/bin/tick-pid.sh" "$H/bin/"
printf '#!/usr/bin/env bash\necho ok\n' > "$H/bin/remote_room.sh"
printf '#!/usr/bin/env bash\nexit 0\n' > "$H/fakebin/docker"
printf '#!/usr/bin/env bash\nexit 0\n' > "$H/fakebin/ssh"
chmod +x "$H/bin/remote_room.sh" "$H/fakebin/docker" "$H/fakebin/ssh"
export HARNESS_HOME="$H" GPU_SLOTS=3 LEASE_CACHE_BASE="$H/cache" PATH="$H/fakebin:$PATH"
unset COST_READ_HOSTS_MAX GPU_KITS_MAX REMOTE_KITS_MAX IDLE_HOLD_S
L="$H/state/locks"; LEASE="$H/bin/lease.sh"; ERR="$H/err"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
ago() { date -u -d "-$1 sec" +%FT%TZ; }
old() { echo "$(( $(date -u +%s) - $1 ))"; }
tick() { ( exec -a claude bash -p -c 'sleep 600; :' ) >/dev/null 2>&1 & PIDS+=("$!"); printf -v "$1" %s "$!"; }
named() { sleep 600 >/dev/null 2>&1 & PIDS+=("$!"); printf -v "$1" %s "$!"; }
tick T1; tick T2; named Q1; named Q2
sleep 0.3
reset() { rm -f "$L"/resource.lease* "$L"/claim.* "$L"/all-wait.* "$L"/slot-wait.* "$L"/cost-wait.* "$L"/operator-priority "$L"/idle-since.*
  printf 'a@one\nb@two\n' > "$L/remote-card"; }
claim() { echo "$1 $2 $(ago 60)" > "$L/claim.$1"; }
remote_all() { for s in 4 5; do echo "$1 $2 $(ago 300) domain=$(( 76 + s )) remote-all${3:+ $3} render" > "$L/resource.lease.$s"; done; }
local_all() { echo "$1 $2 $(ago 300) domain=77 exclusive" > "$L/resource.lease"
  echo "$1 $2 $(ago 300) domain=78 held-for-all" > "$L/resource.lease.2"; echo "$1 $2 $(ago 300) domain=79 held-for-all" > "$L/resource.lease.3"; }
acq() { LEASE_HOLDER_PID=$$ "$LEASE" acquire "$@" 2>"$ERR"; }

# The refusal: another issue holds a host whole and another tick is live.
reset; remote_all 301 "$Q1"; claim 401 "$T1"
out=$(acq 302 --all-remote); rc=$?
check "a whole remote card acquire is refused while another issue holds a host whole and another tick is live" \
  '[ "$rc" -eq 1 ] && [ "$out" = "BUSY cost-read host limit 301@a@one" ] && [ ! -e "$L/resource.lease.6" ]' "rc=$rc $out"
check "the refused whole remote card acquire leaves its cost-wait marker, as a waiting one does" \
  'read -r w t < "$L/cost-wait.302" && [ "$w" = any ] && [ "$t" -ge "$(old 5)" ]' "$(cat "$L/cost-wait.302" 2>/dev/null)"
echo "303 $$ $(ago 30) domain=78 reserved-for-all" > "$L/resource.lease.2"
out=$(acq 303 --all --render); rc=$?
check "a whole local card acquire is refused the same way, and drops its reservations" \
  '[ "$rc" -eq 1 ] && [ "$out" = "BUSY cost-read host limit 301@a@one" ] && [ ! -e "$L/resource.lease" ] && [ ! -e "$L/resource.lease.2" ] && [ ! -e "$L/resource.lease.3" ]' "rc=$rc $out"
reset; local_all 304 "$Q1"; claim 401 "$T1"
out=$(acq 305 --all-remote); rc=$?
check "a host held whole by a local --all hold counts too" '[ "$rc" -eq 1 ] && [ "$out" = "BUSY cost-read host limit 304@local" ]' "rc=$rc $out"

# The same issue's second host.
reset; remote_all 306 "$Q1"; claim 306 "$T2"; claim 401 "$T1"
out=$(acq 306 --all-remote); rc=$?
check "the same issue's second host is refused while another tick is live" \
  '[ "$rc" -eq 1 ] && [ "$out" = "BUSY cost-read host limit 306@a@one" ] && [ ! -e "$L/resource.lease.6" ]' "rc=$rc $out"
rm -f "$L/claim.401"
out=$(acq 306 --all-remote); rc=$?
check "the same issue may hold both hosts while no other tick is live, its own tick aside" \
  '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=6 "*"remote=b@two exclusive=all-remote"* ]]' "rc=$rc $out"

# Alone: today's behaviour.
reset; remote_all 301 "$Q1"; claim 302 "$T1"; claim 402 "$Q2"; claim 403 999999
out=$(acq 302 --all-remote); rc=$?
check "with no other tick live a second host is granted: a claim whose pid is no tick, or is dead, is no live tick" \
  '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=6 "* ]]' "rc=$rc $out"
reset; remote_all 301 "$Q1"
out=$(acq 303 --all --render); rc=$?
check "with no tick live at all a whole local card is granted beside a held remote one" '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=1 "*"exclusive=all"* ]]' "rc=$rc $out"

# What the limit leaves alone.
reset; remote_all 301 "$Q1"; claim 401 "$T1"; local_all 305 $$
out=$(acq 305 --all --render); rc=$?
check "a holder renewing the whole card it already holds is never refused" '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=1 "*"exclusive=all"* ]]' "rc=$rc $out"
reset; remote_all 301 "$Q1" no-cost; claim 401 "$T1"
out=$(acq 302 --all-remote); rc=$?
check "a no-cost hold is no cost read and does not count" '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=6 "* ]]' "rc=$rc $out"
reset; remote_all 301 "$Q1"; claim 401 "$T1"
out=$(acq 302 --all-remote --no-cost); rc=$?
check "a no-cost acquire is no cost read and is not refused" '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=6 "* ]] && [ ! -e "$L/cost-wait.302" ]' "rc=$rc $out"
reset; remote_all 301 "$Q1"; claim 401 "$T1"
out=$(acq 302); rc=$?
check "a single-slot acquire is not limited" '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=1 "* ]]' "rc=$rc $out"

# The waiting line and priority are unchanged.
reset; remote_all 301 "$Q1"; claim 401 "$T1"; echo "$(old 2000) 1 $(old 10)" > "$L/all-wait.303"
out=$(acq 303 --all --render); rc=$?
check "a refused whole-card acquire keeps its place in the whole-card line: its first wait time stays and its tick is recorded" \
  '[ "$rc" -eq 1 ] && read -r first wp last < "$L/all-wait.303" && [ "$first" -le "$(old 1999)" ] && [ "$wp" = "$$" ] && [ "$last" -ge "$(old 5)" ]' "rc=$rc $(cat "$L/all-wait.303")"
out=$("$LEASE" status 2>/dev/null)
check "its wait counts toward priority as usual" '[[ "$out" == *"wait whole-card issue 303 for 33 min (priority)"* ]]' "$out"
out=$(acq 307); rc=$?
check "a single-slot acquire meets that priority as usual: the local card is kept for it, a remote card is not" \
  '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=6 "*"remote=b@two"* ]] && [ ! -e "$L/resource.lease" ]' "rc=$rc $out"
LEASE_HOLDER_PID=$$ "$LEASE" release 307 >/dev/null 2>&1; rm -f "$L/remote-card"
out=$(acq 307); rc=$?
check "with no remote card it waits behind that priority" '[ "$rc" -eq 1 ] && [[ "$out" == "BUSY priority to whole-card issue 303"* ]]' "rc=$rc $out"

# The knob.
reset; remote_all 301 "$Q1"; claim 401 "$T1"
out=$(COST_READ_HOSTS_MAX=0 acq 302 --all-remote); rc=$?
check "COST_READ_HOSTS_MAX=0 turns the limit off" '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=6 "* ]]' "rc=$rc $out"
reset; remote_all 301 "$Q1"; claim 401 "$T1"
out=$(COST_READ_HOSTS_MAX= acq 302 --all-remote); rc=$?
check "an empty COST_READ_HOSTS_MAX turns the limit off" '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=6 "* ]]' "rc=$rc $out"
reset; remote_all 301 "$Q1"; claim 401 "$T1"
out=$(COST_READ_HOSTS_MAX=2 acq 302 --all-remote); rc=$?
check "COST_READ_HOSTS_MAX=2 lets a second host be held whole" '[ "$rc" -eq 0 ] && [[ "$out" == "ACQUIRED slot=6 "* ]]' "rc=$rc $out"
reset; remote_all 301 "$Q1"; claim 401 "$T1"; claim 402 "$T2"
out=$("$LEASE" status 2>/dev/null)
check "status names the limit while it is on, with the hosts held whole and the live ticks" \
  'printf "%s\n" "$out" | grep -qx "cost-read host limit: 1 host(s) held whole while another tick is live (held: 301@a@one; live ticks: 2)"' "$out"
out=$(COST_READ_HOSTS_MAX=0 "$LEASE" status 2>/dev/null)
check "status says nothing of the limit while it is off" '[[ "$out" != *"cost-read host limit"* ]]' "$out"
reset

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
