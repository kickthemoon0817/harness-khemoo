#!/usr/bin/env bash
# Exercises lease.sh's remote whole-card grant against the operator's priority, in a scratch harness
# home with two remote hosts whose room check always answers ok.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
T=$(mktemp -d)
sleep 600 >/dev/null 2>&1 & A=$!
trap 'kill $A 2>/dev/null; rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/state/locks" "$T/cache"
cp "$here/bin/lease.sh" "$T/bin/"; printf '#!/usr/bin/env bash\necho ok\n' > "$T/bin/remote_room.sh"; chmod +x "$T/bin/"*.sh
printf 'a@one\nb@two\n' > "$T/state/locks/remote-card"
export HARNESS_HOME="$T" GPU_SLOTS=3 LEASE_CACHE_BASE="$T/cache"
L="$T/state/locks"; LEASE="$T/bin/lease.sh"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
reset() { rm -f "$L"/resource.lease* "$L"/cost-wait.* "$L"/operator-priority; }
acq() { LEASE_HOLDER_PID=$$ "$LEASE" acquire "$@" 2>&1; }

reset
out=$(acq 10 --all-remote)
check "with no priority a whole-card grant takes the first free card" '[[ "$out" == "ACQUIRED slot=4 "* ]]' "$out"

reset; echo 2504 > "$L/operator-priority"; echo "any $(date +%s)" > "$L/cost-wait.2504"
out=$(acq 10 --all-remote)
check "a prioritized cost read waiting for any card keeps the first free card; this grant takes the next" '[[ "$out" == "ACQUIRED slot=6 "* ]]' "$out"

reset; echo 2504 > "$L/operator-priority"; echo "any $(date +%s)" > "$L/cost-wait.2504"
echo "7 $A 2026-11-26T00:00:00Z domain=82 remote" > "$L/resource.lease.6"
out=$(acq 10 --all-remote)
check "with one card free and a prioritized wait, the grant is refused and names the priority" '[[ "$out" == "BUSY the free card a@one is left to issue 2504"* ]] && [ ! -e "$L/resource.lease.4" ]' "$out"

reset; echo 2504 > "$L/operator-priority"; echo "b@two $(date +%s)" > "$L/cost-wait.2504"
out=$(acq 10 --all-remote)
check "a prioritized wait for one host leaves the other host to this grant" '[[ "$out" == "ACQUIRED slot=4 "* ]]' "$out"

reset; echo 2504 > "$L/operator-priority"; echo "any $(( $(date +%s) - 600 ))" > "$L/cost-wait.2504"
out=$(acq 10 --all-remote)
check "a stale wait marker holds nothing back" '[[ "$out" == "ACQUIRED slot=4 "* ]]' "$out"

reset; echo 2504 > "$L/operator-priority"; echo "any $(date +%s)" > "$L/cost-wait.2504"
out=$(acq 2504 --all-remote)
check "the prioritized issue itself takes the first free card" '[[ "$out" == "ACQUIRED slot=4 "* ]] && [ ! -e "$L/cost-wait.2504" ]' "$out"

reset; echo 2504 > "$L/operator-priority"; echo "any $(date +%s)" > "$L/cost-wait.2504"
out=$(acq 10 --all-remote --no-cost)
check "a no-cost grant yields to the prioritized cost read too" '[[ "$out" == "ACQUIRED slot=6 "* ]]' "$out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
