#!/usr/bin/env bash
# Trims regenerable arrays out of old evidence packets. A packet keeps its
# PNG frames, series, traces and INDEX.md; its .npy AOVs (depth, normals,
# instance) go once the packet's issue is closed (or, for a shared
# parent-<base> folder, its base is no longer a recent head) and nothing in it
# has changed for TRIM_AFTER_DAYS. Baselines named in BASELINES.md are never
# touched. Each trimmed packet gets AOV_TRIMMED.txt saying what went and when.
#   evidence_trim.sh           dry run
#   evidence_trim.sh --apply   trims, and logs to state/logs/evidence_trim.log
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# Cron gives no environment: take the harness token and PATH the way tick.sh does.
export HARNESS_HOME="$H"
[ -r "$H/config/harness.env" ] && set -a && . "$H/config/harness.env" && set +a
export PATH="${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"
EV="${EVIDENCE_ROOT:-$HOME/tmp_workspace/artifacts/manure-plan/evidence}"
REPO="${MAIN_REPO:-$HOME/tmp_workspace/isaac_sim}"
: "${GH_REPO:=MaumAI-Company/isaac_sim}"; : "${TRIM_AFTER_DAYS:=7}"; : "${KEEP_HEADS:=3}"
apply=0; [ "${1:-}" = "--apply" ] && apply=1
LOG="$H/state/logs/evidence_trim.log"
open=$(gh issue list -R "$GH_REPO" --state open --limit 500 --json number --jq '.[].number' 2>/dev/null) || { echo "gh unavailable; nothing trimmed" >&2; exit 1; }
recent=$(git -C "$REPO" rev-list --first-parent -n "$KEEP_HEADS" origin/ai/manure-mpm 2>/dev/null | cut -c1-8)
baselines=$(grep -o -E '`[0-9]{4}-[0-9]{2}-[0-9]{2}-[^`/]+/`' "$EV/BASELINES.md" 2>/dev/null | tr -d '`/')
total=0; packets=0
for path in "$EV"/*/; do
  d=$(basename "$path")
  echo "$baselines" | grep -qx "$d" && continue
  if [[ "$d" =~ ^parent-([0-9a-f]{7,}) ]]; then
    echo "$recent" | grep -q "^${BASH_REMATCH[1]:0:8}" && continue
  elif [[ "$d" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}-([0-9]{3,5})- ]]; then
    echo "$open" | grep -qx "${BASH_REMATCH[1]}" && continue
  else
    continue
  fi
  [ -n "$(find "$path" -newermt "-${TRIM_AFTER_DAYS} days" -print -quit 2>/dev/null)" ] && continue
  bytes=$(find "$path" -name '*.npy' -printf '%s\n' 2>/dev/null | awk '{s+=$1} END {print s+0}')
  [ "$bytes" -gt 0 ] || continue
  total=$((total + bytes)); packets=$((packets + 1))
  echo "trim $d: $((bytes / 1048576)) MiB of .npy"
  if [ "$apply" = 1 ]; then
    find "$path" -name '*.npy' -printf '%P\n' > "$path/AOV_TRIMMED.txt.tmp"
    find "$path" -name '*.npy' -delete
    { echo "Regenerable .npy AOVs removed $(date -u +%FT%TZ) by evidence_trim.sh (closed issue, unchanged for ${TRIM_AFTER_DAYS} days). Re-run the arm to regenerate:"; cat "$path/AOV_TRIMMED.txt.tmp"; } > "$path/AOV_TRIMMED.txt"
    rm -f "$path/AOV_TRIMMED.txt.tmp"
  fi
done
echo "$packets packets, $((total / 1073741824)) GiB$([ "$apply" = 1 ] || echo ' (dry run)')"
[ "$apply" = 1 ] && echo "$(date -u +%FT%TZ) trimmed $packets packets, $((total / 1048576)) MiB; /home $(df -h /home | awk 'NR==2{print $4}') free" >> "$LOG"
exit 0
