#!/usr/bin/env bash
# Cached same-session base control for manure_perf_ab.sh, keyed by the base
# commit the branch is measured against. One control arm per base commit per
# day is enough: the arm is 5 min and the build it needs is 10-15; a cached
# control is valid while it is under MAX_AGE_H old and the fix arm's idle p50
# lands within IDLE_TOL_MS of it (the host-state check that made the control
# same-session in the first place).
#
#   perf_control.sh dir  [base-sha]   -> prints the cached control dir, or nothing
#   perf_control.sh new  [base-sha]   -> prints a fresh dir to run the control arm into
#   perf_control.sh tree [base-sha]   -> prints the shared built base worktree, creating it
#                                        (build + AOT) when absent; the caller holds the lease
#   perf_control.sh check <fix-results.md> [base-sha] -> exit 0 when the cache is usable for
#                                        that fix arm (age + idle p50 within tolerance)
set -euo pipefail
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
REPO=/home/khemoo/tmp_workspace/isaac_sim
WT_ROOT=/home/khemoo/tmp_workspace/ai-worktrees
: "${MAX_AGE_H:=24}" "${IDLE_TOL_MS:=0.5}"
mode="${1:?dir|new|tree|check}"; shift
case "$mode" in check) fix="${1:?fix RESULTS.md}"; shift;; esac
sha="${1:-$(git -C "$REPO" rev-parse --short=8 origin/ai/manure-mpm)}"
cache="$H/state/perf-control/$sha"
case "$mode" in
  dir)  [ -s "$cache/RESULTS.md" ] && echo "$cache" || true ;;
  new)  mkdir -p "$cache"; echo "$cache" ;;
  tree)
    wt="$WT_ROOT/base-$sha"
    if [ ! -d "$wt" ]; then
      git -C "$REPO" fetch -q origin ai/manure-mpm
      git -C "$REPO" worktree add -q --detach "$wt" "$sha"
      cp "$REPO/env/nucleus.env" "$wt/env/" 2>/dev/null || true
      ( cd "$wt" && WORV_IMAGE="${WORV_IMAGE:-docker-pri.maum.ai:443/worv/isaac_sim:3.0.19-manure-mpm}" \
          tools/dev/iter.sh build worv.comm.base worv.comm.ros2 worv.env.manure >"$wt/.perf-control-build.log" 2>&1 )
      rm -f "$wt/extensions/env/worv.env.manure/data/"*.cubin "$wt/extensions/env/worv.env.manure/data/"*.ptx
      docker run --rm -u "$(id -u):$(id -g)" --runtime=runc \
        -v "$wt/extensions/core/worv.core.warp_compat:/exts/worv.core.warp_compat:ro" \
        -v "$wt/extensions/env/worv.env.manure:/exts/worv.env.manure" \
        -v "$H/bin/aot_manure.py:/tmp/aot_manure.py:ro" \
        -e WORV_EXTSUSER=/exts -e CUDA_VISIBLE_DEVICES= -e HOME=/tmp/aothome --entrypoint bash \
        worv-builder:isaac6 -lc 'mkdir -p /tmp/aothome/.cache && /opt/warp-aot/bin/python /tmp/aot_manure.py' >"$wt/.perf-control-aot.log" 2>&1
    fi
    echo "$wt" ;;
  check)
    [ -s "$cache/RESULTS.md" ] || { echo "no cached control for $sha" >&2; exit 1; }
    age_h=$(( ( $(date +%s) - $(stat -c %Y "$cache/RESULTS.md") ) / 3600 ))
    [ "$age_h" -le "$MAX_AGE_H" ] || { echo "cached control is ${age_h} h old (max $MAX_AGE_H)" >&2; exit 1; }
    ci=$(grep -E '^\| idle ' "$cache/RESULTS.md" | head -1 | awk -F'|' '{gsub(/[+ ]/,"",$3); print $3}')
    fi=$(grep -E '^\| idle ' "$fix" | head -1 | awk -F'|' '{gsub(/[+ ]/,"",$3); print $3}')
    [ -n "$ci" ] && [ -n "$fi" ] || { echo "idle p50 unreadable (control '$ci', fix '$fi')" >&2; exit 1; }
    awk -v c="$ci" -v f="$fi" -v t="$IDLE_TOL_MS" 'BEGIN { d = c - f; if (d < 0) d = -d; exit !(d <= t) }' \
      || { echo "idle p50 drift $ci vs $fi over $IDLE_TOL_MS ms: host state differs, re-run the control" >&2; exit 1; }
    echo "$cache" ;;
esac
