#!/usr/bin/env bash
# Exercises bin/card_dispatch.sh in a scratch harness home: a fake lease.sh and remote_room.sh, a
# probe table, and run queues' lease logs written as bin/arm_queue.sh writes them.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/h/bin" "$T/h/state/locks" "$T/h/state/logs" "$T/ev"
cp "$here/bin/card_dispatch.sh" "$T/h/bin/"
cat > "$T/h/bin/lease.sh" <<'L'
#!/usr/bin/env bash
[ -n "${LEASE_CALLS:-}" ] && echo "$*" >> "$LEASE_CALLS"
echo "remote card: a@one (slots 4-5)"
echo "remote card: b@two (slots 6-7)"
for s in ${HELD_SLOTS:-4}; do echo "slot $s HELD 2511 1 t domain=80 remote-all render (alive)"; done
L
printf '#!/usr/bin/env bash\n[ -z "${ROOM_BUSY:-}" ]\n' > "$T/h/bin/remote_room.sh"
cat > "$T/h/bin/smi" <<'L'
#!/usr/bin/env bash
case "$*" in *utilization*) echo "${SMI_UTIL:-0}" ;; *compute-apps*) [ -n "${SMI_APP:-}" ] && echo "$SMI_APP" ;; esac
exit 0
L
chmod +x "$T/h/bin/"*.sh "$T/h/bin/smi"
export HARNESS_HOME="$T/h" HARNESS_STATE="$T/h/state" CARD_DISPATCH_QUEUE_LOGS="$T/ev/*/runs*/lease.log" CARD_DISPATCH_SMI="$T/h/bin/smi"
S="$T/h/state"; D="$T/h/bin/card_dispatch.sh"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
iso() { date -u -d "-$1 min" +%FT%TZ; }
probes() {  # <gpu % of b@two>
  { printf 'time\thost\tconnect_s\tupload_mb_s\tload1\tnproc\tgpu_util_pct\tgpu_mem_used_mb\tstatus\n'
    printf '%s\ta@one\t0.2\t80\t3.0\t16\t90\t9000\tok\n' "$(iso 1)"
    printf '%s\tb@two\t0.2\t80\t3.0\t24\t%s\t20\tok\n' "$(iso 1)" "$1"; } > "$S/host_health.tsv"
}
queue() {  # <packet name> <runs> <want> <bar 0|1> <minutes BUSY>
  local r="$T/ev/$1/$2"; mkdir -p "$r"; rm -f "$r/want-host" "$r/STOP"
  { echo "holder 1 start $(iso "$5") queue=x want=$3 bar=$4 deadline=0"
    for m in $(seq "$5" -1 0); do echo "$(iso "$m") BUSY no remote card is free, alone and quiet"; done; } > "$r/lease.log"
}
reset() { rm -rf "$T/ev/"*; : > "$S/stalls.log"; rm -f "$S/locks/operator-priority" "$S/locks/local-card-issues"; }

reset; probes 0; LEASE_CALLS="$T/lease.calls" "$D"
check "a pass with no waiting queue still reads lease.sh status, which samples idle holds" 'grep -qx status "$T/lease.calls"' "$(cat "$T/lease.calls" 2>/dev/null)"

reset; probes 0; queue 2026-11-26-2504-lanes runs-w03 a@one 0 8; "$D"
check "a queue pinned to a held card 5+ min is unpinned onto the free idle quiet card" \
  '[ "$(cat "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host")" = any ] && grep -q "PULL 2026-11-26-2504-lanes/runs-w03 #2504.s queue pinned to a@one, BUSY 8 min on end, unpinned onto free card b@two" "$S/stalls.log"' "$(cat "$S/stalls.log")"
"$D"
check "an unpinned queue is not pulled twice" '[ "$(grep -c PULL "$S/stalls.log")" = 1 ]' "$(cat "$S/stalls.log")"

reset; probes 0; queue 2026-11-26-2504-lanes runs-w03 a@one 0 3; "$D"
check "a wait under CARD_PULL_AFTER_MIN is left alone" '[ ! -e "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host" ]' "$(cat "$S/stalls.log")"

reset; probes 0; queue 2026-11-26-2490-bar runs a@one 1 8; "$D"
check "a bar read is never moved" '[ ! -e "$T/ev/2026-11-26-2490-bar/runs/want-host" ] && [ ! -s "$S/stalls.log" ]' "$(cat "$S/stalls.log")"

reset; probes 0; queue 2026-11-26-2504-lanes runs-w03 a@one 0 8; touch "$T/ev/2026-11-26-2504-lanes/runs-w03/STOP"; "$D"
check "a queue a STOP file ends is left alone" '[ ! -e "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host" ]' "$(cat "$S/stalls.log")"

reset; probes 0; queue 2026-11-26-2504-lanes runs-w03 a@one 0 8; touch -d "-10 min" "$T/ev/2026-11-26-2504-lanes/runs-w03/lease.log"; "$D"
check "a lease log no runner wrote for 3+ minutes is left alone" '[ ! -e "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host" ]' "$(cat "$S/stalls.log")"

