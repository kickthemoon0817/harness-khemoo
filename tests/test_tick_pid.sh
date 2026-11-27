#!/usr/bin/env bash
# Exercises bin/tick-pid.sh: the nearest ancestor whose argv[0] or executable base name is in
# TICK_AGENT_EXES is the tick. Agent names here are made up, so an agent that runs the suite is
# never mistaken for one.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
export H TP="$here/bin/tick-pid.sh" OUT="$H/out"
cp "$(command -v bash)" "$H/agent-b"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
# Each shell runs a second command so it forks tick-pid.sh instead of exec'ing into it.
run_as() {  # $1 = argv[0] for the shell, $2 = its script; the shell's pid goes to $H/pid
    (exec -a "$1" bash -c 'echo $$ > "$0"; '"$2"'; true' "$H/pid")
}

TICK_AGENT_EXES=agent-a run_as /opt/bin/agent-a '"$TP" > "$OUT"'
check "an ancestor whose argv[0] is a listed agent is the tick" '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

TICK_AGENT_EXES=agent-a run_as bash 'bash -c "echo agent-a -p; \"\$TP\" > \"\$OUT\" 2>&1; echo rc=\$? >> \"\$OUT\""' >/dev/null
check "a shell whose command text names the agent is not the tick" 'grep -q "^ERROR: no ancestor in TICK_AGENT_EXES" "$OUT" && grep -qx "rc=1" "$OUT"' "$(cat "$OUT")"

TICK_AGENT_EXES="codex agent-a" run_as agent-a '(exec -a codex bash -c "echo \$\$ > \"$H/inner\"; \"\$TP\" > \"\$OUT\"; true")'
check "the nearest listed ancestor wins" '[ "$(cat "$OUT")" = "$(cat "$H/inner")" ]' "out=$(cat "$OUT") inner=$(cat "$H/inner")"

(exec -a renamed "$H/agent-b" -c 'echo $$ > "$0"; TICK_AGENT_EXES=agent-b "$TP" > "$OUT"; true' "$H/pid")
check "an ancestor whose executable is a listed agent is the tick" '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
