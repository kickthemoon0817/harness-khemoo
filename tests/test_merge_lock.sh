#!/usr/bin/env bash
# Exercises bin/merge-lock.sh in a scratch harness home, with a sleeping process as a live holder.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
sleep 600 & A=$!
trap 'kill $A 2>/dev/null; rm -rf "$H"' EXIT
mkdir -p "$H/bin"
cp "$here/bin/merge-lock.sh" "$H/bin/merge-lock.sh"
export HARNESS_HOME="$H"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
ML="$H/bin/merge-lock.sh"; F="$H/state/locks/merge.lock"

out=$("$ML" acquire 7); check "a free lock is taken, its directory made" '[[ "$out" == ACQUIRED ]] && grep -q "^7 $$ " "$F"' "$out"
out=$("$ML" release 7); check "the holder releases it" '[[ "$out" == RELEASED ]] && [ ! -e "$F" ]' "$out"
echo "8 $A 2026-09-28T00:00:00Z" > "$F"
out=$("$ML" acquire 9); rc=$?
check "a live other holder keeps it" '[ "$rc" -eq 1 ] && [[ "$out" == "BUSY 8 $A "* ]]' "rc=$rc $out"
rm -f "$F"
for bad in "7 1" $'7\n1 1' "" "-1" "7x"; do
    out=$("$ML" acquire "$bad" 2>&1); rc=$?
    check "an issue of '$(printf %q "$bad")' is refused and writes nothing" '[ "$rc" -eq 2 ] && [ ! -e "$F" ]' "rc=$rc $out"
done
out=$("$ML" status); check "status of a free lock" '[[ "$out" == free ]]' "$out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
