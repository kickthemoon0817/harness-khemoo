#!/usr/bin/env bash
# Nucleus preflight for a kit launch. Exits 0 at once when the host probe
# (bin/nucleus_probe.sh, cron) shows an unbroken run of at least NUCLEUS_STABLE_S
# seconds ending within the last two minutes; otherwise falls back to the
# five-minute in-line loop, and exits 1 on the first failed connect.
set -u
HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
: "${NUCLEUS_HOST:=10.50.2.21}"; : "${NUCLEUS_PORT:=3009}"; : "${NUCLEUS_STABLE_S:=300}"
status="$HARNESS_HOME/state/nucleus/status"
now=$(date +%s)
if [ -r "$status" ]; then
    ok_since=$(grep -o 'ok_since=[0-9]*' "$status" | cut -d= -f2)
    last=$(grep -o 'last=[0-9]*' "$status" | cut -d= -f2)
    if [ -n "$ok_since" ] && [ $(( now - last )) -le 120 ] && [ $(( now - ok_since )) -ge "$NUCLEUS_STABLE_S" ]; then
        echo "nucleus ok: stable $(( (now - ok_since) / 60 )) min (host probe)"; exit 0
    fi
fi
echo "nucleus: no fresh stable reading, probing in line for 5 min"
for i in $(seq 30); do
    timeout 3 bash -c "</dev/tcp/$NUCLEUS_HOST/$NUCLEUS_PORT" 2>/dev/null || { echo "nucleus FAIL at probe $i"; exit 1; }
    sleep 10
done
echo "nucleus ok: 30/30 in line"
