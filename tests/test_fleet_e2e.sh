#!/usr/bin/env bash
# Runs a tick through the real fleet and fleetd: bin/tick.sh in runner mode with FLEET_DISPATCH=1
# dispatches to a fleetd on a unix socket, whose agent adapter is a script that records its stdin and
# what the harness sees of it, and hits a usage limit on the first model. Then the dashboard reads the
# same fleet's status. Needs cargo
# to build the workspace; without it the suite is skipped, except in CI, where that is a failure.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# A suite run inside a tick must not hand its marker to fleetd or the runner.
unset HARNESS_TICK FLEET_SESSION
if ! command -v cargo >/dev/null 2>&1; then
    if [ -n "${CI:-}" ]; then echo "FAIL cargo is not installed, and CI must run this suite"; exit 1; fi
    echo "SKIP test_fleet_e2e.sh: cargo is not installed, so fleet and fleetd cannot be built"
    exit 0
fi
# The workspace pins its toolchain in rust-toolchain.toml, which rustup reads from the cwd.
(cd "$here" && cargo build --quiet --locked --bin fleet --bin fleetd --bin harness) || { echo "FAIL cargo build"; exit 1; }
target=$(cd "$here" && cargo metadata --format-version 1 --no-deps | jq -r .target_directory)
FLEET="$target/debug/fleet"; FLEETD="$target/debug/fleetd"; HARNESS_BIN="${HARNESS_BIN:-$target/debug/harness}"

H=$(mktemp -d)
daemon=""
cleanup() { [ -n "$daemon" ] && kill "$daemon" 2>/dev/null; rm -rf "$H"; }
trap cleanup EXIT
mkdir -p "$H/bin" "$H/prompts" "$H/instance/prompts" "$H/state/locks" "$H/state/logs" "$H/target" "$H/fakebin"
mkdir -p "$H/lib"
cp "$here/bin/tick.sh" "$H/bin/"; cp "$here/lib/instance.sh" "$H/lib/"
echo "Core rules." > "$H/prompts/tick-core.md"
cp "$here/tests/fixtures/instance/prompts/tick.md" "$H/instance/prompts/tick.md"
git -C "$H/target" init -q
cat > "$H/agent.sh" <<'EOF'
printf '=== %s\n' "$1" >> "$AGENT_RECORD"
cat >> "$AGENT_RECORD"
# The tick a claim made here would belong to, and the ticks the hub counts while this one runs.
printf 'self=%s tick=%s\n' "$$" "$("$TICK_PID" 2>&1)" >> "$AGENT_SEEN"
"$HARNESS_BIN" ticks --state "$HUB_STATE" >> "$AGENT_SEEN" 2>&1
if [ "$1" = model-a ]; then
    echo "You've hit your usage limit · resets 5pm"
    exit 1
fi
echo "agent tick on $1 done"
EOF
cat > "$H/fakebin/crontab" <<'EOF'
#!/usr/bin/env bash
echo "*/2 * * * * $HARNESS_HOME/bin/tick.sh"
EOF
printf '#!/bin/sh\nexit 1\n' > "$H/fakebin/gh"
chmod +x "$H/fakebin/crontab" "$H/fakebin/gh"

cat > "$H/fleetd.toml" <<EOF
device = "dev-a"
state_dir = "$H/fleetd"
kill_grace_s = 1

[adapters.fake]
exe = "/bin/sh"
args = ["$H/agent.sh", "{model}"]
models = ["model-a", "model-b"]
workdir_roots = ["$H/target"]
match_exe = ["fleet-e2e-none"]
max_concurrent = 2

