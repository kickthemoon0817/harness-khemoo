#!/usr/bin/env bash
# Runs the whole doctest set, [slow] cases included, on the head of the work
# branch, once per head. Ticks merge on the fast set plus the [slow] cases of the
# test files they change, so this is where every other [slow] case is read. A red
# run files (or updates) one issue naming the failing cases and the merges since
# the last green head. Scheduled from cron; one run at a time.
set -u
# `slow_set.sh fast` reads the fast set (the [slow] suite left out) with its own state.
mode="${1:-full}"
HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# shellcheck source=/dev/null
[ -r "$HARNESS_HOME/config/harness.env" ] && . "$HARNESS_HOME/config/harness.env"
: "${HARNESS_STATE:=$HARNESS_HOME/state}"
: "${TARGET_REPO:?}"; : "${GH_REPO:?}"
: "${WORK_BRANCH:=ai/manure-mpm}"
: "${BUILDER_IMAGE:=worv-builder:isaac6}"
export PATH="${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"
if [ "$mode" = fast ]; then S="$HARNESS_STATE/fast-set"; fast_flag="--fast"; set_name="fast set"; lock="fastset"; else S="$HARNESS_STATE/slow-set"; fast_flag=""; set_name="whole doctest set, \`[slow]\` cases included"; lock="slowset"; fi
# Each mode has a worktree of its own: the hourly fast set never checks out a commit under a whole
# set still testing, and never waits on its tree.
: "${SLOW_SET_WORKTREE:=$(dirname "$TARGET_REPO")/ai-worktrees/$lock}"
mkdir -p "$S/logs" "$HARNESS_STATE/locks"
# shellcheck source=/dev/null
. "$HARNESS_HOME/bin/remote-env.sh"; remote_host_env
# The scheduled suites run on a host no cost read holds, so they never load the
# host a cost read is timing: SLOW_SET_REMOTE_HOST first, then the other remote
# hosts, then the local one. With every host held, the run is deferred to the
# next schedule.
lease_status=$("$HARNESS_HOME/bin/lease.sh" status < /dev/null 2>/dev/null)
# held_by_cost_read <host|local>: a live whole-card lease on that host's slots.
held_by_cost_read() {
    printf '%s\n' "$lease_status" | awk -v h="$1" '
        /^remote card: / { host = $3; split(substr($5, 1, length($5) - 1), r, "-"); lo[host] = r[1]; hi[host] = r[2] }
        /^slot [0-9]+ HELD / && /\(alive\)$/ { held[$2] = $0 }
        END {
            for (s in held) {
                if (h == "local") { remote = 0; for (x in lo) if (s + 0 >= lo[x] && s + 0 <= hi[x]) remote = 1
                                    if (!remote && held[s] ~ / exclusive /) exit 0 }
                else if (s + 0 >= lo[h] && s + 0 <= hi[h] && held[s] ~ / remote-all / && held[s] !~ / no-cost /) exit 0
            }
            exit 1
        }'
}
run_on=""
# The scheduled suites are CPU work, so with this host's CPU lane on (state/local-cpu-lane) they
# run here, under the host's dev build lock and the lane's builder image, which keeps
# both remote cards free for the runs that need a card. Otherwise, with the harness kept to its remote
# hosts, bin/build_host.sh names a remote host and that host's compile lock and builder image.
if [ -s "$HARNESS_STATE/local-cpu-lane" ]; then
    read -r BUILDER_IMAGE _ < "$HARNESS_STATE/local-cpu-lane"
    export DOCKER_HOST=unix:///var/run/docker.sock WORV_DEV_BUILD_LOCK=/tmp/worv-dev-build.lock WORV_BUILDER_IMAGE="$BUILDER_IMAGE" WORV_LOCAL_LANE=1
    run_on=local
