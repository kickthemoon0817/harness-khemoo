#!/usr/bin/env bash
# Runs a queue of scenario arms under one GPU lease that this script holds for its whole run
# (LEASE_HOLDER_PID=$$). It polls `lease.sh acquire` until granted or the deadline, brings each
# arm's tree up with `iter.sh up` and `iter.sh sidecar`, runs the arm, renews the lease before the
# next one, and on every exit stops its container and releases. Launch it detached and poll the
# status file: its last line is always `RELEASED <queue> <time>`.
#
#   arm_queue.sh --issue N --packet DIR --queue FILE [options]
#
# The queue file holds one arm a line ('|'-separated; blank and '#' lines are skipped):
#   tree|arm|scenario|extra_exts|kit_settings|seconds|outdir|flags
#     tree          worktree the arm runs from (its iter.sh and runner)
#     arm           the arm's output folder name, <outdir>/<arm>
#     scenario      a path, absolute or relative to the tree, or a bare name for
#                   tools/eval/scenarios/<name>.toml
#     extra_exts    extensions mounted beside --base-exts (comma-separated)
#     kit_settings  /path=value entries, each passed as --kit-setting (comma-separated)
#     seconds       wall seconds the arm needs before the deadline (default --arm-seconds)
#     outdir        where <arm> goes, absolute or relative to the packet (default the runs folder);
#                   a parent arm names evidence/parent-<base9> here
#     flags         runner flags, comma-separated: --no-moments,--through-sim-s,13
#
# Options:
#   --acquire-args "ARGS"  lease.sh acquire arguments after the issue: "--all-remote", "--render",
#                          "--all --render" (default none: one slot)
#   --deadline EPOCH|+S    no lease poll past EPOCH - --lease-margin, no arm past EPOCH - its seconds
#                          (default $DEADLINE; one of the two is required)
#   --runs SUBDIR          the packet's runs folder (default runs)
#   --collect P1,P2        container paths removed before each arm and copied to <arm>/ after it
#                          (files or folders: /tmp/census.csv,/tmp/worv_support_probe)
#   --want-host HOST       keep only a grant on this remote host (user@host, or local), and ask a
#                          whole-card grant for that host alone (LEASE_WANT_HOST); a grant
#                          elsewhere is released and the poll goes on (default any). Before each
#                          ask the runner reads <runs>/want-host, if present, in its place:
#                          bin/card_dispatch.sh writes `any` there to pull a pinned queue onto a
#                          free card
#   --bar-read             the queue reads the 150 ms bar, so its --want-host stays: no override
#                          moves it to another card
#   --lease-margin S       stop polling for a lease S seconds before the deadline (default 600)
#   --arm-seconds S        an arm's seconds when its line leaves them empty (default 540)
#   --arm-timeout S        the runner's timeout per arm (default 1200)
#   --base-exts "EXTS"     the extensions every arm mounts (default the scenario set below)
#   --runner NAME          scenario: tools/eval/scenario_run.py run --run-mode lockstep_sync (default)
#                          baseline: tools/eval/pbd_baseline.py run --arm engine --run-mode lockstep_sync
#   --base SHA             before each arm, note in status when origin's $ARM_QUEUE_HEAD_REF
#                          (default ai/manure-mpm) has moved off SHA
#   --settle S             before each arm, stop any kit in the container and wait S more seconds
#   --host-load            sample the host's load and the card's processes every 30 s into
#                          host-load.log and <arm>.load, and add load and lease lines to status
#   --nucleus-check        run bin/nucleus_ok.sh before each bring-up; a failure ends the queue
#
# A STOP file (<packet>/STOP or <runs>/STOP) is read while waiting for a lease and before each
# arm; a running arm finishes. An arm whose folder holds anything is skipped. A missing folder is
# claimed with mkdir before the arm; an empty one is taken as this tick's claim. A kit crash or
# GPU out-of-memory voids the arm (<arm>-void<n>) and runs it once more; a solver that could not
# start on the card writes <runs>/STOP. Every queue line is checked before the first acquire.
#
# Files: <runs>/status, lease.log, queue.log, up-<arm>.log, sidecar-<arm>.log, host-load.log;
# <outdir>/<arm>/, <arm>.out, <arm>-gpu.log, <arm>.load, and <outdir>/status for an arm that
# writes outside the runs folder (its lines also go to <runs>/status).
# Exit: 0 the queue ran to its end, 3 it ended early (STOP, deadline, lease lost, nucleus),
# 4 no lease was granted, 2 a usage or queue file error.
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
L="$H/bin/lease.sh"
POLL_S="${ARM_QUEUE_POLL_S:-30}"
WATCH_S="${ARM_QUEUE_WATCH_S:-5}"
SAMPLE_S="${ARM_QUEUE_SAMPLE_S:-30}"
HEAD_REF="${ARM_QUEUE_HEAD_REF:-ai/manure-mpm}"
CUBIN="${ARM_QUEUE_CUBIN:-extensions/env/worv.env.manure/data/wp_manure_mpm_kernels.sm120.cubin}"
VOID_PATTERNS=(-e OUT_OF_DEVICE_MEMORY -e "A crash has occurred" -e "could not start its solver")

