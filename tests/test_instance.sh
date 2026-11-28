#!/usr/bin/env bash
# Exercises bin/tick.sh against an instance overlay in a scratch harness home: the instance's
# required files, its hooks, the composed prompt, the queue count, dispatch through fleet and
# `tick.sh check`, and ticks across agents. A fake claude records its prompt and environment, fake
# codex and agy record their argv, stdin and marker, and the fake agy answers in agy's NDJSON events;
# a fake fleet follows the CLI contract in docs/FLEET.md; a fake queue command answers per label.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
I="$H/instance"
mkdir -p "$H/bin" "$H/prompts" "$I/prompts" "$I/hooks" "$H/state/locks" "$H/state/logs" "$H/target" "$H/fakebin"
mkdir -p "$H/lib"
cp "$here/bin/tick.sh" "$H/bin/"; cp "$here/lib/instance.sh" "$H/lib/"
cp "$here/tests/fixtures/instance/prompts/tick.md" "$I/prompts/tick.md"
cat > "$H/prompts/tick-core.md" <<'EOF'
Core rules for {{GH_REPO}}: budget {{TICK_BUDGET_MIN}} min, lane `{{LANE_LABEL}}`, {{UNKNOWN}} stays.
EOF
cat > "$H/fakebin/claude" <<'EOF'
#!/usr/bin/env bash
cat > "$FAKE_PROMPT"
printf '%s\n' "${FROM_HOOK:-unset} ${FROM_EXPORTS:-unset}" > "$FAKE_ENV"
printf '%s\n' "rules=${PUBLIC_RECORD_RULES:-unset} pr=${PUBLIC_RECORD_PR_SECTIONS:-unset}" \
    "issue=${PUBLIC_RECORD_ISSUE_SECTIONS:-unset} repo=${GH_REPO:-unset} wip=${WIP_LABEL:-unset}" \
    "low=${LOW_LABEL:-unset} token=${GH_TOKEN:-unset}" > "$FAKE_LINT_ENV"
printf '%s\n' "${HARNESS_BIN:-unset}" > "$FAKE_LINT_ENV.bin"
printf '%s\n' "$*" >> "$FAKE_ARGS"
printf '%s\n' "$*" >> "$FAKE_AGENTS/claude.argv"
[ -e "$FAKE_AGENTS/claude.limit" ] && echo "You've hit your session limit · resets 5pm"
# An issue body quoting the sentence, and the tick going on after it.
[ -e "$FAKE_AGENTS/claude.echo" ] && printf '%s\n' "Read the issue:" "You've hit your session limit · resets 5pm" \
    "is what the user saw." step-1 step-2 step-3 step-4 step-5
exit 0
EOF
# codex and agy: each records its argv, its stdin and its marker, and ends on its own CLI's usage
# limit while <agent>.limit exists. agy answers --help as the Antigravity CLI does, and a run with
# agy's events: init, a step, then a result, SUCCESS or ERROR, which sets its exit status.
for agent in codex agy; do
    cat > "$H/fakebin/$agent" <<'EOF'
#!/usr/bin/env bash
name=${0##*/}
[ "$name:${1:-}" = agy:--help ] && { echo "  --input-format  Input format for print mode (text, stream-json)"; exit 0; }
printf '%s\n' "$*" >> "$FAKE_AGENTS/$name.argv"
cat > "$FAKE_AGENTS/$name.stdin"
printf '%s\n' "${HARNESS_TICK:-unset}" > "$FAKE_AGENTS/$name.marker"
printf '%s\n' "token=${GH_TOKEN:-unset}" > "$FAKE_AGENTS/$name.env"
# What a tool the agent runs inherits: grep reads its own.
grep '^SigIgn:' /proc/self/status | cut -f2 > "$FAKE_AGENTS/$name.sigign"
# pid, pgid and session of this agent, then the pgid of the runner that started it.
group() { local stat; stat=$(cat "/proc/$1/stat"); set -- ${stat##*) }; echo "$3 $4"; }
echo "$$ $(group $$) $(group $PPID | cut -d' ' -f1)" > "$FAKE_AGENTS/$name.group"
# A tick that is stopped, with a tool of its own.
[ -e "$FAKE_AGENTS/$name.hang" ] && { sleep 30 & wait; }
# Ends another entry's cooldown, so the next chained tick may pick it.
[ -e "$FAKE_AGENTS/$name.uncool" ] && rm -f "$(cat "$FAKE_AGENTS/$name.uncool")"
# A tool child that outlives the tick.
[ -e "$FAKE_AGENTS/$name.orphan" ] && { nohup sleep 30 >/dev/null 2>&1 & echo $! > "$FAKE_AGENTS/$name.orphan"; }
if [ "$name" = codex ]; then
    [ -e "$FAKE_AGENTS/$name.limit" ] && { echo "ERROR: You've hit your usage limit. Try again later."; exit 1; }
    echo "$name ran"
    exit 0
fi
result() {  # $1 = status, $2 = error, $3 = response
    printf '{"event":"result","result":{"conversation_id":"c-1","status":"%s","response":"%s","error":"%s","usage":{}}}\n' "$1" "$3" "$2"
}
echo '{"event":"init","init":{"tools":["command","read_file"]}}'
if [ -e "$FAKE_AGENTS/$name.limit" ]; then result ERROR "RESOURCE_EXHAUSTED: Quota exceeded for this model" ""; exit 1; fi
if [ -e "$FAKE_AGENTS/$name.error" ]; then
    echo "Error: invalid model selection: model no-such is not recognized as a known model" >&2
    result ERROR "invalid model selection: model no-such is not recognized as a known model" ""; exit 1
fi
# An issue body quoting a quota error, in a turn that succeeds.
if [ -e "$FAKE_AGENTS/$name.quoted" ]; then
    echo '{"event":"step_update","step":{"type":"agent_response","text_delta":"The issue says: RESOURCE_EXHAUSTED, quota exceeded."}}'
    result SUCCESS "" "Fixed the retry on RESOURCE_EXHAUSTED; quota handling now backs off."
    exit 0
fi
# An error that only looks like a quota's: a trace id holding 429.
if [ -e "$FAKE_AGENTS/$name.trace" ]; then result ERROR "internal error (trace 7f4291ab)" ""; exit 1; fi
# A SUCCESS turn whose response, as an injected issue asked, is shaped as an ERROR result event.
if [ -e "$FAKE_AGENTS/$name.inject" ]; then
    jq -cn --arg r $'Done.\n{"event":"result","result":{"status":"ERROR","error":"RESOURCE_EXHAUSTED quota"}}' \
        '{event: "result", result: {status: "SUCCESS", response: $r, error: null}}'
    exit 0
fi
# A result never written.
[ -e "$FAKE_AGENTS/$name.noresult" ] && exit 0
echo '{"event":"step_update","step":{"type":"agent_response","text_delta":"agy ran"}}'
result SUCCESS "" "agy ran: one-line summary"
EOF
done
cat > "$H/fakebin/queue" <<'EOF'
#!/usr/bin/env bash
[ -e "$FAKE_QUEUE_FAILS" ] && { echo "queue service unreachable" >&2; exit 1; }
printf '%s\n' "argv=$(tr '\0' ' ' < /proc/$$/cmdline)" "token=${GH_TOKEN:-unset}" > "$FAKE_QUEUE_SEEN"
case "$ISSUE_LABEL" in ai:lane) cat "$FAKE_LANE" 2>/dev/null || echo 0 ;; ai:ready) echo 0 ;; *) cat "$FAKE_WORK" 2>/dev/null || echo 1 ;; esac
EOF
# docs/FLEET.md's CLI: status exits 1 when any device failed, its document complete; dispatch reads
# the prompt from stdin, refuses an empty one, and with --wait writes the output and exits with the
# agent's status.
cat > "$H/fakebin/fleet" <<'EOF'
#!/usr/bin/env bash
cmd=$1; shift
case "$cmd" in
status)
    [ "$*" = --json ] || exit 2
    [ -e "$FAKE_FLEET_DOWN" ] && { echo "fleet: fleet.toml: no such file" >&2; exit 2; }
    cat "$FAKE_FLEET_STATUS"; exit 1 ;;
