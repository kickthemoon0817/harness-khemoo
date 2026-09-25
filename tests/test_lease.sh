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
reset() { rm -f "$L"/resource.lease* "$L"/slot-wait.*; }
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
out=$("$LEASE" acquire 2 --all --render); check "--all reserves the free slots while a sibling runs" '[[ "$out" == "RESERVING held=2/3"* ]] && grep -q " reserved-for-all$" "$L/resource.lease.2"' "$out"
kill $A; wait $A 2>/dev/null
out=$("$LEASE" acquire 2 --all --render); check "--all acquires in the slot it already held once the sibling is gone" '[[ "$out" == "ACQUIRED slot=2 "*"exclusive=all"* ]] && grep -q " exclusive$" "$L/resource.lease.2"' "$out"
check "--all turns its other reservations into holds" 'grep -q " held-for-all$" "$L/resource.lease" && grep -q " held-for-all$" "$L/resource.lease.3"' "$(cat "$L"/resource.lease*)"
out=$("$LEASE" release 2); check "release drops every slot" '[ ! -e "$L/resource.lease" ] && [ ! -e "$L/resource.lease.2" ] && [ ! -e "$L/resource.lease.3" ]' "$out"

reset; echo "2 $$ $(ago 700) domain=77 reserved-for-all" > "$L/resource.lease"
out=$("$LEASE" acquire 2); check "a tick re-acquiring gets its own slot back" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

old() { echo "$(( $(date -u +%s) - $1 ))"; }

reset; rm -f "$L"/all-wait.*
echo "$(old 2000) $B $(old 10)" > "$L/all-wait.2"
out=$("$LEASE" acquire 3); check "an issue that has waited 30 min for the card blocks a single-slot acquire" '[[ "$out" == "BUSY priority to whole-card issue 2"* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 60) $B $(old 10)" > "$L/all-wait.2"
out=$("$LEASE" acquire 3); check "a younger whole-card wait does not block" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 2000) 999999 $(old 10)" > "$L/all-wait.2"
out=$("$LEASE" acquire 3); check "a wait whose tick is dead gives no priority" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 2000) $B $(old 10)" > "$L/all-wait.2"
echo "2 $B $(ago 700) domain=78 reserved-for-all" > "$L/resource.lease.2"
echo "4 $$ $(ago 30) domain=79 reserved-for-all" > "$L/resource.lease.3"
out=$("$LEASE" acquire 4 --all --render); check "another whole-card waiter yields to the priority issue and drops its reservations" '[[ "$out" == "YIELDING to whole-card issue 2"* ]] && [ ! -e "$L/resource.lease.3" ] && [ -s "$L/all-wait.4" ] && grep -q " reserved-for-all$" "$L/resource.lease.2"' "$out"
out=$("$LEASE" acquire 3); check "the priority issue's reservation never lapses" '[[ "$out" == "BUSY priority to whole-card issue 2"* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 2000) $B $(old 400)" > "$L/all-wait.2"
out=$("$LEASE" acquire 3); check "a wait whose tick stopped asking gives no priority" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
sleep 600 & C=$!
echo "1 $C $(ago 30) domain=77" > "$L/resource.lease"
echo "$(old 2000) 1" > "$L/all-wait.5"
out=$("$LEASE" acquire 5 --all --render); check "the priority issue reserves and keeps its first wait time" '[[ "$out" == "RESERVING held=2/3"* ]] && [ "$(awk "{print \$1}" "$L/all-wait.5")" -le "$(old 1999)" ] && [ "$(awk "{print \$2}" "$L/all-wait.5")" = "$$" ]' "$out"
kill $C; wait $C 2>/dev/null
out=$("$LEASE" acquire 5 --all --render); check "the priority issue acquires once the card drains, and its wait record is cleared" '[[ "$out" == *"exclusive=all"* ]] && [ ! -e "$L/all-wait.5" ]' "$out"
"$LEASE" release 5 >/dev/null

reset; rm -f "$L"/all-wait.*
printf '#!/usr/bin/env bash\necho "ERROR: no claude -p ancestor" >&2; exit 1\n' > "$H/bin/tick-pid.sh"; chmod +x "$H/bin/tick-pid.sh"
out=$("$LEASE" acquire 3 2>&1); rc=$?
check "an acquire with no tick above it is refused, not recorded under a transient pid" '[ "$rc" -eq 2 ] && [[ "$out" == *"no claude -p tick"* ]] && [ ! -e "$L/resource.lease" ]' "rc=$rc $out"
out=$("$LEASE" status 2>&1); rc=$?
check "status needs no tick" '[ "$rc" -eq 0 ] && [[ "$out" == *"slot 1 free"* ]]' "rc=$rc $out"
out=$(LEASE_HOLDER_PID=$$ "$LEASE" acquire 3 2>&1)
check "a script can hold the lease as a named live ancestor" '[[ "$out" == "ACQUIRED slot=1 "* ]] && grep -q "^3 $$ " "$L/resource.lease"' "$out"
LEASE_HOLDER_PID=$$ "$LEASE" release 3 >/dev/null
rm -f "$H/bin/tick-pid.sh"

