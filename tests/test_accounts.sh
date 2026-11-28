#!/usr/bin/env bash
# Exercises several accounts per agent CLI (AGENT_ACCOUNTS_DIR, agent@account:model entries) in a
# scratch harness home. Fake claude, codex and gemini record the folder variable each tick ran with,
# its marker, its slot record and its usage check, and end on their usage-limit message while
# <account folder>/limit exists; claude's `auth status --json` and codex's `login status` answer as
# the real CLIs do, signed out while <account folder>/signed-out exists (codex: while it has no
# auth.json). A fake fetch records what it ran with. With accounts off, a run must be what it was
# before them (fixtures/accounts/off-state.txt and off-state-flow.txt).
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${HARNESS_BIN:?tests/run.sh sets it}"
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
A="$H/agent-accounts"
mkdir -p "$H/bin" "$H/lib" "$H/libexec" "$H/prompts" "$H/instance/prompts" "$H/state" "$H/target" "$H/fakebin" \
    "$H/seen" "$H/home" "$A/claude/claude-work" "$A/claude/claude-home" "$A/codex/codex-work/sessions/2026/11/28" \
    "$A/gemini/gemini-lab"
cp "$here/bin/tick.sh" "$H/bin/"; cp "$here/lib/instance.sh" "$H/lib/"; cp "$here"/libexec/*.sh "$H/libexec/"
cp "$here/prompts/supervisor-core.md" "$here/prompts/public-record.md" "$H/prompts/"
echo "Core rules." > "$H/prompts/tick-core.md"
echo "Work the queue." > "$H/instance/prompts/tick.md"
cat > "$H/fakebin/claude" <<'EOF'
#!/usr/bin/env bash
if [ "${1:-}" = auth ]; then
    echo "${CLAUDE_CONFIG_DIR:-default}" >> "$FAKE_SEEN/claude.auth"
    echo "key=${ANTHROPIC_API_KEY:-unset} token=${CLAUDE_CODE_OAUTH_TOKEN:-unset}" >> "$FAKE_SEEN/claude.auth-keys"
    if [ -e "$CLAUDE_CONFIG_DIR/signed-out" ]; then
        echo '{"loggedIn": false, "authMethod": "none", "loginExpiresAt": null}'
        exit 1
    fi
    printf '{"loggedIn": true, "authMethod": "claude.ai", "loginExpiresAt": "%s", "email": "owner@example.com", "orgId": "0000", "orgName": "Example Org"}\n' \
        "$(cat "$CLAUDE_CONFIG_DIR/expires" 2>/dev/null || echo 2099-01-01T00:00:00.000Z)"
    exit 0
fi
prompt=$(cat)
case $prompt in
    "You are the supervisor"*)
        # Its environment holds nothing of the harness's: the path comes from where this fake is.
        env | grep -E '^(CLAUDE_CONFIG_DIR|CODEX_HOME|ANTHROPIC_API_KEY|CLAUDE_CODE_OAUTH_TOKEN)=' > "${0%/fakebin/claude}/seen/sup-env"
        echo '{"directive": null, "github": []}'
        exit 0 ;;
esac
dir=${CLAUDE_CONFIG_DIR:-default}
{ echo "dir=$dir"; echo "marker=$HARNESS_TICK"; echo "slot=$(cut -d' ' -f4- "$HARNESS_STATE/locks/slot1.tick")"
  echo "keys=${ANTHROPIC_API_KEY:-unset} ${CLAUDE_CODE_OAUTH_TOKEN:-unset}"
  echo "usage=$("$HARNESS_HOME/libexec/usage-ok.sh")"; } >> "$FAKE_SEEN/claude.ticks"
if [ -e "$dir/limit" ]; then rm -f "$dir/limit"; echo "You've hit your session limit · resets 5pm"; exit 1; fi
exit 0
EOF
cat > "$H/fakebin/codex" <<'EOF'
#!/usr/bin/env bash
if [ "${1:-}" = login ]; then
    [ -e "$CODEX_HOME/auth.json" ] && { echo "Logged in using ChatGPT" >&2; exit 0; }
    echo "Not logged in" >&2
    exit 1
fi
cat >/dev/null
{ echo "dir=${CODEX_HOME:-default}"; echo "marker=$HARNESS_TICK"; echo "usage=$("$HARNESS_HOME/libexec/usage-ok.sh")"; } >> "$FAKE_SEEN/codex.ticks"
[ -e "${CODEX_HOME:-x}/limit" ] && { rm -f "$CODEX_HOME/limit"; echo "ERROR: You've hit your usage limit. Try again later."; exit 1; }
exit 0
EOF
cat > "$H/fakebin/gemini" <<'EOF'
#!/usr/bin/env bash
cat >/dev/null
echo "dir=${GEMINI_CLI_HOME:-default}" >> "$FAKE_SEEN/gemini.ticks"
EOF
cat > "$H/fakebin/fetch" <<'EOF'
#!/usr/bin/env bash
echo "${CLAUDE_CONFIG_DIR:-unset} ${USAGE_CACHE:-unset} ${USAGE_CREDENTIALS_FILE:-unset}" >> "$FAKE_SEEN/fetch"
EOF
printf '#!/usr/bin/env bash\ncase "$ISSUE_LABEL" in ai) echo 1 ;; *) echo 0 ;; esac\n' > "$H/fakebin/queue"
printf '#!/usr/bin/env bash\necho "*/2 * * * * $HARNESS_HOME/bin/tick.sh"\n' > "$H/fakebin/crontab"
printf '#!/usr/bin/env bash\nexit 1\n' > "$H/fakebin/gh"
chmod +x "$H/fakebin/"*
git -C "$H/target" init -q
touch "$A/codex/codex-work/auth.json"
write_env() {  # any argument is one more line
    cat > "$H/instance/harness.env" <<EOF
TARGET_REPO=$H/target
WORKTREE_ROOT=$H/worktrees
GH_REPO=example-org/example-repo
QUEUE_DEPTH_CMD=$H/fakebin/queue
AGENT_BIN_claude=$H/fakebin/claude
AGENT_BIN_codex=$H/fakebin/codex
AGENT_BIN_gemini=$H/fakebin/gemini
AGENT_FLAGS_claude=
AGENT_FLAGS_codex=
AGENT_ACCOUNTS_DIR=$A
TICK_MODELS="claude@work:model-a claude@home:model-a codex@work:model-x"
MAX_SLOTS=1
STAGGER_SECONDS=0
CLAIM_GRACE_S=0
USAGE_FETCH=$H/no-fetch
USAGE_FETCH_claude_work=$H/no-fetch
USAGE_FETCH_claude_home=$H/no-fetch
USAGE_SOURCE_gemini=none
ANTHROPIC_API_KEY=sk-ant-default-account
EOF
    [ $# -gt 0 ] && printf '%s\n' "$@" >> "$H/instance/harness.env"
}
write_env
# shellcheck disable=SC2046 # one name a word
unset HARNESS_TICK FLEET_SESSION HARNESS_STATE GH_TOKEN SUPERVISOR CLAUDE_CONFIG_DIR CODEX_HOME GEMINI_CLI_HOME \
    AGENT_ACCOUNTS_DIR $(compgen -v USAGE_)
# The default account's credentials, in the environment the launcher starts from.
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH" PATH="$H/fakebin:$PATH" HOME="$H/home" FAKE_SEEN="$H/seen" \
    ANTHROPIC_API_KEY=sk-ant-default-account CLAUDE_CODE_OAUTH_TOKEN=default-oauth-token
S="$H/state"; LOG="$S/logs/allocator.log"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
cache() {  # $1 = the account folder, $2 = its 5-hour percent
    printf '{"timestamp": %s000, "data": {"fiveHourPercent": %s, "weeklyPercent": 10}}\n' "$(date +%s)" "$2" > "$1/usage-cache.json"
}
cache "$A/claude/claude-work" 10; cache "$A/claude/claude-home" 40
printf '{"timestamp":"2026-11-28T00:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":30,"window_minutes":10080,"resets_at":%s},"secondary":null}}}\n' \
    "$(( $(date +%s) + 3600 ))" > "$A/codex/codex-work/sessions/2026/11/28/rollout-a.jsonl"
settle() {
    local n=0 quiet=0
    while [ "$quiet" -lt 3 ] && [ "$n" -lt 400 ]; do
        if pgrep -f "$H/bin/tick.sh" >/dev/null; then quiet=0; else quiet=$((quiet + 1)); fi
        sleep 0.05; n=$((n + 1))
    done
}
launch() { rm -f "$H/seen/"*.ticks; : > "$LOG" 2>/dev/null; "$H/bin/tick.sh"; settle; }
check_with() { write_env "$@"; "$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?; }

# --- with accounts off, a run is what it was before them -------------------------------------------
off=$(bash "$here/tests/fixtures/accounts/off-run.sh" "$here" 2>&1)
check "accounts off: the ticks' variables, markers, records, usage, check, state, logs and snapshots are as before" \
    '[ "$off" = "$(cat "$here/tests/fixtures/accounts/off-state.txt")" ]' \
    "$(diff <(printf '%s\n' "$off") "$here/tests/fixtures/accounts/off-state.txt")"
off=$(bash "$here/tests/fixtures/accounts/off-run-flow.sh" "$here" 2>&1)
check "accounts off: the supervisor, a lane tick chaining into a bare model's and a landing tick are as before" \
    '[ "$off" = "$(cat "$here/tests/fixtures/accounts/off-state-flow.txt")" ]' \
    "$(diff <(printf '%s\n' "$off") "$here/tests/fixtures/accounts/off-state-flow.txt")"

# --- check -------------------------------------------------------------------------------------------
check_with
check "check passes with every account folder in place" '[ "$rc" -eq 0 ] && grep -q "instance complete" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check "check prints each account's sign-in and method from its own CLI" \
    'grep -q "^account:       claude@work: signed in (claude.ai), ends 2099-01-01T00:00:00.000Z; cap 1 five_hour=10% weekly=10%$" "$H/check.txt" && grep -q "^account:       codex@work: signed in (chatgpt); cap 1 primary=30%$" "$H/check.txt" && grep -q "^account:       gemini@lab: sign-in unknown (gemini.s CLI has no sign-in check); cap 1$" "$H/check.txt"' \
    "$(grep account: "$H/check.txt")"
check "each account's CLI was asked with its own folder" \
    'grep -qx "$A/claude/claude-work" "$H/seen/claude.auth" && grep -qx "$A/claude/claude-home" "$H/seen/claude.auth" && ! grep -qx default "$H/seen/claude.auth"' \
    "$(cat "$H/seen/claude.auth")"
check "no email, organization or key the CLI printed reaches check or the state" \
    '! grep -rqE "example\.com|Example Org|0000" "$H/check.txt" "$S/accounts"' "$(cat "$S"/accounts/*.json)"
check "the sign-in state is kept owner-only" '[ "$(stat -c %a "$S/accounts")" = 700 ] && [ "$(stat -c %a "$S/accounts/claude@work.json")" = 600 ]' \
    "$(stat -c '%a %n' "$S/accounts" "$S"/accounts/*)"
check_with "TICK_MODELS='claude@work:model-a claude@gone:model-a'"
check "check refuses an account whose folder is missing, and names it" \
    '[ "$rc" -eq 1 ] && grep -q "^missing: an account folder for claude@gone: $A/claude/claude-gone: " "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
rm -f "$S/locks/instance-incomplete.stamp"
launch
check "the launcher starts nothing while an account folder is missing, and says why" \
    '[ ! -e "$H/seen/claude.ticks" ] && grep -q "instance incomplete: an account folder for claude@gone" "$LOG"' "$(cat "$LOG")"
chmod 777 "$A/claude/claude-work"
check_with
check "check refuses an account folder every user can write to, and says how to fix it" \
    '[ "$rc" -eq 1 ] && grep -q "^missing: an account folder for claude@work: $A/claude/claude-work is writable by its group or every user: chmod go-w $A/claude/claude-work$" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
chmod 775 "$A/claude"; chmod 755 "$A/claude/claude-work"
check_with
check "check refuses a group-writable agent folder too" \
    '[ "$rc" -eq 1 ] && grep -q "^missing: an account folder for claude@work: $A/claude is writable by its group" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
chmod 755 "$A/claude"
check_with "TICK_MODELS='claude@work codex@work:model-x'"
check "check refuses an entry that names an account but no model, and names it" \
    '[ "$rc" -eq 1 ] && grep -q "missing: .*.claude@work. names an account but no model: write agent@account:model" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "USAGE_SOURCE_claude_work=json:$H/shared.json" "USAGE_SOURCE_claude_home=json:$H/shared.json"
check "check refuses two accounts reading one file, naming both" \
    '[ "$rc" -eq 1 ] && grep -q "missing: a usage configuration that harness usage reads (.*claude@work and claude@home both read their usage from $H/shared.json" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
ln -s "$A/claude/claude-home" "$A/claude/claude-link"
check_with "TICK_MODELS=claude@link:model-a"
check "check refuses a symlinked account folder" \
    '[ "$rc" -eq 1 ] && grep -q "^missing: an account folder for claude@link: .* is a symlink" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
rm "$A/claude/claude-link"
check_with "TICK_MODELS=claude@Work:model-a"
check "check refuses an account name that is not one" \
    '[ "$rc" -eq 1 ] && grep -q "missing: a usage configuration that harness usage reads (.*account .Work. is not" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "FLEET_DISPATCH=1" "FLEET_BIN=$H/fakebin/gh"
check "check refuses an account entry with FLEET_DISPATCH=1, naming it" \
    '[ "$rc" -eq 1 ] && grep -q "missing: .*claude@work names an account; a FLEET_DISPATCH=1 tick runs on the device.s own" "$H/check.txt" && grep -q "missing: .*codex@work names an account" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
check_with "TICK_MODELS=claude:model-a" "FLEET_DISPATCH=1" "FLEET_BIN=$H/fakebin/gh" "SUPERVISOR=1" "SUPERVISOR_MODEL=claude@gone:model-s"
check "with FLEET_DISPATCH=1 the supervisor, which runs here, may name an account, and its folder is checked" \
    'grep -q "^missing: an account folder for claude@gone: " "$H/check.txt" && ! grep -q "claude@gone names an account" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
check_with "TICK_MODELS=gemini@lab:model-g" "USAGE_SOURCE_gemini="
check "a gemini account needs its own source or the agent's" \
    '[ "$rc" -eq 1 ] && grep -q "USAGE_SOURCE_gemini_lab or USAGE_SOURCE_gemini unset: name gemini@lab.s usage source, or none" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "TICK_MODELS=gemini@lab:model-g" "USAGE_SOURCE_gemini=" "USAGE_SOURCE_gemini_lab=none"
check "an account's own source satisfies check" '[ "$rc" -eq 0 ]' "rc=$rc $(cat "$H/check.txt")"

# --- ticks ---------------------------------------------------------------------------------------------
write_env
launch
check "the first entry runs on its account's folder, and only that tick carries it" \
    '[ "$(sed -n 1p "$H/seen/claude.ticks")" = "dir=$A/claude/claude-work" ] && [ ! -e "$H/seen/codex.ticks" ]' "$(cat "$H/seen/claude.ticks" 2>/dev/null)"
check "the marker and the slot record name the account" \
    'grep -qx "marker=state=$S slot=1 agent=claude@work model=model-a kind=work" "$H/seen/claude.ticks" && grep -qx "slot=claude@work model-a work" "$H/seen/claude.ticks"' \
    "$(cat "$H/seen/claude.ticks")"
check "usage-ok.sh in the tick checks its own account's reading" \
    'grep -q "^usage=claude@work: five_hour=10% weekly=10% .*: go on$" "$H/seen/claude.ticks"' "$(cat "$H/seen/claude.ticks")"
check "the allocator's line counts each account on its own" \
    'grep -q "claude@work=1\[five_hour:10%,weekly:10%\] claude@home=1\[five_hour:40%,weekly:10%\] codex@work=1\[primary:30%\] pool=1" "$LOG"' "$(cat "$LOG")"
check "the trailer names the account" 'grep -q "^\[tick.sh\] exit=0 .*agent=claude@work model=model-a$" "$S"/logs/tick-*.log' "$(cat "$S"/logs/tick-*.log)"
check "a tick on an account gets none of the default account's credential variables" \
    'grep -qx "keys=unset unset" "$H/seen/claude.ticks"' "$(cat "$H/seen/claude.ticks")"
check "nor does its sign-in check" '[ "$(sort -u "$H/seen/claude.auth-keys")" = "key=unset token=unset" ]' "$(sort -u "$H/seen/claude.auth-keys")"

touch "$A/claude/claude-work/limit"
launch
check "a usage limit cools only that account's entry" \
    '[ -e "$S/model-cooldown/claude@work:model-a" ] && [ ! -e "$S/model-cooldown/claude@home:model-a" ] && grep -q "claude@work:model-a hit its usage limit" "$LOG"' \
    "$(ls "$S/model-cooldown" 2>/dev/null) $(cat "$LOG")"
check "the tick then runs at once on the next account, with its own folder and reading" \
    '[ "$(grep -c "^dir=" "$H/seen/claude.ticks")" = 2 ] && [ "$(grep "^dir=" "$H/seen/claude.ticks" | sed -n 2p)" = "dir=$A/claude/claude-home" ] && grep -q "^usage=claude@home: five_hour=40%" "$H/seen/claude.ticks"' \
    "$(cat "$H/seen/claude.ticks")"
out=$(set -a; . "$H/instance/harness.env"; set +a; HARNESS_STATE="$S" "$HARNESS_BIN" usage)
check "harness usage weighs each account, the cooled one cooling alone" \
    'grep -q "^claude@work: cap 1 of 1, 0 running, every entry cooling down, claude-cache:$A/claude/claude-work/usage-cache.json" <<<"$out" && grep -q "^claude@home: cap 1 of 1, 0 running, claude-cache:" <<<"$out"' "$out"

rm -rf "$S/model-cooldown" "$S/accounts"
touch "$A/claude/claude-home/signed-out" "$A/claude/claude-work/limit"
launch
check "a signed-out account is logged once and counts as cap 0" \
    '[ "$(grep -c "claude@home is signed out; none of its ticks start until it signs in again" "$LOG")" = 1 ] && grep -q "claude@home=0\[" "$LOG"' "$(cat "$LOG")"
check "with one account cooling and the other signed out, the tick moves on to codex's account" \
    '[ "$(grep -c "^dir=" "$H/seen/claude.ticks")" = 1 ] && grep -qx "dir=$A/codex/codex-work" "$H/seen/codex.ticks" && grep -qx "marker=state=$S slot=1 agent=codex@work model=model-x kind=work" "$H/seen/codex.ticks" && grep -q "^usage=codex@work: primary=30%" "$H/seen/codex.ticks"' \
    "claude=$(cat "$H/seen/claude.ticks") codex=$(cat "$H/seen/codex.ticks" 2>/dev/null)"
jq '.checked_at = 0' "$S/accounts/claude@home.json" > "$H/old.json" && cp "$H/old.json" "$S/accounts/claude@home.json"
"$H/bin/tick.sh"; settle
check "checked again and still signed out, it is not logged again" \
    '[ "$(grep -c "claude@home is signed out" "$LOG")" = 1 ] && [ "$(jq -r .checked_at "$S/accounts/claude@home.json")" != 0 ]' "$(cat "$LOG")"
rm -f "$A/claude/claude-home/signed-out"
jq '.checked_at = 0' "$S/accounts/claude@home.json" > "$H/old.json" && cp "$H/old.json" "$S/accounts/claude@home.json"
launch
check "signed in again, it is logged and runs" \
    'grep -q "claude@home is signed in again" "$LOG" && grep -qx "dir=$A/claude/claude-home" "$H/seen/claude.ticks"' "$(cat "$LOG")"
touch "$A/claude/claude-work/signed-out"
jq '.checked_at = 0' "$S/accounts/claude@work.json" > "$H/old.json" && cp "$H/old.json" "$S/accounts/claude@work.json"
AGENT_ACCOUNTS_DIR="$A" HARNESS_STATE="$S" AGENT_BIN_claude="$H/fakebin/claude" "$HARNESS_BIN" accounts >/dev/null
launch
check "a sign-out that harness accounts found is still logged by the launcher, once" \
    '[ "$(grep -c "claude@work is signed out" "$LOG")" = 1 ] && [ "$(jq -r .state "$S/accounts/claude@work.json")" = signed_out ]' "$(cat "$LOG")"
rm -f "$A/claude/claude-work/signed-out" "$S/accounts/claude@work.json"
rm -rf "$S/model-cooldown"

write_env "TICK_MODELS=gemini@lab:model-g"
launch
check "a gemini account runs with GEMINI_CLI_HOME" '[ "$(cat "$H/seen/gemini.ticks")" = "dir=$A/gemini/gemini-lab" ]' "$(cat "$H/seen/gemini.ticks" 2>/dev/null)"

# --- usage per account -------------------------------------------------------------------------------
echo '{}' > "$A/claude/claude-work/.credentials.json"
write_env "USAGE_FETCH_claude_work=$H/fakebin/fetch" "USAGE_FETCH_claude_home=$H/fakebin/fetch" "USAGE_REFRESH_AFTER=0"
export USAGE_CREDENTIALS_FILE="$H/home/.claude/.credentials.json"; launch; unset USAGE_CREDENTIALS_FILE
check "each account's fetch runs with its folder, its cache and its own credentials file only" \
    'grep -qx "$A/claude/claude-work $A/claude/claude-work/usage-cache.json $A/claude/claude-work/.credentials.json" "$H/seen/fetch" && grep -qx "$A/claude/claude-home $A/claude/claude-home/usage-cache.json unset" "$H/seen/fetch"' \
    "$(sort -u "$H/seen/fetch")"
mkdir -p "$A/claude/claude-bare"; cache "$A/claude/claude-bare" 10
rm -f "$H/seen/fetch"
write_env "USAGE_FETCH=$H/fakebin/fetch" "USAGE_REFRESH_AFTER=0" "TICK_MODELS='claude@bare:model-a codex@work:model-x'"
launch
check "a claude account without its own fetch runs none, reads as no reading and is logged once" \
    '[ ! -e "$H/seen/fetch" ] && grep -qx "dir=$A/codex/codex-work" "$H/seen/codex.ticks" && [ ! -e "$H/seen/claude.ticks" ]' \
    "fetch=$(cat "$H/seen/fetch" 2>/dev/null) codex=$(cat "$H/seen/codex.ticks" 2>/dev/null)"
"$H/bin/tick.sh"; settle
check "the launcher logs it once, and the pool counts it as cap 0" \
    '[ "$(grep -c "claude@bare: no usage fetch for this account: set USAGE_FETCH_claude_bare; none of its ticks start" "$LOG")" = 1 ] && grep -q "claude@bare=0\[unknown\]" "$LOG"' "$(cat "$LOG")"
out=$(AGENT_ACCOUNTS_DIR="$A" HARNESS_STATE="$S" AGENT_BIN_claude="$H/fakebin/claude" "$HARNESS_BIN" accounts)
check "harness accounts says why the account allows nothing" \
    'grep -q "^claude@bare: signed in (claude.ai), ends .*; cap 0, no reading (no usage fetch for this account: set USAGE_FETCH_claude_bare)$" <<<"$out"' "$out"
rm -rf "$A/claude/claude-bare"
write_env "USAGE_SOURCE_claude_work=json:$H/work.json"
printf '{"timestamp": %s000, "windows": [{"name": "hourly", "used_pct": 75, "window_minutes": 60}]}\n' "$(date +%s)" > "$H/work.json"
out=$(set -a; . "$H/instance/harness.env"; set +a; HARNESS_STATE="$S" "$HARNESS_BIN" usage --json | jq -c '[.agents[] | [.agent, .account, .source, .cap]]')
check "USAGE_SOURCE_<agent>_<account> replaces one account's reading" \
    '[ "$out" = "[[\"claude\",\"work\",\"json:$H/work.json\",1],[\"claude\",\"home\",\"claude-cache:$A/claude/claude-home/usage-cache.json\",1],[\"codex\",\"work\",\"codex-sessions:$A/codex/codex-work/sessions\",1]]" ]' "$out"

write_env "USAGE_SOURCE_gemini=json:$H/gemini.json" "TICK_MODELS='gemini@lab:model-g codex@work:model-x'"
launch
"$H/bin/tick.sh"; settle
check "a gemini account never reads the agent's source: it allows nothing, logged once" \
    '[ "$(grep -c "gemini@lab: an account never reads USAGE_SOURCE_gemini: set USAGE_SOURCE_gemini_lab; none of its ticks start" "$LOG")" = 1 ] && [ ! -e "$H/seen/gemini.ticks" ] && grep -q "gemini@lab=0\[unknown\]" "$LOG"' "$(cat "$LOG")"

# --- the supervisor ----------------------------------------------------------------------------------
mkdir -p "$A/claude/claude-sup"; rm -f "$H/seen/fetch"
write_env "SUPERVISOR=1" "SUPERVISOR_MODEL=claude@sup:model-s" "USAGE_FETCH_claude_sup=$H/fakebin/fetch" "USAGE_REFRESH_AFTER=0" \
    "SUPERVISOR_MIN_INTERVAL_S=0"
launch
"$H/bin/tick.sh"; settle
check "the supervisor's account gets its own fetch run, though no tick entry names it" \
    'grep -q "^$A/claude/claude-sup $A/claude/claude-sup/usage-cache.json " "$H/seen/fetch"' "$(cat "$H/seen/fetch" 2>/dev/null)"
check "held back for want of a reading, the supervisor says so once" \
    '[ "$(grep -c "supervisor not due: claude@sup has no fresh usage reading" "$LOG")" = 1 ]' "$(cat "$LOG")"
rm -rf "$A/claude/claude-sup" "$S/supervisor"
write_env "SUPERVISOR=1" "SUPERVISOR_MODEL=claude@work:model-s"
CLAUDE_CONFIG_DIR="$H/home/.claude" "$H/bin/tick.sh" supervisor manual
check "the supervisor runs on its account's folder, with none of the default account's credentials" \
    '[ "$(cat "$H/seen/sup-env" 2>/dev/null)" = "CLAUDE_CONFIG_DIR=$A/claude/claude-work" ]' "$(cat "$H/seen/sup-env" 2>/dev/null)"
check "its run is logged under its account" 'grep -q "^\[tick.sh\] exit=0 .*agent=claude@work model=model-s$" "$S"/logs/supervisor-*.log' "$(cat "$S"/logs/supervisor-*.log)"

# --- harness accounts ----------------------------------------------------------------------------------
out=$(AGENT_ACCOUNTS_DIR="$A" HARNESS_STATE="$S" AGENT_BIN_claude="$H/fakebin/claude" "$HARNESS_BIN" accounts --json)
check "harness accounts lists every folder with its sign-in, and nothing the CLI printed beyond it" \
    '[ "$(jq -c "[.accounts[] | [.agent, .account, .sign_in.state]]" <<<"$out")" = "[[\"claude\",\"home\",\"signed_in\"],[\"claude\",\"work\",\"signed_in\"],[\"codex\",\"work\",\"signed_in\"],[\"gemini\",\"lab\",\"unknown\"]]" ] && ! grep -qE "example\.com|Example Org" <<<"$out"' \
    "$out"
out=$(AGENT_ACCOUNTS_DIR="$A" HARNESS_STATE="$S" "$HARNESS_BIN" accounts env claude@work codex@work); rc=$?
check "harness accounts env prints the env(1) arguments each account's tick runs with" \
    '[ "$rc" -eq 0 ] && grep -qx -- "--unset=ANTHROPIC_API_KEY" <<<"$out" && grep -qx -- "--unset=OPENAI_API_KEY" <<<"$out" && [ "$(grep -v "^--unset=" <<<"$out")" = "CLAUDE_CONFIG_DIR=$A/claude/claude-work"$'"'"'\n'"'"'"CODEX_HOME=$A/codex/codex-work" ]' "rc=$rc $out"
out=$("$HARNESS_BIN" accounts); rc=$?
check "harness accounts without a root says accounts are off" '[ "$rc" -eq 0 ] && [ "$out" = "accounts off: AGENT_ACCOUNTS_DIR is not set" ]' "rc=$rc $out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
