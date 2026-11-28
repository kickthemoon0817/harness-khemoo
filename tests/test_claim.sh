#!/usr/bin/env bash
# Exercises libexec/claim.sh in a scratch harness home, with a sleeping process as a live holder.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
sleep 600 & A=$!
trap 'kill $A 2>/dev/null; rm -rf "$H"' EXIT
mkdir -p "$H/libexec" "$H/state/locks"
cp "$here/libexec/claim.sh" "$H/libexec/claim.sh"
export HARNESS_HOME="$H"
L="$H/state/locks"; CLAIM="$H/libexec/claim.sh"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }

out=$("$CLAIM" acquire 7); check "a free issue is claimed by this tick" '[[ "$out" == ACQUIRED ]] && grep -q "^7 $$ " "$L/claim.7"' "$out"
out=$("$CLAIM" acquire 7); check "the holder may claim again" '[[ "$out" == ACQUIRED ]]' "$out"
echo "8 $A 2026-09-28T00:00:00Z" > "$L/claim.8"
out=$("$CLAIM" acquire 8); rc=$?
check "a live other holder keeps the issue" '[ "$rc" -eq 1 ] && [[ "$out" == "BUSY 8 $A "* ]] && grep -q "^8 $A " "$L/claim.8"' "rc=$rc $out"
sleep 0 & D=$!; wait $D
echo "9 $D 2026-09-28T00:00:00Z" > "$L/claim.9"
out=$("$CLAIM" acquire 9); check "a dead holder's issue is taken over" '[[ "$out" == ACQUIRED ]] && grep -q "^9 $$ " "$L/claim.9"' "$out"
out=$("$CLAIM" status 8); check "status names a live holder" '[[ "$out" == "HELD 8 $A "* ]]' "$out"
echo "10 $D 2026-09-28T00:00:00Z" > "$L/claim.10"
out=$("$CLAIM" status 10); check "status names a dead holder" '[[ "$out" == "ORPHAN 10 $D "* ]]' "$out"
out=$("$CLAIM" status 11); check "status of an unclaimed issue is free" '[[ "$out" == free ]]' "$out"
mkdir -p "$H/st"
out=$(HARNESS_STATE="$H/st" "$CLAIM" acquire 12); check "the claim lives under HARNESS_STATE when set" '[[ "$out" == ACQUIRED ]] && [ -s "$H/st/locks/claim.12" ] && [ ! -e "$L/claim.12" ]' "$out"
"$CLAIM" acquire abc >/dev/null 2>&1; rc=$?; check "a non-numeric issue is refused" '[ "$rc" -eq 2 ]' "rc=$rc"
"$CLAIM" take 7 >/dev/null 2>&1; rc=$?; check "an unknown command is refused" '[ "$rc" -eq 2 ]' "rc=$rc"

# With libexec/tick-pid.sh beside it, the claim belongs to the tick that the runner marked, whatever
# agent runs it, and a shell outside any tick of this harness takes none.
cp "$here/libexec/tick-pid.sh" "$H/libexec/"
export HARNESS_STATE="$H/state" CLAIM H
unset HARNESS_TICK FLEET_SESSION
HARNESS_TICK="state=$H/state slot=1 agent=gemini model=m kind=work" \
    bash -c 'echo $$ > "$H/tick"; bash -c "\"\$CLAIM\" acquire 13 > \"\$H/out\""; true'
check "a tool shell's claim belongs to its marked tick" \
    'grep -qx ACQUIRED "$H/out" && grep -q "^13 $(cat "$H/tick") " "$L/claim.13"' "$(cat "$H/out") $(cat "$L/claim.13" 2>/dev/null)"
HARNESS_TICK="state=$H/elsewhere slot=1 agent=gemini model=m kind=work" \
    bash -c 'echo $$ > "$H/tick"; bash -c "\"\$CLAIM\" acquire 14 > \"\$H/out\" 2>&1; echo rc=\$? >> \"\$H/out\""; true'
check "a tick of another state directory takes no claim here" \
    '! grep -q "^14 $(cat "$H/tick") " "$L/claim.14" 2>/dev/null' "$(cat "$H/out")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
