#!/usr/bin/env bash
# Exercises bin/lease.sh's idle-hold reclaim in a scratch harness home: sleeping processes as live
# holders, a fake docker whose running containers each test names per daemon, and a fake ssh that
# runs the remote command against the fake docker as that host's daemon.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
sleep 600 >/dev/null 2>&1 & P1=$!
sleep 600 >/dev/null 2>&1 & P2=$!
sleep 600 >/dev/null 2>&1 & P3=$!
sleep 600 >/dev/null 2>&1 & P4=$!
trap 'kill $P1 $P2 $P3 $P4 2>/dev/null; rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/state/locks" "$H/cache" "$H/fake" "$H/fakebin"
cp "$here/bin/lease.sh" "$H/bin/lease.sh"
F="$H/fake"
# running.<daemon> lists the daemon's running containers; down.<daemon> makes it fail to answer.
cat > "$H/fakebin/docker" <<'D'
#!/usr/bin/env bash
d=${FAKE_DAEMON:-local}
echo "$d DOCKER_HOST=${DOCKER_HOST:-} $*" >> "$FAKE/calls"
[ -e "$FAKE/down.$d" ] && { echo "Cannot connect to the Docker daemon" >&2; exit 1; }
[ "${1:-}" = ps ] && cat "$FAKE/running.$d" 2>/dev/null
exit 0
D
# unreachable.<host> makes ssh to that host fail as a dropped connection does.
cat > "$H/fakebin/ssh" <<'S'
#!/usr/bin/env bash
while [ "$#" -gt 0 ]; do case "$1" in -o) shift 2 ;; -*) shift ;; *) break ;; esac; done
host=$1; shift
[ -e "$FAKE/unreachable.$host" ] && exit 255
FAKE_DAEMON=$host exec bash -c "$*"
S
printf '#!/usr/bin/env bash\necho ok\n' > "$H/bin/remote_room.sh"
chmod +x "$H/fakebin/docker" "$H/fakebin/ssh" "$H/bin/remote_room.sh"
export HARNESS_HOME="$H" GPU_SLOTS=3 LEASE_CACHE_BASE="$H/cache" FAKE="$F" PATH="$H/fakebin:$PATH"
unset IDLE_HOLD_S IDLE_SAMPLE_GAP_S DOCKER_HOST
L="$H/state/locks"; LEASE="$H/bin/lease.sh"; LOG="$H/state/logs/lease.log"; ERR="$H/err"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
ago() { date -u -d "-$1 sec" +%FT%TZ; }
old() { echo "$(( $(date -u +%s) - $1 ))"; }
reset() { rm -f "$L"/resource.lease* "$L"/idle-since.* "$L"/device.lease "$L"/operator-priority "$L"/remote-card "$F"/*; }
# An idle record as lease.sh keeps it: idle since <s ago>, last sampled <s ago>, holder issue and pid.
idle() { echo "$(old "$2") $(old "$3") $4 $5" > "$L/idle-since.$1"; }
st() { "$LEASE" status 2>"$ERR"; }
acq() { LEASE_HOLDER_PID=$$ "$LEASE" acquire "$@" 2>"$ERR"; }

# The clock, end to end: first sample, a sample before the limit, a sample past it.
reset
echo "11 $P1 $(ago 30) domain=77" > "$L/resource.lease"
out=$(IDLE_HOLD_S=3 DOCKER_HOST=ssh://elsewhere "$LEASE" status 2>"$ERR")
check "the first sample of an idle hold records it and reclaims nothing" \
  '[ -s "$L/resource.lease" ] && [ -s "$L/idle-since.1" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"
check "a local slot is read on this host's daemon, whatever DOCKER_HOST the caller carries" \
  'grep -q "^local DOCKER_HOST= ps " "$F/calls"' "$(cat "$F/calls")"
sleep 1.2
out=$(IDLE_HOLD_S=3 st)
check "an idle hold is not reclaimed before IDLE_HOLD_S" '[ -s "$L/resource.lease" ] && [ ! -s "$ERR" ] && [[ "$out" == *"slot 1 HELD 11 "* ]]' "$out $(cat "$ERR")"
sleep 2.2
out=$(IDLE_HOLD_S=3 st)
check "an idle hold is reclaimed once it has been idle IDLE_HOLD_S on end" \
  'grep -qE "^RECLAIMED slot=1 from 11 \(no kit for [3-9] s\)$" "$ERR" && [ ! -e "$L/resource.lease" ] && [ ! -e "$L/idle-since.1" ] && [[ "$out" == *"slot 1 free"* ]] && [[ "$out" != *RECLAIMED* ]]' "$out / $(cat "$ERR")"
check "the reclaim is logged with the hold it released" \
  'grep -qE "Z RECLAIMED slot=1 from 11 \(no kit for [0-9]+ s\) \[11 $P1 .* domain=77\] seen by status$" "$LOG"' "$(cat "$LOG" 2>/dev/null)"

# Holds the rule leaves alone, each idle past the limit.
reset
echo "12 $P1 $(ago 900) domain=77" > "$L/resource.lease"; idle 1 700 30 12 "$P1"; echo "9 12" > "$L/operator-priority"
echo "13 $P2 $(ago 30) domain=78 reserved-for-all" > "$L/resource.lease.2"; idle 2 700 30 13 "$P2"
echo "14 $P3 $(ago 900) domain=0 device" > "$L/device.lease"
out=$(st)
check "a hold of an issue the operator named is never reclaimed, and keeps no idle record" \
  '[ -s "$L/resource.lease" ] && [ ! -e "$L/idle-since.1" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"
check "a whole-card reservation is left to lapse on its own" '[ -s "$L/resource.lease.2" ] && [ ! -e "$L/idle-since.2" ]' "$(cat "$ERR")"
check "a device lease is never reclaimed" '[ -s "$L/device.lease" ] && [[ "$out" == *"device HELD 14 "* ]]' "$out"
rm -f "$L/operator-priority"
out=$(st)
check "once the operator drops the issue, its hold's clock starts from the next sample" \
  '[ -s "$L/resource.lease" ] && [ -s "$L/idle-since.1" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"

# What starts the clock again.
reset
echo "21 $P1 $(ago 900) domain=77" > "$L/resource.lease"; idle 1 700 30 21 "$P1"
echo worv-iter > "$F/running.local"
out=$(st)
check "a container seen running clears its slot's idle record" '[ -s "$L/resource.lease" ] && [ ! -e "$L/idle-since.1" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"
reset
echo "23 $P2 $(ago 30) domain=77" > "$L/resource.lease"; idle 1 700 30 22 "$P1"
out=$(st)
check "a slot that changed holder starts its idle record again" \
  '[ -s "$L/resource.lease" ] && [ ! -s "$ERR" ] && read -r s l i p < "$L/idle-since.1" && [ "$s" = "$l" ] && [ "$i $p" = "23 $P2" ]' "$(cat "$L/idle-since.1") $(cat "$ERR")"
reset
echo "24 $P1 $(ago 900) domain=77" > "$L/resource.lease"; idle 1 700 400 24 "$P1"
out=$(st)
check "a gap of more than IDLE_SAMPLE_GAP_S between samples starts the clock again" \
  '[ -s "$L/resource.lease" ] && [ ! -s "$ERR" ] && read -r s l _ < "$L/idle-since.1" && [ "$s" = "$l" ]' "$(cat "$L/idle-since.1") $(cat "$ERR")"
reset
echo "25 $$ $(ago 900) domain=77" > "$L/resource.lease"; idle 1 700 30 25 $$
out=$(LEASE_HOLDER_PID=$$ "$LEASE" release 25)
check "a release drops the slot's idle record" '[[ "$out" == "RELEASED slot=1" ]] && [ ! -e "$L/idle-since.1" ]' "$out"
out=$(acq 25); out2=$(st)
check "a slot released and taken again starts a new clock" '[[ "$out" == "ACQUIRED slot=1 "* ]] && [ -s "$L/resource.lease" ] && [ ! -s "$ERR" ]' "$out / $out2 / $(cat "$ERR")"

# A daemon that cannot be read never reclaims.
reset
echo "31 $P1 $(ago 900) domain=77" > "$L/resource.lease"; idle 1 700 30 31 "$P1"; touch "$F/down.local"
out=$(st)
check "a hold whose daemon does not answer is never reclaimed, and its clock starts again" \
  '[ -s "$L/resource.lease" ] && [ ! -e "$L/idle-since.1" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"
rm -f "$F/down.local"; out=$(st)
check "the time the daemon did not answer does not count" '[ -s "$L/resource.lease" ] && [ -s "$L/idle-since.1" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"

# acquire samples too, and takes the slot it frees.
reset
echo "41 $P1 $(ago 900) domain=77" > "$L/resource.lease"; idle 1 700 30 41 "$P1"
out=$(GPU_SLOTS=1 GPU_KITS_MAX=1 acq 42)
check "an acquire reclaims an idle hold and takes the slot it frees" \
  'grep -qE "^RECLAIMED slot=1 from 41 \(no kit for 70[0-9] s\)$" "$ERR" && [[ "$out" == "ACQUIRED slot=1 "* ]] && grep -q "^42 $$ " "$L/resource.lease"' "$out / $(cat "$ERR")"
check "the log names the acquire that saw it" 'grep -q "RECLAIMED slot=1 from 41 .* seen by acquire 42$" "$LOG"' "$(tail -1 "$LOG")"
reset
echo "43 $P1 $(ago 900) domain=77" > "$L/resource.lease"; idle 1 500 30 43 "$P1"
out=$(GPU_SLOTS=1 GPU_KITS_MAX=1 acq 44)
check "an acquire leaves a hold idle less than IDLE_HOLD_S" '[[ "$out" == BUSY* ]] && grep -q "^43 $P1 " "$L/resource.lease" && [ ! -s "$ERR" ]' "$out / $(cat "$ERR")"
check "IDLE_HOLD_S=0 turns the rule off" \
  'out=$(IDLE_HOLD_S=0 st) && idle 1 900 30 43 "$P1" && out=$(IDLE_HOLD_S=0 st) && grep -q "^43 $P1 " "$L/resource.lease" && [ ! -s "$ERR" ]' "$(cat "$ERR")"
rm -f "$L"/slot-wait.*

# A whole-card hold is idle only while every slot it holds is.
reset
echo "51 $P1 $(ago 900) domain=77 exclusive" > "$L/resource.lease"
echo "51 $P1 $(ago 900) domain=78 held-for-all" > "$L/resource.lease.2"
echo "51 $P1 $(ago 900) domain=79 held-for-all" > "$L/resource.lease.3"
for s in 1 2 3; do idle "$s" 700 30 51 "$P1"; done
echo worv-iter-3 > "$F/running.local"
out=$(st)
check "a whole-card hold with any of its slots' containers running keeps the card" '[ -s "$L/resource.lease" ] && [ -s "$L/resource.lease.2" ] && [ -s "$L/resource.lease.3" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"
rm -f "$F/running.local"; idle 3 500 30 51 "$P1"
out=$(st)
check "a whole-card hold is idle only as long as its most recently busy slot" '[ -s "$L/resource.lease" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"
idle 3 700 30 51 "$P1"
out=$(st)
check "a whole-card hold idle on every slot gives the whole card back" \
  '[ "$(grep -c "^RECLAIMED slot=[123] from 51 (no kit for 70[0-9] s)$" "$ERR")" = 3 ] && ! ls "$L"/resource.lease* >/dev/null 2>&1' "$(cat "$ERR")"

# Remote slots are read on their own host.
reset
printf 'a@one\nb@two\n' > "$L/remote-card"
echo "61 $P1 $(ago 900) domain=80 remote" > "$L/resource.lease.4"
echo "62 $P2 $(ago 900) domain=81 remote render" > "$L/resource.lease.5"
echo "63 $P3 $(ago 900) domain=82 remote-all render" > "$L/resource.lease.6"
echo "63 $P3 $(ago 900) domain=83 remote-all render" > "$L/resource.lease.7"
idle 4 700 30 61 "$P1"; idle 5 700 30 62 "$P2"; idle 6 700 30 63 "$P3"; idle 7 700 30 63 "$P3"
printf 'worv-iter-5\nworv-iter-6\nworv-iter-7\n' > "$F/running.a@one"
out=$(st)
check "a remote slot idle on its own host is reclaimed" 'grep -q "^RECLAIMED slot=4 from 61 " "$ERR" && [ ! -e "$L/resource.lease.4" ]' "$(cat "$ERR")"
check "a remote slot whose container runs on its host is kept" '[ -s "$L/resource.lease.5" ] && [ ! -e "$L/idle-since.5" ]' "$(cat "$ERR")"
check "a whole remote card is read on its host, not on another host running the same names" \
  'grep -q "^RECLAIMED slot=6 from 63 " "$ERR" && grep -q "^RECLAIMED slot=7 from 63 " "$ERR" && [ ! -e "$L/resource.lease.6" ] && [ ! -e "$L/resource.lease.7" ]' "$(cat "$ERR")"
check "each remote host is read once a sample, with ssh" \
  '[ "$(grep -c "^a@one " "$F/calls")" = 1 ] && [ "$(grep -c "^b@two " "$F/calls")" = 1 ] && ! grep -q "^local " "$F/calls"' "$(cat "$F/calls")"
rm -f "$F"/*
echo "64 $P4 $(ago 900) domain=82 remote" > "$L/resource.lease.6"; idle 6 700 30 64 "$P4"; touch "$F/unreachable.b@two"
out=$(st)
check "a remote hold whose host does not answer is never reclaimed" '[ -s "$L/resource.lease.6" ] && [ ! -e "$L/idle-since.6" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"
rm -f "$F/unreachable.b@two"; echo worv-iter-6 > "$F/running.b@two"; idle 6 700 30 64 "$P4"; touch "$F/down.a@one"
out=$(st)
check "a remote container seen running keeps its hold" '[ -s "$L/resource.lease.6" ] && [ ! -e "$L/idle-since.6" ] && [ ! -s "$ERR" ]' "$(cat "$ERR")"
reset

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
