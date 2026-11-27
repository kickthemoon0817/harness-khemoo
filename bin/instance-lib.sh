# Sourced by the core scripts: finds the harness home, the instance overlay and its config.
# The instance holds everything specific to one installation (docs/INSTANCE.md); the core
# reads it only through harness_load and harness_hook.

# Sets and exports HARNESS_HOME, HARNESS_INSTANCE_DIR and HARNESS_STATE, sources the config
# file, puts the instance's bin/ first on PATH and exports the names listed in TICK_EXPORTS.
# An empty GH_REPO is derived from TARGET_REPO's origin remote and exported, so every gh call
# and queue count names the same repository; it stays empty when the origin names none.
# The config is sourced, not exported wholesale: a caller that wants every key exported wraps
# the call in `set -a` / `set +a`. HARNESS_ENV_FILE names the file read, present or not.
harness_load() {
    local name
    HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
    # Environment only: the config cannot name the directory it is read from.
    HARNESS_INSTANCE_DIR="${HARNESS_INSTANCE_DIR:-$HARNESS_HOME/instance}"
    HARNESS_ENV_FILE="${HARNESS_ENV:-$HARNESS_INSTANCE_DIR/harness.env}"
    # shellcheck source=/dev/null
    [ -r "$HARNESS_ENV_FILE" ] && . "$HARNESS_ENV_FILE"
    : "${HARNESS_STATE:=$HARNESS_HOME/state}"
    if [ -z "${GH_REPO:-}" ] && [ -n "${TARGET_REPO:-}" ]; then
        GH_REPO=$(harness_repo_from_origin "$TARGET_REPO")
    fi
    [ -n "${GH_REPO:-}" ] && export GH_REPO
    export HARNESS_HOME HARNESS_INSTANCE_DIR HARNESS_STATE
    export PATH="$HARNESS_INSTANCE_DIR/bin:$PATH"
    for name in ${TICK_EXPORTS:-}; do
        if [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then export "${name?}"
        else echo "instance-lib: TICK_EXPORTS: '$name' is not a variable name" >&2; fi
    done
}

# Prints the [HOST/]OWNER/NAME that gh takes for the origin remote of checkout $1, or nothing
# when it has no origin or the URL has no owner/name path.
harness_repo_from_origin() {
    local url host path
    url=$(git -C "$1" remote get-url origin 2>/dev/null) || return 0
    case "$url" in
        *://*) url=${url#*://}; url=${url#*@}; host=${url%%/*}; path=${url#*/} ;;
        *:*) url=${url#*@}; host=${url%%:*}; path=${url#*:} ;;
        *) return 0 ;;
    esac
    host=${host%%:*}; path=${path%/}; path=${path%.git}
    [[ "$path" =~ ^[^/]+/[^/]+$ ]] || return 0
    if [ "$host" = github.com ]; then echo "$path"; else echo "$host/$path"; fi
}

# Runs the instance hook hooks/<name> with the given arguments and returns its status, or 3
# when the instance has none. A hook named *.sh is sourced into the caller's shell, so what it
# exports reaches everything the caller starts; any other hook is executed, under
# `timeout HOOK_TIMEOUT_S` when the caller sets that.
harness_hook() {
    local name=$1 f rc had_u=""
    shift
    f="$HARNESS_INSTANCE_DIR/hooks/$name"
    case "$name" in
        *.sh)
            [ -r "$f" ] || return 3
            # An unset variable in the hook must not end the caller under its `set -u`.
            [[ $- == *u* ]] && had_u=1
            set +u
            # shellcheck source=/dev/null
            . "$f" "$@"; rc=$?
            [ -n "$had_u" ] && set -u
            return "$rc"
            ;;
    esac
    [ -x "$f" ] || return 3
    if [ -n "${HOOK_TIMEOUT_S:-}" ]; then timeout "$HOOK_TIMEOUT_S" "$f" "$@"; else "$f" "$@"; fi
}
