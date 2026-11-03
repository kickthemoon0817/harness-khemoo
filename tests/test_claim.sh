#!/usr/bin/env bash
# Exercises bin/claim.sh in a scratch harness home, with a sleeping process as a live holder.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
sleep 600 & A=$!
trap 'kill $A 2>/dev/null; rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/state/locks"
cp "$here/bin/claim.sh" "$H/bin/claim.sh"
export HARNESS_HOME="$H"
L="$H/state/locks"; CLAIM="$H/bin/claim.sh"
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
"$CLAIM" acquire abc >/dev/null 2>&1; rc=$?; check "a non-numeric issue is refused" '[ "$rc" -eq 2 ]' "rc=$rc"
"$CLAIM" take 7 >/dev/null 2>&1; rc=$?; check "an unknown command is refused" '[ "$rc" -eq 2 ]' "rc=$rc"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