usage() { sed -n '2,/^set -u$/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//'; }
die() { echo "arm_queue.sh: $*" >&2; exit 2; }
ts() { date -u +%FT%TZ; }
nap() { sleep "$1" & wait $!; }
is_uint() { [[ "$1" =~ ^[0-9]+$ ]]; }

issue="" packet="" qfile="" acquire_args="" deadline="${DEADLINE:-}" runs=runs collect="" want=any
lease_margin=600 arm_seconds=540 arm_timeout=1200 runner=scenario base="" settle=0 host_load="" nucleus="" bar_read=""
base_exts="worv.core.runtime worv.core.scene worv.core.utils worv.comm.base worv.comm.ros2 worv.env.manure worv.robots.catalog worv.robots.locomotion worv.robots.attachments worv.robots.teleport"
while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --host-load) host_load=1; shift; continue ;;
    --nucleus-check) nucleus=1; shift; continue ;;
    --bar-read) bar_read=1; shift; continue ;;
  esac
  [ $# -ge 2 ] || die "$1 needs a value (--help for usage)"
  case "$1" in
    --issue) issue=$2 ;;
    --packet) packet=$2 ;;
    --queue) qfile=$2 ;;
    --acquire-args) acquire_args=$2 ;;
    --deadline) deadline=$2 ;;
    --runs) runs=$2 ;;
    --collect) collect=$2 ;;
    --want-host) want=$2 ;;
    --lease-margin) lease_margin=$2 ;;
    --arm-seconds) arm_seconds=$2 ;;
    --arm-timeout) arm_timeout=$2 ;;
    --base-exts) base_exts=$2 ;;
    --runner) runner=$2 ;;
    --base) base=$2 ;;
    --settle) settle=$2 ;;
    *) die "unknown argument: $1 (--help for usage)" ;;
  esac
  shift 2
done