reset; probes 0; queue 2026-11-26-2504-lanes runs-w03 a@one 0 8; ROOM_BUSY=1 "$D"
check "a free card that is not quiet takes nothing" '[ ! -e "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host" ]' "$(cat "$S/stalls.log")"

reset; probes 55; queue 2026-11-26-2504-lanes runs-w03 a@one 0 8; "$D"
check "a free card that works on its last probe takes nothing" '[ ! -e "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host" ]' "$(cat "$S/stalls.log")"

reset; probes 0; queue 2026-11-26-2504-lanes runs-w03 a@one 0 8; HELD_SLOTS="4 6" "$D"
check "with no card free nothing is pulled" '[ ! -e "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host" ]' "$(cat "$S/stalls.log")"

reset; probes 0; queue 2026-11-26-2494-memo runs a@one 0 20; queue 2026-11-26-2504-lanes runs-w03 a@one 0 8; echo 2504 > "$S/locks/operator-priority"; "$D"
check "with one free card the operator's priority is pulled first, though it waited less" \
  '[ -e "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host" ] && [ ! -e "$T/ev/2026-11-26-2494-memo/runs/want-host" ]' "$(cat "$S/stalls.log")"
reset; probes 0; queue 2026-11-26-2494-memo runs a@one 0 20; queue 2026-11-26-2504-lanes runs-w03 a@one 0 8; "$D"
check "with no priority the longest wait is pulled first" \
  '[ -e "$T/ev/2026-11-26-2494-memo/runs/want-host" ] && [ ! -e "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host" ]' "$(cat "$S/stalls.log")"

reset; probes 0; queue 2026-11-26-2504-lanes runs-w03 b@two 0 8; "$D"
check "a queue pinned to the free card itself is not touched" '[ ! -e "$T/ev/2026-11-26-2504-lanes/runs-w03/want-host" ]' "$(cat "$S/stalls.log")"

# This host's card, for the issues the owner opened it to.
W="$T/ev/2026-11-27-2535-dense/runs-idm-a"
reset; probes 0; queue 2026-11-27-2535-dense runs-idm-a b@two 0 8; echo 2535 > "$S/locks/local-card-issues"; HELD_SLOTS="4 6" "$D"
check "a listed issue's pinned queue is unpinned onto this host's free idle card" \
  '[ "$(cat "$W/want-host")" = local ] && grep -q "PULL 2026-11-27-2535-dense/runs-idm-a #2535.s queue pinned to b@two, BUSY 8 min on end, unpinned onto this host.s free card" "$S/stalls.log"' "$(cat "$S/stalls.log")"
HELD_SLOTS="4 6" "$D"
check "a queue already sent to this host's card is not pulled again" '[ "$(grep -c PULL "$S/stalls.log")" = 1 ]' "$(cat "$S/stalls.log")"
reset; probes 0; queue 2026-11-27-2535-dense runs-idm-a b@two 0 8; echo 2537 > "$S/locks/local-card-issues"; HELD_SLOTS="4 6" "$D"
check "an issue the owner did not list never goes to this host's card" '[ ! -e "$W/want-host" ] && [ ! -s "$S/stalls.log" ]' "$(cat "$S/stalls.log")"
reset; probes 0; queue 2026-11-27-2535-dense runs-idm-a b@two 0 8; echo 2535 > "$S/locks/local-card-issues"; HELD_SLOTS="2 4 6" "$D"
check "this host's card with a live lease on a slot takes nothing" '[ ! -e "$W/want-host" ]' "$(cat "$S/stalls.log")"
reset; probes 0; queue 2026-11-27-2535-dense runs-idm-a b@two 0 8; echo 2535 > "$S/locks/local-card-issues"; HELD_SLOTS="4 6" SMI_APP=4242 "$D"
check "this host's card with a process computing on it takes nothing" '[ ! -e "$W/want-host" ]' "$(cat "$S/stalls.log")"
reset; probes 0; queue 2026-11-27-2535-dense runs-idm-a b@two 0 8; echo 2535 > "$S/locks/local-card-issues"; HELD_SLOTS="4 6" SMI_UTIL=37 "$D"
check "this host's card that is busy takes nothing" '[ ! -e "$W/want-host" ]' "$(cat "$S/stalls.log")"
reset; probes 0; queue 2026-11-27-2535-dense runs-idm-a a@one 0 20; queue 2026-11-27-2532-c1 runs-p a@one 0 8; printf '2532\n2535\n' > "$S/locks/local-card-issues"; HELD_SLOTS="4" "$D"
check "with a remote card and this host's card free, two waiters get one each" \
  '[ "$(cat "$W/want-host")" = any ] && [ "$(cat "$T/ev/2026-11-27-2532-c1/runs-p/want-host")" = local ]' "$(cat "$S/stalls.log")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
