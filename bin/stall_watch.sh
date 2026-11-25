#!/usr/bin/env bash
# Appends a line to state/stalls.log for each stall no GitHub event shows:
#   <UTC ISO> STALL lock_wait <lock> <detail>
#       a process has waited STALL_LOCK_MIN (15) minutes or more on a remote build or
#       suite lock (STALL_LOCK_GLOBS); the detail names the waiters and what the holder runs,
#       and when the holder waits on a remote-docker tree lock, that lock's holder and its copy
#   <UTC ISO> STALL gpu_idle <host> <detail>
#       a card read 0 % on every bin/host_health.sh probe of the last STALL_GPU_IDLE_MIN (30)
#       minutes while a process waits on one of those locks, or while the allocator's last
#       line in state/logs/allocator.log counts claimable work
#   <UTC ISO> STALL host_degraded <host> <detail>
#       bin/host_health.sh has moved the host last; once per demotion
# A kind and key is alerted again at most every STALL_REALERT_MIN (30) minutes.
# Scheduled from cron every 5 minutes. It reads local state, lslocks and /proc
# only, never a remote host, and exits 0.
set -u
HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# shellcheck source=/dev/null
[ -r "$HARNESS_HOME/config/harness.env" ] && . "$HARNESS_HOME/config/harness.env"
: "${HARNESS_STATE:=$HARNESS_HOME/state}"
export PATH="${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"
: "${STALL_LOCK_MIN:=15}"
: "${STALL_GPU_IDLE_MIN:=30}"
: "${STALL_REALERT_MIN:=30}"
: "${STALL_LOCK_GLOBS:=/tmp/worv-dev-build-remote*.lock /tmp/worv-fast-set-remote*.lock}"
: "${STALL_LOG_KEEP:=5000}"
S="$HARNESS_STATE"
OUT="$S/stalls.log"
SEEN="$S/stall_watch.seen"
mkdir -p "$S/locks" 2>/dev/null || exit 0
exec 9>"$S/locks/stall_watch.lock" || exit 0
flock -n 9 || exit 0
now=$(date +%s); iso=$(date -u +%FT%TZ)

# The last alert of each kind and key: "<kind> <key> <epoch> <token>" a line. A
# host_degraded token is the demotion's time, so each demotion alerts once.
declare -A last=() token=()
if [ -r "$SEEN" ]; then
  while read -r k key t tok _; do
    [[ -n "${key:-}" && "${t:-}" =~ ^[0-9]+$ ]] || continue
    last["$k $key"]=$t; token["$k $key"]=${tok:--}
  done < "$SEEN"
