#!/usr/bin/env bash
# Exercises libexec/tick-pid.sh: the tick is the first ancestor whose pid and start time a slot file
# of this state directory records, or, under FLEET_SESSION, the ancestor fleetd started. The suite
# may itself run under a tick, so a case where none of its own processes is the tick only checks
# that none of them was chosen.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
mkdir -p "$H/state/locks" "$H/other/locks"
export H TP="$here/libexec/tick-pid.sh" OUT="$H/out" HARNESS_STATE="$H/state"
unset HARNESS_TICK FLEET_SESSION
cc -o "$H/nodump" "$here/tests/fixtures/nodump.c" || { echo "FAIL cc cannot build the non-dumpable stand-in"; exit 1; }
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
# What a shell runs to record itself as slot 1's tick in state directory $RECORD_IN, as the runner
# records the agent it starts; RECORD_SKEW shifts the start time, as a reused pid's would differ.
export RECORD='stat=$(cat /proc/$$/stat); read -ra f <<<"${stat##*) }"; printf "%s %s codex m work\n" $$ $(( ${f[19]} + ${RECORD_SKEW:-0} )) > "$RECORD_IN/locks/slot1.tick"'
# The agent shell records itself, writes its pid to $H/pid and runs tick-pid.sh under a tool shell
# of its own; every shell runs a second command, so none execs into the next.
agent() {  # $1 = the state directory the agent is recorded in
    rm -f "$H"/state/locks/slot*.tick "$H"/other/locks/slot*.tick
    RECORD_IN=$1 bash -c 'eval "$RECORD"; echo $$ > "$H/pid"; bash -c "\"\$TP\" > \"\$OUT\" 2>&1; true"; true'
}

agent "$H/state"
check "the recorded ancestor is the tick, not the tool shell under it" \
    '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

agent "$H/other"
check "a slot file of another state directory records no tick here" \
    '[ "$(cat "$OUT")" != "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

RECORD_SKEW=-1 agent "$H/state"
check "a slot file whose start time differs, as a reused pid's does, records no tick" \
    '[ "$(cat "$OUT")" != "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

rm -f "$H"/state/locks/slot*.tick
bash -c 'echo $$ > "$H/pid"; bash -c "\"\$TP\" > \"\$OUT\" 2>&1; true"; true'
check "without a slot file no shell here is the tick" \
    '[ "$(cat "$OUT")" != "$(cat "$H/pid")" ] && ! grep -qx "$(cat "$H/pid")" "$OUT"' "out=$(cat "$OUT")"

# A tool runner that clears its environment changes nothing: no environment is read.
RECORD_IN=$H/state bash -c 'eval "$RECORD"; echo $$ > "$H/pid"; env -i TP="$TP" OUT="$OUT" HARNESS_STATE="$HARNESS_STATE" bash -c "\"\$TP\" > \"\$OUT\" 2>&1; true"; true'
check "a tool shell with a cleared environment still finds its tick" \
    '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

# A non-dumpable agent: its environ and fd cannot be read, its stat can. Its tool shell waits for the
# runner to record it, as a tool call comes after the agent started.
rm -f "$H"/state/locks/slot*.tick
"$H/nodump" 'until [ -s "$H/state/locks/slot1.tick" ]; do sleep 0.05; done; cat /proc/$PPID/environ >/dev/null 2>&1 && echo readable > "$H/environ"; "$TP" > "$OUT" 2>&1' 1 &
nd=$!
sleep 0.2
RECORD_IN=$H/state bash -c 'stat=$(cat /proc/'"$nd"'/stat); read -ra f <<<"${stat##*) }"; printf "%s %s codex m work\n" '"$nd"' ${f[19]} > "$RECORD_IN/locks/slot1.tick"'
wait "$nd"
check "a non-dumpable agent, whose environment no one may read, is the tick" \
    '[ "$(cat "$OUT")" = "$nd" ] && [ ! -e "$H/environ" ]' "out=$(cat "$OUT") pid=$nd environ=$(cat "$H/environ" 2>/dev/null)"

# A tool child that outlives the tick is reparented away from it: it is no part of any tick.
rm -f "$OUT"
RECORD_IN=$H/state bash -c 'eval "$RECORD"; echo $$ > "$H/pid"; nohup bash -c "sleep 0.5; \"\$TP\" > \"\$OUT\" 2>&1; echo rc=\$? >> \"\$OUT\"" >/dev/null 2>&1 & true'
for _ in $(seq 100); do grep -q "^rc=" "$OUT" 2>/dev/null && break; sleep 0.05; done
check "a nohup'd tool child that outlives its tick finds no tick" \
    '! grep -qx "$(cat "$H/pid")" "$OUT" && grep -q "^rc=" "$OUT"' "out=$(cat "$OUT" 2>/dev/null) pid=$(cat "$H/pid")"

# On a fleet device the tick is the process fleetd started; the name is the executable's.
cp "$(command -v bash)" "$H/fleetd"
"$H/fleetd" -c 'bash -c "echo \$\$ > \"\$H/pid\"; FLEET_SESSION=d-1-abc bash -c \"\\\"\\\$TP\\\" > \\\"\\\$OUT\\\" 2>&1; true\"; true"; true'
check "under FLEET_SESSION, the process fleetd started is the tick" \
    '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"
FLEET_SESSION=d-1-abc bash -c 'echo $$ > "$H/pid"; bash -c "\"\$TP\" > \"\$OUT\" 2>&1; echo rc=\$? >> \"\$OUT\""; true'
check "FLEET_SESSION without fleetd above finds no tick" \
    'grep -qx "rc=1" "$OUT" && grep -q "no ancestor was started by fleetd" "$OUT"' "$(cat "$OUT")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