dispatch)
    printf '%s\n' "$cmd $*" >> "$FAKE_FLEET_ARGV"
    model=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --model) model=$2; shift 2 ;;
            --device|--adapter|--workdir|--timeout|--label) [ $# -ge 2 ] || exit 2; shift 2 ;;
            --wait|--json) shift ;;
            *) echo "fleet: unknown argument $1" >&2; exit 2 ;;
        esac
    done
    prompt=$(cat)
    [ -n "$prompt" ] || { echo "fleet: empty prompt" >&2; exit 125; }
    printf '%s' "$prompt" > "$FAKE_FLEET_STDIN"
    # A wait that is stopped, with a helper of its own.
    [ -e "$FAKE_FLEET_HANG" ] && { sleep 30 & wait; }
    [ "$model" = "$(cat "$FAKE_FLEET_LIMIT" 2>/dev/null)" ] && { echo "You've hit your usage limit · resets 5pm"; exit 1; }
    echo "agent output on $model"
    exit "$(cat "$FAKE_FLEET_RC" 2>/dev/null || echo 0)" ;;
*) exit 2 ;;
esac
EOF
# ssh -G prints the host an alias resolves to, and an unknown name as itself.
cat > "$H/fakebin/ssh" <<'EOF'
#!/usr/bin/env bash
[ "$1" = -G ] || exit 255
case "$2" in github-work) echo "hostname github.com" ;; *) echo "hostname $2" ;; esac
EOF
cat > "$H/fakebin/crontab" <<'EOF'
#!/usr/bin/env bash
[ -e "$FAKE_NO_CRON" ] || echo "*/2 * * * * $HARNESS_HOME/bin/tick.sh"
EOF
chmod +x "$H/fakebin/"*
git -C "$H/target" init -q
# The fixture's values, with a repository name that a naive substitution would mangle.
write_env() {
    { grep -v '^GH_REPO=\|^TARGET_REPO=\|^WORKTREE_ROOT=' "$here/tests/fixtures/instance/harness.env"
      cat <<EOF
TARGET_REPO=$H/target
WORKTREE_ROOT=$H/worktrees
GH_REPO='o/r&x|y\\z'
QUEUE_DEPTH_CMD=$H/fakebin/queue
AGENT_BIN_claude=$H/fakebin/claude
AGENT_FLAGS_claude=
MAX_SLOTS=1
USAGE_CACHE=$H/usage-cache.json
USAGE_FETCH=$H/no-fetch
USAGE_SOURCE_codex=json:$H/codex-usage.json
CLAIM_GRACE_S=0
FROM_EXPORTS=exported
TICK_EXPORTS="FROM_EXPORTS"
EOF
      printf '%s\n' "$@"; } > "$I/harness.env"
}
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH" FAKE_PROMPT="$H/prompt.txt" FAKE_ENV="$H/env.txt" \
    FAKE_ARGS="$H/args.txt" FAKE_LANE="$H/lane.txt" FAKE_LINT_ENV="$H/lint-env.txt" \
    PATH="$H/fakebin:$PATH" FAKE_QUEUE_FAILS="$H/queue-fails" FAKE_QUEUE_SEEN="$H/queue-seen.txt" FAKE_NO_CRON="$H/no-cron" \
    FAKE_FLEET_STATUS="$here/tests/fixtures/fleet/status.json" FAKE_FLEET_DOWN="$H/fleet-down" FAKE_FLEET_ARGV="$H/fleet-argv.txt" \
    FAKE_FLEET_STDIN="$H/fleet-stdin.txt" FAKE_FLEET_LIMIT="$H/fleet-limit" FAKE_FLEET_RC="$H/fleet-rc" \
    FAKE_AGENTS="$H/agents" FAKE_WORK="$H/work.txt" FAKE_FLEET_HANG="$H/fleet-hang" HOME="$H/home"
