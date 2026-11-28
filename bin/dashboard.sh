#!/usr/bin/env bash
# Starts `harness dashboard` detached on 127.0.0.1 and prints its URL, which carries this start's access token.
#   dashboard.sh [harness dashboard options]   start it, or print the URL of the one already running
#   dashboard.sh status                   print its pid and URL (exit 1 when it is not running)
#   dashboard.sh stop                     stop it
# HARNESS_HOME names the harness it reads (default: this script's repo), DASHBOARD_PORT its port (8787,
# or --port), DASHBOARD_DATA_DIR where its own history, cache, pid file, URL and log go (default:
# <state>/dashboard, the state directory being HARNESS_STATE from the environment, else from harness.env,
# else <home>/state). It writes nothing else, and nothing there is readable by other users. HARNESS_ENV
# and HARNESS_INSTANCE_DIR reach the dashboard, which finds the config with them. HARNESS_BIN names the
# harness binary (default: <home>/target/release/harness when it is executable, else harness on PATH).
set -u
umask 077
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
H="${HARNESS_HOME:-$(cd "$here/.." && pwd)}"
if [ -z "${HARNESS_BIN:-}" ]; then
  if [ -x "$H/target/release/harness" ]; then HARNESS_BIN=$H/target/release/harness; else HARNESS_BIN=harness; fi
fi
# The dashboard resolves the state directory, so this script and the server agree on it.
DATA="${DASHBOARD_DATA_DIR:-}"
[ -n "$DATA" ] || DATA=$("$HARNESS_BIN" dashboard --harness-home "$H" --print-data-dir) || exit 1
PORT="${DASHBOARD_PORT:-8787}"
PIDFILE="$DATA/dashboard.pid"
URLFILE="$DATA/dashboard.url"
LOG="$DATA/dashboard.log"

# The pid file's process, when it is alive and still a dashboard, its first argument `dashboard`: a
# reused pid is not ours.
running_pid() {
  local p
  p=$(awk '{print $1}' "$PIDFILE" 2>/dev/null)
  [[ "$p" =~ ^[0-9]+$ ]] && kill -0 "$p" 2>/dev/null && tr '\0' '\n' < "/proc/$p/cmdline" 2>/dev/null \
    | sed -n 2p | grep -qx 'dashboard' && echo "$p"
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
    if kill -0 "$p" 2>/dev/null; then
      kill -KILL "$p"
      for _ in $(seq 1 10); do kill -0 "$p" 2>/dev/null || break; sleep 0.2; done
      if kill -0 "$p" 2>/dev/null; then echo "pid $p is still alive after SIGKILL" >&2; exit 1; fi
    fi
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
mkdir -p "$DATA" || exit 1
# The pid and URL files hold the access token: never write them where another user could.
[ -d "$DATA" ] && [ -O "$DATA" ] || { echo "dashboard.sh: $DATA is not a directory of this user's" >&2; exit 1; }
chmod 700 "$DATA" || exit 1
rm -f "$URLFILE"
setsid nohup "$HARNESS_BIN" dashboard --harness-home "$H" --data-dir "$DATA" --port "$PORT" "$@" \
  >> "$LOG" 2>&1 < /dev/null &
pid=$!
echo "$pid $PORT" > "$PIDFILE"
# The URL file names the port the server bound, which differs from $PORT under --port 0.
for _ in $(seq 1 150); do
  if [ -s "$URLFILE" ] && IFS= read -r url < "$URLFILE" &&
    printf 'url = "%s/healthz?t=%s"\n' "${url%%/\?t=*}" "${url##*\?t=}" |
      curl -qsf --config - --max-time 2 > /dev/null 2>&1; then
    bound="${url#http://127.0.0.1:}"
    echo "$pid ${bound%%/*}" > "$PIDFILE"
    cat "$URLFILE"; exit 0
  fi
  kill -0 "$pid" 2>/dev/null || break
  sleep 0.2
done
echo "dashboard did not answer on port $PORT; last log lines:" >&2
tail -n 5 "$LOG" | sed 's/?t=[A-Za-z0-9_-]*/?t=[token]/' >&2
exit 1
