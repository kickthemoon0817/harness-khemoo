#!/usr/bin/env bash
# Runs two copies of the landing suite at once, each in its own scratch harness home on this host.
# Each counts live ticks, landing ticks among them, so either would fail if it counted the other's.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
bash "$here/test_tick_landing.sh" >"$out/a" 2>&1 & a=$!
bash "$here/test_tick_landing.sh" >"$out/b" 2>&1 & b=$!
wait "$a"; ra=$?
wait "$b"; rb=$?
pass=0; fail=0
for run in a b; do
    rc=$ra; [ "$run" = b ] && rc=$rb
    if [ "$rc" -eq 0 ]; then echo "PASS run $run of the landing suite, alongside the other"; pass=$((pass + 1))
    else echo "FAIL run $run of the landing suite, alongside the other :: $(grep '^FAIL' "$out/$run" | head -3)"; fail=$((fail + 1)); fi
done
echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