[adapters.fake.env]
AGENT_RECORD = "$H/agent-stdin.txt"
AGENT_SEEN = "$H/agent-seen.txt"
TICK_PID = "$here/libexec/tick-pid.sh"
HARNESS_BIN = "$HARNESS_BIN"
HUB_STATE = "$H/state"
EOF
cat > "$H/instance/fleet.toml" <<EOF
[devices.dev-a]
transport = "local"
socket = "$H/fleetd/fleetd.sock"
EOF
cat > "$H/instance/harness.env" <<EOF
TARGET_REPO=$H/target
WORKTREE_ROOT=$H/worktrees
GH_REPO=example-org/example-repo
TICK_AGENT=fake
TICK_MODELS="model-a model-b"
FLEET_DISPATCH=1
FLEET_BIN=$FLEET
FLEET_STATUS_CMD="$FLEET status --json"
MAX_SLOTS=1
USAGE_CACHE=$H/usage-cache.json
USAGE_FETCH=$H/no-fetch
EOF

"$FLEETD" --config "$H/fleetd.toml" 2>"$H/fleetd.log" & daemon=$!
for _ in $(seq 100); do [ -S "$H/fleetd/fleetd.sock" ] && break; sleep 0.1; done

pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }

check "fleetd is up" '[ -S "$H/fleetd/fleetd.sock" ]' "$(cat "$H/fleetd.log")"
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH"
unset HARNESS_INSTANCE_DIR HARNESS_ENV FLEET_CONFIG HARNESS_STATE
"$H/bin/tick.sh" runner 1 0
log=$(ls "$H"/state/logs/tick-*-slot1.log 2>/dev/null | head -1)
record="$H/agent-stdin.txt"

check "the prompt reached the agent on stdin, on each model" \
    'grep -qx "=== model-a" "$record" && grep -qx "=== model-b" "$record" && [ "$(grep -c "^Core rules\.$" "$record")" -eq 2 ]' \
    "$(cat "$record" 2>/dev/null)"
check "the tick log has the agent's output and fleet's exit status" \
    'grep -q "agent tick on model-b done" "$log" && grep -q "^fleet: dev-a: session d-" "$log" && grep -q "exit=1 .*model=model-a" "$log" && grep -q "exit=0 .*model=model-b" "$log"' \
    "$(cat "$log" 2>/dev/null)"
check "a usage-limit line cools the model down" \
    '[ -f "$H/state/model-cooldown/fake:model-a" ] && [ ! -f "$H/state/model-cooldown/fake:model-b" ] && grep -q "fake:model-a hit its usage limit" "$H/state/logs/allocator.log"' \
    "$(ls "$H/state/model-cooldown" 2>/dev/null); $(cat "$H/state/logs/allocator.log" 2>/dev/null)"

seen="$H/agent-seen.txt"
check "on the device, the dispatched agent is the tick a claim belongs to" \
    '[ "$(grep -Ec "^self=([0-9]+) tick=\1$" "$seen")" -eq 2 ]' \
    "$(cat "$seen" 2>/dev/null)"
check "on the hub, harness ticks counts the waiting fleet dispatch as the tick, on each model" \
    'grep -Eq "^[0-9]+ [0-9]+ 1 fake model-a work$" "$seen" && grep -Eq "^[0-9]+ [0-9]+ 1 fake model-b work$" "$seen"' \
    "$(cat "$seen" 2>/dev/null)"

snapshot=$("$HARNESS_BIN" dashboard --harness-home "$H" --gh "$H/fakebin/gh" --usage-cache "$H/usage-cache.json" \
    --remote-interval 0 --claimable-interval 0 --once 2>"$H/dashboard.err")
# One line: availability, the first device's name, error, whether it reported CPUs and its first
# adapter, then every session as model,adapter,state in sorted order.
panel=$(printf '%s' "$snapshot" | jq -r '.fleet as $f
    | if ($f.devices | length) == 0 then "no devices: \($f.error)"
      else $f.devices[0] as $d
        | [($f.available | tostring), $d.name, ($d.error | tostring), ($d.cpus > 0 | tostring), $d.adapters[0].name,
           ([$f.sessions[] | [.model, .adapter, .state] | join(",")] | sort | join(" "))]
        | join(" ")
      end' 2>&1)
check "the dashboard's fleet panel reads the real fleet status" \
    '[ "$panel" = "true dev-a null true fake model-a,fake,exited 1 model-b,fake,exited 0" ]' \
    "$panel; $(tail -5 "$H/dashboard.err")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
