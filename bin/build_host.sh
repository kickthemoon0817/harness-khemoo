#!/usr/bin/env bash
# Prints the export line a tick evals before `tools/dev/iter.sh build`, naming the host to compile
# on, that host's compile lock and the builder image there:
#   export DOCKER_HOST=ssh://<host> WORV_DEV_BUILD_LOCK=<lock dir>/worv-dev-build-remote-<name>.lock WORV_BUILDER_IMAGE=worv-builder:isaac6
# With state/local-cpu-lane present (owner, 2026-11-26: this host's CPU takes compiles, never its
# card), this host is a lane after the remote ones: it takes a compile when no remote lane is
# free and unwaited, under the host's own dev build lock (shared with the owner's own builds) and
# the builder image the file names:
#   export DOCKER_HOST=unix:///var/run/docker.sock WORV_DEV_BUILD_LOCK=<lock dir>/worv-dev-build.lock WORV_BUILDER_IMAGE=<image>
# Each host compiles one build at a time under its lock. Hosts are read in the harness order
# (state/locks/harness-first-remote-only). A host that a live cost read holds whole is left out,
# unless every host is; a hold marked no-cost does not count. A host a waiting cost read asks for
# (state/locks/cost-wait.<issue>, refreshed within COST_WAIT_FRESH_S, 120) is left out too, so it
# can go quiet; a wait for any card keeps the unheld host with the lowest load free of compiles.
# Only when every host is left out are they all candidates again. The first host whose lock is free
# wins. When every lock is taken, the host with the fewest waiters wins, the earlier on a tie.
#
#   BUILD_LOCK_NAMES  "<user@host>=<name> …" (default: the two harness hosts' names)
#   BUILD_LOCK_DIR    where the locks live (default /tmp)
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
S="${HARNESS_STATE:-$H/state}"
: "${BUILD_LOCK_NAMES:=worv@10.50.1.6=5090 worv@worv-worker-03.local=w03}"
: "${BUILD_LOCK_DIR:=/tmp}"
mapfile -t hosts < <(grep -v '^[[:space:]]*$' "$S/locks/harness-first-remote-only" 2>/dev/null)
[ "${#hosts[@]}" -gt 0 ] || { echo "build_host: no harness host is named" >&2; exit 1; }
LOCAL_LANE_FILE="$S/local-cpu-lane"
local_image=""
# BUILD_HOST_REMOTE_ONLY=1 leaves the local lane out (the scheduled suites run on remote hosts only).
[ -z "${BUILD_HOST_REMOTE_ONLY:-}" ] && [ -s "$LOCAL_LANE_FILE" ] && read -r local_image _ < "$LOCAL_LANE_FILE"
lock_of() {
  local h=$1 pair name=""
  [ "$h" = local ] && { echo "$BUILD_LOCK_DIR/worv-dev-build.lock"; return; }
  for pair in $BUILD_LOCK_NAMES; do [ "${pair%%=*}" = "$h" ] && name=${pair#*=}; done
  [ -n "$name" ] || name=$(printf '%s' "${h#*@}" | tr -c 'A-Za-z0-9' '-')
  echo "$BUILD_LOCK_DIR/worv-dev-build-remote-$name.lock"
}
held=$("$H/bin/lease.sh" status < /dev/null 2>/dev/null | awk '
    /^remote card: / { host = $3; split(substr($5, 1, length($5) - 1), r, "-"); lo[host] = r[1]; hi[host] = r[2] }
    /^slot [0-9]+ HELD / && / remote-all / && !/ no-cost / && /\(alive\)$/ { held[$2] = 1 }
    END { for (h in lo) for (s in held) if (s + 0 >= lo[h] && s + 0 <= hi[h]) { print h; break } }')
waited=$("$H/bin/cost_wait_hosts.sh" | awk '{ print $1 }')
cands=()
for h in "${hosts[@]}"; do
  grep -qxF "$h" <<< "$held" && continue
  grep -qxF "$h" <<< "$waited" && continue
  cands+=("$h")
done
if [ -z "$local_image" ]; then
  [ "${#cands[@]}" -gt 0 ] || for h in "${hosts[@]}"; do grep -qxF "$h" <<< "$held" || cands+=("$h"); done
  [ "${#cands[@]}" -gt 0 ] || cands=("${hosts[@]}")
else
  cands+=(local)
fi
best="" best_wait=""
for h in "${cands[@]}"; do
  l=$(lock_of "$h")
  if flock -n "$l" true 2>/dev/null; then best=$h; break; fi
  w=$(lslocks -n -o PATH,BLOCKER 2>/dev/null | awk -v p="$l" '$1 == p && $2 != "" { n++ } END { print n + 0 }')
  if [ -z "$best_wait" ] || [ "$w" -lt "$best_wait" ]; then best=$h; best_wait=$w; fi
done
if [ "$best" = local ]; then
  echo "export DOCKER_HOST=unix:///var/run/docker.sock WORV_DEV_BUILD_LOCK=$(lock_of local) WORV_BUILDER_IMAGE=$local_image WORV_LOCAL_LANE=1"
else
  echo "export DOCKER_HOST=ssh://$best WORV_DEV_BUILD_LOCK=$(lock_of "$best") WORV_BUILDER_IMAGE=worv-builder:isaac6 WORV_LOCAL_LANE="
fi
