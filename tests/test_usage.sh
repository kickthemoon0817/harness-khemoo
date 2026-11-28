#!/usr/bin/env bash
# Exercises the usage windows (`harness usage`, libexec/usage-ok.sh) for each agent against scratch
# readings: Claude's cache, codex's session rollouts, a normalized file refreshed by a fetch script,
# and none. No real cache, rollout or fetch script is read, so no API call is made.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
# A scratch harness home with no config, so the real readings are never read.
mkdir -p "$H/lib" "$H/libexec" "$H/state" "$H/sessions/2026/11/28"
cp "$here/lib/instance.sh" "$H/lib/"; cp "$here/libexec/usage-ok.sh" "$H/libexec/"
: "${HARNESS_BIN:?tests/run.sh sets it}"
unset HARNESS_TICK FLEET_ADAPTER FLEET_SESSION TICK_AGENT TICK_WIDTH LANE_MODEL $(compgen -v USAGE_)
export HARNESS_HOME="$H" HARNESS_STATE="$H/state" HOME="$H/home" MAX_SLOTS=8 \
    USAGE_CACHE="$H/usage-cache.json" USAGE_FETCH="$H/no-fetch" USAGE_SOURCE_codex="codex-sessions:$H/sessions"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
cache() {  # $1 five, $2 week, $3 age in seconds (default 0)
    printf '{"timestamp": %s000, "data": {"fiveHourPercent": %s, "weeklyPercent": %s}}\n' \
        "$(( $(date +%s) - ${3:-0} ))" "$1" "$2" > "$USAGE_CACHE"
}
rollout() {  # $1 primary used %, $2 its reset in seconds from now, $3 age of the file in seconds
    local f="$H/sessions/2026/11/28/rollout-a.jsonl"
    printf '{"timestamp":"2026-11-28T00:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":%s,"window_minutes":10080,"resets_at":%s},"secondary":null}}}\n' \
        "$1" "$(( $(date +%s) + $2 ))" > "$f"
    touch -d "@$(( $(date +%s) - $3 ))" "$f"
}
# The pool's cap for the models $1, and each agent's cap, from the JSON report.
pool() { TICK_MODELS="$1" "$HARNESS_BIN" usage --json | jq -r '"\(.pool_cap) " + ([.agents[] | "\(.agent)=\(.cap)"] | join(" "))'; }
ok() {  # runs usage-ok.sh as a tick of agent $1; leaves its output in out and status in rc
    out=$(HARNESS_TICK="state=$H/state slot=1 agent=$1 model=m kind=work" "$H/libexec/usage-ok.sh"); rc=$?
}

cache 10 20
check "claude's windows low: the full width" '[ "$(pool claude:opus)" = "8 claude=8" ]' "$(pool claude:opus)"
cache 60 20
check "the 5-hour window at its half step" '[ "$(pool claude:opus)" = "4 claude=4" ]' "$(pool claude:opus)"
cache 70 20
check "the 5-hour window at its one step" '[ "$(pool claude:opus)" = "1 claude=1" ]' "$(pool claude:opus)"
cache 80 20
check "the 5-hour window at its stop: nothing starts" '[ "$(pool claude:opus)" = "0 claude=0" ]' "$(pool claude:opus)"
cache 10 80
check "the weekly window at its half step" '[ "$(pool claude:opus)" = "4 claude=4" ]' "$(pool claude:opus)"
cache 10 85
check "the weekly window at its one step" '[ "$(pool claude:opus)" = "1 claude=1" ]' "$(pool claude:opus)"
cache 10 88
check "the weekly window at its stop: nothing starts" '[ "$(pool claude:opus)" = "0 claude=0" ]' "$(pool claude:opus)"
cache 10 20 4000
check "a stale reading starts nothing" '[ "$(pool claude:opus)" = "0 claude=0" ]' "$(pool claude:opus)"
rm -f "$USAGE_CACHE"
check "no reading at all starts nothing" '[ "$(pool claude:opus)" = "0 claude=0" ]' "$(pool claude:opus)"
cache 92 10
check "the ceiling holds whatever the steps say" '[ "$(USAGE_STEPS_SHORT="95 96 97" pool claude:opus)" = "0 claude=0" ]' "$(USAGE_STEPS_SHORT="95 96 97" pool claude:opus)"
cache 65 10
check "USAGE_STEPS_SHORT moves the short steps" '[ "$(USAGE_STEPS_SHORT="40 50 66" pool claude:opus)" = "1 claude=1" ]' "$(USAGE_STEPS_SHORT="40 50 66" pool claude:opus)"
cache 10 66
check "a per-window override moves one window's steps" '[ "$(USAGE_STEPS_claude_weekly="50 60 66" pool claude:opus)" = "0 claude=0" ]' "$(USAGE_STEPS_claude_weekly="50 60 66" pool claude:opus)"
out=$(USAGE_STEPS_LONG="80 85" "$HARNESS_BIN" usage 2>&1); rc=$?
check "malformed steps are an error that names the key" '[ "$rc" -eq 2 ] && [[ "$out" == *"USAGE_STEPS_LONG"* ]]' "rc=$rc $out"

