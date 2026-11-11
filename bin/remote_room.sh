#!/usr/bin/env bash
# Whether the remote card's host has room for one more harness job beside
# whatever else runs there. Prints "ok ram=<MiB> vram=<MiB>" and exits 0, or
# "busy <why>" and exits 1. A host that does not answer is busy.
#
#   remote_room.sh            a kit without rendering, a build or a test suite
#   remote_room.sh --render   a rendering kit
#
# The host is shared: other sessions' memory is theirs, so a job goes there
# only while the free memory covers it with a margin.
H="$(cd "$(dirname "$0")/.." && pwd)"
host="${REMOTE_CARD_HOST:-$(cat "$H/state/locks/remote-card" 2>/dev/null)}"
[ -n "$host" ] || { echo "busy no remote card is named"; exit 1; }
: "${REMOTE_MIN_RAM_MIB:=16384}"
: "${REMOTE_MIN_VRAM_MIB:=6144}"
: "${REMOTE_MIN_VRAM_RENDER_MIB:=9216}"
need_vram=$REMOTE_MIN_VRAM_MIB
[ "${1:-}" = --render ] && need_vram=$REMOTE_MIN_VRAM_RENDER_MIB
read -r ram vram < <(ssh -o BatchMode=yes -o ConnectTimeout=5 "$host" \
  'awk "/MemAvailable/ {printf \"%d \", \$2 / 1024}" /proc/meminfo; nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits | head -1' \
  2>/dev/null)
[[ "$ram" =~ ^[0-9]+$ ]] && [[ "$vram" =~ ^[0-9]+$ ]] || { echo "busy $host does not answer"; exit 1; }
if [ "$ram" -lt "$REMOTE_MIN_RAM_MIB" ]; then echo "busy ram=$ram MiB available, under $REMOTE_MIN_RAM_MIB"; exit 1; fi
if [ "$vram" -lt "$need_vram" ]; then echo "busy vram=$vram MiB free, under $need_vram"; exit 1; fi
echo "ok ram=$ram vram=$vram"
