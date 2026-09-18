#!/usr/bin/env bash
# Atomic resource lease for the one GPU + warm container. The ONLY sanctioned
# way to take, check or drop the lease; a bare write to the lease file is a bug.
#   lease.sh acquire <issue> [domain]
#                              -> prints ACQUIRED or BUSY <holder line>; exit 0/1
#                              The holder line names the DDS domain its kit and
#                              gate runs use (env ROS_DOMAIN_ID when unstated,
#                              '-' when neither): every kit on this host shares
#                              one network namespace, so a sibling reads the
#                              domain it must not take.
#   lease.sh release <issue>   -> drops the lease only if this tick's pid holds it
#   lease.sh status            -> prints the holder and whether its pid is alive
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
L="$H/state/locks/resource.lease"; K="$H/state/locks/resource.lock"; G=/tmp/isaac-cppmig-gpu-runtime.lock
pid=$("$H/bin/tick-pid.sh" 2>/dev/null || echo "$PPID")
case "${1:-}" in
  acquire)
    issue="${2:?issue number}"
    domain="${3:-${ROS_DOMAIN_ID:-}}"
    flock "$K" -c "flock $G -c '
      if [ -s \"$L\" ]; then
        hp=\$(awk \"{print \\\$2}\" \"$L\")
        if kill -0 \"\$hp\" 2>/dev/null && [ \"\$hp\" != \"$pid\" ]; then echo \"BUSY \$(cat \"$L\")\"; exit 1; fi
      fi
      echo \"$issue $pid \$(date -u +%FT%TZ) domain=${domain:--}\" > \"$L\"; echo ACQUIRED'"
    ;;
  release)
    flock "$K" -c "if [ -s \"$L\" ] && [ \"\$(awk '{print \$2}' \"$L\")\" = \"$pid\" ]; then rm -f \"$L\"; echo RELEASED; else echo \"NOT-HOLDER \$(cat \"$L\" 2>/dev/null)\"; fi"
    ;;
  status)
    if [ -s "$L" ]; then hp=$(awk '{print $2}' "$L"); kill -0 "$hp" 2>/dev/null && echo "HELD $(cat "$L") (alive)" || echo "ORPHAN $(cat "$L") (pid dead)"; else echo FREE; fi
    ;;
  *) echo "usage: lease.sh acquire <issue> [domain] | release | status" >&2; exit 2;;
esac
