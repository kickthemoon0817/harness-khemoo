#!/usr/bin/env bash
# Prints what the launcher leaves behind with the supervisor off, in a scratch home built from the
# harness checkout $1 and run with $HARNESS_BIN: a firing that starts a tick, which claims an issue,
# then one at a full window with a reset ahead. It prints the dashboard snapshot's top-level keys,
# every state file's path and the allocator log, times taken out. test_supervisor.sh compares it
# with off-state.txt, printed by a tree without the supervisor: with the supervisor off, nothing may
# differ, so never regenerate it from this tree.
set -u
src=$1
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/lib" "$H/libexec" "$H/prompts" "$H/instance/prompts" "$H/state" "$H/target" "$H/fakebin"
cp "$src/bin/tick.sh" "$H/bin/"; cp "$src/lib/instance.sh" "$H/lib/"; cp "$src"/libexec/*.sh "$H/libexec/"
echo "Core rules." > "$H/prompts/tick-core.md"
echo "Work the queue." > "$H/instance/prompts/tick.md"
cat > "$H/fakebin/claude" <<'FAKE'
#!/usr/bin/env bash
cat >/dev/null
"$HARNESS_HOME/libexec/claim.sh" acquire 7 >/dev/null
FAKE
printf '#!/usr/bin/env bash\ncase "$ISSUE_LABEL" in ai) echo 1 ;; *) echo 0 ;; esac\n' > "$H/fakebin/queue"
printf '#!/usr/bin/env bash\necho "*/2 * * * * $HARNESS_HOME/bin/tick.sh"\n' > "$H/fakebin/crontab"
printf '#!/usr/bin/env bash\nexit 1\n' > "$H/fakebin/gh"
chmod +x "$H/fakebin/"*
git -C "$H/target" init -q
cat > "$H/instance/harness.env" <<ENV
TARGET_REPO=$H/target
WORKTREE_ROOT=$H/worktrees
GH_REPO=example-org/example-repo
QUEUE_DEPTH_CMD=$H/fakebin/queue
AGENT_BIN_claude=$H/fakebin/claude
AGENT_FLAGS_claude=
TICK_MODELS=claude:model-a
MAX_SLOTS=2
STAGGER_SECONDS=0
CLAIM_GRACE_S=0
USAGE_CACHE=$H/usage-cache.json
USAGE_FETCH=$H/no-fetch
ENV
unset HARNESS_TICK FLEET_SESSION HARNESS_STATE GH_TOKEN SUPERVISOR
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH" PATH="$H/fakebin:$PATH"
usage() {  # $1 = the 5-hour window's percent; $2, when given, its reset as Unix seconds
    local reset=""
    [ -n "${2:-}" ] && reset=", \"fiveHourResetsAt\": \"$(date -u -d "@$2" +%Y-%m-%dT%H:%M:%S.000000+00:00)\""
    printf '{"timestamp": %s000, "data": {"fiveHourPercent": %s, "weeklyPercent": 10%s}}\n' "$(date +%s)" "$1" "$reset" > "$H/usage-cache.json"
}
# pgrep can miss a process mid-exec, so it settles only after three empty looks in a row.
settle() {
    local n=0 quiet=0
    while [ "$quiet" -lt 3 ] && [ "$n" -lt 400 ]; do
        if pgrep -f "$H/bin/tick.sh" >/dev/null; then quiet=0; else quiet=$((quiet + 1)); fi
        sleep 0.05; n=$((n + 1))
    done
}
usage 10; "$H/bin/tick.sh"; settle
usage 85 $(( $(date +%s) + 5400 )); "$H/bin/tick.sh"; settle
"$HARNESS_BIN" dashboard --once --harness-home "$H" --remote-interval 0 --claimable-interval 0 2>/dev/null | jq -c 'keys'
(cd "$H" && find state -type f | sed -E 's/[0-9]{8}T[0-9]{6}Z/TS/g' | LC_ALL=C sort)
sed -E 's/^[0-9]{8}T[0-9]{6}Z //' "$H/state/logs/allocator.log"
