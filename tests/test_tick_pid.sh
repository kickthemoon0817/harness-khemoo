#!/usr/bin/env bash
# Exercises libexec/tick-pid.sh: the nearest ancestor matching an `exe` or `exe:arg` entry of
# TICK_AGENT_EXES is the tick. Agent names here are made up where they can be, so an agent that runs
# the suite is never mistaken for one; the default's cases use claude and only check that the fake
# process was or was not chosen, since a real `claude -p` may sit further up.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
export H TP="$here/libexec/tick-pid.sh" OUT="$H/out"
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

TICK_AGENT_EXES=agent-a:-p run_as agent-a '"$TP" > "$OUT" 2>&1'
check "an entry with an argument refuses another first argument" '[ "$(cat "$OUT")" != "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

# bash's argv[1] here is -c, as an interactive `claude` has no -p.
(unset TICK_AGENT_EXES; run_as claude '"$TP" > "$OUT" 2>&1')
check "by default an interactive claude is not the tick" '[ "$(cat "$OUT")" != "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

# bash -p only skips the startup files here; it makes argv[1] -p.
(unset TICK_AGENT_EXES; exec -a claude bash -p -c 'echo $$ > "$0"; "$TP" > "$OUT"; true' "$H/pid")
check "by default claude -p is the tick" '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

# bash runs the script file named exec, so the process reads as `codex exec`.
printf '%s\n' 'echo $$ > "$H/pid"; "$TP" > "$OUT" 2>&1; true' > "$H/exec"
(unset TICK_AGENT_EXES; cd "$H" && exec -a codex bash exec)
check "by default codex exec is not the tick" '[ "$(cat "$OUT")" != "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"
(export TICK_AGENT_EXES="claude:-p codex:exec"; cd "$H" && exec -a codex bash exec)
check "codex exec is the tick once codex:exec is listed" '[ "$(cat "$OUT")" = "$(cat "$H/pid")" ]' "out=$(cat "$OUT") pid=$(cat "$H/pid")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