is_uint "$issue" || die "--issue needs an issue number"
[ -d "$packet" ] || die "--packet needs an existing folder${packet:+: $packet}"
packet=$(cd "$packet" && pwd)
[ -r "$qfile" ] || die "--queue needs a readable file${qfile:+: $qfile}"
qfile=$(cd "$(dirname "$qfile")" && pwd)/$(basename "$qfile")
qname=$(basename "$qfile")
[[ "$deadline" =~ ^\+[0-9]+$ ]] && deadline=$(( $(date +%s) + ${deadline#+} ))
is_uint "$deadline" || die "--deadline (or DEADLINE) needs an epoch second or +seconds"
for v in lease_margin arm_seconds arm_timeout settle; do is_uint "${!v}" || die "--${v//_/-} needs whole seconds"; done
[[ "$runs" =~ ^[A-Za-z0-9._-]+$ ]] || die "--runs needs a folder name"
read -ra acq <<< "$acquire_args"
read -ra base_list <<< "${base_exts//,/ }"
[ ${#base_list[@]} -gt 0 ] || die "--base-exts is empty"
IFS=, read -ra collect_list <<< "$collect"
for c in "${collect_list[@]}"; do [[ "$c" == /?* ]] || die "--collect needs absolute container paths: $c"; done
case "$runner" in
  scenario) runner_py=tools/eval/scenario_run.py; runner_args=(--run-mode lockstep_sync) ;;
  baseline) runner_py=tools/eval/pbd_baseline.py; runner_args=(--arm engine --run-mode lockstep_sync) ;;
  *) die "--runner is scenario or baseline" ;;
esac

# The whole file is checked before the lease is asked for, so a typo never holds a card.
Q_TREE=() Q_ARM=() Q_SCEN=() Q_EXTRA=() Q_SET=() Q_SECS=() Q_ODIR=() Q_FLAGS=()
lineno=0
while IFS= read -r line || [ -n "$line" ]; do
  lineno=$((lineno + 1)); line=${line%$'\r'}
  [[ "$line" =~ ^[[:space:]]*(#|$) ]] && continue
  pipes=${line//[^|]/}
  [ ${#pipes} -le 7 ] || die "$qname:$lineno has more than 8 fields"
  IFS='|' read -r tree arm scenario extra settings secs odir flags <<< "$line"
  if [ -z "$arm" ] || [ -z "$scenario" ]; then die "$qname:$lineno needs tree|arm|scenario"; fi
  [[ "$arm" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || die "$qname:$lineno: arm '$arm' is not a folder name"
  [ -x "$tree/tools/dev/iter.sh" ] || die "$qname:$lineno: $tree has no tools/dev/iter.sh"
  tree=$(cd "$tree" && pwd)
  [[ "$extra" != -* ]] || die "$qname:$lineno: field 4 is extra_exts and field 8 the flags ($extra)"
  [ -z "$secs" ] || is_uint "$secs" || die "$qname:$lineno: seconds '$secs' is not whole seconds"
  case "$odir" in ""|/*) ;; *) odir=$packet/$odir ;; esac
  Q_TREE+=("$tree") Q_ARM+=("$arm") Q_SCEN+=("$scenario") Q_EXTRA+=("$extra") Q_SET+=("$settings")
  Q_SECS+=("${secs:-$arm_seconds}") Q_ODIR+=("${odir%/}") Q_FLAGS+=("$flags")
done < "$qfile"
[ ${#Q_ARM[@]} -gt 0 ] || die "$qname holds no arm"

OUT=$packet/$runs; ST=$OUT/status; LEASE_LOG=$OUT/lease.log
mkdir -p "$OUT"
echo "arm_queue.sh: holder $$, status in $ST"
exec >> "$OUT/queue.log" 2>&1
export LEASE_HOLDER_PID=$$ MANURE_GATE_LEASE_ISSUE=$issue

LEASE_EXPORT="" SIDECAR="" current="" up_tree="" need_renew="" arm_pid="" gpu_pid="" load_pid="" host_pid=""
declare -A pre_lease=()

stopped() { [ -e "$packet/STOP" ] || [ -e "$OUT/STOP" ]; }
kill_tree() { [ -n "$1" ] || return 0; pkill -P "$1" 2>/dev/null; kill "$1" 2>/dev/null; }
host_of() { case "${DOCKER_HOST:-}" in ssh://*) echo "${DOCKER_HOST#ssh://}" ;; *) echo local ;; esac; }
load1() { cut -d' ' -f1 /proc/loadavg; }
gpu_used() { nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits 2>/dev/null | head -1; }
rapps() { nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader 2>/dev/null | tr '\n' ';'; }
hload() {
  if [ "$(host_of)" = local ]; then cut -d' ' -f1-3 /proc/loadavg
  else ssh -o BatchMode=yes -o ConnectTimeout=10 "$(host_of)" cat /proc/loadavg 2>/dev/null | cut -d' ' -f1-3; fi
}
# Arm lines go to the runs folder's status, and to <outdir>/status when the arm writes elsewhere.
arm_st() { echo "$*" >> "$ST"; [ "$odir" = "$OUT" ] || echo "$*" >> "$odir/status"; }

# A new grant may name another host: the variables the last export line set (PATH and
# DOCKER_HOST among them) go back to their values before any lease, then the new line applies.
apply_export() {
  local name kv kvs
  for name in "${!pre_lease[@]}"; do
    if [ "${pre_lease[$name]}" = $'\x01unset' ]; then unset "$name"; else export "$name=${pre_lease[$name]}"; fi
  done
  read -ra kvs <<< "${1#export }"
  for kv in "${kvs[@]}"; do
    name=${kv%%=*}
    [ -n "${pre_lease[$name]+x}" ] || pre_lease[$name]=${!name-$'\x01unset'}
  done
  eval "$1"
}

take_grant() {
  LEASE_EXPORT=$(printf '%s\n' "$1" | grep -m1 '^export ')
  apply_export "$LEASE_EXPORT"
  export LEASE_EXPORT
  SIDECAR="${WORV_ITER_CONTAINER:-}-ros2cli"
  current="" need_renew=""
  echo "granted: $LEASE_EXPORT $(ts)" >> "$ST"
  if [ -n "$host_load" ]; then
    kill_tree "$host_pid"
    ( while :; do echo "$(ts) $(hload);$(rapps)"; nap "$SAMPLE_S"; done ) >> "$OUT/host-load.log" 2>&1 &
    host_pid=$!
  fi
}

# Polls until a grant on the wanted host: 0 granted, 1 STOP, 2 the deadline. $1 is appended to
# the status line ("" at the start, " before <arm>" after a yield).
poll_lease() {
  local out rc host
  while :; do
    if stopped; then echo "STOP while waiting$1 $(ts)" >> "$ST"; return 1; fi
    if [ -z "$bar_read" ] && [ -s "$OUT/want-host" ]; then
      read -r w _ < "$OUT/want-host"
      if [ -n "$w" ] && [ "$w" != "$want" ]; then echo "$(ts) want-host override: $want -> $w" >> "$LEASE_LOG"; want=$w; fi
    fi
    if [ "$want" = any ] || [ "$want" = local ]; then out=$("$L" acquire "$issue" "${acq[@]}" 2>&1); rc=$?
    else out=$(LEASE_WANT_HOST=$want "$L" acquire "$issue" "${acq[@]}" 2>&1); rc=$?; fi
    echo "$(ts) $out" >> "$LEASE_LOG"
    if [ $rc -eq 0 ] && [[ "$out" == *ACQUIRED* ]] && printf '%s\n' "$out" | grep -q '^export '; then
      host=$(printf '%s\n' "$out" | grep -o 'remote=[^ ]*' | head -1); host=${host#remote=}
      if [ "$want" = any ] || [ "${host:-local}" = "$want" ]; then take_grant "$out"; return 0; fi
      echo "$(ts) grant on ${host:-local}, not $want: released" >> "$LEASE_LOG"
      "$L" release "$issue" >> "$LEASE_LOG" 2>&1
    fi
    if [ $(( $(date +%s) + lease_margin )) -gt "$deadline" ]; then echo "NO-LEASE by deadline$1 $(ts)" >> "$ST"; return 2; fi
    nap "$POLL_S"
  done
}

down_current() {
  [ -n "$up_tree" ] || return 0
  (cd "$up_tree" && tools/dev/iter.sh down) >> "$LEASE_LOG" 2>&1
  up_tree="" current=""
}

# Re-acquire between runs, never during one: a refusal (YIELDED) has already released the
# slot, so the container stops at once and the queue joins the line again.
renew() {
  [ -n "$need_renew" ] || return 0
  local r rc e
  r=$("$L" acquire "$issue" "${acq[@]}" 2>&1); rc=$?
  echo "$(ts) renew before $1: $r" >> "$LEASE_LOG"
  if [ $rc -eq 0 ] && [[ "$r" == *ACQUIRED* ]]; then
    e=$(printf '%s\n' "$r" | grep -m1 '^export ')
    if [ -n "$e" ] && [ "$e" != "$LEASE_EXPORT" ]; then down_current; take_grant "$r"; fi
    need_renew=""
    return 0
  fi
  echo "YIELD before $1 $(ts)" >> "$ST"
  down_current
  poll_lease " before $1"
}

# shellcheck disable=SC2317  # reached through the EXIT trap
cleanup() {
  local rc=$?
  trap - EXIT INT TERM HUP
  if [ -n "$arm_pid" ]; then
    kill_tree "$arm_pid"
    [ -n "$SIDECAR" ] && docker exec "$SIDECAR" pkill -f /tmp/worv_eval_scenario >/dev/null 2>&1
  fi
  kill_tree "$gpu_pid"; kill_tree "$load_pid"; kill_tree "$host_pid"
  down_current
  "$L" release "$issue" >> "$LEASE_LOG" 2>&1
  echo "RELEASED $qname $(ts)" >> "$ST"
  exit "$rc"
}

# 0 ran or skipped, 1 bring-up failed or void (run it once more), 3 end the queue.
run_arm() {
  local i=$1
  local tree=${Q_TREE[$i]} arm=${Q_ARM[$i]} scenario=${Q_SCEN[$i]} extra=${Q_EXTRA[$i]}
  local settings=${Q_SET[$i]} need=${Q_SECS[$i]} odir=${Q_ODIR[$i]:-$OUT} flags=${Q_FLAGS[$i]}
  local adir=$odir/$arm aout=$odir/$arm.out
  local -a extra_list set_list flag_list ks=() exts
  IFS=, read -ra extra_list <<< "$extra"
  IFS=, read -ra set_list <<< "$settings"
  IFS=, read -ra flag_list <<< "$flags"
  exts=("${base_list[@]}" "${extra_list[@]}")

  if stopped; then echo "STOP before $arm $(ts)" >> "$ST"; return 3; fi
  mkdir -p "$odir"
  if [ -n "$(ls -A "$adir" 2>/dev/null)" ]; then arm_st "$arm skipped: $adir exists $(ts)"; return 0; fi
  if [ $(( $(date +%s) + need )) -gt "$deadline" ]; then echo "DEADLINE before $arm $(ts)" >> "$ST"; return 3; fi
  renew "$arm" || return 3
  # A parent arm's folder is shared across ticks: mkdir is the claim, and an empty folder is
  # the claim this tick made before it started the queue.
  if [ ! -e "$adir" ] && ! mkdir "$adir" 2>/dev/null; then arm_st "$arm skipped: $adir claimed by another queue $(ts)"; return 0; fi

  local key="$tree|$extra"
  if [ "$key" != "$current" ]; then
    if [ -n "$nucleus" ] && ! "$H/bin/nucleus_ok.sh" >> "$LEASE_LOG" 2>&1; then
      echo "nucleus not ok before $arm $(ts)" >> "$ST"; rmdir "$adir" 2>/dev/null; return 3
    fi
    up_tree=$tree
    if ! (cd "$tree" && tools/dev/iter.sh up "${exts[@]}") > "$OUT/up-$arm.log" 2>&1; then
      arm_st "$arm up failed $(ts)"; current=""; rmdir "$adir" 2>/dev/null; return 1
    fi
    if ! (cd "$tree" && tools/dev/iter.sh sidecar) > "$OUT/sidecar-$arm.log" 2>&1; then
      arm_st "$arm sidecar failed $(ts)"; current=""; rmdir "$adir" 2>/dev/null; return 1
    fi
    current=$key
  fi
  if [ "$(docker inspect -f '{{.State.Running}}' "$SIDECAR" 2>/dev/null)" != true ]; then
    arm_st "$arm no sidecar $(ts)"; current=""; rmdir "$adir" 2>/dev/null; return 1
  fi
  docker exec "$SIDECAR" pkill -f /tmp/worv_eval_scenario >/dev/null 2>&1
  if [ "$settle" -gt 0 ]; then
    docker exec "$WORV_ITER_CONTAINER" pkill -f "^/isaac-sim/kit/kit" >/dev/null 2>&1
    for _ in $(seq 1 30); do
      docker exec "$WORV_ITER_CONTAINER" pgrep -f "^/isaac-sim/kit/kit" >/dev/null 2>&1 || break; nap 2
    done
    nap "$settle"
  fi
  local before c s spec
  before=$(docker exec "$WORV_ITER_CONTAINER" grep -m1 "Logging to file" /tmp/kit.log 2>/dev/null)
  for s in "${set_list[@]}"; do ks+=(--kit-setting "$s"); done
  for c in "${collect_list[@]}"; do docker exec "$WORV_ITER_CONTAINER" rm -rf "$c" >/dev/null 2>&1; done
  case "$scenario" in /*|*/*|*.toml) spec=$scenario ;; *) spec=tools/eval/scenarios/$scenario.toml ;; esac
  if [ -n "$base" ]; then
    local head
    head=$(timeout 60 git -C "$tree" ls-remote origin "refs/heads/$HEAD_REF" 2>/dev/null | cut -c1-9)
    [ -n "$head" ] && [ "$head" != "${base:0:9}" ] && echo "head is $head before $arm; the pair stays on ${base:0:9} $(ts)" >> "$ST"
  fi
  local extra_fields
  extra_fields="host=$(host_of)"
  [ -f "$tree/$CUBIN" ] && extra_fields+=" cubin=$(sha256sum "$tree/$CUBIN" | cut -c1-12)"
  if [ -n "$host_load" ]; then
    extra_fields+=" load=$(hload | tr ' ' ,) card_apps=[$(rapps)]"
    "$L" status 2>/dev/null | grep -E '^slot [0-9]+ ' | sed "s/^/$arm lease /" >> "$ST"
  fi
  arm_st "$arm start $(ts) load1=$(load1) gpu_used_mib=$(gpu_used) tree=$tree commit=$(git -C "$tree" rev-parse HEAD 2>/dev/null) slot=${WORV_ITER_CONTAINER:-} domain=${ROS_DOMAIN_ID:-} scenario=$scenario mounts=[${exts[*]}] setting=[$settings] flags=[$flags] $extra_fields"
  # The runner refuses an existing --out folder, so the claim folder goes just before it starts.
  rmdir "$adir" 2>/dev/null
  (cd "$tree" && exec timeout "$arm_timeout" python3 "$runner_py" run "$spec" "${runner_args[@]}" "${flag_list[@]}" "${ks[@]}" --out "$adir") > "$aout" 2>&1 &
  arm_pid=$!
  need_renew=1
  ( while kill -0 "$arm_pid" 2>/dev/null; do echo "$(ts) card_used_mib=$(gpu_used)"; nap "$WATCH_S"; done ) > "${aout%.out}-gpu.log" 2>&1 &
  gpu_pid=$!
  if [ -n "$host_load" ]; then
    ( while nap "$SAMPLE_S"; do echo "$(date -u +%T) load=$(hload) gpu=$(gpu_used) apps=[$(rapps)]"; done ) > "$odir/$arm.load" 2>&1 &
    load_pid=$!
  fi
  local void="" solver="" now_head
  while kill -0 "$arm_pid" 2>/dev/null; do
    nap "$WATCH_S"
    now_head=$(docker exec "$WORV_ITER_CONTAINER" grep -m1 "Logging to file" /tmp/kit.log 2>/dev/null)
    if [ -n "$now_head" ] && [ "$now_head" != "$before" ] &&
       docker exec "$WORV_ITER_CONTAINER" grep -q "${VOID_PATTERNS[@]}" /tmp/kit.log 2>/dev/null; then
      void=1
      docker exec "$WORV_ITER_CONTAINER" grep -q "could not start its solver" /tmp/kit.log 2>/dev/null && solver=1
      nap 3; kill_tree "$arm_pid"
      docker exec "$SIDECAR" pkill -f /tmp/worv_eval_scenario >/dev/null 2>&1
      break
    fi
  done
  wait "$arm_pid"; local rc=$?
  arm_pid=""
  kill_tree "$gpu_pid"; kill_tree "$load_pid"; gpu_pid="" load_pid=""
  if [ -n "$void" ]; then
    local n=1
    while [ -e "$adir-void$n" ] || [ -e "$odir/$arm-void$n.out" ]; do n=$((n + 1)); done
    [ -e "$adir" ] && mv "$adir" "$adir-void$n"
    [ -e "$aout" ] && mv "$aout" "$odir/$arm-void$n.out"
    arm_st "$arm void$n: kit GPU out-of-memory or crash, gpu_used_mib=$(gpu_used) $(ts)"
    # A solver that could not start on the card ran the CPU reference: the card has no room for
    # this profile beside the other kits, so the queue stops rather than retry into the same card.
    if [ -n "$solver" ] || grep -q "could not start its solver" "$adir-void$n/kit.log" 2>/dev/null; then
      echo "STOP: $arm could not start its solver on the card $(ts)" > "$OUT/STOP"
    fi
    return 1
  fi
  if [ -d "$adir" ]; then
    for c in "${collect_list[@]}"; do
      docker exec "$WORV_ITER_CONTAINER" test -e "$c" 2>/dev/null &&
        docker cp "$WORV_ITER_CONTAINER:$c" "$adir/$(basename "$c")" >> "$ST" 2>&1
    done
  fi
  local end_fields=""
  [ -n "$host_load" ] && end_fields=" load=$(hload | tr ' ' ,)"
  arm_st "$arm rc=$rc end $(ts) load1=$(load1)$end_fields"
  return 0
}

trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
trap 'exit 129' HUP
echo "holder $$ start $(ts) queue=$qfile want=$want bar=${bar_read:-0} deadline=$deadline" >> "$LEASE_LOG"
poll_lease "" || exit 4

ended_early=""
for i in "${!Q_ARM[@]}"; do
  run_arm "$i"; r=$?
  if [ $r -eq 1 ]; then run_arm "$i"; r=$?; fi
  if [ $r -eq 3 ]; then ended_early=1; break; fi
done
echo "ALLDONE $qname $(ts)" >> "$ST"
[ -z "$ended_early" ] || exit 3
exit 0
