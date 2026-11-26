#!/usr/bin/env bash
# Starts bin/dashboard.py detached on 127.0.0.1 and prints its URL, which carries this start's access token.
#   dashboard.sh [dashboard.py options]   start it, or print the URL of the one already running
#   dashboard.sh status                   print its pid and URL (exit 1 when it is not running)
#   dashboard.sh stop                     stop it
# HARNESS_HOME names the harness it reads (default: this script's repo), DASHBOARD_PORT its port (8787,
# or --port), DASHBOARD_DATA_DIR where its own history, cache, pid file, URL and log go (default:
# <home>/state/dashboard). It writes nothing else, and nothing there is readable by other users.
set -u
umask 077
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
H="${HARNESS_HOME:-$(cd "$here/.." && pwd)}"
DATA="${DASHBOARD_DATA_DIR:-$H/state/dashboard}"
PORT="${DASHBOARD_PORT:-8787}"
PIDFILE="$DATA/dashboard.pid"
URLFILE="$DATA/dashboard.url"
LOG="$DATA/dashboard.log"

# The pid file's process, when it is alive and still a dashboard: a reused pid is not ours.
running_pid() {
  local p
  p=$(awk '{print $1}' "$PIDFILE" 2>/dev/null)
  [[ "$p" =~ ^[0-9]+$ ]] && kill -0 "$p" 2>/dev/null && tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null \
    | grep -q 'dashboard\.py' && echo "$p"
}
running_url() { cat "$URLFILE" 2>/dev/null || echo "http://127.0.0.1:$(awk '{print $2}' "$PIDFILE")/ (URL file missing)"; }

case "${1:-}" in
  status)
    if p=$(running_pid); then echo "running pid $p $(running_url)"; exit 0; fi
    echo "not running"; exit 1 ;;
  stop)
    p=$(running_pid) || { echo "not running"; exit 0; }
    kill "$p"
    for _ in $(seq 1 25); do kill -0 "$p" 2>/dev/null || break; sleep 0.2; done
    rm -f "$PIDFILE" "$URLFILE"
    echo "stopped pid $p"; exit 0 ;;
esac

args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  case "${args[$i]}" in
    --port) PORT="${args[$((i + 1))]:-$PORT}" ;;
    --port=*) PORT="${args[$i]#--port=}" ;;
  esac
done
if p=$(running_pid); then echo "already running (pid $p): $(running_url)"; exit 0; fi
mkdir -p "$DATA"
chmod 700 "$DATA"
rm -f "$URLFILE"
setsid nohup python3 "$here/dashboard.py" --harness-home "$H" --data-dir "$DATA" --port "$PORT" "$@" \
  >> "$LOG" 2>&1 < /dev/null &
pid=$!
echo "$pid $PORT" > "$PIDFILE"
for _ in $(seq 1 50); do
  if [ -s "$URLFILE" ] && curl -sf --max-time 2 "http://127.0.0.1:$PORT/healthz" > /dev/null 2>&1; then
    cat "$URLFILE"; exit 0
  fi
  kill -0 "$pid" 2>/dev/null || break
  sleep 0.2
done
echo "dashboard did not answer on port $PORT; last log lines:" >&2
tail -n 5 "$LOG" | sed 's/?t=[A-Za-z0-9_-]*/?t=[token]/' >&2
exit 1
