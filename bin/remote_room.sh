#!/usr/bin/env bash
# Whether the remote card's host has room for one more harness job beside
# whatever else runs there. Prints "ok ram=<MiB> vram=<MiB>" and exits 0, or
# "busy <why>" and exits 1. A host that does not answer is busy.
#
#   remote_room.sh            a kit without rendering, a build or a test suite
#   remote_room.sh --render   a rendering kit
#   remote_room.sh --alone    a whole card for a cost read: no other process on it, host quiet
#   remote_room.sh --alone-card  a whole card for a run that times nothing: no other process on it, any load
#
# The host is shared: other sessions' memory is theirs, so a job goes there
# only while the free memory covers it with a margin.
H="$(cd "$(dirname "$0")/.." && pwd)"
host="${REMOTE_CARD_HOST:-$(grep -v '^[[:space:]]*$' "$H/state/locks/remote-card" 2>/dev/null | head -1)}"
[ -n "$host" ] || { echo "busy no remote card is named"; exit 1; }
: "${REMOTE_MIN_RAM_MIB:=16384}"
: "${REMOTE_MIN_VRAM_MIB:=6144}"
: "${REMOTE_MIN_VRAM_RENDER_MIB:=9216}"
need_vram=$REMOTE_MIN_VRAM_MIB
[ "${1:-}" = --render ] && need_vram=$REMOTE_MIN_VRAM_RENDER_MIB
# --alone: the card runs no compute process and the host's load is low, as a cost read needs.
# --alone-card: the card runs no compute process; the load is not read, since the run times nothing.
if [ "${1:-}" = --alone ] || [ "${1:-}" = --alone-card ]; then
  need_vram=$REMOTE_MIN_VRAM_RENDER_MIB
  read -r apps load cores < <(ssh -o BatchMode=yes -o ConnectTimeout=5 "$host" \
    'printf "%s " "$(nvidia-smi --query-compute-apps=pid --format=csv,noheader 2>/dev/null | grep -c .)"; printf "%s " "$(cut -d" " -f1 /proc/loadavg)"; nproc' 2>/dev/null)
  [[ "$apps" =~ ^[0-9]+$ ]] || { echo "busy $host does not answer"; exit 1; }
  if [ "$apps" -gt 0 ]; then echo "busy $apps compute process(es) on the card"; exit 1; fi
  # Quiet means load1 under half the host's cores, and never under REMOTE_ALONE_MAX_LOAD (6): the kit's
  # main thread is what a cost read times, and a build on the other half of a wide host leaves it a core.
  max_load=$(awk -v c="${cores:-0}" -v m="${REMOTE_ALONE_MAX_LOAD:-6}" 'BEGIN { h = c / 2; print (h > m ? h : m) }')
  if [ "${1:-}" = --alone ] && ! awk -v l="$load" -v m="$max_load" 'BEGIN { exit !(l + 0 < m + 0) }'; then echo "busy load1=$load (quiet under $max_load)"; exit 1; fi
fi
read -r ram vram < <(ssh -o BatchMode=yes -o ConnectTimeout=5 "$host" \
  'awk "/MemAvailable/ {printf \"%d \", \$2 / 1024}" /proc/meminfo; nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits | head -1' \
  2>/dev/null)
[[ "$ram" =~ ^[0-9]+$ ]] && [[ "$vram" =~ ^[0-9]+$ ]] || { echo "busy $host does not answer"; exit 1; }
if [ "$ram" -lt "$REMOTE_MIN_RAM_MIB" ]; then echo "busy ram=$ram MiB available, under $REMOTE_MIN_RAM_MIB"; exit 1; fi
if [ "$vram" -lt "$need_vram" ]; then echo "busy vram=$vram MiB free, under $need_vram"; exit 1; fi
echo "ok ram=$ram vram=$vram"
