#!/usr/bin/env bash
# Exercises bin/tick.sh's landing tick in a scratch harness home: a fake claude records its lane,
# a fake queue command answers per label, and held slot locks stand for running ticks.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
holders=()
cleanup() { for p in "${holders[@]}"; do kill "$p" 2>/dev/null; done; rm -rf "$H"; }
trap cleanup EXIT
mkdir -p "$H/bin" "$H/config" "$H/state/locks" "$H/state/logs" "$H/target" "$H/fakebin" "$H/livebin"
cp "$here/bin/tick.sh" "$here/bin/usage-lib.sh" "$here/bin/remote-env.sh" "$H/bin/"
cat > "$H/fakebin/claude" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$2" | head -1 >> "$FAKE_LANES"
EOF
cat > "$H/fakebin/queue" <<'EOF'
#!/usr/bin/env bash
case "$ISSUE_LABEL" in ai:ready) cat "$FAKE_READY" ;; *) cat "$FAKE_WORK" ;; esac
EOF
cat > "$H/livebin/claude" <<'EOF'
#!/usr/bin/env bash
sleep 30
EOF
chmod +x "$H/fakebin/claude" "$H/fakebin/queue" "$H/livebin/claude"
cat > "$H/config/harness.env" <<EOF
TARGET_REPO=$H/target
QUEUE_DEPTH_CMD=$H/fakebin/queue
CLAUDE_BIN=$H/fakebin/claude
CLAUDE_FLAGS=
TICK_MODELS=fake-model
MAX_SLOTS=3
USAGE_CACHE=$H/usage-cache.json
USAGE_FETCH=$H/no-fetch
WEEK_HALF_AT=64
WEEK_ONE_AT=70
CLAIM_GRACE_S=0
EOF
export HARNESS_HOME="$H" FAKE_LANES="$H/lanes.txt" FAKE_READY="$H/ready.txt" FAKE_WORK="$H/work.txt"
L="$H/state/locks"; LOG="$H/state/logs/allocator.log"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
usage() { printf '{"timestamp": %s000, "data": {"fiveHourPercent": %s, "weeklyPercent": %s}}\n' "$(date +%s)" "$1" "$2" > "$H/usage-cache.json"; }
# -o: the lock is flock's own, so killing it frees the slot.
hold() { flock -o "$L/slot$1.lock" sleep 60 >/dev/null 2>&1 & holders+=($!); }
release_all() { for p in "${holders[@]}"; do kill "$p" 2>/dev/null; done; holders=(); sleep 0.3; }
launch() {  # run the launcher once and wait for any runner it started
    : > "$FAKE_LANES"; : > "$LOG" 2>/dev/null || true
    "$H/bin/tick.sh"
    sleep 2
}

# Half width on a 3-slot pool is 2. Both allowed slots held, one issue waiting for the slot.
usage 10 68; echo 5 > "$FAKE_WORK"; echo 1 > "$FAKE_READY"
hold 1; hold 2; sleep 0.3
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
# A process whose command line carries the landing lane stands for a live landing tick.
"$H/livebin/claude" -p "This tick is the merge slot's landing tick: live" >/dev/null 2>&1 & holders+=($!)
sleep 0.3
launch
check "a live landing tick keeps a second one from starting" '[ ! -s "$FAKE_LANES" ]' "lanes=$(cat "$FAKE_LANES")"
release_all

usage 10 68; echo 1 > "$FAKE_READY"; hold 1; sleep 0.3
launch
check "with a slot free inside the width, the normal tick starts and no landing tick" \
    '! grep -q "landing tick" "$LOG" 2>/dev/null && [ -s "$FAKE_LANES" ] && ! grep -q "landing tick" "$FAKE_LANES"' \
    "log=$(cat "$LOG" 2>/dev/null) lanes=$(cat "$FAKE_LANES")"
release_all

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