# Codex reads its newest rollout, which stays true past its age until the window resets.
cache 10 20
rollout 82 3600 0
check "codex's rollout window steps down as a long window" '[ "$(pool codex:gpt-5.5)" = "4 codex=4" ]' "$(pool codex:gpt-5.5)"
rollout 82 3600 86400
check "a day-old rollout holds until its window resets" '[ "$(pool codex:gpt-5.5)" = "4 codex=4" ]' "$(pool codex:gpt-5.5)"
rollout 95 -60 86400
check "a rollout whose window reset reads as empty" '[ "$(pool codex:gpt-5.5)" = "8 codex=8" ]' "$(pool codex:gpt-5.5)"
rm -f "$H/sessions/2026/11/28/rollout-a.jsonl"
check "no rollout: codex starts nothing" '[ "$(pool codex:gpt-5.5)" = "0 codex=0" ]' "$(pool codex:gpt-5.5)"
check "gemini has no windows by default: the full width" '[ "$(pool gemini:gemini-3-pro)" = "8 gemini=8" ]' "$(pool gemini:gemini-3-pro)"

# The pool sums the agents' caps, up to the width.
cache 75 10; rollout 86 3600 0
check "the pool is the agents' caps summed" '[ "$(pool "claude:opus codex:gpt-5.5")" = "2 claude=1 codex=1" ]' "$(pool "claude:opus codex:gpt-5.5")"
check "the pool never passes the width" '[ "$(pool "claude:opus gemini:g")" = "8 claude=1 gemini=8" ]' "$(pool "claude:opus gemini:g")"
check "a bare model runs on TICK_AGENT" '[ "$(TICK_AGENT=codex pool gpt-5.5)" = "1 codex=1" ]' "$(TICK_AGENT=codex pool gpt-5.5)"

# A normalized file of any windows, refreshed by its fetch script when stale.
cat > "$H/fetch" <<EOF
#!/usr/bin/env bash
printf '{"timestamp": %s000, "windows": [{"name": "hourly", "used_pct": 65, "window_minutes": 60}, {"name": "monthly", "used_pct": 10, "window_minutes": 43200}]}\n' "\$(date +%s)" > "$H/gemini.json"
EOF
chmod +x "$H/fetch"
printf '{"timestamp": 1000, "windows": []}\n' > "$H/gemini.json"
export USAGE_SOURCE_gemini="json:$H/gemini.json" USAGE_FETCH_gemini="$H/fetch"
check "a stale normalized file is refreshed by its fetch script" '[ "$(pool gemini:g)" = "4 gemini=4" ]' "$(pool gemini:g); $(cat "$H/gemini.json")"
unset USAGE_SOURCE_gemini USAGE_FETCH_gemini

# usage-ok.sh asks about the agent its tick runs.
cache 87 87; ok claude
check "usage-ok.sh under the stand-down point exits 0" '[ "$rc" -eq 0 ] && [[ "$out" == "claude: five_hour=87% weekly=87% "*": go on" ]]' "rc=$rc $out"
cache 88 10; ok claude
check "usage-ok.sh two under the 5-hour ceiling exits 1" '[ "$rc" -eq 1 ] && [[ "$out" == *"start nothing"* ]]' "rc=$rc $out"
cache 10 88; ok claude
check "usage-ok.sh two under the weekly ceiling exits 1" '[ "$rc" -eq 1 ]' "rc=$rc $out"
cache 10 95; ok claude
check "usage-ok.sh past the ceiling exits 1" '[ "$rc" -eq 1 ]' "rc=$rc $out"
own() { USAGE_CEILING_PCT=95 USAGE_TICK_STOP_PCT=90 USAGE_TICK_STOP_LONG_PCT=93 ok claude; }
cache 89 92; own
check "usage-ok.sh lets a long window pass the short point up to its own" '[ "$rc" -eq 0 ]' "rc=$rc $out"
cache 90 10; own
check "usage-ok.sh stops at the short point when the long one is higher" '[ "$rc" -eq 1 ]' "rc=$rc $out"
cache 10 93; own
check "usage-ok.sh stops at the long point" '[ "$rc" -eq 1 ] && [[ "$out" == *"long_stop=93%"* ]]' "rc=$rc $out"
cache 10 10 4000; ok claude
check "usage-ok.sh with a stale reading exits 2" '[ "$rc" -eq 2 ] && [[ "$out" == "usage unknown"* ]]' "rc=$rc $out"
rollout 89 3600 0; cache 10 10; ok codex
check "usage-ok.sh in a codex tick reads codex's windows" '[ "$rc" -eq 1 ] && [[ "$out" == "codex: primary=89% "* ]]' "rc=$rc $out"
ok gemini
check "usage-ok.sh in a gemini tick with no windows goes on" '[ "$rc" -eq 0 ] && [[ "$out" == "gemini: no windows "*": go on" ]]' "rc=$rc $out"
out=$(FLEET_ADAPTER=codex "$H/libexec/usage-ok.sh"); rc=$?
check "usage-ok.sh under fleetd reads the adapter's agent" '[ "$rc" -eq 1 ] && [[ "$out" == "codex: "* ]]' "rc=$rc $out"
out=$(HARNESS_TICK="state=$H/elsewhere slot=1 agent=codex model=m kind=work" TICK_AGENT=claude "$H/libexec/usage-ok.sh"); rc=$?
check "another installation's marker names no agent here" '[ "$rc" -eq 0 ] && [[ "$out" == "claude: "* ]]' "rc=$rc $out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
