#!/usr/bin/env bash
# Exercises libexec/tick-pid.sh: the tick is the nearest ancestor that carries HARNESS_TICK for this
# state directory, or FLEET_SESSION, while its parent does not. The suite may itself run under a
# tick, whose processes sit further up, so a case where none of its own processes is the tick only
# checks that none of them was chosen.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
export H TP="$here/libexec/tick-pid.sh" OUT="$H/out" HARNESS_STATE="$H/state"
unset HARNESS_TICK FLEET_SESSION
MARK="state=$H/state slot=1 agent=codex model=m kind=work"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
# The outer shell writes its pid to $H/pid and runs $1 under a tool shell of its own, which runs
# tick-pid.sh; every shell runs a second command, so none execs into the next.
under() {  # $1 = what the tool shell runs; the outer shell's environment is the caller's
    bash -c 'echo $$ > "$H/pid"; bash -c "$0; true"; true' "$1"
}
tp='"$TP" > "$OUT" 2>&1'

HARNESS_TICK="$MARK" under "$tp"
check "the topmost process carrying the marker is the tick, not the tool shell under it" \
    '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

HARNESS_TICK="state=$H/other slot=1 agent=codex model=m kind=work" under "$tp"
check "a marker of another state directory is not this harness's tick" \
    '[ "$(cat "$OUT")" != "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

HARNESS_TICK="state=$H/state-2 slot=1 agent=codex model=m kind=work" under "$tp"
check "a state directory that only starts with this one is another" \
    '[ "$(cat "$OUT")" != "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

FLEET_SESSION=d-1-abc under "$tp"
check "the topmost process carrying FLEET_SESSION is a dispatched tick" \
    '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

under "$tp"
check "without a marker there is no tick among the shells" \
    '[ "$(cat "$OUT")" != "$(cat "$H/pid")" ] && ! grep -qx "$(cat "$H/pid")" "$OUT"' "out=$(cat "$OUT")"

# A tool runner that clears its environment leaves an unmarked gap; the agent above it is the tick.
HARNESS_TICK="$MARK" under 'env -i H="$H" TP="$TP" OUT="$OUT" HARNESS_STATE="$HARNESS_STATE" bash -c "\"\$TP\" > \"\$OUT\" 2>&1; true"'
check "a tool shell with a cleared environment still finds the tick above it" \
    '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

# An npm-installed agent CLI runs as node with its script as argv[1], and starts its native agent.
printf '%s\n' 'echo $$ > "$H/pid"; bash -c "\"\$TP\" > \"\$OUT\" 2>&1; true"; true' > "$H/agent.js"
(export HARNESS_TICK="$MARK"; exec -a node bash "$H/agent.js")
check "a node shim, not the agent under it, is the tick" \
    '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

# The tick of another installation runs this one's: only the inner marker names this state.
HARNESS_TICK="state=$H/other slot=1 agent=a model=m kind=work" \
    bash -c 'HARNESS_TICK="$0" bash -c "echo \$\$ > \"\$H/inner\"; \"\$TP\" > \"\$OUT\" 2>&1; true"; true' "$MARK"
check "a tick under another installation's tick is its own" \
    '[ "$(cat "$OUT")" = "$(cat "$H/inner")" ]' "out=$(cat "$OUT") inner=$(cat "$H/inner")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