unset HARNESS_TICK FLEET_SESSION HARNESS_STATE
mkdir -p "$H/agents"
LOG="$H/state/logs/allocator.log"
usage() {  # $1 = claude's 5-hour percent; codex's one window stays at 10
    printf '{"timestamp": %s000, "data": {"fiveHourPercent": %s, "weeklyPercent": 10}}\n' "$(date +%s)" "$1" > "$H/usage-cache.json"
    printf '{"timestamp": %s000, "windows": [{"name": "five_hour", "used_pct": 10, "window_minutes": 300}]}\n' "$(date +%s)" > "$H/codex-usage.json"
}
usage 10
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
# Waits until no tick.sh of this scratch home runs: the launcher's detached runners and the forks
# that become them both carry its path. Gives up after 20 s, so a stuck runner fails its check.
settle() {
    local n=0 quiet=0
    # Three empty polls in a row: on Ubuntu 26.04 a poll right after the launcher exits has missed
    # the runner it had just started.
    while [ "$quiet" -lt 3 ] && [ "$n" -lt 400 ]; do
        if pgrep -f "$H/bin/tick.sh" >/dev/null; then quiet=0; else quiet=$((quiet + 1)); fi
        sleep 0.05; n=$((n + 1))
    done
}
launch() {  # run the launcher once and wait for the runner it may start
    rm -f "$FAKE_PROMPT" "$FAKE_ENV" "$FAKE_ARGS" "$FAKE_FLEET_ARGV" "$FAKE_FLEET_STDIN" "$H/state/logs/"tick-*.log "$FAKE_AGENTS"/*.argv; : > "$LOG"
    rm -f "$H/state/locks/heartbeat.stamp"
    "$H/bin/tick.sh"
    settle
}

write_env; mv "$I/prompts/tick.md" "$H/tick.md.away"
launch
check "a missing tick.md starts nothing" '[ ! -e "$FAKE_PROMPT" ]' "$(cat "$FAKE_PROMPT" 2>/dev/null)"
check "a missing tick.md is logged" 'grep -q "instance incomplete: $I/prompts/tick.md" "$LOG"' "$(cat "$LOG")"
"$H/bin/tick.sh"; settle
check "the incomplete instance is logged at most hourly" '[ "$(grep -c "instance incomplete" "$LOG")" -eq 1 ]' "$(cat "$LOG")"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check exits 1 on an incomplete instance and names what is missing" \
    '[ "$rc" -eq 1 ] && grep -q "missing: $I/prompts/tick.md" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
mv "$H/tick.md.away" "$I/prompts/tick.md"
mv "$I/harness.env" "$H/env.away"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check exits 1 without harness.env" '[ "$rc" -eq 1 ] && grep -q "missing: $I/harness.env" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
mv "$H/env.away" "$I/harness.env"
write_env "WORKTREE_ROOT="
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check exits 1 when a required key is empty" '[ "$rc" -eq 1 ] && grep -q "WORKTREE_ROOT unset" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
write_env
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check exits 0 on a complete instance" '[ "$rc" -eq 0 ] && grep -q "instance complete" "$H/check.txt" && grep -q "model lane:    off" "$H/check.txt" && grep -q "paused:        no" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"

check_with() {  # extra harness.env lines; leaves the output in $H/check.txt and the status in rc
    write_env "$@"; "$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
}
check_with "TARGET_REPO=$H"
check "check fails when TARGET_REPO is not a git checkout" '[ "$rc" -eq 1 ] && grep -q "missing: a git checkout at TARGET_REPO ($H)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
: > "$H/plain-file"
check_with "WORKTREE_ROOT=$H/plain-file/trees"
check "check fails when WORKTREE_ROOT cannot be created" '[ "$rc" -eq 1 ] && grep -q "missing: a WORKTREE_ROOT that exists or can be created" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "WORKTREE_ROOT=$H/not-yet/trees"
check "a WORKTREE_ROOT that can be created passes" '[ "$rc" -eq 0 ] && [ ! -e "$H/not-yet" ]' "rc=$rc $(cat "$H/check.txt")"
check_with "QUEUE_DEPTH_CMD=\"$H/plain-file --flag\""
check "check fails when QUEUE_DEPTH_CMD is not executable" '[ "$rc" -eq 1 ] && grep -q "missing: an executable QUEUE_DEPTH_CMD ($H/plain-file)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "AGENT_BIN_claude=no-such-claude"
check "check fails when an agent's binary does not resolve" '[ "$rc" -eq 1 ] && grep -q "missing: an executable AGENT_BIN_claude (no-such-claude)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "TICK_MODELS='fake-model codex:model-x'" "AGENT_BIN_codex=no-such-codex"
check "check names every agent of the model list it cannot run" '[ "$rc" -eq 1 ] && grep -q "missing: an executable AGENT_BIN_codex (no-such-codex)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "TICK_MODELS='fake-model Codex:x'"
check "check fails on a model list harness usage refuses" '[ "$rc" -eq 1 ] && grep -q "missing: a usage configuration that harness usage reads (.*TICK_MODELS" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "TICK_MODELS='fake-model codex:model-x agy:model-g'" "AGENT_BIN_codex=$H/fakebin/codex" "AGENT_BIN_agy=$H/fakebin/agy"
check "check prints each agent's command line, with the prompt on stdin, and its usage" \
    '[ "$rc" -eq 0 ] && grep -qxF "agent codex: $H/fakebin/codex exec --model <model> --dangerously-bypass-approvals-and-sandbox - < prompt" "$H/check.txt" && grep -qxF "agent agy: $H/fakebin/agy --model <model> --input-format stream-json --output-format stream-json --dangerously-skip-permissions < prompt as one stream-json line" "$H/check.txt" && grep -qxF "agent claude: $H/fakebin/claude -p --model <model> < prompt" "$H/check.txt" && grep -qx "usage:         pool: 1 of 1, room for 1 more" "$H/check.txt" && grep -q "^usage:         agy: cap 1 of 1, 0 running, none: no windows" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
# The Antigravity IDE's launcher of the same name: a VS Code-style CLI, with no --input-format.
printf '#!/usr/bin/env bash\nprintf "%%s\\n" 1.107.0 0123abc arm64\n' > "$H/fakebin/ide-agy"; chmod +x "$H/fakebin/ide-agy"
check_with "TICK_MODELS='fake-model agy:model-g'" "AGENT_BIN_agy=$H/fakebin/ide-agy"
check "check refuses an agy that is not the Antigravity CLI, naming it and the fix" \
    '[ "$rc" -eq 1 ] && grep -qF "missing: an agy that is the Antigravity CLI: $H/fakebin/ide-agy takes no --input-format, so it is another program of that name, such as the Antigravity IDE'"'"'s launcher; install the CLI, or set AGENT_BIN_agy=\$HOME/.local/bin/agy where it is" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
check_with "AGENT_BIN_agy=$H/fakebin/ide-agy"
check "an agy outside the model list is not looked at" '[ "$rc" -eq 0 ]' "rc=$rc $(cat "$H/check.txt")"
# Its --help is asked once per binary and modification time: the launcher fires every few minutes.
cat > "$H/fakebin/count-agy" <<'EOF'
#!/usr/bin/env bash
echo x >> "$FAKE_AGENTS/count-agy.help"
echo "  --input-format  Input format for print mode"
EOF
chmod +x "$H/fakebin/count-agy"; rm -f "$FAKE_AGENTS/count-agy.help"
check_with "TICK_MODELS='fake-model agy:model-g'" "AGENT_BIN_agy=$H/fakebin/count-agy"
"$H/bin/tick.sh" check >/dev/null 2>&1
check "agy's --help is asked once for one binary, kept in the state" \
    '[ "$rc" -eq 0 ] && [ "$(wc -l < "$FAKE_AGENTS/count-agy.help")" = 1 ]' "rc=$rc $(cat "$FAKE_AGENTS/count-agy.help" 2>/dev/null)"
touch -d "@$(( $(date +%s) - 60 ))" "$H/fakebin/count-agy"
"$H/bin/tick.sh" check >/dev/null 2>&1
check "and again once the binary changes" '[ "$(wc -l < "$FAKE_AGENTS/count-agy.help")" = 2 ]' "$(cat "$FAKE_AGENTS/count-agy.help" 2>/dev/null)"
# A --help that failed to answer is no verdict: the next check asks again.
cat > "$H/fakebin/mute-agy" <<'EOF'
#!/usr/bin/env bash
echo x >> "$FAKE_AGENTS/mute-agy.help"
exit 1
EOF
chmod +x "$H/fakebin/mute-agy"; rm -f "$FAKE_AGENTS/mute-agy.help"
check_with "TICK_MODELS='fake-model agy:model-g'" "AGENT_BIN_agy=$H/fakebin/mute-agy"
"$H/bin/tick.sh" check >/dev/null 2>&1
check "an agy whose --help says nothing is refused, and asked again at the next check" \
    '[ "$rc" -eq 1 ] && [ "$(wc -l < "$FAKE_AGENTS/mute-agy.help")" = 2 ]' "rc=$rc $(cat "$FAKE_AGENTS/mute-agy.help" 2>/dev/null)"
check_with "CLAUDE_FLAGS=" "WEEK_HALF_AT=64" "TICK_AGENT_EXES=claude:-p"
check "check names each key it does not read, and the key to set instead" \
    '[ "$rc" -eq 1 ] && grep -q "missing: .*CLAUDE_FLAGS is not read; set AGENT_FLAGS_claude instead" "$H/check.txt" && grep -q "WEEK_HALF_AT is not read; set USAGE_STEPS_LONG instead" "$H/check.txt" && grep -q "TICK_AGENT_EXES is not read; remove it" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
check_with "HARNESS_BIN=$H/plain-file"
check "check fails when HARNESS_BIN is not executable" '[ "$rc" -eq 1 ] && grep -q "missing: an executable HARNESS_BIN ($H/plain-file)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "GH_REPO="
check "check fails when GH_REPO is unset and the origin names none" '[ "$rc" -eq 1 ] && grep -q "GH_REPO unset" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
git -C "$H/target" remote add origin git@github.com:derived-org/derived-repo.git
check_with "GH_REPO="
check "an unset GH_REPO is derived from the origin remote" '[ "$rc" -eq 0 ] && grep -q "repository:    derived-org/derived-repo" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
launch
check "the derived GH_REPO reaches the prompt" 'sed -n 3p "$FAKE_PROMPT" | grep -q "^Core rules for derived-org/derived-repo:"' "$(cat "$FAKE_PROMPT" 2>/dev/null)"
derive() {  # origin URL, expected repository or "none"
    want_repo=$2; git -C "$H/target" remote set-url origin "$1"; check_with "GH_REPO="
    if [ "$2" = none ]; then
        check "no GH_REPO is derived from $1" '[ "$rc" -eq 1 ] && grep -q "set GH_REPO in harness.env" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
    else
        check "GH_REPO $2 is derived from $1" '[ "$rc" -eq 0 ] && grep -qx "repository:    $want_repo" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
    fi
}
derive git@github-work:alias-org/alias-repo.git alias-org/alias-repo
derive ssh://git@github-work/alias-org/alias-repo alias-org/alias-repo
derive git@no-such-alias:o/n.git none
derive file:///o/n none
derive https://github.com/https-org/https-repo.git https-org/https-repo
derive git@github.com:scp-org/scp-repo.git scp-org/scp-repo
derive https://ghe.example.com/ent-org/ent-repo ghe.example.com/ent-org/ent-repo
git -C "$H/target" remote remove origin
printf '#!/usr/bin/env bash\nexit 0\n' > "$I/hooks/worktree-busy"; chmod -x "$I/hooks/worktree-busy"
check_with
check "check reports a hook that is not executable as ignored" 'grep -q "hook worktree-busy: present but not executable (ignored)" "$H/check.txt" && grep -q "hook worktree-remove: none" "$H/check.txt"' "$(cat "$H/check.txt")"
rm -f "$I/hooks/worktree-busy"
touch "$H/state/paused"; check_with
check "check reports state/paused" 'grep -q "paused:        yes, $H/state/paused exists" "$H/check.txt"' "$(cat "$H/check.txt")"
rm -f "$H/state/paused"; touch "$FAKE_NO_CRON"; check_with
check "check reports a missing cron line as paused" 'grep -q "paused:        yes, no active crontab line" "$H/check.txt"' "$(cat "$H/check.txt")"
rm -f "$FAKE_NO_CRON"

T="$H/template-instance"; cp -r "$here/templates/instance" "$T"; mv "$T/harness.env.example" "$T/harness.env"
HARNESS_INSTANCE_DIR="$T" "$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "the template's placeholders fail check" \
    '[ "$rc" -eq 1 ] && grep -q "missing: a git checkout at TARGET_REPO (/path/to/checkout)" "$H/check.txt" && grep -q "missing: a WORKTREE_ROOT that exists or can be created (/path/to/worktrees)" "$H/check.txt" && grep -q "GH_REPO unset" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
write_env

cat > "$I/hooks/env.sh" <<'EOF'
export FROM_HOOK=sourced
EOF
launch
check "a complete instance starts a tick" '[ -s "$FAKE_PROMPT" ]' "log=$(cat "$LOG")"
check "the first line is the lane line, then a blank line" \
    '[ "$(sed -n 1p "$FAKE_PROMPT")" = "This tick runs on fake-model." ] && [ -z "$(sed -n 2p "$FAKE_PROMPT")" ]' "$(cat "$FAKE_PROMPT")"
check "the core rules follow, with keys substituted literally" \
    '[ "$(sed -n 3p "$FAKE_PROMPT")" = "Core rules for o/r&x|y\\z: budget 120 min, lane \`\`, {{UNKNOWN}} stays." ]' "$(sed -n 3p "$FAKE_PROMPT")"
check "the installation section comes last" \
    '[ "$(sed -n 5p "$FAKE_PROMPT")" = "## This installation" ] && tail -1 "$FAKE_PROMPT" | grep -qF "Fake installation rules for o/r&x|y\\z: work only issues labelled \`ai\`."' "$(cat "$FAKE_PROMPT")"
check "hooks/env.sh exports and TICK_EXPORTS reach the tick" '[ "$(cat "$FAKE_ENV")" = "sourced exported" ]' "$(cat "$FAKE_ENV" 2>/dev/null)"

write_env "PUBLIC_RECORD_RULES=$H/rules.json" "PUBLIC_RECORD_PR_SECTIONS='Summary,Why'" \
    "PUBLIC_RECORD_ISSUE_SECTIONS=Summary" "WIP_LABEL=bot:wip" "LOW_LABEL=bot:low" "GH_TOKEN=secret-token"
GH_TOKEN=env-token launch
check "the lint's settings and the labels reach the tick, GH_TOKEN does not" \
    '[ "$(cat "$FAKE_LINT_ENV")" = "$(printf "%s\n" "rules=$H/rules.json pr=Summary,Why" "issue=Summary repo=o/r&x|y\\z wip=bot:wip" "low=bot:low token=unset")" ]' \
    "$(cat "$FAKE_LINT_ENV" 2>/dev/null)"
check "HARNESS_BIN reaches the tick, for the lint" '[ "$(cat "$FAKE_LINT_ENV.bin")" = "$HARNESS_BIN" ]' "$(cat "$FAKE_LINT_ENV.bin" 2>/dev/null)"
check "the queue count gets the config's GH_TOKEN in its environment, never in its argv" \
    'grep -qx "token=secret-token" "$FAKE_QUEUE_SEEN" && grep -q "^argv=.*queue" "$FAKE_QUEUE_SEEN" && ! grep "^argv=" "$FAKE_QUEUE_SEEN" | grep -q secret-token' \
    "$(cat "$FAKE_QUEUE_SEEN" 2>/dev/null)"
write_env

touch "$FAKE_QUEUE_FAILS"
launch
check "a failed queue count is logged as an error and a tick still runs" \
    'grep -q "ERROR queue count failed (label ai: exit 1, output .*, stderr .queue service unreachable.); counting 1 claimable issue" "$LOG" && [ -s "$FAKE_PROMPT" ]' "log=$(cat "$LOG")"
printf '#!/usr/bin/env bash\n' > "$H/fakebin/silent-queue"; chmod +x "$H/fakebin/silent-queue"
write_env "QUEUE_DEPTH_CMD=$H/fakebin/silent-queue"
launch
check "an empty queue count is logged as an error, not read as a count" \
    'grep -q "ERROR queue count failed (label ai: exit 0, output .., " "$LOG"' "log=$(cat "$LOG")"
rm -f "$FAKE_QUEUE_FAILS"
write_env

printf 'false\n' > "$I/hooks/env.sh"
launch
check "a failing env.sh is logged and the tick still runs" \
    'grep -q "hooks/env.sh failed (exit 1)" "$LOG" && [ "$(cat "$FAKE_ENV")" = "unset exported" ]' "log=$(cat "$LOG") env=$(cat "$FAKE_ENV" 2>/dev/null)"
rm -f "$I/hooks/env.sh"

write_env "LANE_MODEL=lane-model" "LANE_LABEL=ai:lane"; echo 1 > "$FAKE_LANE"
launch
check "a claimable lane issue runs on the lane model" \
    'head -1 "$FAKE_PROMPT" | grep -q "^This tick runs on lane-model, the model lane: claim only an issue labelled \`ai:lane\`" && grep -q -- "--model lane-model" "$FAKE_ARGS"' "$(head -1 "$FAKE_PROMPT" 2>/dev/null)"
echo 0 > "$FAKE_LANE"
launch
check "with no lane issue, the tick leaves lane issues alone" \
    'head -1 "$FAKE_PROMPT" | grep -q "^This tick runs on fake-model: never claim an issue labelled \`ai:lane\`"' "$(head -1 "$FAKE_PROMPT" 2>/dev/null)"
write_env
write_env "LANE_MODEL=claude:lane-model" "LANE_LABEL=ai:lane" "TICK_MODELS=codex:model-x" "AGENT_BIN_codex=$H/fakebin/codex"
echo 1 > "$FAKE_LANE"; usage 80
launch
check "a lane that cannot run opens its issues to the work ticks" \
    'head -1 "$FAKE_AGENTS/codex.stdin" | grep -qx "This tick runs on model-x. lane-model cannot run now, on its usage limit or windows, so an issue labelled \`ai:lane\` may be claimed too." && grep -q "claude:lane-model cannot run" "$LOG" && grep -q "^[^ ]* codex=1\[five_hour:10%\] pool=1 room=1 " "$LOG"' \
    "log=$(cat "$LOG") stdin=$(head -1 "$FAKE_AGENTS/codex.stdin" 2>/dev/null)"
usage 10; echo 0 > "$FAKE_LANE"
write_env

printf '#!/usr/bin/env bash\necho "service down since noon"\necho second line\nexit 0\n' > "$I/hooks/hold"; chmod +x "$I/hooks/hold"
launch
"$H/bin/tick.sh"; settle
check "a hold hook that exits 0 holds the harness" '[ ! -e "$FAKE_PROMPT" ]' "$(cat "$FAKE_PROMPT" 2>/dev/null)"
check "the hold reason is logged once" '[ "$(grep -c "held: service down since noon$" "$LOG")" -eq 1 ] && [ "$(wc -l < "$LOG")" -eq 1 ]' "$(cat "$LOG")"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1
check "check reports the hold" 'grep -q "held: service down since noon" "$H/check.txt"' "$(cat "$H/check.txt")"
printf '#!/usr/bin/env bash\necho "not today"\nexit 1\n' > "$I/hooks/hold"
launch
check "a hold hook that exits 1 holds nothing" '[ -s "$FAKE_PROMPT" ] && ! grep -q held "$LOG"' "log=$(cat "$LOG")"
printf '#!/usr/bin/env bash\necho "stuck"\nsleep 30\nexit 0\n' > "$I/hooks/hold"
write_env "HOLD_TIMEOUT_S=1"
start=$(date +%s); launch; took=$(( $(date +%s) - start ))
check "a hold hook that hangs past HOLD_TIMEOUT_S holds nothing" '[ -s "$FAKE_PROMPT" ] && ! grep -q held "$LOG" && [ "$took" -lt 10 ]' "took=${took}s log=$(cat "$LOG")"
rm -f "$I/hooks/hold"

check "the default path runs claude, not fleet" '[ -s "$FAKE_PROMPT" ] && [ ! -e "$FAKE_FLEET_ARGV" ]' "prompt=$(cat "$FAKE_PROMPT" 2>/dev/null)"
fleet_env() { write_env "FLEET_DISPATCH=1" "FLEET_BIN=$H/fakebin/fleet" "LONG_TICK_BUDGET_MIN=90" "$@"; }
fleet_env
launch
check "FLEET_DISPATCH runs fleet dispatch with the argv the contract gives, and not claude" \
    '[ "$(cat "$FAKE_FLEET_ARGV")" = "dispatch --device auto --adapter claude --workdir $H/target --model fake-model --wait --timeout 7200" ] && [ ! -e "$FAKE_PROMPT" ]' \
    "argv=$(cat "$FAKE_FLEET_ARGV" 2>/dev/null) log=$(cat "$LOG")"
check "the prompt goes on stdin, never into argv" \
    '[ "$(head -1 "$FAKE_FLEET_STDIN")" = "This tick runs on fake-model." ] && grep -q "^Core rules for" "$FAKE_FLEET_STDIN" && ! grep -q "Core rules" "$FAKE_FLEET_ARGV"' \
    "stdin=$(cat "$FAKE_FLEET_STDIN" 2>/dev/null)"
echo 7 > "$FAKE_FLEET_RC"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "the dispatched agent's exit code and output reach the tick log" \
    'grep -q "^agent output on fake-model$" "$tick_log" && grep -q "^\[tick.sh\] exit=7 duration=.*model=fake-model$" "$tick_log"' \
    "$(cat "$tick_log" 2>/dev/null)"
rm -f "$FAKE_FLEET_RC"
fleet_env "TICK_MODELS='model-a model-b'"; echo model-a > "$FAKE_FLEET_LIMIT"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "a usage limit in the dispatched output cools the model and the tick re-runs on the next" \
    '[ -e "$H/state/model-cooldown/claude:model-a" ] && grep -q "claude:model-a hit its usage limit" "$LOG" && [ "$(grep -o -- "--model [^ ]*" "$FAKE_FLEET_ARGV" | tr "\n" " ")" = "--model model-a --model model-b " ] && grep -q "^\[tick.sh\] exit=0 .*model=model-b$" "$tick_log"' \
    "argv=$(cat "$FAKE_FLEET_ARGV" 2>/dev/null) log=$(cat "$LOG")"
rm -rf "$FAKE_FLEET_LIMIT" "$H/state/model-cooldown"
fleet_env "TICK_AGENT=codex" "FLEET_DEVICE=dev-a"
launch
check "TICK_AGENT and FLEET_DEVICE reach the dispatch" 'grep -q -- "^dispatch --device dev-a --adapter codex " "$FAKE_FLEET_ARGV"' "$(cat "$FAKE_FLEET_ARGV" 2>/dev/null)"
fleet_env "TICK_MODELS='aider:model-g'" "USAGE_SOURCE_aider=none"
launch
check "through fleet, an entry's agent is the adapter" 'grep -q -- "^dispatch --device auto --adapter aider .*--model model-g " "$FAKE_FLEET_ARGV"' "$(cat "$FAKE_FLEET_ARGV" 2>/dev/null)"
fleet_env "TICK_MODELS='agy:model-g'"
launch
check "through fleet, agy's prompt is dispatched as its one stream-json line" \
    'grep -q -- "^dispatch --device auto --adapter agy .*--model model-g " "$FAKE_FLEET_ARGV" && [ "$(wc -l < "$FAKE_FLEET_STDIN")" -le 1 ] && jq -e ".event == \"user\" and (.message.content | startswith(\"This tick runs on model-g.\"))" "$FAKE_FLEET_STDIN" >/dev/null' \
    "$(head -c 300 "$FAKE_FLEET_STDIN" 2>/dev/null)"
fleet_env
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check reports fleet's devices and passes with one that can take a tick" \
    '[ "$rc" -eq 0 ] && grep -q "^dispatch:      fleet: $H/fakebin/fleet dispatch --device auto" "$H/check.txt" && grep -qx "fleet device:  dev-a (tls) reachable, claude 1/2 running" "$H/check.txt" && grep -qx "fleet device:  dev-b (ssh) unreachable: unreachable: ssh exited 255" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
touch "$FAKE_FLEET_DOWN"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check fails when fleet status fails" \
    '[ "$rc" -eq 1 ] && grep -q "gave no status document" "$H/check.txt" && grep -qx "missing: a fleet device that can take a claude tick" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
rm -f "$FAKE_FLEET_DOWN"
fleet_env "FLEET_DEVICE=dev-b"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check fails when FLEET_DEVICE names a device that cannot be reached" '[ "$rc" -eq 1 ]' "rc=$rc $(cat "$H/check.txt")"
fleet_env "FLEET_BIN=no-such-fleet"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check fails when FLEET_BIN does not resolve" '[ "$rc" -eq 1 ] && grep -q "missing: an executable FLEET_BIN (no-such-fleet)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
fleet_env "TICK_MODELS='fake-model agy:model-g'"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check names each agent no fleet device offers" \
    '[ "$rc" -eq 1 ] && grep -qx "fleet device:  dev-a (tls) reachable, claude 1/2 running, no agy adapter" "$H/check.txt" && grep -qx "missing: a fleet device that can take a agy tick" "$H/check.txt" && ! grep -q "take a claude tick" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
write_env "TICK_AGENT=aider"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check fails when an agent without a local profile runs without fleet" \
    '[ "$rc" -eq 1 ] && grep -q "agent aider needs FLEET_DISPATCH=1" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
rm -f "$H/state/locks/instance-incomplete.stamp"
launch
check "the launcher refuses an agent without a local profile and logs why" \
    '[ ! -e "$FAKE_PROMPT" ] && [ ! -e "$FAKE_FLEET_ARGV" ] && grep -q "instance incomplete: .*agent aider needs FLEET_DISPATCH=1" "$LOG"' "log=$(cat "$LOG")"
write_env

agents_env() {
    write_env "TICK_MODELS='claude:model-c codex:model-x agy:model-g'" "AGENT_BIN_codex=$H/fakebin/codex" \
        "AGENT_BIN_agy=$H/fakebin/agy" "MODEL_COOLDOWN_S=600" "$@"
}
agents_env
launch
check "the first entry runs on its agent with the prompt on stdin" \
    '[ "$(cat "$FAKE_AGENTS/claude.argv")" = "-p --model model-c" ] && head -1 "$FAKE_PROMPT" | grep -qx "This tick runs on model-c." && [ ! -e "$FAKE_AGENTS/codex.argv" ]' \
    "$(cat "$FAKE_AGENTS/claude.argv" 2>/dev/null); $(head -1 "$FAKE_PROMPT" 2>/dev/null)"
touch "$FAKE_AGENTS/claude.limit" "$FAKE_AGENTS/codex.limit"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "a limit message from each agent cools that entry and the tick moves on to the next agent" \
    '[ -e "$H/state/model-cooldown/claude:model-c" ] && [ -e "$H/state/model-cooldown/codex:model-x" ] && [ ! -e "$H/state/model-cooldown/agy:model-g" ] && grep -q "codex:model-x hit its usage limit" "$LOG" && grep -q "^\[tick.sh\] exit=0 .*agent=agy model=model-g$" "$tick_log"' \
    "log=$(cat "$LOG") tick=$(cat "$tick_log" 2>/dev/null)"
check "codex and agy get their argv, with the prompt on stdin and never in argv" \
    '[ "$(cat "$FAKE_AGENTS/codex.argv")" = "exec --model model-x --dangerously-bypass-approvals-and-sandbox -" ] && [ "$(cat "$FAKE_AGENTS/agy.argv")" = "--model model-g --input-format stream-json --output-format stream-json --dangerously-skip-permissions" ] && grep -q "^Core rules for" "$FAKE_AGENTS/codex.stdin" && ! grep -q "Core rules\|This tick" "$FAKE_AGENTS"/*.argv' \
    "$(head -c 300 "$FAKE_AGENTS"/*.argv)"
check "agy's prompt is one stream-json line: a user message whose content is the whole prompt" \
    '[ "$(wc -l < "$FAKE_AGENTS/agy.stdin")" -eq 1 ] && jq -e ".event == \"user\" and (.message.content | startswith(\"This tick runs on model-g.\n\")) and (.message.content | contains(\"Core rules for o/r&x|y\\\\z\"))" "$FAKE_AGENTS/agy.stdin" >/dev/null' \
    "$(head -c 300 "$FAKE_AGENTS/agy.stdin")"
check "the tick log keeps agy's events and gives its result's response as the summary" \
    'grep -qx "{\"event\":\"init\",\"init\":{\"tools\":\[\"command\",\"read_file\"\]}}" "$tick_log" && grep -qx "agy ran: one-line summary" "$tick_log"' "$(cat "$tick_log" 2>/dev/null)"
check "a tick and every tool it runs take SIGINT and SIGQUIT" \
    '[ $(( 16#$(cat "$FAKE_AGENTS/agy.sigign") & 6 )) -eq 0 ] && [ $(( 16#$(cat "$FAKE_AGENTS/codex.sigign") & 6 )) -eq 0 ]' \
    "agy=$(cat "$FAKE_AGENTS/agy.sigign" 2>/dev/null) codex=$(cat "$FAKE_AGENTS/codex.sigign" 2>/dev/null)"
read -r g_pid g_pgid g_sid g_runner < "$FAKE_AGENTS/agy.group"
check "a tick leads a process group and session of its own, apart from its runner's" \
    '[ "$g_pgid" = "$g_pid" ] && [ "$g_sid" = "$g_pid" ] && [ "$g_runner" != "$g_pid" ]' "$(cat "$FAKE_AGENTS/agy.group")"
check "every agent's tick carries its marker and never GH_TOKEN" \
    '[ "$(cat "$FAKE_AGENTS/agy.marker")" = "state=$H/state slot=1 agent=agy model=model-g kind=work" ] && grep -qx "token=unset" "$FAKE_AGENTS/codex.env" && grep -qx "token=unset" "$FAKE_AGENTS/agy.env"' \
    "$(cat "$FAKE_AGENTS/agy.marker" 2>/dev/null)"
launch
check "while both cool down the next tick starts on agy at once" \
    '[ ! -e "$FAKE_AGENTS/claude.argv" ] && [ ! -e "$FAKE_AGENTS/codex.argv" ] && [ -s "$FAKE_AGENTS/agy.argv" ]' "log=$(cat "$LOG")"
rm -rf "$H/state/model-cooldown" "$FAKE_AGENTS"/*.limit
usage 80
launch
check "an agent whose windows allow no tick is passed over for the next agent" \
    '[ ! -e "$FAKE_AGENTS/claude.argv" ] && [ "$(cat "$FAKE_AGENTS/codex.argv")" = "exec --model model-x --dangerously-bypass-approvals-and-sandbox -" ] && grep -q "claude=0\[five_hour:80%,weekly:10%\] codex=1\[five_hour:10%\] agy=1\[\] pool=1 room=1 " "$LOG"' \
    "log=$(cat "$LOG")"
usage 10
agents_env "AGENT_FLAGS_codex='--skip-git-repo-check -m other --model=x --model y -mz *'" "TICK_MODELS=codex:model-x"
launch
check "AGENT_FLAGS_<agent> replaces the agent's flags, never globbed, every model flag dropped" \
    '[ "$(cat "$FAKE_AGENTS/codex.argv")" = "exec --model model-x --skip-git-repo-check * -" ]' "$(cat "$FAKE_AGENTS/codex.argv" 2>/dev/null)"
agents_env "AGENT_FLAGS_agy='--mode accept-edits -p foo --prompt x --print y --print=z --prompt=w -p=v -pq --output-format text --input-format=text --model x'" "TICK_MODELS=agy:model-g"
launch
check "agy's flags may not undo how its profile reads the prompt and writes its events, nor leave a prompt in argv" \
    '[ "$(cat "$FAKE_AGENTS/agy.argv")" = "--model model-g --input-format stream-json --output-format stream-json --mode accept-edits" ]' "$(cat "$FAKE_AGENTS/agy.argv" 2>/dev/null)"
agents_env "TICK_MODELS='agy:model-g codex:model-x'"
touch "$FAKE_AGENTS/agy.limit"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "agy's ERROR result naming a quota cools its entry and the tick moves on to the next agent" \
    '[ -e "$H/state/model-cooldown/agy:model-g" ] && grep -q "agy:model-g hit its usage limit" "$LOG" && [ -s "$FAKE_AGENTS/codex.argv" ] && grep -q "^\[tick.sh\] exit=1 .*agent=agy model=model-g$" "$tick_log"' \
    "log=$(cat "$LOG") tick=$(cat "$tick_log" 2>/dev/null)"
rm -rf "$H/state/model-cooldown" "$FAKE_AGENTS"/*.limit
agents_env "AGENT_LIMIT_RE_agy='custom quota'" "TICK_MODELS='agy:model-g codex:model-x'"
touch "$FAKE_AGENTS/agy.limit"
launch
check "AGENT_LIMIT_RE_<agent> replaces the agent's limit message" \
    '[ ! -e "$H/state/model-cooldown/agy:model-g" ] && [ ! -e "$FAKE_AGENTS/codex.argv" ]' "log=$(cat "$LOG")"
rm -rf "$H/state/model-cooldown" "$FAKE_AGENTS"/*.limit
agents_env "TICK_MODELS='agy:model-g codex:model-x'"
touch "$FAKE_AGENTS/agy.error"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "agy's ERROR result on anything but a limit fails the tick and cools nothing" \
    '[ ! -e "$H/state/model-cooldown/agy:model-g" ] && [ ! -e "$FAKE_AGENTS/codex.argv" ] && grep -q "^\[tick.sh\] exit=1 .*agent=agy model=model-g$" "$tick_log"' \
    "log=$(cat "$LOG") tick=$(cat "$tick_log" 2>/dev/null)"
rm -f "$FAKE_AGENTS/agy.error"
touch "$FAKE_AGENTS/agy.quoted"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "a quota error quoted in a SUCCESS turn is no usage limit" \
    '[ ! -e "$H/state/model-cooldown/agy:model-g" ] && grep -q "RESOURCE_EXHAUSTED" "$tick_log" && [ ! -e "$FAKE_AGENTS/codex.argv" ] && grep -q "^\[tick.sh\] exit=0 .*agent=agy" "$tick_log"' \
    "log=$(cat "$LOG") tick=$(cat "$tick_log" 2>/dev/null)"
rm -f "$FAKE_AGENTS/agy.quoted"
touch "$FAKE_AGENTS/agy.inject"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "a SUCCESS response shaped as an ERROR result event is the summary, never a usage limit" \
    '[ ! -e "$H/state/model-cooldown/agy:model-g" ] && [ ! -e "$FAKE_AGENTS/codex.argv" ] && grep -qx "{\"event\":\"result\",\"result\":{\"status\":\"ERROR\",\"error\":\"RESOURCE_EXHAUSTED quota\"}}" "$tick_log" && grep -q "^\[tick.sh\] exit=0 .*agent=agy model=model-g$" "$tick_log" && [ "$(grep -c "^\[tick.sh\]" "$tick_log")" = 1 ]' \
    "log=$(cat "$LOG") tick=$(cat "$tick_log" 2>/dev/null)"
rm -f "$FAKE_AGENTS/agy.inject"
touch "$FAKE_AGENTS/agy.trace"
launch
check "an ERROR whose trace id holds 429 is no usage limit" \
    '[ ! -e "$H/state/model-cooldown/agy:model-g" ] && [ ! -e "$FAKE_AGENTS/codex.argv" ]' "log=$(cat "$LOG")"
rm -f "$FAKE_AGENTS/agy.trace"
touch "$FAKE_AGENTS/agy.noresult"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "an agy turn that writes no result failed, whatever agy exited with" \
    'grep -q "^\[tick.sh\] exit=1 .*agent=agy model=model-g$" "$tick_log"' "tick=$(cat "$tick_log" 2>/dev/null)"
rm -f "$FAKE_AGENTS/agy.noresult"
rm -rf "$H/state/model-cooldown"
agents_env "TICK_MODELS='claude:model-c codex:model-x'"
touch "$FAKE_AGENTS/claude.echo"
launch
check "a limit sentence quoted mid-output, with more output after it, cools nothing" \
    '[ ! -e "$H/state/model-cooldown/claude:model-c" ] && [ ! -e "$FAKE_AGENTS/codex.argv" ] && grep -q "session limit" "$H/state/logs/"tick-*.log' "log=$(cat "$LOG")"
rm -f "$FAKE_AGENTS/claude.echo"
agents_env "TICK_MODELS=codex:model-x"
touch "$FAKE_AGENTS/codex.orphan"
launch
orphan=$(cat "$FAKE_AGENTS/codex.orphan")
check "a tool child that outlives its tick is no tick" \
    'kill -0 "$orphan" 2>/dev/null && [ -z "$("$HARNESS_BIN" ticks --state "$H/state")" ] && [ ! -e "$H/state/locks/slot1.tick" ]' \
    "orphan=$orphan ticks=$("$HARNESS_BIN" ticks --state "$H/state")"
kill "$orphan" 2>/dev/null; rm -f "$FAKE_AGENTS/codex.orphan"

holders=()
trap 'for p in "${holders[@]}"; do kill "$p" 2>/dev/null; done; rm -rf "$H"' EXIT
live() {  # $1 slot, $2 agent, $3 kind: holds the slot and records the holder as its tick
    local stat; local -a f
    flock -o "$H/state/locks/slot$1.lock" sleep 60 >/dev/null 2>&1 & holders+=($!)
    until ! flock -n "$H/state/locks/slot$1.lock" true; do sleep 0.05; done
    stat=$(cat "/proc/$!/stat"); read -ra f <<<"${stat##*) }"
    printf '%s %s %s %s m %s\n' "$!" "${f[19]}" "$(cat /proc/sys/kernel/random/boot_id)" "$2" "$3" > "$H/state/locks/slot$1.tick"
}
release() { for p in "${holders[@]}"; do kill "$p" 2>/dev/null; wait "$p" 2>/dev/null; done; holders=(); rm -f "$H"/state/locks/slot*.tick; }
# claude at its cap 2 (65 %, half of 4) with two live ticks: the next tick runs on codex.
usage 65
agents_env "MAX_SLOTS=4" "TICK_MODELS='claude:model-c codex:model-x'"
live 1 claude work; live 2 claude work
launch
check "an agent whose live ticks fill its cap is passed over for the next agent" \
    '[ ! -e "$FAKE_AGENTS/claude.argv" ] && [ -s "$FAKE_AGENTS/codex.argv" ] && grep -q "claude=2\[five_hour:65%,weekly:10%\] codex=4\[five_hour:10%\] pool=4 room=4 busy=2 claimable=1 -> spawning 1" "$LOG"' \
    "log=$(cat "$LOG")"
release
# claude past its cap (0) with four ticks still running takes nothing of codex's room.
usage 80
agents_env "MAX_SLOTS=6" "TICK_MODELS='claude:model-c codex:model-x'"
for slot in 1 2 3 4; do live "$slot" claude work; done
launch
check "one agent's ticks past its cap leave another agent its room" \
    '[ -s "$FAKE_AGENTS/codex.argv" ] && grep -q "claude=0\[five_hour:80%,weekly:10%\] codex=6\[five_hour:10%\] pool=6 room=6 busy=4 claimable=1 -> spawning 1" "$LOG"' \
    "log=$(cat "$LOG")"
release
# An agent each of whose entries cools down adds nothing, though its ticks still run: claude at 75 %
# has room for one, and codex's two live ticks take none of it.
usage 75
agents_env "MAX_SLOTS=4" "TICK_MODELS='claude:model-c codex:model-x'"
mkdir -p "$H/state/model-cooldown"; touch "$H/state/model-cooldown/codex:model-x"
live 1 codex work; live 2 codex work
launch
check "a cooling agent's live ticks hold slots but no room" \
    '[ -s "$FAKE_AGENTS/claude.argv" ] && grep -q "claude=1\[five_hour:75%,weekly:10%\] codex=4\[five_hour:10%\] pool=1 room=1 busy=2 " "$LOG"' "log=$(cat "$LOG")"
release; rm -rf "$H/state/model-cooldown"
# The lane's own agent at its cap with a lane tick running: no lane tick, and the pool leaves it out.
usage 10
printf '{"timestamp": %s000, "windows": [{"name": "five_hour", "used_pct": 75, "window_minutes": 300}]}\n' "$(date +%s)" > "$H/codex-usage.json"
agents_env "MAX_SLOTS=4" "TICK_MODELS=claude:model-c" "LANE_MODEL=codex:lane-x" "LANE_LABEL=ai:lane"
echo 1 > "$FAKE_LANE"
live 1 codex lane
launch
check "a lane-only agent at its cap runs no lane tick and sizes no pool" \
    '[ ! -e "$FAKE_AGENTS/codex.argv" ] && head -1 "$FAKE_PROMPT" | grep -q "^This tick runs on model-c: never claim" && grep -q "^[^ ]* claude=4\[five_hour:10%,weekly:10%\] pool=4 room=4 busy=1 " "$LOG"' \
    "log=$(cat "$LOG") prompt=$(head -1 "$FAKE_PROMPT" 2>/dev/null)"
release; echo 0 > "$FAKE_LANE"; usage 10
agents_env "MAX_SLOTS=2" "CLAIM_GRACE_S=360" "TICK_MODELS=claude:model-c"
live 1 claude work
launch
check "a young live tick stands for a queued issue, so none is left to start for" \
    '[ ! -e "$FAKE_AGENTS/claude.argv" ] && ! grep -q spawning "$LOG"' "log=$(cat "$LOG")"
release
agents_env "MAX_SLOTS=3" "CLAIM_GRACE_S=360" "TICK_MODELS=claude:model-c" "LANE_MODEL=claude:lane-model" "LANE_LABEL=ai:lane"
echo 2 > "$FAKE_WORK"; echo 1 > "$FAKE_LANE"
live 1 claude lane
launch
check "a young lane tick stands for the lane's queued issue, so the next tick leaves the lane alone" \
    'head -1 "$FAKE_PROMPT" | grep -q "^This tick runs on model-c: never claim an issue labelled" && grep -q "spawning 1$" "$LOG"' \
    "log=$(cat "$LOG") prompt=$(head -1 "$FAKE_PROMPT" 2>/dev/null)"
release
agents_env "MAX_SLOTS=3" "TICK_MODELS=claude:model-c" "LANE_MODEL=claude:lane-model" "LANE_LABEL=ai:lane"
echo 1 > "$FAKE_WORK"
live 1 claude lane
launch
check "an old lane tick has claimed, so the lane's queued issue gets a lane tick" \
    'head -1 "$FAKE_PROMPT" | grep -q "^This tick runs on lane-model, the model lane"' "prompt=$(head -1 "$FAKE_PROMPT" 2>/dev/null)"
release; rm -f "$FAKE_WORK"; echo 0 > "$FAKE_LANE"
# Chaining: a short tick chains when CHAIN_MIN_TICK_S is 0. The codex tick ends claude's cooldown,
# so the chained tick runs on claude, the better entry: the next tick may be another agent's.
mkdir -p "$H/state/model-cooldown"; touch "$H/state/model-cooldown/claude:model-c"
echo "$H/state/model-cooldown/claude:model-c" > "$FAKE_AGENTS/codex.uncool"
agents_env "TICK_MODELS='claude:model-c codex:model-x'" "CHAIN_MIN_TICK_S=0" "MAX_CHAIN=2" "MAX_SLOTS=2"
launch
check "a chained tick may run on another agent" \
    '[ "$(wc -l < "$FAKE_AGENTS/codex.argv")" -eq 1 ] && [ "$(wc -l < "$FAKE_AGENTS/claude.argv")" -eq 1 ] && grep -q "slot1 chains tick 2" "$LOG"' \
    "log=$(cat "$LOG")"
# claude at 75 % has room for one tick, which a live claude tick in the other slot takes, so the
# chained tick, picking past claude at its cap once codex ends its cooldown, runs on codex again.
usage 75; touch "$H/state/model-cooldown/claude:model-c"
agents_env "TICK_MODELS='claude:model-c codex:model-x'" "CHAIN_MIN_TICK_S=0" "MAX_CHAIN=2" "MAX_SLOTS=2"
live 2 claude work
launch
check "a chained tick's pick respects the cap of the agent it picks" \
    '[ ! -e "$FAKE_AGENTS/claude.argv" ] && [ "$(wc -l < "$FAKE_AGENTS/codex.argv")" -eq 2 ] && grep -q "slot1 chains tick 2" "$LOG"' \
    "log=$(cat "$LOG") claude=$(cat "$FAKE_AGENTS/claude.argv" 2>/dev/null)"
release; rm -f "$FAKE_AGENTS"/codex.uncool
rm -rf "$H/state/model-cooldown"; usage 10
printf '{"timestamp": 1000, "data": {"fiveHourPercent": 10, "weeklyPercent": 10}}\n' > "$H/usage-cache.json"
rm -f "$H/state/locks/usage-unknown.stamp"
agents_env "TICK_MODELS=claude:model-c"
launch
check "a stale reading starts nothing and is logged" \
    '[ ! -e "$FAKE_AGENTS/claude.argv" ] && grep -q "no fresh usage reading for claude; none of its ticks start" "$LOG"' "log=$(cat "$LOG")"
usage 10
agents_env "TICK_MODELS='claude:model-c codex:model-x'"
mkdir -p "$H/state/model-cooldown"; touch "$H/state/model-cooldown/claude:model-c" "$H/state/model-cooldown/codex:model-x"
launch
check "with every entry cooling down the launcher stops early and quietly" '[ ! -s "$LOG" ] && [ ! -e "$FAKE_AGENTS/claude.argv" ]' "log=$(cat "$LOG")"
rm -rf "$H/state/model-cooldown"
check_with "TICK_MODELS='fake-model aider:model-g'" "FLEET_DISPATCH=1" "FLEET_BIN=$H/fakebin/fleet"
check "an agent without a default source must name one" \
    '[ "$rc" -eq 1 ] && grep -q "missing: .*USAGE_SOURCE_aider unset: name aider.s usage source, or none" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "TICK_MODELS='fake-model agy:model-g'" "AGENT_BIN_agy=$H/fakebin/agy"
check "agy needs no source: only its usage-limit cooldown gates it" '[ "$rc" -eq 0 ]' "rc=$rc $(cat "$H/check.txt")"
mkdir -p "$H/oldenv"
printf '#!/bin/sh\ncase "$1" in --default-signal*) echo "env: unrecognized option" >&2; exit 125 ;; esac\nexec /usr/bin/env "$@"\n' > "$H/oldenv/env"
chmod +x "$H/oldenv/env"
check_with "HARNESS_PATH=$H/oldenv:$H/fakebin:$PATH"
check "an env without --default-signal is named as missing" \
    '[ "$rc" -eq 1 ] && grep -q "missing: an env that takes --default-signal (GNU coreutils 8.31 or later)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
rm -f "$H/state/locks/instance-incomplete.stamp"
launch
check "the launcher logs that env as an incomplete instance and starts nothing" \
    'grep -q "instance incomplete: an env that takes --default-signal" "$LOG" && [ ! -e "$FAKE_PROMPT" ]' "log=$(cat "$LOG")"
write_env
check_with "AGENT_LIMIT_RE_claude="
check "an empty limit pattern is refused" '[ "$rc" -eq 1 ] && grep -q "missing: .*AGENT_LIMIT_RE_claude is empty" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
write_env
CLAUDE_FLAGS=--from-the-environment "$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "an old key in the environment, not in harness.env, is not this config's" '[ "$rc" -eq 0 ]' "rc=$rc $(cat "$H/check.txt")"
write_env

# Stopping a tick: TERM to its process group ends the agent and its tool, never the runner, which
# writes the trailer. $1 names the check; the tick is the one slot 1 records.
stop_tick() {
    local n=0 pid runner
    rm -f "$H/state/logs/"tick-*.log; : > "$LOG"
    "$H/bin/tick.sh"
    until [ -s "$H/state/locks/slot1.tick" ] || [ "$n" -ge 200 ]; do sleep 0.05; n=$((n + 1)); done
    read -r pid _ < "$H/state/locks/slot1.tick"
    runner=$(pgrep -f "$H/bin/tick.sh runner" | head -1)
    sleep 0.3
    kill -TERM -- "-$pid"
    sleep 0.5
    tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
    check "$1" \
        '[ -n "$runner" ] && [ "$runner" != "$pid" ] && grep -q "^\[tick.sh\] exit=143 " "$tick_log"' \
        "pid=$pid runner=$runner log=$(cat "$tick_log" 2>/dev/null)"
    settle
}
agents_env "TICK_MODELS=codex:model-x"
touch "$FAKE_AGENTS/codex.hang"
stop_tick "TERM to a local tick's group stops it, and its runner writes the trailer"
rm -f "$FAKE_AGENTS/codex.hang"
fleet_env
touch "$FAKE_FLEET_HANG"
stop_tick "TERM to a fleet dispatch's group stops the wait, and its runner writes the trailer"
rm -f "$FAKE_FLEET_HANG"
write_env

mv "$I" "$H/elsewhere"
HARNESS_INSTANCE_DIR="$H/elsewhere" "$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "HARNESS_INSTANCE_DIR names the instance" '[ "$rc" -eq 0 ] && grep -q "config:        $H/elsewhere/harness.env" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
mv "$H/elsewhere" "$I"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
