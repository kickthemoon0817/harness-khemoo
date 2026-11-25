#!/usr/bin/env bash
# Probes every remote card's host and keeps the harness's host order to the healthy ones.
# Scheduled from cron every 3 minutes; one run at a time.
#
# Each run appends one row a host to state/host_health.tsv (kept to the last
# HOST_HEALTH_KEEP_ROWS rows), tab-separated:
#   time  host  connect_s  upload_mb_s  load1  nproc  gpu_util_pct  gpu_mem_used_mb  status
# status is ok, slow (the upload ran under HOST_MIN_UPLOAD_MB_S, or past
# HOST_UPLOAD_TIMEOUT_S: upload_mb_s reads "timeout") or down (no ssh session).
# The upload sends HOST_UPLOAD_BYTES of random data to `cat > /dev/null` over the
# session that timed the connect, so the connect time is not counted in it.
#
# With harness-only on (state/locks/harness-first-remote-only present), the hosts
# ticks use are the operator's preference, state/locks/harness-host-preference,
# with every degraded host moved last and none dropped: a degraded host still
# takes work that copies little. Absent, the preference is seeded once from the
# current harness-only list. bin/host_order.sh holds the policy and
# state/host_health.streaks the per-host streaks. A changed order is applied
# through `lease.sh harness-only --on` and logged to state/logs/host_health.log;
# the slots and leases are never touched, and a running tick keeps the host it
# started on. With harness-only off the probe still records its rows and streaks.
set -u
HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# shellcheck source=/dev/null
[ -r "$HARNESS_HOME/config/harness.env" ] && . "$HARNESS_HOME/config/harness.env"
: "${HARNESS_STATE:=$HARNESS_HOME/state}"
export PATH="${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"
: "${HOST_MIN_UPLOAD_MB_S:=5}"
: "${HOST_UPLOAD_BYTES:=1000000}"
: "${HOST_UPLOAD_TIMEOUT_S:=30}"
: "${HOST_CONNECT_TIMEOUT_S:=30}"
: "${HOST_HEALTH_KEEP_ROWS:=5000}"
LEASE="${HOST_HEALTH_LEASE:-$HARNESS_HOME/bin/lease.sh}"
LOCKS="$HARNESS_STATE/locks"
TSV="$HARNESS_STATE/host_health.tsv"
STREAKS="$HARNESS_STATE/host_health.streaks"
PREF="$LOCKS/harness-host-preference"
ONLY="$LOCKS/harness-first-remote-only"
LOG="$HARNESS_STATE/logs/host_health.log"
mkdir -p "$LOCKS" "$HARNESS_STATE/logs"
exec 9>"$LOCKS/host_health.lock"; flock -n 9 || exit 0

mapfile -t hosts < <(grep -v '^[[:space:]]*$' "$LOCKS/remote-card" 2>/dev/null)
[ "${#hosts[@]}" -gt 0 ] || exit 0
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
head -c "$HOST_UPLOAD_BYTES" /dev/urandom > "$work/payload"
ts=$(date -u +%FT%TZ)
secs() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }

# probe <host> <index>: writes this run's row for the host to $work/row.<index>.
probe() {
  local h=$1 i=$2 ctl t0 t1 connect load cores util mem upload status rc
  ctl="$work/c$i"
  # ssh takes an option's first value, so the master and client settings lead the shared ones.
  local o=(-o BatchMode=yes -o ConnectTimeout="$HOST_CONNECT_TIMEOUT_S" -o ControlPath="$ctl")
  t0=$(date +%s.%N)
  # The master session goes to the background holding none of this script's
  # descriptors: an inherited run lock or cron's output pipe would outlive the run.
  if ! timeout "$HOST_CONNECT_TIMEOUT_S" ssh -o ControlMaster=yes -o ControlPersist=120 "${o[@]}" -fN "$h" \
      < /dev/null > /dev/null 2>&1 9>&-; then
    printf '%s\t%s\t-\t-\t-\t-\t-\t-\tdown\n' "$ts" "$h" > "$work/row.$i"
    return
  fi
  t1=$(date +%s.%N); connect=$(secs "$t0" "$t1")
  # shellcheck disable=SC2016
  read -r load cores util mem < <(timeout 20 ssh -o ControlMaster=no "${o[@]}" "$h" \
    'printf "%s %s " "$(cut -d" " -f1 /proc/loadavg)" "$(nproc)"; nvidia-smi --query-gpu=utilization.gpu,memory.used --format=csv,noheader,nounits 2>/dev/null | head -1 | tr -d ,' \
    < /dev/null 2>/dev/null)
  t0=$(date +%s.%N)
  timeout "$HOST_UPLOAD_TIMEOUT_S" ssh -o ControlMaster=no "${o[@]}" "$h" 'cat > /dev/null' < "$work/payload" > /dev/null 2>&1
  rc=$?; t1=$(date +%s.%N)
  if [ "$rc" -eq 0 ]; then
    upload=$(awk -v n="$HOST_UPLOAD_BYTES" -v a="$t0" -v b="$t1" 'BEGIN { d = b - a; if (d < 0.001) d = 0.001; printf "%.2f", n / 1e6 / d }')
    status=$(awk -v u="$upload" -v m="$HOST_MIN_UPLOAD_MB_S" 'BEGIN { print (u + 0 >= m + 0 ? "ok" : "slow") }')
  elif [ "$rc" -eq 124 ]; then upload=timeout; status=slow
  else upload=-; status=down
  fi
  timeout 15 ssh -o ControlPath="$ctl" -O exit "$h" < /dev/null > /dev/null 2>&1
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$ts" "$h" "$connect" "$upload" \
    "${load:--}" "${cores:--}" "${util:--}" "${mem:--}" "$status" > "$work/row.$i"
}

