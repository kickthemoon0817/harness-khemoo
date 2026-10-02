# Sourced by tick.sh and the harness's cron jobs (needs HARNESS_HOME and HARNESS_STATE).
# With the local host off (`lease.sh local --off`), every docker command they run
# goes to the remote host through bin/remote-docker: builds and tests as well as
# kits. It is an exported shell function rather than a PATH entry, because Claude
# Code's shell snapshot replaces PATH, while an exported function reaches every
# bash a tick starts, scripts included.
#
# A docker binary reached past the function (`timeout docker`, `xargs docker`,
# `env docker`, Python's subprocess, a `sh` script) reads DOCKER_HOST, which then
# names a socket that does not exist, so it fails to connect instead of running on
# this host. The function takes that value as unset. `DOCKER_HOST= command docker`
# still reaches this host's daemon on purpose.
remote_host_env() {
  local h
  [ -e "$HARNESS_STATE/locks/local-card-off" ] || return 0
  h=$(cat "$HARNESS_STATE/locks/remote-card" 2>/dev/null) && [ -n "$h" ] || return 0
  export WORV_REMOTE_DOCKER_HOST="ssh://$h" WORV_REMOTE_DOCKER_SHIM="$HARNESS_HOME/bin/remote-docker/docker"
  export WORV_LOCAL_OFF_DOCKER_HOST="unix:///run/local-host-off/use-the-docker-function-or-bash-c.sock"
  export DOCKER_HOST="$WORV_LOCAL_OFF_DOCKER_HOST"
  docker() {
    local host=${DOCKER_HOST:-}
    if [ -z "$host" ] || [ "$host" = "$WORV_LOCAL_OFF_DOCKER_HOST" ]; then host=$WORV_REMOTE_DOCKER_HOST; fi
    DOCKER_HOST="$host" "$WORV_REMOTE_DOCKER_SHIM" "$@"
  }
  export -f docker
}
