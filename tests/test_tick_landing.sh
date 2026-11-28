#!/usr/bin/env bash
# Exercises bin/tick.sh's landing tick in a scratch harness home: a fake claude records its lane,
# a fake queue command answers per label, and processes holding a slot lock and recorded in its slot
# file, as the runner records a tick, stand for running ticks.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
holders=()
cleanup() { for p in "${holders[@]}"; do kill "$p" 2>/dev/null; done; rm -rf "$H"; }
trap cleanup EXIT
mkdir -p "$H/bin" "$H/prompts" "$H/instance/prompts" "$H/state/locks" "$H/state/logs" "$H/target" "$H/fakebin"
mkdir -p "$H/lib"
cp "$here/bin/tick.sh" "$H/bin/"; cp "$here/lib/instance.sh" "$H/lib/"
echo "Core rules." > "$H/prompts/tick-core.md"
cp "$here/tests/fixtures/instance/prompts/tick.md" "$H/instance/prompts/tick.md"
cat > "$H/fakebin/claude" <<'EOF'
#!/usr/bin/env bash
head -1 >> "$FAKE_LANES"
EOF
cat > "$H/fakebin/queue" <<'EOF'
#!/usr/bin/env bash
case "$ISSUE_LABEL" in ai:ready) cat "$FAKE_READY" ;; *) cat "$FAKE_WORK" ;; esac
EOF
# tick.sh reads the crontab to tell a paused harness; the stub keeps the host's out of it.
cat > "$H/fakebin/crontab" <<'EOF'
#!/usr/bin/env bash
echo "*/2 * * * * $HARNESS_HOME/bin/tick.sh"
EOF
chmod +x "$H/fakebin/claude" "$H/fakebin/queue" "$H/fakebin/crontab"
git -C "$H/target" init -q
cat > "$H/instance/harness.env" <<EOF
TARGET_REPO=$H/target
WORKTREE_ROOT=$H/worktrees
GH_REPO=example-org/example-repo
QUEUE_DEPTH_CMD=$H/fakebin/queue
AGENT_BIN_claude=$H/fakebin/claude
AGENT_FLAGS_claude=
TICK_MODELS=fake-model
MAX_SLOTS=3
USAGE_CACHE=$H/usage-cache.json
USAGE_FETCH=$H/no-fetch
USAGE_STEPS_LONG="64 70 88"
CLAIM_GRACE_S=0
EOF
unset HARNESS_TICK FLEET_SESSION HARNESS_STATE
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH" FAKE_LANES="$H/lanes.txt" FAKE_READY="$H/ready.txt" FAKE_WORK="$H/work.txt"
L="$H/state/locks"; LOG="$H/state/logs/allocator.log"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
usage() { printf '{"timestamp": %s000, "data": {"fiveHourPercent": %s, "weeklyPercent": %s}}\n' "$(date +%s)" "$1" "$2" > "$H/usage-cache.json"; }
# Polls every 50 ms until the command $@ succeeds, for at most 20 s.
until_ok() { local n=0; until "$@"; do [ "$n" -lt 400 ] || return 1; sleep 0.05; n=$((n + 1)); done; }
# Records process $1 as slot $2's tick of kind $3 in state directory $4 (default this home's).
record() {
    local stat state=${4:-$H/state}; local -a f
    mkdir -p "$state/locks"
    stat=$(cat "/proc/$1/stat"); read -ra f <<<"${stat##*) }"
    printf '%s %s %s claude fake-model %s\n' "$1" "${f[19]}" "$(cat /proc/sys/kernel/random/boot_id)" "$3" > "$state/locks/slot$2.tick"
}
# A claude tick holding slot $1. -o: the lock is flock's own, so killing it frees the slot. Returns
# once the slot is held.
hold() {
    flock -o "$L/slot$1.lock" sleep 60 >/dev/null 2>&1 & holders+=($!)
    until_ok held "$1"
    record "$!" "$1" work
}
held() { ! flock -n "$L/slot$1.lock" true; }
# Starts a stand-in tick of kind $1, recorded as slot 9's in state directory $2 (default this home's).
live() {
    sleep 30 & holders+=("$!")
    record "$!" 9 "$1" "${2:-}"
}
release_all() { for p in "${holders[@]}"; do kill "$p" 2>/dev/null; wait "$p" 2>/dev/null; done; holders=(); }
# The launcher's detached runners and the forks that become them both carry its path.
no_runner() { ! pgrep -f "$H/bin/tick.sh" >/dev/null; }
settle() { until_ok no_runner; }
launch() {  # run the launcher once and wait for any runner it started
    : > "$FAKE_LANES"; : > "$LOG" 2>/dev/null || true
    "$H/bin/tick.sh"
    settle
}

