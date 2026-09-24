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

old() { echo "$(( $(date -u +%s) - $1 ))"; }

reset; rm -f "$L"/all-wait.*
echo "$(old 2000) $B" > "$L/all-wait.2"
out=$("$LEASE" acquire 3); check "an issue that has waited 30 min for the card blocks a single-slot acquire" '[[ "$out" == "BUSY priority to whole-card issue 2"* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 60) $B" > "$L/all-wait.2"
out=$("$LEASE" acquire 3); check "a younger whole-card wait does not block" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 2000) 999999" > "$L/all-wait.2"
out=$("$LEASE" acquire 3); check "a wait whose tick is dead gives no priority" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 2000) $B" > "$L/all-wait.2"
echo "2 $B $(ago 700) domain=78 reserved-for-all" > "$L/resource.lease.2"
echo "4 $$ $(ago 30) domain=79 reserved-for-all" > "$L/resource.lease.3"
out=$("$LEASE" acquire 4 --all); check "another whole-card waiter yields to the priority issue and drops its reservations" '[[ "$out" == "YIELDING to whole-card issue 2"* ]] && [ ! -e "$L/resource.lease.3" ] && [ -s "$L/all-wait.4" ] && grep -q " reserved-for-all$" "$L/resource.lease.2"' "$out"
out=$("$LEASE" acquire 3); check "the priority issue's reservation never lapses" '[[ "$out" == "BUSY priority to whole-card issue 2"* ]]' "$out"

reset; rm -f "$L"/all-wait.*
sleep 600 & C=$!
echo "1 $C $(ago 30) domain=77" > "$L/resource.lease"
echo "$(old 2000) 1" > "$L/all-wait.5"
out=$("$LEASE" acquire 5 --all); check "the priority issue reserves and keeps its first wait time" '[[ "$out" == "RESERVING held=2/3"* ]] && [ "$(awk "{print \$1}" "$L/all-wait.5")" -le "$(old 1999)" ] && [ "$(awk "{print \$2}" "$L/all-wait.5")" = "$$" ]' "$out"
kill $C; wait $C 2>/dev/null
out=$("$LEASE" acquire 5 --all); check "the priority issue acquires once the card drains, and its wait record is cleared" '[[ "$out" == *"exclusive=all"* ]] && [ ! -e "$L/all-wait.5" ]' "$out"
"$LEASE" release 5 >/dev/null

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
