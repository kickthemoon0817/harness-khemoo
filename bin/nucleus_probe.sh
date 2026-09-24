#!/usr/bin/env bash
# Host-side Nucleus reachability probe, run from cron every minute.
# Six TCP connects ten seconds apart; the status file carries the start of the
# current unbroken run of successes so a tick can read "stable for N minutes"
# without waiting through its own five-minute loop.
set -u
HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
: "${NUCLEUS_HOST:=10.50.2.21}"; : "${NUCLEUS_PORT:=3009}"
dir="$HARNESS_HOME/state/nucleus"; mkdir -p "$dir"
status="$dir/status"
ok_since=$(grep -o 'ok_since=[0-9]*' "$status" 2>/dev/null | cut -d= -f2)
for i in 1 2 3 4 5 6; do
    now=$(date +%s)
    if timeout 3 bash -c "</dev/tcp/$NUCLEUS_HOST/$NUCLEUS_PORT" 2>/dev/null; then
        [ -n "$ok_since" ] || ok_since=$now
        printf 'ok_since=%s last=%s result=ok\n' "$ok_since" "$now" >"$status.tmp"
    else
        ok_since=""
        printf 'ok_since= last=%s result=fail\n' "$now" >"$status.tmp"
        echo "$(date -u -d @$now +%FT%TZ) FAIL" >>"$dir/flaps.log"
    fi
    mv -f "$status.tmp" "$status"
    [ "$i" -lt 6 ] && sleep 10
done
