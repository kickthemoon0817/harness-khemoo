#!/usr/bin/env bash
# Prints "<user@host> <issue>" for each card a waiting cost read asks to be quiet. A cost read left
# waiting by `lease.sh acquire --all-remote` writes state/locks/cost-wait.<issue> =
# "<wanted host or any> <epoch>", refreshed every 30 s while its runner asks. A marker older than
# COST_WAIT_FRESH_S (120) belongs to no live runner and is skipped. A wait for any card names the
# remote host no live lease holds with the lowest load in state/host_health.tsv.
#
#   cost_wait_hosts.sh [--except-issue <n>]   leave out that issue's own wait
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
S="${HARNESS_STATE:-$H/state}"
: "${COST_WAIT_FRESH_S:=120}"
except=""; [ "${1:-}" = --except-issue ] && except="${2:-}"
now=$(date +%s); any_held=""; any_read=""
for f in "$S"/locks/cost-wait.*; do
  [ -f "$f" ] || continue
  issue=${f##*.}
  [ -n "$except" ] && [ "$issue" = "$except" ] && continue
  read -r want t _ < "$f"
  [[ "${t:-}" =~ ^[0-9]+$ ]] && [ $(( now - t )) -le "$COST_WAIT_FRESH_S" ] || continue
  if [ "$want" != any ]; then echo "$want $issue"; continue; fi
  if [ -z "$any_read" ]; then
    any_read=1
    any_held=$("$H/bin/lease.sh" status < /dev/null 2>/dev/null | awk '
        /^remote card: / { host = $3; split(substr($5, 1, length($5) - 1), r, "-"); lo[host] = r[1]; hi[host] = r[2] }
        /^slot [0-9]+ HELD / && /\(alive\)$/ { held[$2] = 1 }
        END { for (h in lo) for (s in held) if (s + 0 >= lo[h] && s + 0 <= hi[h]) { print h; break } }')
  fi
  quiet=$(awk -F'\t' -v held=" $(echo $any_held) " '
      $1 != "time" && index(held, " " $2 " ") == 0 && $5 ~ /^[0-9.]+$/ { load[$2] = $5 }
      END { best = ""; for (h in load) if (best == "" || load[h] + 0 < load[best] + 0) best = h; print best }' \
      "$S/host_health.tsv" 2>/dev/null)
  [ -n "$quiet" ] && echo "$quiet $issue"
done
exit 0