for i in "${!hosts[@]}"; do probe "${hosts[$i]}" "$i" & done
wait

[ -s "$TSV" ] || printf 'time\thost\tconnect_s\tupload_mb_s\tload1\tnproc\tgpu_util_pct\tgpu_mem_used_mb\tstatus\n' > "$TSV"
cat "$work"/row.* > "$work/rows" 2>/dev/null
cat "$work/rows" >> "$TSV"
awk -F'\t' '{ print $2, $9 }' "$work/rows" > "$work/results"
if [ "$(wc -l < "$TSV")" -gt $(( HOST_HEALTH_KEEP_ROWS + 1 )) ]; then
  { head -1 "$TSV"; tail -n "$HOST_HEALTH_KEEP_ROWS" "$TSV"; } > "$TSV.new" && mv "$TSV.new" "$TSV"
fi

# The hosts harness ticks use now, read as lease.sh reads them: an empty list is the first remote card.
current=$(grep -v '^[[:space:]]*$' "$ONLY" 2>/dev/null | paste -sd' ' -)
[ -n "$current" ] || current=${hosts[0]}
if [ -e "$ONLY" ] && [ ! -e "$PREF" ]; then
  tr ' ' '\n' <<< "$current" > "$PREF"
  echo "$ts preference seeded from harness-first-remote-only: $current" >> "$LOG"
fi

now=$(date +%s)
pref_in="$PREF"; [ -e "$ONLY" ] || pref_in=/dev/null
policy=$(HOST_HEALTH_NOW=$now "$HARNESS_HOME/bin/host_order.sh" "$pref_in" "$STREAKS" "$work/results") || exit 0
sed -n 's/^streak //p' <<< "$policy" > "$STREAKS.new" && mv "$STREAKS.new" "$STREAKS"
[ -e "$ONLY" ] || exit 0

# lease.sh refuses a host that is not a remote card, so the order keeps only those.
order=""
read -ra policy_order <<< "$(sed -n 's/^order *//p' <<< "$policy")"
for h in "${policy_order[@]}"; do
  printf '%s\n' "${hosts[@]}" | grep -qxF "$h" && order+="${order:+ }$h"
done
[ -n "$order" ] && [ "$order" != "$current" ] || exit 0
# The log names each host that changed state this run, with its latest probe.
why=""
while read -r h st b g since; do
  [ "$since" = "$now" ] || continue
  probe_row=$(awk -F'\t' -v h="$h" '$2 == h { print $9 ", upload " $4 " MB/s, connect " $3 " s" }' "$work/rows")
  if [ "$st" = demoted ]; then why+="; $h degraded after $b bad probes ($probe_row)"
  else why+="; $h healthy after $g good probes ($probe_row)"; fi
done < "$STREAKS"
[ -n "$why" ] || why="; the harness-only list was not the preference's order"
# shellcheck disable=SC2086
out=$("$LEASE" harness-only --on $order 2>&1 < /dev/null); rc=$?
echo "$ts order $current -> $order$why; lease.sh exit=$rc: $out" >> "$LOG"
exit 0