# Half width on a 3-slot pool is 2. Both allowed slots held, one issue waiting for the slot.
usage 10 68; echo 5 > "$FAKE_WORK"; echo 1 > "$FAKE_READY"
hold 1; hold 2
launch
check "a full pool starts one landing tick for an ai:ready issue" \
    'grep -q "spawning 1 landing tick" "$LOG" && grep -q "landing tick: claim only an issue labelled" "$FAKE_LANES"' \
    "log=$(cat "$LOG" 2>/dev/null) lanes=$(cat "$FAKE_LANES")"
check "the landing tick runs once and does not chain" '[ "$(wc -l < "$FAKE_LANES")" -eq 1 ]' "$(cat "$FAKE_LANES")"

echo 0 > "$FAKE_READY"
launch
check "no issue waiting for the slot: nothing starts past the width" \
    '[ ! -s "$FAKE_LANES" ] && ! grep -q "landing tick" "$LOG" 2>/dev/null' "lanes=$(cat "$FAKE_LANES")"

echo 1 > "$FAKE_READY"; usage 10 88
launch
check "a window that allows no slot starts no landing tick" '[ ! -s "$FAKE_LANES" ]' "lanes=$(cat "$FAKE_LANES")"

usage 10 68
live landing
launch
check "a live landing tick keeps a second one from starting" '[ ! -s "$FAKE_LANES" ]' "lanes=$(cat "$FAKE_LANES")"
release_all

# Another installation's landing tick on the same host is not this one's.
hold 1; hold 2
live landing "$H/other-state"
launch
check "another state directory's landing tick does not count" \
    'grep -q "landing tick: claim only an issue labelled" "$FAKE_LANES"' "log=$(cat "$LOG" 2>/dev/null) lanes=$(cat "$FAKE_LANES")"
release_all

usage 10 68; echo 1 > "$FAKE_READY"; hold 1
launch
check "with a slot free inside the width, the normal tick starts and no landing tick" \
    '! grep -q "landing tick" "$LOG" 2>/dev/null && [ -s "$FAKE_LANES" ] && ! grep -q "landing tick" "$FAKE_LANES"' \
    "log=$(cat "$LOG" 2>/dev/null) lanes=$(cat "$FAKE_LANES")"
release_all

# Through fleet the landing tick carries its label too, for the sessions fleet lists.
cat > "$H/fakebin/fleet" <<'EOF'
#!/usr/bin/env bash
[ "$1" = dispatch ] || exit 2
printf '%s\n' "$*" >> "$FAKE_FLEET_ARGV"
head -1 >> "$FAKE_LANES"
EOF
chmod +x "$H/fakebin/fleet"
printf '%s\n' "FLEET_DISPATCH=1" "FLEET_BIN=$H/fakebin/fleet" >> "$H/instance/harness.env"
export FAKE_FLEET_ARGV="$H/fleet-argv.txt"
usage 10 68; echo 1 > "$FAKE_READY"; hold 1; hold 2
launch
check "through fleet, the landing tick is dispatched with its label" \
    'grep -q "landing tick: claim only an issue labelled" "$FAKE_LANES" && grep -q -- "--label tick=landing$" "$FAKE_FLEET_ARGV"' \
    "lanes=$(cat "$FAKE_LANES") argv=$(cat "$FAKE_FLEET_ARGV" 2>/dev/null)"
live landing
launch
check "a live dispatched landing tick keeps a second one from starting" '[ ! -s "$FAKE_LANES" ]' "lanes=$(cat "$FAKE_LANES")"
release_all

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