sleep 600 & D=$!
trap 'kill $A $B $D 2>/dev/null; rm -rf "$H"' EXIT
reset; rm -f "$L"/all-wait.*
echo "1 $D $(ago 30) domain=77" > "$L/resource.lease"
echo "2 $B $(ago 30) domain=78" > "$L/resource.lease.2"
out=$("$LEASE" acquire 3); check "a third kit waits while two run, though a slot file is free" '[[ "$out" == "BUSY kit limit 2/2"* ]] && [ ! -e "$L/resource.lease.3" ]' "$out"
out=$(GPU_KITS_MAX=3 "$LEASE" acquire 3); check "the kit limit is a setting" '[[ "$out" == "ACQUIRED slot=3 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "1 $D $(ago 30) domain=77" > "$L/resource.lease"
echo "2 $B $(ago 30) domain=78 reserved-for-all" > "$L/resource.lease.2"
out=$("$LEASE" acquire 3); check "a whole-card reservation is not a kit" '[[ "$out" == "ACQUIRED slot=3 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "3 $$ $(ago 60) domain=77" > "$L/resource.lease"
echo "$(old 2000) $B $(old 10)" > "$L/all-wait.2"
out=$("$LEASE" acquire 3); check "a holder re-acquiring while a whole-card issue has priority is asked to yield" '[[ "$out" == "ACQUIRED slot=1 "* ]] && [[ "$out" == *"YIELD-REQUESTED whole-card issue 2"* ]]' "$out"
rm -f "$L"/all-wait.*
out=$("$LEASE" acquire 3); check "without a priority issue no yield is asked" '[[ "$out" == "ACQUIRED slot=1 "* ]] && [[ "$out" != *YIELD* ]]' "$out"

reset; rm -f "$L"/all-wait.*
out=$("$LEASE" acquire 3 --all 2>&1); rc=$?
check "a whole-card acquire that does not say it renders is refused" '[ "$rc" -eq 2 ] && [[ "$out" == REFUSED* ]] && [ ! -e "$L/resource.lease" ]' "rc=$rc $out"


reset; rm -f "$L"/all-wait.*
echo "$(old 1300) $B $(old 10)" > "$L/slot-wait.5"
out=$("$LEASE" acquire 3); check "a single-slot issue that has waited 20 min is first in line for a free slot" '[[ "$out" == "BUSY queued behind single-slot issue 5"* ]] && [ ! -e "$L/resource.lease" ] && [ -s "$L/slot-wait.3" ]' "$out"
out=$("$LEASE" status); check "status names the issue first in line" '[[ "$out" == *"wait single-slot issue 5 for 21 min (first in line)"* ]]' "$out"
out=$("$LEASE" acquire 5); check "the issue first in line takes the slot, and its wait record is cleared" '[[ "$out" == "ACQUIRED slot=1 "* ]] && [ ! -e "$L/slot-wait.5" ]' "$out"
out=$("$LEASE" acquire 3); check "the next waiter is not queued behind a wait that ended" '[[ "$out" == "ACQUIRED slot="* ]] && [ ! -e "$L/slot-wait.3" ]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 600) $B $(old 10)" > "$L/slot-wait.5"
out=$("$LEASE" acquire 3); check "a younger single-slot wait does not queue others" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 1300) $B $(old 400)" > "$L/slot-wait.5"
out=$("$LEASE" acquire 3); check "a single-slot wait whose tick stopped asking does not queue others" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 1300) 999999 $(old 10)" > "$L/slot-wait.5"
out=$("$LEASE" acquire 3); check "a single-slot wait whose tick is dead does not queue others" '[[ "$out" == "ACQUIRED slot=1 "* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "1 $D $(ago 30) domain=77" > "$L/resource.lease"
echo "2 $B $(ago 30) domain=78" > "$L/resource.lease.2"
out=$("$LEASE" acquire 7); first1=$(awk '{print $1}' "$L/slot-wait.7")
sleep 1; out=$("$LEASE" acquire 7); first2=$(awk '{print $1}' "$L/slot-wait.7"); last2=$(awk '{print $3}' "$L/slot-wait.7")
check "a refused single-slot acquire records its wait and keeps the first time across calls" '[[ "$out" == "BUSY kit limit 2/2"* ]] && [ "$first1" = "$first2" ] && [ "$last2" -gt "$first1" ]' "$out first1=$first1 first2=$first2 last2=$last2"

reset; rm -f "$L"/all-wait.*
echo "3 $$ $(ago 60) domain=77" > "$L/resource.lease"
echo "$(old 1300) $B $(old 10)" > "$L/slot-wait.5"
out=$("$LEASE" acquire 3); check "a holder re-acquiring its own slot is not queued" '[[ "$out" == "ACQUIRED slot=1 "* ]] && [[ "$out" != *YIELD* ]]' "$out"

reset; rm -f "$L"/all-wait.*
echo "$(old 2000) $B $(old 10)" > "$L/all-wait.2"
echo "$(old 1300) $B $(old 10)" > "$L/slot-wait.5"
out=$("$LEASE" acquire 5); check "whole-card priority comes before the single-slot line" '[[ "$out" == "BUSY priority to whole-card issue 2"* ]]' "$out"
out=$("$LEASE" status); check "status names the whole-card priority issue" '[[ "$out" == *"wait whole-card issue 2 for 33 min (priority)"* ]]' "$out"
rm -f "$L"/all-wait.* "$L"/slot-wait.*

reset; rm -f "$L"/all-wait.*
echo "$(old 1300) $B $(old 10)" > "$L/slot-wait.5"
echo "1 $D $(ago 30) domain=77" > "$L/resource.lease"
out=$("$LEASE" acquire 5 --all --render); check "a whole-card waiter still reserves while a single-slot issue waits" '[[ "$out" == "RESERVING held=2/3"* ]]' "$out"
"$LEASE" release 5 >/dev/null; rm -f "$L"/all-wait.* "$L"/slot-wait.*

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
