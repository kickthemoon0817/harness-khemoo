# Sourced by tick.sh and the harness's cron jobs (needs HARNESS_HOME and HARNESS_STATE).
# With the local host off (`lease.sh local --off`), every docker command they run
# goes to the remote host through bin/remote-docker: builds and tests as well as
# kits. It is an exported shell function rather than a PATH entry, because Claude
# Code's shell snapshot replaces PATH, while an exported function reaches every
# bash a tick starts, scripts included. A docker run from Python or compose does
# not see the function and stays on this host, where its mounts are.
remote_host_env() {
  local h
  [ -e "$HARNESS_STATE/locks/local-card-off" ] || return 0
  h=$(cat "$HARNESS_STATE/locks/remote-card" 2>/dev/null) && [ -n "$h" ] || return 0
  export WORV_REMOTE_DOCKER_HOST="ssh://$h" WORV_REMOTE_DOCKER_SHIM="$HARNESS_HOME/bin/remote-docker/docker"
  docker() { DOCKER_HOST="${DOCKER_HOST:-$WORV_REMOTE_DOCKER_HOST}" "$WORV_REMOTE_DOCKER_SHIM" "$@"; }
  export -f docker
}