elif [ -e "$HARNESS_STATE/locks/harness-first-remote-only" ]; then
    lane=$(BUILD_HOST_REMOTE_ONLY=1 "$HARNESS_HOME/bin/build_host.sh") || exit 1
    eval "$lane"
    run_on=${DOCKER_HOST#ssh://}
    export PATH="$HARNESS_HOME/bin/remote-docker:$PATH"
    if held_by_cost_read "$run_on"; then
        echo "$(date -u +%FT%TZ) $set_name deferred: every host is held by a cost read" >> "$S/deferred.log"
        exit 0
    fi
fi
for h in ${SLOW_SET_REMOTE_HOST:-} $(grep -v '^[[:space:]]*$' "$HARNESS_STATE/locks/remote-card" 2>/dev/null); do
    [ -z "$run_on" ] || break
    [ -e "$HARNESS_STATE/locks/local-card-off" ] && break
    held_by_cost_read "$h" && continue
    REMOTE_CARD_HOST="$h" "$HARNESS_HOME/bin/remote_room.sh" >/dev/null 2>&1 || continue
    run_on="$h"; break
done
if [ -n "$run_on" ] && [ -z "${WORV_DEV_BUILD_LOCK:-}" ]; then
    export DOCKER_HOST="ssh://$run_on" PATH="$HARNESS_HOME/bin/remote-docker:$PATH" \
        WORV_DEV_BUILD_LOCK=/tmp/worv-dev-build-remote.lock
elif [ -z "$run_on" ] && held_by_cost_read local; then
    echo "$(date -u +%FT%TZ) $set_name deferred: every host is held by a cost read" >> "$S/deferred.log"
    exit 0
fi
exec 7>"$HARNESS_STATE/locks/$lock.lock"; flock -n 7 || exit 0
[ -e "$HARNESS_STATE/paused" ] && exit 0

git -C "$TARGET_REPO" fetch -q origin "$WORK_BRANCH" || exit 1
sha=$(git -C "$TARGET_REPO" rev-parse "origin/$WORK_BRANCH")
[ "$(cat "$S/last_run" 2>/dev/null)" = "$sha" ] && exit 0
short=${sha:0:8}; ts=$(date -u +%Y%m%dT%H%M%SZ); log="$S/logs/$ts-$short.log"; out="$S/logs/$ts-$short"
mkdir -p "$out"

if [ -d "$SLOW_SET_WORKTREE" ]; then
    git -C "$SLOW_SET_WORKTREE" checkout -q --detach "$sha" >>"$log" 2>&1 || exit 1
else
    git -C "$TARGET_REPO" worktree add -q --detach "$SLOW_SET_WORKTREE" "$sha" >>"$log" 2>&1 || exit 1
fi
echo "slow set at $sha, $(date -u +%FT%TZ)" >>"$log"
( cd "$SLOW_SET_WORKTREE" && tools/dev/iter.sh build worv.env.manure ) >>"$log" 2>&1
build_rc=$?
rc=$build_rc
bins=()
if [ "$build_rc" -eq 0 ]; then
    for b in "$SLOW_SET_WORKTREE"/extensions/env/worv.env.manure/bin/tests/test_*; do
        [ -x "$b" ] || continue
        name=$(basename "$b"); bins+=("$name")
        docker run --rm --runtime=runc -e CUDA_VISIBLE_DEVICES= -e NVIDIA_VISIBLE_DEVICES=void \
            -e DOCTEST_SHARD_DIR=/out/$name -v "$SLOW_SET_WORKTREE":/w:ro -v "$out":/out \
            --entrypoint bash "$BUILDER_IMAGE" \
            -lc "mkdir -p /out/$name && bash /w/tools/test/run_doctest_sharded.sh $fast_flag /w/extensions/env/worv.env.manure/bin/tests/$name" \
            >"$out/$name.txt" 2>&1
        r=$?; echo "$name exit=$r" >>"$log"; [ "$r" -ne 0 ] && rc=$r
    done
fi
echo "$sha" > "$S/last_run"
if [ "$rc" -eq 0 ]; then
    echo "$sha" > "$S/last_green"; echo "GREEN $sha" >>"$log"; exit 0
fi

# Red: one open issue carries every red run; a new head appends a comment.
green=$(cat "$S/last_green" 2>/dev/null)
range=${green:+$(git -C "$TARGET_REPO" log --merges --format='- %h %s' "$green..$sha" | head -40)}
# A failing case is one whose own process exited non-zero, read from the runner's
# per-case status files (line N of `cases` is `case.<N>.rc`): a `should_fail` case
# that failed as expected exits 0, and its ERROR lines are not a failure. Each
# failing case runs once more alone: one that passes alone is load-sensitive, one
# that fails alone is a regression, and the issue says which.
# The whole set holds the fast set, so while the fast set's issue is open, a case
# that issue already names is its to repair: it is left out here, and a whole-set
# run red only in such cases files nothing.
failing=""; owned=""; fast_open=""; owned_text=""
if [ "$mode" = full ]; then
    fast_open=$(gh issue list --repo "$GH_REPO" --state open --search 'in:title "the fast set is red"' \
        --json number -q '.[0].number' 2>/dev/null)
    [ -n "$fast_open" ] && owned_text=$(gh issue view "$fast_open" --repo "$GH_REPO" --json body,comments \
        --jq '.body, .comments[].body' 2>/dev/null)
fi
for f in "$out"/*.txt; do
    [ -e "$f" ] || continue
    name=$(basename "$f" .txt)
    [[ "$name" =~ ^test_[A-Za-z0-9_]+$ ]] || continue
    shard="$out/$name"
    [ -r "$shard/cases" ] || continue
    i=0
    while IFS= read -r c; do
        i=$((i + 1)); index=$(printf '%06d' "$i")
        read -r status _ <"$shard/case.$index.rc" 2>/dev/null || status=255
        [ "$status" = 0 ] && continue
        [[ "$c" =~ [[:cntrl:]] ]] && continue
        if [ -n "$owned_text" ] && grep -qF -- "$c" <<<"$owned_text"; then
            owned="$owned$name :: $c\n"; continue
        fi
        # The case name is an argument, never shell text: a name is test output.
        # doctest splits -tc= on commas, so they are escaped as the runner escapes them.
        filter=$(printf '%s' "$c" | sed -e 's/\\/\\\\/g' -e 's/,/\\,/g')
        docker run --rm --runtime=runc -e CUDA_VISIBLE_DEVICES= -e NVIDIA_VISIBLE_DEVICES=void \
            -v "$SLOW_SET_WORKTREE":/w:ro \
            --entrypoint "/w/extensions/env/worv.env.manure/bin/tests/$name" "$BUILDER_IMAGE" \
            "-tc=$filter" >"$out/alone.$name.$index.txt" 2>&1 \
            && verdict="passes alone: load-sensitive" || verdict="fails alone: regression"
        case "$status" in
            124) errors="killed past its CPU time limit" ;;
            125) errors="killed: it gained no CPU time within the stall window" ;;
            *) errors=$(grep -E -A1 'ERROR|FATAL' "$shard/case.$index.log" 2>/dev/null | grep -v '^--$' | head -6) ;;
        esac
        failing="$failing$name :: $c  [$verdict]\n${errors:-exit $status, no error line printed}\n"
    done <"$shard/cases"
done
if [ -z "$failing" ] && [ -n "$owned" ] && [ "$build_rc" -eq 0 ]; then
    printf 'RED %s, only in cases #%s owns:\n%b' "$sha" "$fast_open" "$owned" >>"$log"
    exit 0
fi
[ -z "$failing" ] && failing=$(cat "$out"/*.txt 2>/dev/null | grep -E '^GATE:|^ERROR:|^\[doctest\] test cases:' | head -40)
[ "$build_rc" -ne 0 ] && failing="build failed:\n$(tail -30 "$log")"
[ -n "$owned" ] && failing="$failing\nAlso red, and #$fast_open's to repair:\n$owned"
body=$(printf 'Operator: **take this before any other claimable issue.**\n\nThe %s is red on `%s` at `%s` (`bin/slow_set.sh %s`, host-only; device cases skip).\n\n**Failing:**\n```\n%b\n```\n\n**Merges since the last green head%s:**\n%s\n\nFind which merge turned it red (run the failing case with `-tc=` on each merge in the range), fix it in that area, and prove it with the same case. Full per-binary output is in the harness state, `%s`.\n' \
    "$set_name" "$WORK_BRANCH" "$short" "$mode" "$failing" "${green:+ \`${green:0:8}\`}" "${range:-"(no green head recorded yet)"}" "$out")
# A red on a head that has since moved is not filed: a repair may have landed while the set ran,
# and the next run tests the new head (last_run holds the old one).
git -C "$TARGET_REPO" fetch -q origin "$WORK_BRANCH" 2>/dev/null
now=$(git -C "$TARGET_REPO" rev-parse "origin/$WORK_BRANCH" 2>/dev/null)
if [ -n "$now" ] && [ "$now" != "$sha" ]; then
    printf 'RED %s, not filed: the head moved to %s while the set ran\n' "$sha" "$now" >>"$log"
    exit 0
fi
open=$(gh issue list --repo "$GH_REPO" --state open --search "in:title \"the $set_name is red\"" --json number -q '.[0].number' 2>/dev/null)
if [ -n "$open" ]; then
    gh issue comment "$open" --repo "$GH_REPO" --body "$body" >>"$log" 2>&1
else
    gh issue create --repo "$GH_REPO" --label ai --label ai:plan --title "test: the $set_name is red on $WORK_BRANCH" --body "$body" >>"$log" 2>&1
fi
echo "RED $sha" >>"$log"
