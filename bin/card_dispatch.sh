#!/usr/bin/env bash
# Pulls a queued run onto a free card. A run queue (bin/arm_queue.sh) pinned by --want-host waits for
# that one card even while another sits free. A pair read holds on any card (runbook), so once a
# pinned queue has said BUSY for CARD_PULL_AFTER_MIN (5) minutes on end while another remote card is
# free (no live lease on its slots), idle on its last probe and quiet (bin/remote_room.sh --alone),
# this writes `any` to the queue's <runs>/want-host, which the runner reads before its next ask. One
# queue a free card a pass, the operator's priority first, then the longest wait. A queue that reads
# the bar (--bar-read), or that a STOP file ends, is never moved. Each pull is a line in
# state/logs/card_dispatch.log and a `PULL` line in state/stalls.log. Cron every minute; exits 0.
# This host's card takes part only for the issues the owner opened it to
# (state/locks/local-card-issues): when no live lease holds one of its slots and nothing computes
# on it, a listed issue's waiting queue gets `local`.
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
S="${HARNESS_STATE:-$H/state}"
: "${CARD_PULL_AFTER_MIN:=5}"
: "${CARD_DISPATCH_QUEUE_LOGS:=$HOME/tmp_workspace/artifacts/manure-plan/evidence/*/runs*/lease.log}"
mkdir -p "$S/logs" "$S/locks" 2>/dev/null || exit 0
exec 9>"$S/locks/card_dispatch.lock" || exit 0
flock -n 9 || exit 0
now=$(date +%s); iso=$(date -u +%FT%TZ)

# Pinned waiters: "<priority rank> <BUSY since epoch> <wanted host> <runs dir> <issue>".
prio=" $(cat "$S/locks/operator-priority" 2>/dev/null | tr '\n' ' ') "
waiters=$(for lg in $CARD_DISPATCH_QUEUE_LOGS; do
  [ -f "$lg" ] || continue
  [ $(( now - $(stat -c %Y "$lg") )) -le 180 ] || continue
  runs=$(dirname "$lg"); packet=$(dirname "$runs")
  [ -e "$runs/STOP" ] || [ -e "$packet/STOP" ] && continue
  read -r want bar < <(awk '/^holder [0-9]+ start / { w = ""; b = 0; for (i = 1; i <= NF; i++) { if ($i ~ /^want=/) w = substr($i, 6); if ($i == "bar=1") b = 1 } }
                            END { print w, b }' "$lg")
  [ -n "$want" ] && [ "$want" != any ] && [ "$want" != local ] && [ "$bar" != 1 ] || continue
  read -r over _ < "$runs/want-host" 2>/dev/null; case "${over:-}" in any|local) continue ;; esac
  start=$(awk '/^20[0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9:]+Z / { if ($2 == "BUSY") { if (!run) { s = $1; run = 1 } } else run = 0 }
               END { if (run) print s }' "$lg")
  [ -n "$start" ] || continue
  t0=$(date -u -d "$start" +%s 2>/dev/null) || continue
  [ $(( now - t0 )) -ge $(( CARD_PULL_AFTER_MIN * 60 )) ] || continue
  issue=$(basename "$packet" | grep -oE '^[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9]+' | awk -F- '{ print $4 }')
  case "$prio" in *" ${issue:-x} "*) rank=0 ;; *) rank=1 ;; esac
  echo "$rank $t0 $want $runs ${issue:-?}"
done | sort -k1,1n -k2,2n)
[ -n "$waiters" ] || exit 0

# Free cards: remote hosts with no live lease on any slot, idle on their last probe.
lease_status=$("$H/bin/lease.sh" status < /dev/null 2>/dev/null)
free=$(printf '%s\n' "$lease_status" | awk '
    /^remote card: / { host = $3; split(substr($5, 1, length($5) - 1), r, "-"); lo[host] = r[1]; hi[host] = r[2] }
    /^slot [0-9]+ HELD / && /\(alive\)$/ { held[$2] = 1 }
    END { for (h in lo) { busy = 0; for (s in held) if (s + 0 >= lo[h] && s + 0 <= hi[h]) busy = 1; if (!busy) print h } }')
pulled=""
for h in $free; do
  idle=$(awk -F'\t' -v h="$h" '$1 != "time" && $2 == h { g = $7 } END { print (g == "0") ? 1 : 0 }' "$S/host_health.tsv" 2>/dev/null)
  [ "$idle" = 1 ] || continue
  REMOTE_CARD_HOST="$h" "$H/bin/remote_room.sh" --alone >/dev/null 2>&1 || continue
  while read -r _ t0 want runs issue; do
    [ -n "${runs:-}" ] && [ "$want" != "$h" ] || continue
    case " $pulled " in *" $runs "*) continue ;; esac
    echo any > "$runs/want-host"
    key="$(basename "$(dirname "$runs")")/$(basename "$runs")"
    line="#$issue's queue pinned to $want, BUSY $(( (now - t0) / 60 )) min on end, unpinned onto free card $h"
    echo "$iso PULL $key $line" >> "$S/stalls.log"
    echo "$iso $key $line" >> "$S/logs/card_dispatch.log"
    pulled+=" $runs"
    break
  done <<< "$waiters"
done

# This host's card: its slots are the ones below every remote card's.
: "${CARD_DISPATCH_SMI:=nvidia-smi}"
lfile="$S/locks/local-card-issues"
if [ -s "$lfile" ]; then
  local_held=$(printf '%s\n' "$lease_status" | awk '
      /^remote card: / { split(substr($5, 1, length($5) - 1), r, "-"); if (min == "" || r[1] + 0 < min) min = r[1] + 0 }
      /^slot [0-9]+ HELD / && /\(alive\)$/ { held[$2] = 1 }
      END { b = 0; for (s in held) if (min == "" || s + 0 < min) b = 1; print b }')
  util=$("$CARD_DISPATCH_SMI" --query-gpu=utilization.gpu --format=csv,noheader,nounits 2>/dev/null | head -1 | tr -d ' ')
  apps=$("$CARD_DISPATCH_SMI" --query-compute-apps=pid --format=csv,noheader 2>/dev/null | grep -c '[0-9]')
  if [ "$local_held" = 0 ] && [ "$util" = 0 ] && [ "$apps" = 0 ]; then
    while read -r _ t0 want runs issue; do
      [ -n "${runs:-}" ] && grep -qx "$issue" "$lfile" || continue
      case " $pulled " in *" $runs "*) continue ;; esac
      echo local > "$runs/want-host"
      key="$(basename "$(dirname "$runs")")/$(basename "$runs")"
      line="#$issue's queue pinned to $want, BUSY $(( (now - t0) / 60 )) min on end, unpinned onto this host's free card"
      echo "$iso PULL $key $line" >> "$S/stalls.log"
      echo "$iso $key $line" >> "$S/logs/card_dispatch.log"
      pulled+=" $runs"
      break
    done <<< "$waiters"
  fi
fi
exit 0
