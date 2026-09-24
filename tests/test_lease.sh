#!/usr/bin/env bash
# Exercises bin/lease.sh in a scratch harness home, with sleeping processes as live holders.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
sleep 600 & A=$!
sleep 600 & B=$!
trap 'kill $A $B 2>/dev/null; rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/state/locks" "$H/cache/cache-slot2" "$H/cache/cache-slot3"
cp "$here/bin/lease.sh" "$H/bin/lease.sh"
export HARNESS_HOME="$H" GPU_SLOTS=3 LEASE_CACHE_BASE="$H/cache"
L="$H/state/locks"; LEASE="$H/bin/lease.sh"
ago() { date -u -d "-$1 sec" +%FT%TZ; }
reset() { rm -f "$L"/resource.lease*; }
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }

reset; echo "1 $A $(ago 30) domain=77" > "$L/resource.lease"
echo "2 $B $(ago 30) domain=78 reserved-for-all" > "$L/resource.lease.2"
echo "2 $B $(ago 30) domain=79 reserved-for-all" > "$L/resource.lease.3"
out=$("$LEASE" acquire 3); check "fresh reservations block a single-slot acquire" '[[ "$out" == BUSY* ]]' "$out"

reset; echo "1 $A $(ago 30) domain=77" > "$L/resource.lease"
echo "2 $B $(ago 700) domain=78 reserved-for-all" > "$L/resource.lease.2"
echo "2 $B $(ago 700) domain=79 reserved-for-all" > "$L/resource.lease.3"
out=$("$LEASE" acquire 3); check "a lapsed reservation is taken, lowest slot first" '[[ "$out" == "ACQUIRED slot=2 "* ]] && grep -q "^3 $$ " "$L/resource.lease.2"' "$out"

reset; echo "2 $B $(ago 700) domain=77 reserved-for-all" > "$L/resource.lease"
echo "1 $A $(ago 30) domain=78" > "$L/resource.lease.2"
out=$("$LEASE" acquire 3); check "a free slot is preferred over a lapsed reservation" '[[ "$out" == "ACQUIRED slot=3 "* ]]' "$out"

reset; echo "2 $B $(ago 700) domain=77 exclusive" > "$L/resource.lease"
echo "2 $B $(ago 700) domain=78 held-for-all" > "$L/resource.lease.2"
echo "2 $B $(ago 700) domain=79 held-for-all" > "$L/resource.lease.3"
out=$("$LEASE" acquire 3); check "a whole-card hold never lapses" '[[ "$out" == BUSY* ]]' "$out"

reset; echo "1 $A $(ago 30) domain=77" > "$L/resource.lease"
out=$("$LEASE" acquire 2 --all); check "--all reserves the free slots while a sibling runs" '[[ "$out" == "RESERVING held=2/3"* ]] && grep -q " reserved-for-all$" "$L/resource.lease.2"' "$out"
kill $A; wait $A 2>/dev/null
out=$("$LEASE" acquire 2 --all); check "--all acquires in the slot it already held once the sibling is gone" '[[ "$out" == "ACQUIRED slot=2 "*"exclusive=all"* ]] && grep -q " exclusive$" "$L/resource.lease.2"' "$out"
check "--all turns its other reservations into holds" 'grep -q " held-for-all$" "$L/resource.lease" && grep -q " held-for-all$" "$L/resource.lease.3"' "$(cat "$L"/resource.lease*)"
out=$("$LEASE" release 2); check "release drops every slot" '[ ! -e "$L/resource.lease" ] && [ ! -e "$L/resource.lease.2" ] && [ ! -e "$L/resource.lease.3" ]' "$out"

reset; echo "2 $$ $(ago 700) domain=77 reserved-for-all" > "$L/resource.lease"
out=$("$LEASE" acquire 2); check "a tick re-acquiring gets its own slot back" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
