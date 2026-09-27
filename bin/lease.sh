#!/usr/bin/env bash
# Atomic GPU lease. The ONLY sanctioned way to take, check or drop a lease; a bare
# write to a lease file is a bug.
#
# The card holds up to GPU_SLOTS kit sessions at once. Each slot is its own lease
# file, warm container and DDS domain, so sibling sessions never share a
# container or a ROS graph:
#   slot 1: state/locks/resource.lease    container worv-iter    domain 77
#   slot K: state/locks/resource.lease.K  container worv-iter-K  domain 76+K
# Kits booting together against one shader and cooking cache stall on it, so
# slot K>1 binds its own cache root /tmp/isaac-sim/cache-slotK, seeded once
# from slot 1's warm cache on the first acquire that finds it missing.
#
#   lease.sh acquire <issue> [domain]
#       -> ACQUIRED slot=K file=<lease> container=<name> domain=<id>
#          export MANURE_GATE_LEASE_FILE=<lease> WORV_ITER_CONTAINER=<name> ROS_DOMAIN_ID=<id>
#                 [WORV_ITER_CACHE_ROOT=<root>]   (slots 2+ only)
#          or BUSY <holders>; exit 0/1. A stated domain applies to slot 1 only.
#       Run every iter.sh, gate and kit command of the tick under that export line.
#   lease.sh acquire <issue> --all --render
#       -> the whole card, for a capture that cannot share it (a baseline). Without --render the call
#          is refused (exit 2): a run that renders nothing takes one slot. Each call reserves every
#          free slot for this tick, so siblings cannot take a slot it is waiting on, and prints
#          RESERVING held=<n>/<N> <holders> (exit 1) until the last sibling releases; then ACQUIRED
#          and the export line of the slot it runs in (the one it already held, else slot 1).
#          A reservation lapses ALL_RESERVE_TTL_S (600 s) after it is made: a single-slot acquire
#          may then take the slot. Once ACQUIRED, the other slots are held and never lapse.
#          An issue that has waited ALL_PRIORITY_AFTER_S (1800 s) for the whole card, across the
#          ticks that resume it, takes priority: its reservations stop lapsing, single-slot
#          acquires print BUSY priority ..., and other whole-card waiters print YIELDING (exit 1).
#          At most GPU_KITS_MAX (2) single-slot kits run at once; a third acquire prints
#          BUSY kit limit (exit 1) while a free slot file remains.
#          A tick re-acquiring the slot it holds while a whole-card issue has priority gets
#          YIELDED (exit 1): its slot is released and it joins the single-slot line. Ticks re-acquire
#          between runs, so the yield lands after the run in progress.
#       A single-slot acquire that finds no slot records its wait. The issue that has waited
#       longest, at least SLOT_PRIORITY_AFTER_S (1200 s) across the ticks that resume it, is first
#       in line: a slot that frees goes to it, and other ticks print BUSY queued behind ... (exit 1).
#   lease.sh acquire <issue> --render
#       -> one slot, as a plain acquire, for a kit that renders (a class B session with moments).
#          The slot is marked render. While another tick's rendering kit is live the call prints
#          BUSY render (exit 1) and records a render wait, which holds no place in the line ahead
#          of kits that can start, since two rendering kits overfill the card.
#   lease.sh acquire <issue> --device
#       -> a device doctest run (iter.sh build --test --device) beside the kits, no kit slot:
#          ACQUIRED device file=<lease> and an export line naming that file, or BUSY device <why>
#          (exit 1). Refused while a whole-card run holds the card, while another device run is
#          live, or when the card's used memory plus DEVICE_TEST_MIB would pass its size less
#          CARD_MARGIN_MIB. A whole-card acquire waits for a live device run to end.
#   lease.sh prioritize <issue> | prioritize --clear
#       -> the operator's override: the named issues, in the order named, go before every line on
#          their next acquire, whole-card or single-slot, and each is used up by that acquire.
#          Needs no tick.
#   lease.sh release <issue>   -> drops every slot this tick's pid holds
#   lease.sh status            -> every slot's holder and whether its pid is alive
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
: "${GPU_SLOTS:=3}"
# At most GPU_KITS_MAX single-slot kits share the card at once: a kit rendering
# the robot's cameras needs 5-7 GB of the 16 GB card. GPU_SLOTS still names
# every slot file, so a whole-card acquire sees every holder.
: "${GPU_KITS_MAX:=2}"
LOCKS="$H/state/locks"; K="$LOCKS/resource.lock"; G=/tmp/isaac-cppmig-gpu-runtime.lock
# An explicit holder (a task wrapper, or a script that holds the lease for its
# whole run) must be a live ancestor, never an arbitrary lease claimant.
holder="${CODEX_TASK_WRAPPER_PID:-${LEASE_HOLDER_PID:-}}"
if [[ -n "$holder" ]]; then
  pid="$holder"
  [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null || {
    echo "ERROR: task wrapper PID is not live" >&2; exit 2;
  }
  ancestor=$PPID
  while [[ "$ancestor" =~ ^[0-9]+$ ]] && (( ancestor > 1 )) && [[ "$ancestor" != "$pid" ]]; do
    ancestor=$(sed -n 's/.*) [A-Za-z] \([0-9][0-9]*\) .*/\1/p' "/proc/$ancestor/stat" 2>/dev/null)
  done
  [[ "$ancestor" == "$pid" ]] || {
    echo "ERROR: task wrapper PID is not an ancestor" >&2; exit 2;
  }
elif [ -x "$H/bin/tick-pid.sh" ] && [ "${1:-}" != status ] && [ "${1:-}" != prioritize ]; then
  # A lease belongs to the tick. From a loop detached from the tick's tree the
  # walk finds no tick, and recording the loop's own PID would leave a lease
  # that reads as dead the moment the loop ends, handing the slot to a sibling.
  pid=$("$H/bin/tick-pid.sh" 2>/dev/null) || {
    echo "ERROR: no claude -p tick above this shell; call lease.sh from the tick's own shell, not a detached loop, or set LEASE_HOLDER_PID to the live script that holds the lease" >&2
    exit 2
  }
else
  pid=$PPID
fi
slot_file() { [ "$1" -eq 1 ] && echo "$LOCKS/resource.lease" || echo "$LOCKS/resource.lease.$1"; }
# A device doctest run takes no kit slot: it holds this file, and the card's
# memory, read at the acquire, says whether it fits beside the running kits.
DEVICE_FILE="$LOCKS/device.lease"
: "${DEVICE_TEST_MIB:=3072}"
: "${CARD_MARGIN_MIB:=1024}"
card_mib() { nvidia-smi --query-gpu="memory.$1" --format=csv,noheader,nounits 2>/dev/null | head -1 | tr -dc '0-9'; }
device_holder_live() { [ -s "$DEVICE_FILE" ] && kill -0 "$(awk '{print $2}' "$DEVICE_FILE")" 2>/dev/null; }
# A whole-card reservation keeps a slot from single-slot ticks for at most
# ALL_RESERVE_TTL_S, counted from when the slot was first reserved. After that
# a single-slot acquire may take it, so a long run on another slot never
# leaves the reserved slots idle behind it.
: "${ALL_RESERVE_TTL_S:=600}"
lapsed() {
  local line since
  line=$(cat "$1" 2>/dev/null) || return 1
  [[ "$line" == *" reserved-for-all" ]] || return 1
  since=$(date -u -d "$(awk '{print $3}' <<<"$line")" +%s 2>/dev/null) || return 1
  [ $(( $(date -u +%s) - since )) -ge "$ALL_RESERVE_TTL_S" ] || return 1
  [ "$(awk '{print $1}' <<<"$line")" != "$(priority_issue)" ]
}
# Lapsing alone lets a steady stream of single-slot runs starve a whole-card
# tick. So an issue that has waited ALL_PRIORITY_AFTER_S for the whole card,
# across the ticks that resume it, takes priority, and the oldest such wait
# wins. Its reservations never lapse, single-slot acquires wait for it, and
# other whole-card waiters yield to it, so the card drains to it within the
# longest run in progress. all-wait.<issue> holds "<first wait epoch> <pid>
# <last call epoch>"; a wait counts only while its tick has asked within
# ALL_WAIT_FRESH_S, so a tick that stops asking for the card holds nothing: a
# waiter gone off to host work leaves the card to the others within two minutes.
: "${ALL_PRIORITY_AFTER_S:=1800}"
: "${ALL_WAIT_FRESH_S:=120}"
: "${SLOT_PRIORITY_AFTER_S:=1200}"
oldest_wait() {
  local kind="$1" after="$2" m first wpid last wkind best="" best_t="" now
  now=$(date -u +%s)
  for m in "$LOCKS"/"$kind".*; do
    [ -s "$m" ] || continue
    read -r first wpid last wkind < "$m"
    # A wait for a rendering slot cannot be served while another rendering kit
    # runs, so it holds no place ahead of kits that can start now.
    [ "${wkind:-}" = render ] && [ "$render_busy" = 1 ] && continue
    [[ "$first" =~ ^[0-9]+$ ]] && [[ "$last" =~ ^[0-9]+$ ]] && kill -0 "$wpid" 2>/dev/null || continue
    [ $(( now - last )) -le "$ALL_WAIT_FRESH_S" ] || continue
    [ $(( now - first )) -ge "$after" ] || continue
    if [ -z "$best" ] || [ "$first" -lt "$best_t" ]; then best=${m##*/"$kind".}; best_t=$first; fi
  done
  echo "$best ${best_t:-}"
}
# The two lines are served oldest first: a whole-card wait takes priority only
# while no single-slot issue in line has waited longer, so a three-hour
# single-slot wait is not passed by a thirty-minute whole-card one.
# The operator's override (`lease.sh prioritize`): issues named in order, each
# first in its line while its tick is asking, until its next acquire.
OPERATOR_FILE="$LOCKS/operator-priority"
operator_first() {
  local kind="$1" n m first wpid last wkind now
  [ -s "$OPERATOR_FILE" ] || return 0
  now=$(date -u +%s)
  for n in $(cat "$OPERATOR_FILE"); do
    m="$LOCKS/$kind.$n"
    [ -s "$m" ] || continue
    read -r first wpid last wkind < "$m"
    [ "${wkind:-}" = render ] && [ "$render_busy" = 1 ] && continue
    [[ "$last" =~ ^[0-9]+$ ]] && kill -0 "$wpid" 2>/dev/null || continue
    [ $(( now - last )) -le "$ALL_WAIT_FRESH_S" ] || continue
    echo "$n"; return 0
  done
}
operator_done() {
  [ -s "$OPERATOR_FILE" ] || return 0
  local rest; rest=$(tr ' ' '\n' < "$OPERATOR_FILE" | grep -vx "$1" | tr '\n' ' ')
  if [ -n "${rest// /}" ]; then echo "$rest" > "$OPERATOR_FILE"; else rm -f "$OPERATOR_FILE"; fi
}
priority_issue() {
  local card card_t line line_t op
  op=$(operator_first all-wait); [ -n "$op" ] && { echo "$op"; return; }
  [ -n "$(operator_first slot-wait)" ] && { echo ""; return; }
  read -r card card_t <<<"$(oldest_wait all-wait "$ALL_PRIORITY_AFTER_S")"
  [ -n "$card" ] || { echo ""; return; }
  read -r line line_t <<<"$(oldest_wait slot-wait "$SLOT_PRIORITY_AFTER_S")"
  if [ -n "$line" ] && [ "$line_t" -lt "$card_t" ]; then echo ""; return; fi
  echo "$card"
}
# Single-slot waits form a line of their own, so a freed slot goes to the tick
# that has asked longest rather than to whichever tick polls first. A wait
# counts from the issue's first refused acquire, across the ticks that resume
# it, while its tick keeps asking within ALL_WAIT_FRESH_S.
slot_first_issue() {
  local issue _t op
  op=$(operator_first slot-wait); [ -n "$op" ] && { echo "$op"; return; }
  read -r issue _t <<<"$(oldest_wait slot-wait "$SLOT_PRIORITY_AFTER_S")"; echo "$issue"
}
note_wait() {
  local m="$LOCKS/${2:-all-wait}.$1" first=""
  [ -s "$m" ] && read -r first _ < "$m"
  [[ "$first" =~ ^[0-9]+$ ]] || first=$(date -u +%s)
  echo "$first $pid $(date -u +%s)${3:+ $3}" > "$m"
}
# One rendering kit at a time: two of them overfill the card (about 8 GB each on
# a 16 GB card), while a rendering kit fits beside an identity kit (5.1 GB).
render_busy=0
render_live() {
  local s f hp
  for s in $(seq 1 "$GPU_SLOTS"); do
    f=$(slot_file $s); [ -s "$f" ] || continue
    [[ "$(cat "$f")" == *" render" ]] || continue
    hp=$(awk '{print $2}' "$f")
    [ "$hp" != "$pid" ] && kill -0 "$hp" 2>/dev/null && return 0
  done
  return 1
}
slot_container() { [ "$1" -eq 1 ] && echo worv-iter || echo "worv-iter-$1"; }
CACHE_BASE="${LEASE_CACHE_BASE:-/tmp/isaac-sim}"
SEED_IMAGE="${WORV_BUILDER_IMAGE:-worv-builder:isaac6}"
slot_cache() { [ "$1" -eq 1 ] || echo "$CACHE_BASE/cache-slot$1"; }
# The caches are root-owned (docker creates the bind sources), so the copy runs
# in a container. A partial copy is staged under .seeding and renamed, so a
# killed seed is retried rather than taken for a warm root.
seed_cache() {
  local root="$1" name
  [ -d "$root" ] && return 0
  name=$(basename "$root")
  docker run --rm --runtime=runc -v "$CACHE_BASE":/c --entrypoint bash "$SEED_IMAGE" -lc \
    "rm -rf /c/$name.seeding && mkdir -p /c/cache/kit /c/cache/ov /c/cache/glcache /c/cache/computecache \
     && cp -a /c/cache /c/$name.seeding && mv /c/$name.seeding /c/$name" >&2
}
case "${1:-}" in
  acquire)
    issue="${2:?issue number}"
    stated="${3:-}"
    if [ "$stated" = "--device" ]; then
      exec 8>"$K"; flock 8; exec 9>"$G"; flock 9
      for s in $(seq 1 "$GPU_SLOTS"); do
        f=$(slot_file $s)
        if [ -s "$f" ] && [[ "$(cat "$f")" == *" exclusive" || "$(cat "$f")" == *" held-for-all" ]] \
           && kill -0 "$(awk '{print $2}' "$f")" 2>/dev/null; then
          echo "BUSY device: a whole-card run holds the card [$(cat "$f")]"; exit 1
        fi
      done
      # A whole-card acquire waits while a device holder lives, so a holder that
      # takes the lane run after run would hold the card from a whole-card issue
      # with priority for its whole tick. The lane yields to that issue: a new
      # device run waits, and the holder asking again gives its lease up.
      first_issue=$(priority_issue)
      if [ -n "$first_issue" ] && [ "$first_issue" != "$issue" ]; then
        if device_holder_live && [ "$(awk '{print $2}' "$DEVICE_FILE")" = "$pid" ]; then
          rm -f "$DEVICE_FILE"
          echo "YIELDED device to whole-card issue $first_issue, which has priority; ask again once it has run"; exit 1
        fi
        echo "BUSY device: whole-card issue $first_issue has priority"; exit 1
      fi
      if device_holder_live && [ "$(awk '{print $2}' "$DEVICE_FILE")" != "$pid" ]; then
        echo "BUSY device: another device run is live [$(cat "$DEVICE_FILE")]"; exit 1
      fi
      used=$(card_mib used); total=$(card_mib total)
      if [ -z "$used" ] || [ -z "$total" ]; then echo "BUSY device: the card's memory could not be read"; exit 1; fi
      if [ $(( used + DEVICE_TEST_MIB )) -gt $(( total - CARD_MARGIN_MIB )) ]; then
        echo "BUSY device: ${used} MiB used + ${DEVICE_TEST_MIB} MiB for the tests would pass ${total} MiB less ${CARD_MARGIN_MIB}"; exit 1
      fi
      echo "$issue $pid $(date -u +%FT%TZ) domain=0 device" > "$DEVICE_FILE"
      echo "ACQUIRED device file=$DEVICE_FILE (card ${used}/${total} MiB used)"
      echo "export MANURE_GATE_LEASE_FILE=$DEVICE_FILE"
      exit 0
    fi
    if [ "$stated" = "--all" ]; then
      # The whole card is for runs that render: an identity or measurement run
      # renders nothing and takes one slot, so a whole-card acquire says it renders.
      if [ "${4:-}" != "--render" ]; then
        echo "REFUSED: the whole card is only for a run that renders; pass --render for one, else take one slot with a plain acquire" >&2
        exit 2
      fi
      exec 8>"$K"; flock 8; exec 9>"$G"; flock 9
      first_issue=$(priority_issue)
      if [ -n "$first_issue" ] && [ "$first_issue" != "$issue" ]; then
        # Yield to the issue with priority: drop this tick's reservations, keep the wait on record.
        for s in $(seq 1 "$GPU_SLOTS"); do
          f=$(slot_file $s)
          [ -s "$f" ] && [ "$(awk '{print $2}' "$f")" = "$pid" ] && [[ "$(cat "$f")" == *" reserved-for-all" ]] && rm -f "$f"
        done
        note_wait "$issue"
        echo "YIELDING to whole-card issue $first_issue, which has waited longest"; exit 1
      fi
      mine=0; primary=""; others=""
      for s in $(seq 1 "$GPU_SLOTS"); do
        f=$(slot_file $s); hp=""
        [ -s "$f" ] && hp=$(awk '{print $2}' "$f")
        if [ -n "$hp" ] && [ "$hp" != "$pid" ] && kill -0 "$hp" 2>/dev/null; then
          others="$others[$(cat "$f")] "; continue
        fi
        # Free, dead or already ours: reserve it for this tick.
        [ "$hp" = "$pid" ] && [ -z "$primary" ] && primary=$s
        [ "$hp" = "$pid" ] || echo "$issue $pid $(date -u +%FT%TZ) domain=$(( 76 + s )) reserved-for-all" > "$f"
        mine=$(( mine + 1 ))
      done
      if device_holder_live && [ "$(awk '{print $2}' "$DEVICE_FILE")" != "$pid" ]; then
        others="$others[$(cat "$DEVICE_FILE")] "
      fi
      if [ -n "$others" ]; then
        note_wait "$issue"
        echo "RESERVING held=$mine/$GPU_SLOTS waiting for $others"; exit 1
      fi
      rm -f "$LOCKS/all-wait.$issue" "$LOCKS/slot-wait.$issue"
      operator_done "$issue"
      [ -z "$primary" ] && primary=1
      f=$(slot_file $primary); c=$(slot_container $primary); d=$(( 76 + primary ))
      cr=$(slot_cache $primary)
      if [ -n "$cr" ] && ! seed_cache "$cr"; then
        echo "ERROR: could not seed $cr from $CACHE_BASE/cache" >&2; exit 1
      fi
      echo "$issue $pid $(date -u +%FT%TZ) domain=$d exclusive" > "$f"
      # The reservations become holds, which never lapse while the capture runs.
      for s in $(seq 1 "$GPU_SLOTS"); do
        [ "$s" -eq "$primary" ] && continue
        rf=$(slot_file $s)
        [ -s "$rf" ] && [ "$(awk '{print $2}' "$rf")" = "$pid" ] \
          && echo "$issue $pid $(date -u +%FT%TZ) domain=$(( 76 + s )) held-for-all" > "$rf"
      done
      echo "ACQUIRED slot=$primary file=$f container=$c domain=$d exclusive=all${cr:+ cache=$cr}"
      echo "export MANURE_GATE_LEASE_FILE=$f WORV_ITER_CONTAINER=$c ROS_DOMAIN_ID=$d${cr:+ WORV_ITER_CACHE_ROOT=$cr}"
      exit 0
    fi
    exec 8>"$K"; flock 8; exec 9>"$G"; flock 9
    render=""
    if [ "$stated" = "--render" ]; then render=1; stated=""; fi
    render_live && render_busy=1
    # This tick's own slot first, then a free or dead one, then a lapsed reservation.
    held=""; free=""; stale=""; own=""
    for s in $(seq 1 "$GPU_SLOTS"); do
      f=$(slot_file $s); hp=""
      [ -s "$f" ] && hp=$(awk '{print $2}' "$f")
      if [ -n "$hp" ] && kill -0 "$hp" 2>/dev/null; then
        if [ "$hp" = "$pid" ]; then held=$s; own=$s; break; fi
        [ -z "$stale" ] && lapsed "$f" && stale=$s
      else
        [ -z "$free" ] && free=$s
      fi
    done
    [ -z "$held" ] && held=${free:-$stale}
    # A holder renewing its slot between arms keeps the slot's render mark, however it asks.
    [ -n "$own" ] && [[ "$(cat "$(slot_file "$own")")" == *" render" ]] && render=1
    if [ -n "$render" ] && [ "$render_busy" = 1 ] && { [ -z "$own" ] || [ "$held" != "$own" ]; }; then
      note_wait "$issue" slot-wait render
      echo "BUSY render: one rendering kit at a time $(for s in $(seq 1 "$GPU_SLOTS"); do printf '[%s] ' "$(cat "$(slot_file $s)" 2>/dev/null)"; done)"; exit 1
    fi
    if [ -n "$held" ] && [ "$held" != "$own" ]; then
      kits=0
      for s in $(seq 1 "$GPU_SLOTS"); do
        f=$(slot_file $s); [ -s "$f" ] || continue
        [[ "$(cat "$f")" == *" reserved-for-all" ]] && continue
        hp=$(awk '{print $2}' "$f"); kill -0 "$hp" 2>/dev/null && kits=$(( kits + 1 ))
      done
      if [ "$kits" -ge "$GPU_KITS_MAX" ]; then
        note_wait "$issue" slot-wait
        echo "BUSY kit limit $kits/$GPU_KITS_MAX $(for s in $(seq 1 "$GPU_SLOTS"); do printf '[%s] ' "$(cat "$(slot_file $s)" 2>/dev/null)"; done)"; exit 1
      fi
    fi
    # A rendering holder renewing between runs hands its slot to a render wait the
    # operator named: one rendering kit fits the card, so without this a queue of
    # many arms keeps a critical-path render waiting through all of them.
    if [ -n "$render" ] && [ -n "$own" ] && [ "$held" = "$own" ]; then
      op=$(operator_first slot-wait)
      if [ -n "$op" ] && [ "$op" != "$issue" ] && [ "$(awk '{print $4}' "$LOCKS/slot-wait.$op" 2>/dev/null)" = render ]; then
        rm -f "$(slot_file "$own")"
        note_wait "$issue" slot-wait render
        echo "YIELDED render slot=$own to operator-priority issue $op: the lease is released; acquire again with --render once its session ends"; exit 1
      fi
    fi
    # A whole-card issue with priority gets every slot that frees, and a holder
    # re-acquiring between runs hands its slot over: an advisory yield let a
    # queue keep the card while the reserved slots sat idle.
    first_issue=$(priority_issue)
    if [ -n "$first_issue" ] && [ "$first_issue" != "$issue" ]; then
      if [ -n "$own" ] && [ "$held" = "$own" ]; then
        rm -f "$(slot_file "$own")"
        note_wait "$issue" slot-wait
        echo "YIELDED slot=$own to whole-card issue $first_issue, which has waited longest: the lease is released; acquire again once the card is back"; exit 1
      fi
      note_wait "$issue" slot-wait
      echo "BUSY priority to whole-card issue $first_issue"; exit 1
    fi
    # A slot that frees goes to the single-slot issue first in line.
    slot_first=$(slot_first_issue)
    if [ -n "$held" ] && [ "$held" != "$own" ] && [ -n "$slot_first" ] && [ "$slot_first" != "$issue" ]; then
      note_wait "$issue" slot-wait
      echo "BUSY queued behind single-slot issue $slot_first, which has waited longest"; exit 1
    fi
    if [ -z "$held" ]; then
      note_wait "$issue" slot-wait
      echo "BUSY $(for s in $(seq 1 "$GPU_SLOTS"); do printf '[%s] ' "$(cat "$(slot_file $s)" 2>/dev/null)"; done)"; exit 1
    fi
    f=$(slot_file $held); c=$(slot_container $held)
    if [ "$held" -eq 1 ] && [ -n "$stated" ]; then d=$stated; else d=$(( 76 + held )); fi
    cr=$(slot_cache $held)
    if [ -n "$cr" ] && ! seed_cache "$cr"; then
      echo "ERROR: could not seed $cr from $CACHE_BASE/cache" >&2; exit 1
    fi
    echo "$issue $pid $(date -u +%FT%TZ) domain=$d${render:+ render}" > "$f"
    rm -f "$LOCKS/slot-wait.$issue"
    [ "$held" != "$own" ] && operator_done "$issue"
    echo "ACQUIRED slot=$held file=$f container=$c domain=$d${cr:+ cache=$cr}"
    echo "export MANURE_GATE_LEASE_FILE=$f WORV_ITER_CONTAINER=$c ROS_DOMAIN_ID=$d${cr:+ WORV_ITER_CACHE_ROOT=$cr}"
    ;;
  prioritize)
    exec 8>"$K"; flock 8
    if [ "${2:-}" = "--clear" ]; then rm -f "$OPERATOR_FILE"; echo "CLEARED"; exit 0; fi
    n="${2:?issue number or --clear}"
    [[ "$n" =~ ^[0-9]+$ ]] || { echo "usage: lease.sh prioritize <issue> | --clear" >&2; exit 2; }
    { [ -s "$OPERATOR_FILE" ] && tr ' ' '\n' < "$OPERATOR_FILE" | grep -vx "$n" | tr '\n' ' '; echo "$n"; } | tr -s ' \n' ' ' | sed 's/^ //; s/ $//' > "$OPERATOR_FILE.new"
    mv -f "$OPERATOR_FILE.new" "$OPERATOR_FILE"
    echo "PRIORITIZED $(cat "$OPERATOR_FILE")"
    ;;
  release)
    exec 8>"$K"; flock 8
    out="NOT-HOLDER"
    for s in $(seq 1 "$GPU_SLOTS"); do
      f=$(slot_file $s)
      if [ -s "$f" ] && [ "$(awk '{print $2}' "$f")" = "$pid" ]; then rm -f "$f"; out="RELEASED slot=$s"; fi
    done
    if [ -s "$DEVICE_FILE" ] && [ "$(awk '{print $2}' "$DEVICE_FILE")" = "$pid" ]; then rm -f "$DEVICE_FILE"; out="RELEASED device"; fi
    echo "$out"
    ;;
  status)
    for s in $(seq 1 "$GPU_SLOTS"); do
      f=$(slot_file $s)
      if [ -s "$f" ]; then hp=$(awk '{print $2}' "$f"); kill -0 "$hp" 2>/dev/null && echo "slot $s HELD $(cat "$f") (alive)" || echo "slot $s ORPHAN $(cat "$f") (pid dead)"
      else echo "slot $s free"; fi
    done
    if [ -s "$DEVICE_FILE" ]; then
      device_holder_live && echo "device HELD $(cat "$DEVICE_FILE") (alive)" || echo "device ORPHAN $(cat "$DEVICE_FILE") (pid dead)"
    fi
    [ -s "$OPERATOR_FILE" ] && echo "operator priority: $(cat "$OPERATOR_FILE")"
    now=$(date -u +%s); card_first=$(priority_issue); line_first=$(slot_first_issue)
    for m in "$LOCKS"/all-wait.* "$LOCKS"/slot-wait.*; do
      [ -s "$m" ] || continue
      read -r first wpid last < "$m"
      [[ "$first" =~ ^[0-9]+$ ]] && [[ "$last" =~ ^[0-9]+$ ]] && kill -0 "$wpid" 2>/dev/null || continue
      [ $(( now - last )) -le "$ALL_WAIT_FRESH_S" ] || continue
      kind=${m##*/}; n=${kind#*.}; kind=${kind%%.*}
      tag=""
      [ "$kind" = all-wait ] && [ "$n" = "$card_first" ] && tag=" (priority)"
      [ "$kind" = slot-wait ] && [ "$n" = "$line_first" ] && tag=" (first in line)"
      label=single-slot; [ "$kind" = all-wait ] && label=whole-card
      echo "wait $label issue $n for $(( (now - first) / 60 )) min$tag"
    done
    ;;
  *) echo "usage: lease.sh acquire <issue> [domain|--all --render|--device] | release <issue> | prioritize <issue>|--clear | status" >&2; exit 2;;
esac
