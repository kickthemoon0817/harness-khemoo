#!/usr/bin/env bash
# Exercises bin/lease.sh's idle-hold clocks and on-demand takeover in a scratch harness home. Live
# holders are sleeping processes; a tick holder runs as `claude -p` (bin/tick-pid.sh's test), with a
# child whose command line stands for what the tick is doing. A fake docker lists the running
# containers each test names per daemon, and a fake ssh runs the remote command against it as that
# host's daemon.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
PIDS=()
trap 'kill "${PIDS[@]}" 2>/dev/null; rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/state/locks" "$H/cache/cache-slot2" "$H/cache/cache-slot3" "$H/fake" "$H/fakebin"
cp "$here/bin/lease.sh" "$here/bin/tick-pid.sh" "$H/bin/"
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
unset IDLE_HOLD_S IDLE_SAMPLE_GAP_S DOCKER_HOST GPU_KITS_MAX REMOTE_KITS_MAX
L="$H/state/locks"; LEASE="$H/bin/lease.sh"; LOG="$H/state/logs/lease.log"; ERR="$H/err"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
ago() { date -u -d "-$1 sec" +%FT%TZ; }
old() { echo "$(( $(date -u +%s) - $1 ))"; }
reset() { rm -f "$L"/resource.lease* "$L"/idle-since.* "$L"/device.lease "$L"/operator-priority "$L"/remote-card \
  "$L"/slot-wait.* "$L"/all-wait.* "$L"/cost-wait.* "$F"/* "$LOG"; }
# tick VAR [command line]: a live `claude -p` holder, with a child running under that command line.
tick() {
  local var=$1; shift
  if [ "$#" -gt 0 ]; then ( exec -a claude bash -p -c "bash -c 'sleep 600; :' $*; :" ) >/dev/null 2>&1 &
  else ( exec -a claude bash -p -c 'sleep 600; :' ) >/dev/null 2>&1 &
  fi
  PIDS+=("$!"); printf -v "$var" %s "$!"
}
# named VAR: a live holder that is no tick, as a script or the owner's own session is.
named() { sleep 600 >/dev/null 2>&1 & PIDS+=("$!"); printf -v "$1" %s "$!"; }
# An idle record as lease.sh keeps it: idle since <s ago>, last sampled <s ago>, holder issue and pid.
idle() { echo "$(old "$2") $(old "$3") $4 $5" > "$L/idle-since.$1"; }
hold() { echo "$2 $3 $(ago 900) domain=$(( 76 + $1 ))${4:+ $4}" > "$(lf "$1")"; }
lf() { if [ "$1" -eq 1 ]; then echo "$L/resource.lease"; else echo "$L/resource.lease.$1"; fi; }
st() { "$LEASE" status 2>"$ERR"; }
acq() { LEASE_HOLDER_PID=$$ "$LEASE" acquire "$@" 2>"$ERR"; }
# has TEXT ERE: a line of TEXT matches the extended regular expression.
has() { printf '%s\n' "$1" | grep -qE -- "$2"; }
tick T1; tick T2; named N1; named N2
sleep 0.3

# Sampling keeps the clocks and reports them; it never releases.
reset; hold 1 11 "$T1"
out=$(IDLE_HOLD_S=3 DOCKER_HOST=ssh://elsewhere "$LEASE" status 2>"$ERR")
check "the first sample of an idle tick hold starts its clock and prints it" \
  '[ -s "$L/idle-since.1" ] && has "$out" "^slot 1 HELD 11 $T1 [^ ]+ domain=77 idle 0 s \(alive\)$" && [ ! -s "$ERR" ]' "$out / $(cat "$ERR")"
check "a local slot is read on this host's daemon, whatever DOCKER_HOST the caller carries" \
  'grep -q "^local DOCKER_HOST= ps " "$F/calls"' "$(cat "$F/calls")"
idle 1 700 30 11 "$T1"
out=$(st)
check "status never releases a hold idle past IDLE_HOLD_S: it reports it" \
  'grep -q "^11 $T1 " "$L/resource.lease" && has "$out" "^slot 1 HELD 11 .* idle 70[0-9] s \(alive\)$" && [ ! -s "$ERR" ] && [ ! -e "$LOG" ]' "$out / $(cat "$ERR")"

# The clock end to end, then the take: not before IDLE_HOLD_S, and on demand after it.
reset; hold 1 21 "$T1"
IDLE_HOLD_S=3 st >/dev/null; sleep 1.2
out=$(GPU_SLOTS=1 IDLE_HOLD_S=3 acq 22)
check "an idle hold is not taken before IDLE_HOLD_S" '[[ "$out" == BUSY* ]] && grep -q "^21 $T1 " "$L/resource.lease" && [ ! -s "$ERR" ]' "$out / $(cat "$ERR")"
sleep 2.2; rm -f "$L"/slot-wait.*
out=$(GPU_SLOTS=1 IDLE_HOLD_S=3 acq 22)
check "an idle hold is taken once idle IDLE_HOLD_S on end, by an acquire that needs it" \
  '[[ "$out" == "ACQUIRED slot=1 "* ]] && grep -q "^22 $$ " "$L/resource.lease" && grep -qE "^RECLAIMED slot=1 from 21 to 22 \(no kit for [3-9] s\)$" "$ERR"' "$out / $(cat "$ERR")"

# On demand: only an acquire of another issue number that finds no slot it can use.
reset; hold 1 31 "$T1"; idle 1 700 30 31 "$T1"
out=$(GPU_SLOTS=1 acq 32)
check "an acquire by another issue with no free slot takes the idle hold, and the lease names it" \
  '[[ "$out" == "ACQUIRED slot=1 "* ]] && grep -q "^32 $$ " "$L/resource.lease" && grep -qE "^RECLAIMED slot=1 from 31 to 32 \(no kit for 70[0-9] s\)$" "$ERR" && [ ! -e "$L/idle-since.1" ]' "$out / $(cat "$ERR")"
check "the take is logged with the old and the new lease and the acquire that made it" \
  'grep -qE "Z RECLAIMED slot=1 from 31 to 32 \(no kit for [0-9]+ s\) \[31 $T1 .*\] -> \[32 $$ .*\] by acquire 32$" "$LOG"' "$(cat "$LOG" 2>/dev/null)"
reset; hold 1 33 "$T1"; idle 1 700 30 33 "$T1"
out=$(acq 34)
check "an acquire with a free slot takes the free one, not the idle one" \
  '[[ "$out" == "ACQUIRED slot=2 "* ]] && grep -q "^33 $T1 " "$L/resource.lease" && [ ! -s "$ERR" ]' "$out / $(cat "$ERR")"
reset; hold 1 35 "$T1"; idle 1 700 30 35 "$T1"; hold 2 36 "$N1"
out=$(acq 37)
check "with the kits at their limit, a free slot file is no slot to use: the idle hold is taken" \
  '[[ "$out" == "ACQUIRED slot=1 "* ]] && grep -q "^37 $$ " "$L/resource.lease" && [ ! -e "$L/resource.lease.3" ]' "$out / $(cat "$ERR")"
reset; hold 1 38 "$T1"; idle 1 700 30 38 "$T1"
out=$(GPU_SLOTS=1 acq 38)
check "an acquire of the hold's own issue never takes it" '[[ "$out" == BUSY* ]] && grep -q "^38 $T1 " "$L/resource.lease" && [ ! -s "$ERR" ]' "$out / $(cat "$ERR")"
reset; hold 1 39 "$T1"; idle 1 500 30 39 "$T1"
out=$(GPU_SLOTS=1 acq 40)
check "a hold idle less than IDLE_HOLD_S is not taken" '[[ "$out" == BUSY* ]] && grep -q "^39 $T1 " "$L/resource.lease"' "$out / $(cat "$ERR")"
reset; hold 1 41 "$T1"; idle 1 700 30 41 "$T1"
out=$(GPU_SLOTS=1 acq owner)
check "an acquirer that is no issue number never takes a hold, since iter.sh would not refuse the old line" \
  '[[ "$out" == BUSY* ]] && grep -q "^41 $T1 " "$L/resource.lease"' "$out / $(cat "$ERR")"
reset; hold 1 42 "$T1"; idle 1 700 30 42 "$T1"; echo "$(old 1300) $N2 $(old 10)" > "$L/slot-wait.5"
out=$(GPU_SLOTS=1 acq 43)
check "a take waits behind the single-slot issue first in line" '[[ "$out" == "BUSY queued behind single-slot issue 5"* ]] && grep -q "^42 $T1 " "$L/resource.lease"' "$out / $(cat "$ERR")"
reset; hold 1 44 "$T1"; idle 1 900 30 44 "$T1"
out=$(GPU_SLOTS=1 IDLE_HOLD_S=0 acq 45); out2=$(IDLE_HOLD_S=0 st)
check "IDLE_HOLD_S=0 turns the rule off" '[[ "$out" == BUSY* ]] && grep -q "^44 $T1 " "$L/resource.lease" && [[ "$out2" != *" idle "* ]]' "$out / $out2"

# Busy while the holder brings a kit up or runs one; a compile is idle.
for what in "iter.sh up|tools/dev/iter.sh up worv.env.manure" "iter.sh kit|tools/dev/iter.sh kit" \
            "arm_queue.sh|bin/arm_queue.sh --issue 51" "scenario_run.py|tools/eval/scenario_run.py run" \
            "a docker run naming the slot's container|docker run -d --name worv-iter --gpus all"; do
  tick TB "${what#*|}"; sleep 0.3
  reset; hold 1 51 "$TB"; idle 1 700 30 51 "$TB"
  out=$(GPU_SLOTS=1 acq 52)
  check "a holder running ${what%%|*} under it is busy: not taken, and its clock starts again" \
    '[[ "$out" == BUSY* ]] && grep -q "^51 $TB " "$L/resource.lease" && [ ! -e "$L/idle-since.1" ]' "$out / $(cat "$ERR")"
  kill "$TB" 2>/dev/null
done
for what in "iter.sh build|tools/dev/iter.sh build worv.env.manure" "a docker run naming another container|docker run -d --name worv-iter-ros2cli"; do
  tick TB "${what#*|}"; sleep 0.3
  reset; hold 1 53 "$TB"; idle 1 700 30 53 "$TB"
  out=$(GPU_SLOTS=1 acq 54)
  check "a holder whose only work is ${what%%|*} is idle and taken" '[[ "$out" == "ACQUIRED slot=1 "* ]] && grep -q "^54 $$ " "$L/resource.lease"' "$out / $(cat "$ERR")"
  kill "$TB" 2>/dev/null
done
( exec -a claude bash -p -c 'sleep 600; :' "the prompt names tools/dev/iter.sh up and bin/arm_queue.sh" ) >/dev/null 2>&1 &
PIDS+=("$!"); TP=$!; sleep 0.3
reset; hold 1 57 "$TP"; idle 1 700 30 57 "$TP"
out=$(GPU_SLOTS=1 acq 58)
check "the tick's own command line, its prompt, is not read for kit work" '[[ "$out" == "ACQUIRED slot=1 "* ]] && grep -q "^58 $$ " "$L/resource.lease"' "$out / $(cat "$ERR")"
reset; hold 1 55 "$T1"; idle 1 700 30 55 "$T1"; printf '#!/usr/bin/env bash\nexit 1\n' > "$H/fakebin/ps"; chmod +x "$H/fakebin/ps"
out=$(GPU_SLOTS=1 acq 56); rm -f "$H/fakebin/ps"
check "a process table that cannot be read counts as busy" '[[ "$out" == BUSY* ]] && grep -q "^55 $T1 " "$L/resource.lease"' "$out / $(cat "$ERR")"

# Holds the rule never takes.
reset; hold 1 61 "$N1"; idle 1 700 30 61 "$N1"; hold 2 owner "$N2"; hold 3 62 "$T2"; idle 3 700 30 62 "$T2"
out=$(acq 63); out2=$(st)
check "a named holder is never taken and keeps no clock" \
  '[[ "$out" == "ACQUIRED slot=3 "* ]] && grep -q "^61 $N1 " "$L/resource.lease" && grep -q "^owner $N2 " "$L/resource.lease.2" && [ ! -e "$L/idle-since.1" ] && [[ "$out2" != *" idle "* ]]' "$out / $out2"
reset; hold 1 64 "$T1"; idle 1 700 30 64 "$T1"; echo "9 64" > "$L/operator-priority"
out=$(GPU_SLOTS=1 acq 65)
check "a hold of an issue the operator named is never taken and keeps no clock" '[[ "$out" == BUSY* ]] && grep -q "^64 $T1 " "$L/resource.lease" && [ ! -e "$L/idle-since.1" ]' "$out / $(cat "$ERR")"
reset; echo "66 $T1 $(ago 30) domain=77 reserved-for-all" > "$L/resource.lease"; idle 1 700 30 66 "$T1"
out=$(GPU_SLOTS=1 acq 67); out2=$(st)
check "a whole-card reservation keeps no clock; it lapses on its own" '[[ "$out" == BUSY* ]] && grep -q "^66 $T1 " "$L/resource.lease" && [ ! -e "$L/idle-since.1" ]' "$out / $(cat "$ERR")"
reset; hold 1 68 "$T1"; idle 1 700 30 68 "$T1"; echo "69 $T2 $(ago 900) domain=0 device" > "$L/device.lease"
out=$(acq 70 --device); out2=$(st)
check "a device lease is never taken, and a device acquire takes no slot" \
  '[[ "$out" == BUSY*device* ]] && grep -q "^69 $T2 " "$L/device.lease" && grep -q "^68 $T1 " "$L/resource.lease" && has "$out2" "^device HELD 69 "' "$out / $out2"

# What starts a clock again.
reset; hold 1 71 "$T1"; idle 1 700 30 71 "$T1"; echo worv-iter > "$F/running.local"
out=$(st)
check "a container seen running clears its slot's clock" '[ ! -e "$L/idle-since.1" ] && [[ "$out" != *" idle "* ]]' "$out"
reset; hold 1 72 "$T2"; idle 1 700 30 73 "$T1"
out=$(st)
check "a slot that changed holder starts its clock again" \
  'read -r s l i p < "$L/idle-since.1" && [ "$s" = "$l" ] && [ "$i $p" = "72 $T2" ]' "$(cat "$L/idle-since.1")"
reset; hold 1 74 "$T1"; idle 1 700 400 74 "$T1"
out=$(st)
check "a gap of more than IDLE_SAMPLE_GAP_S between samples starts the clock again" 'read -r s l _ < "$L/idle-since.1" && [ "$s" = "$l" ]' "$(cat "$L/idle-since.1")"
reset; echo "75 $$ $(ago 900) domain=77" > "$L/resource.lease"; idle 1 700 30 75 $$
out=$(LEASE_HOLDER_PID=$$ "$LEASE" release 75)
check "a release drops the slot's clock" '[[ "$out" == "RELEASED slot=1" ]] && [ ! -e "$L/idle-since.1" ]' "$out"
reset; hold 1 76 "$T1"; idle 1 700 30 76 "$T1"; touch "$F/down.local"
out=$(GPU_SLOTS=1 acq 77)
check "a hold whose daemon does not answer is never taken, and its clock starts again" \
  '[[ "$out" == BUSY* ]] && grep -q "^76 $T1 " "$L/resource.lease" && [ ! -e "$L/idle-since.1" ]' "$out / $(cat "$ERR")"

# Whole-card acquires take the idle holds on the card they need, on demand.
reset; hold 1 81 "$T1"; idle 1 700 30 81 "$T1"; hold 2 82 "$T2"; idle 2 700 30 82 "$T2"
out=$(acq 83 --all --render)
check "a whole-card acquire takes the idle single holds on its card and gets the card" \
  '[[ "$out" == "ACQUIRED slot=1 "*"exclusive=all"* ]] && grep -q "^83 $$ .* exclusive$" "$L/resource.lease" && grep -q "^83 $$ .* held-for-all$" "$L/resource.lease.2" && grep -q "^RECLAIMED slot=1 from 81 to 83 " "$ERR" && grep -q "^RECLAIMED slot=2 from 82 to 83 " "$ERR"' "$out / $(cat "$ERR")"
reset; hold 1 84 "$T1"; idle 1 700 30 84 "$T1"; hold 2 85 "$N1"
out=$(acq 86 --all --render)
check "a whole-card acquire takes no idle hold while another hold keeps it from the card" \
  '[[ "$out" == RESERVING* ]] && grep -q "^84 $T1 " "$L/resource.lease" && [ ! -s "$ERR" ]' "$out / $(cat "$ERR")"
reset
echo "87 $T1 $(ago 900) domain=77 exclusive" > "$L/resource.lease"
echo "87 $T1 $(ago 900) domain=78 held-for-all" > "$L/resource.lease.2"
echo "87 $T1 $(ago 900) domain=79 held-for-all" > "$L/resource.lease.3"
for s in 1 2 3; do idle "$s" 700 30 87 "$T1"; done
echo worv-iter-3 > "$F/running.local"
out=$(acq 88 --all --render)
check "a whole-card hold with any of its slots busy is not taken" '[[ "$out" == RESERVING* ]] && grep -q "^87 $T1 " "$L/resource.lease" && [ ! -s "$ERR" ]' "$out / $(cat "$ERR")"
rm -f "$F/running.local"; idle 3 700 30 87 "$T1"
out=$(acq 89); out2=$(st)
check "a single-slot acquire never takes one slot of an idle whole-card hold" '[[ "$out" == BUSY* ]] && grep -q "^87 $T1 " "$L/resource.lease" && has "$out2" "^slot 2 HELD 87 .* idle 70[0-9] s \(alive\)$"' "$out / $out2"
out=$(acq 88 --all --render)
check "a whole-card acquire takes an idle whole-card hold" \
  '[[ "$out" == "ACQUIRED slot=1 "*"exclusive=all"* ]] && [ "$(grep -c "^RECLAIMED slot=[123] from 87 to 88 (no kit for 70[0-9] s)$" "$ERR")" = 3 ] && [ "$(grep -l "^88 $$ " "$L"/resource.lease* | wc -l)" = 3 ]' "$out / $(cat "$ERR")"

# Remote cards: read on their own host, taken on demand, free slots first.
reset; printf 'a@one\nb@two\n' > "$L/remote-card"
hold 4 91 "$T1" remote; hold 5 92 "$T2" "remote render"; hold 6 93 "$T1" remote
idle 4 700 30 91 "$T1"; idle 5 700 30 92 "$T2"; idle 6 700 30 93 "$T1"
printf 'worv-iter-5\nworv-iter-6\n' > "$F/running.a@one"
out=$(st)
check "a remote slot is read on its own host, once a sample, over ssh" \
  'has "$out" "^slot 4 HELD 91 .* idle 70[0-9] s \(alive\)$" && has "$out" "^slot 6 HELD 93 .* idle 70[0-9] s \(alive\)$" && has "$out" "^slot 5 HELD 92 .* render \(alive\)$" && [ "$(grep -c "^a@one " "$F/calls")" = 1 ] && [ "$(grep -c "^b@two " "$F/calls")" = 1 ]' "$out / $(cat "$F/calls")"
touch "$F/unreachable.b@two"; idle 6 700 30 93 "$T1"; out=$(st); rm -f "$F/unreachable.b@two"
check "a remote hold whose host does not answer keeps no clock" '[ ! -e "$L/idle-since.6" ] && has "$out" "^slot 6 HELD 93 .* remote \(alive\)$"' "$out"
full_local() { hold 1 94 "$N1"; hold 2 95 "$N2"; }
reset; printf 'a@one\nb@two\n' > "$L/remote-card"; full_local
hold 4 96 "$T1" remote; idle 4 700 30 96 "$T1"; hold 5 97 "$N1" remote; hold 6 97 "$N2" remote; hold 7 97 "$N2" remote
out=$(acq 98)
check "with no slot free on any card, a single-slot acquire takes a remote idle hold" \
  '[[ "$out" == *"ACQUIRED slot=4 "*"remote=a@one"* ]] && grep -q "^98 $$ .* remote$" "$L/resource.lease.4" && grep -q "^RECLAIMED slot=4 from 96 to 98 " "$ERR"' "$out / $(cat "$ERR")"
reset; printf 'a@one\nb@two\n' > "$L/remote-card"; full_local
hold 4 96 "$T1" remote; idle 4 700 30 96 "$T1"; hold 5 97 "$N1" remote
out=$(acq 99)
check "a free slot on a later remote card comes before a remote idle hold" \
  '[[ "$out" == *"ACQUIRED slot=6 "*"remote=b@two"* ]] && grep -q "^96 $T1 " "$L/resource.lease.4"' "$out / $(cat "$ERR")"
reset; printf 'a@one\nb@two\n' > "$L/remote-card"
hold 1 100 "$T1"; idle 1 700 30 100 "$T1"; hold 2 101 "$N1"
out=$(acq 102)
check "a free remote slot comes before a local idle hold" '[[ "$out" == *"ACQUIRED slot=4 "*"remote=a@one"* ]] && grep -q "^100 $T1 " "$L/resource.lease"' "$out / $(cat "$ERR")"
reset; printf 'a@one\nb@two\n' > "$L/remote-card"
echo "103 $T1 $(ago 900) domain=80 remote-all render" > "$L/resource.lease.4"
echo "103 $T1 $(ago 900) domain=81 remote-all render" > "$L/resource.lease.5"
idle 4 700 30 103 "$T1"; idle 5 700 30 103 "$T1"; hold 6 104 "$N1" remote
out=$(acq 105 --all-remote)
check "a whole remote card acquire takes an idle whole remote card" \
  '[[ "$out" == "ACQUIRED slot=4 "*"remote=a@one exclusive=all-remote"* ]] && grep -q "^RECLAIMED slot=4 from 103 to 105 " "$ERR" && grep -q "^RECLAIMED slot=5 from 103 to 105 " "$ERR" && grep -q "^105 $$ .* remote-all render$" "$L/resource.lease.5"' "$out / $(cat "$ERR")"
reset; printf 'a@one\nb@two\n' > "$L/remote-card"
echo "106 $T1 $(ago 900) domain=80 remote-all render" > "$L/resource.lease.4"
echo "106 $T1 $(ago 900) domain=81 remote-all render" > "$L/resource.lease.5"
idle 4 700 30 106 "$T1"; idle 5 700 30 106 "$T1"
out=$(acq 107 --all-remote)
check "a whole remote card acquire takes a free card before an idle one" \
  '[[ "$out" == "ACQUIRED slot=6 "*"remote=b@two"* ]] && grep -q "^106 $T1 " "$L/resource.lease.4" && [ ! -s "$ERR" ]' "$out / $(cat "$ERR")"
reset

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