fi
alerted=0
alert() {  # <kind> <key> <detail> [token]
  local id="$1 $2" tok=${4:--}
  if [ "$1" = host_degraded ]; then
    [ "${token[$id]:-}" = "$tok" ] && return
  elif [ -n "${last[$id]:-}" ] && [ $(( now - ${last[$id]} )) -lt $(( STALL_REALERT_MIN * 60 )) ]; then
    return
  fi
  echo "$iso STALL $1 $2 $3" >> "$OUT"
  last[$id]=$now; token[$id]=$tok; alerted=1
}
clip() {  # <text> [width]
  local s=${1//$'\n'/ } w=${2:-160}
  [ "${#s}" -gt "$w" ] && s="${s:0:$(( w - 3 ))}..."
  printf '%s' "$s"
}

# Every process once: parent, age and command line.
declare -A ppid=() age=() args=() kids=()
while read -r p pp et a; do
  ppid[$p]=$pp; age[$p]=$et; args[$p]=$a; kids[$pp]+=" $p"
done < <(timeout 20 ps -e -ww -o pid=,ppid=,etimes=,args= 2>/dev/null)

# lock_wait. lslocks names a waiter by its BLOCKER column, and the holder by the pid
# that took the lock, which may be dead while a child that inherited the descriptor
# holds it; the processes holding it are the ones whose descriptor on the file
# carries a lock line in /proc/<pid>/fdinfo.
locks=$(timeout 20 lslocks -r -n -o PID,INODE,MAJ:MIN,BLOCKER 2>/dev/null)
declare -A wait_on=()
while read -r p i d b; do [ -n "${b:-}" ] && wait_on[$p]="$i $d"; done <<< "$locks"

# root_holder <file>: the holder of the file's lock whose parent does not hold it, if one is found.
root_holder() {
  local p fd
  local -A holds=()
  for p in $(timeout 20 fuser "$1" 2>/dev/null | tr -cs '0-9' ' '); do
    for fd in /proc/"$p"/fd/*; do
      [ "$(readlink "$fd" 2>/dev/null)" = "$1" ] && grep -q '^lock:' "/proc/$p/fdinfo/${fd##*/}" 2>/dev/null && holds[$p]=1
    done
  done
  for p in "${!holds[@]}"; do
    [ -n "${holds[${ppid[$p]:-0}]:-}" ] || { echo "$p"; return; }
  done
}
# descendants <pid>: deepest first, so the leaves, what it is doing now, lead.
descendants() {
  local level=$1 depth=0 found="" next q
  while [ -n "${level// /}" ] && [ "$depth" -lt 12 ]; do
    next=""
    for q in $level; do next+="${kids[$q]:-}"; done
    found="$next $found"; level=$next; depth=$(( depth + 1 ))
  done
  printf '%s' "$found"
}
# doing <pid> [width ages]: up to four of its descendants' command lines, with their ages when asked.
doing() {
  local out="" n=0 q c
  for q in $(descendants "$1"); do
    [[ "${args[$q]:-}" =~ ^(/usr/bin/)?sleep\ [0-9.]+$ ]] && continue
    c=$(clip "${args[$q]:-?}" "${2:-160}")
    [ -n "${3:-}" ] && c+=" ($(( ${age[$q]:-0} / 60 )) min)"
    [[ " | $out | " == *" | $c | "* ]] && continue
    out+="${out:+ | }$c"; n=$(( n + 1 ))
    [ "$n" -ge 4 ] && break
  done
  printf '%s' "$out"
}
# waited_lock <pid>: the file a waiting process waits on, found among its own descriptors.
waited_lock() {
  local want=${wait_on[$1]:-} fd f
  [ -n "$want" ] || return 0
  for fd in /proc/"$1"/fd/*; do
    f=$(readlink "$fd" 2>/dev/null) || continue
    [ -f "$f" ] && [ "$(stat -c '%i %Hd:%Ld' "$f" 2>/dev/null)" = "$want" ] && { printf '%s' "$f"; return 0; }
  done
}

waiters_total=0
for pattern in $STALL_LOCK_GLOBS; do
  for f in $pattern; do
    [ -f "$f" ] || continue
    id=$(stat -c '%i %Hd:%Ld' "$f" 2>/dev/null) || continue
    ino=${id% *}; dev=${id#* }
    waiters=(); locker=""
    while read -r p i d b; do
      [[ "$i" = "$ino" && "$d" = "$dev" ]] || continue
      if [ -n "${b:-}" ]; then waiters+=("$p"); else locker=$p; fi
    done <<< "$locks"
    [ "${#waiters[@]}" -gt 0 ] || continue
    waiters_total=$(( waiters_total + ${#waiters[@]} ))
    longest=0; over=0
    for p in "${waiters[@]}"; do
      a=${age[$p]:-0}
      [ "$a" -gt "$longest" ] && longest=$a
      [ "$a" -ge $(( STALL_LOCK_MIN * 60 )) ] && over=$(( over + 1 ))
    done
    [ "$over" -gt 0 ] || continue
    holder="holder not found"; acts=""; blocked=""
    p=$(root_holder "$f")
    if [ -n "$p" ]; then
      holder="holder pid $p, up $(( ${age[$p]:-0} / 60 )) min ($(clip "${args[$p]:-?}"))"
      acts=$(doing "$p")
      # A holder whose remote-docker shim waits on a tree lock (bin/remote-docker/docker) is
      # blocked by that lock's holder, usually a copy to a remote host: name it and its age.
      for q in $(descendants "$p"); do
        t=$(waited_lock "$q")
        [[ "$t" == */remote-docker-locks/*.lock ]] || continue
        th=$(root_holder "$t")
        if [ -n "$th" ]; then
          ta=$(doing "$th" 240 ages)
          blocked="; blocked: pid $q waits on tree lock $t, held by pid $th, up $(( ${age[$th]:-0} / 60 )) min ($(clip "${args[$th]:-?}"))${ta:+; it is doing: $ta}"
        else
          blocked="; blocked: pid $q waits on tree lock $t, holder not found"
        fi
        break
      done
    fi
    [ -n "$locker" ] && [ ! -e "/proc/$locker" ] && holder+=", lslocks names locker pid $locker (dead)"
    alert lock_wait "$f" "${#waiters[@]} waiting (pids ${waiters[*]}), $over of them ${STALL_LOCK_MIN}+ min, longest $(( longest / 60 )) min; $holder${acts:+; doing: $acts}$blocked"
  done
done

# gpu_idle: the work waiting is a lock waiter or the allocator's last claimable count.
alloc=$(grep 'claimable=' "$S/logs/allocator.log" 2>/dev/null | tail -1)
claimable=$(sed -n 's/.*claimable=\([0-9][0-9]*\).*/\1/p' <<< "$alloc")
if [ -r "$S/host_health.tsv" ] && { [ "$waiters_total" -gt 0 ] || [ "${claimable:-0}" -gt 0 ]; }; then
  cutoff=$(date -u -d "@$(( now - STALL_GPU_IDLE_MIN * 60 ))" +%FT%TZ)
  half=$(date -u -d "@$(( now - STALL_GPU_IDLE_MIN * 30 ))" +%FT%TZ)
  # A host counts when every row in the window reads 0 % and the rows reach back over half of it.
  while read -r h n mem; do
    alert gpu_idle "$h" "0 % on all $n probes of the last $STALL_GPU_IDLE_MIN min (memory used ${mem} MiB) while $waiters_total lock waiter(s) and allocator line '${alloc:-none}'"
  done < <(awk -F'\t' -v c="$cutoff" -v half="$half" '
      $1 == "time" || $1 < c { next }
      { n[$2]++; mem[$2] = $8; if (!($2 in first) || $1 < first[$2]) first[$2] = $1; if ($7 != "0") busy[$2] = 1 }
      END { for (h in n) if (!(h in busy) && first[h] <= half) print h, n[h], mem[h] }' "$S/host_health.tsv")
fi

# host_degraded: "<host> <ok|demoted> <bad> <good> <since>" a line, from bin/host_health.sh.
if [ -r "$S/host_health.streaks" ]; then
  while read -r h st b _ since _; do
    [ "$st" = demoted ] || continue
    probe=$(awk -F'\t' -v h="$h" '$2 == h { r = $9 ", upload " $4 " MB/s, connect " $3 " s" } END { print r }' "$S/host_health.tsv" 2>/dev/null)
    alert host_degraded "$h" "moved last at $(date -u -d "@${since:-0}" +%FT%TZ), $b bad probes in a row now; last probe: ${probe:-none}" "$since"
  done < "$S/host_health.streaks"
fi

if [ "$alerted" -eq 1 ]; then
  for id in "${!last[@]}"; do echo "$id ${last[$id]} ${token[$id]:--}"; done > "$SEEN.new" && mv "$SEEN.new" "$SEEN"
  if [ "$(wc -l < "$OUT")" -gt "$STALL_LOG_KEEP" ]; then
    tail -n "$STALL_LOG_KEEP" "$OUT" > "$OUT.new" && mv "$OUT.new" "$OUT"
  fi
fi
exit 0
