#!/usr/bin/env bash
# Exercises the usage ceiling (lib/usage.sh, libexec/usage-ok.sh) against a scratch cache, with
# no fetch script, so no API call is made.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
# A scratch harness home with no config, so the real cache and fetch script are never read.
mkdir -p "$H/lib" "$H/libexec"
cp "$here/lib/usage.sh" "$here/lib/instance.sh" "$H/lib/"; cp "$here/libexec/usage-ok.sh" "$H/libexec/"
export USAGE_CACHE="$H/usage-cache.json" USAGE_FETCH="$H/no-fetch" HARNESS_HOME="$H"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
cache() {  # $1 five, $2 week, $3 age in seconds (default 0)
    printf '{"timestamp": %s000, "data": {"fiveHourPercent": %s, "weeklyPercent": %s}}\n' \
        "$(( $(date +%s) - ${3:-0} ))" "$1" "$2" > "$USAGE_CACHE"
}
# shellcheck source=/dev/null
. "$H/lib/usage.sh"
MAX_SLOTS=8
slots() { cache "$1" "$2" "${3:-0}"; compute_allowed; echo "$allowed"; }

check "both windows low: full width" '[ "$(slots 10 20)" -eq 8 ]' "$(slots 10 20)"
check "5-hour at its half step" '[ "$(slots 60 20)" -eq 4 ]' "$(slots 60 20)"
check "5-hour at its one step" '[ "$(slots 70 20)" -eq 1 ]' "$(slots 70 20)"
check "5-hour at its stop: nothing starts" '[ "$(slots 80 20)" -eq 0 ]' "$(slots 80 20)"
check "weekly at its half step" '[ "$(slots 10 80)" -eq 4 ]' "$(slots 10 80)"
check "weekly at its one step" '[ "$(slots 10 85)" -eq 1 ]' "$(slots 10 85)"
check "weekly at its stop: nothing starts" '[ "$(slots 10 88)" -eq 0 ]' "$(slots 10 88)"
check "the tighter window decides" '[ "$(slots 65 86)" -eq 1 ]' "$(slots 65 86)"
check "a stale reading starts nothing" '[ "$(slots 10 20 4000)" -eq 0 ]' "$(slots 10 20 4000)"
rm -f "$USAGE_CACHE"
compute_allowed
check "no reading at all starts nothing" '[ "$allowed" -eq 0 ] && [ "$pct" = "?" ]' "allowed=$allowed pct=$pct"
# Steps set at or past the ceiling still stop at it.
cache 92 10; FIVE_NONE_AT=95 compute_allowed
check "the ceiling holds whatever the steps say" '[ "$allowed" -eq 0 ]' "allowed=$allowed"

cache 87 87; out=$("$H/libexec/usage-ok.sh"); rc=$?
check "usage-ok.sh under the stand-down point exits 0" '[ "$rc" -eq 0 ] && [[ "$out" == *": go on" ]]' "rc=$rc $out"
cache 88 10; out=$("$H/libexec/usage-ok.sh"); rc=$?
check "usage-ok.sh two under the 5-hour ceiling exits 1" '[ "$rc" -eq 1 ] && [[ "$out" == *"start nothing"* ]]' "rc=$rc $out"
cache 10 88; out=$("$H/libexec/usage-ok.sh"); rc=$?
check "usage-ok.sh two under the weekly ceiling exits 1" '[ "$rc" -eq 1 ]' "rc=$rc $out"
cache 10 95; out=$("$H/libexec/usage-ok.sh"); rc=$?
check "usage-ok.sh past the ceiling exits 1" '[ "$rc" -eq 1 ]' "rc=$rc $out"
# A weekly stand-down point of its own, above a 5-hour point set well under the ceiling.
own() { USAGE_CEILING_PCT=95 USAGE_TICK_STOP_PCT=90 USAGE_TICK_STOP_WEEK_PCT=93 "$H/libexec/usage-ok.sh"; }
cache 89 92; out=$(own); rc=$?
check "usage-ok.sh lets the weekly window pass the 5-hour point up to its own" '[ "$rc" -eq 0 ]' "rc=$rc $out"
cache 90 10; out=$(own); rc=$?
check "usage-ok.sh stops at the 5-hour point when the weekly one is higher" '[ "$rc" -eq 1 ]' "rc=$rc $out"
cache 10 93; out=$(own); rc=$?
check "usage-ok.sh stops at the weekly point" '[ "$rc" -eq 1 ] && [[ "$out" == *"week_stop=93%"* ]]' "rc=$rc $out"
cache 10 10 4000; out=$("$H/libexec/usage-ok.sh"); rc=$?
check "usage-ok.sh with a stale reading exits 2" '[ "$rc" -eq 2 ] && [[ "$out" == "usage unknown"* ]]' "rc=$rc $out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
