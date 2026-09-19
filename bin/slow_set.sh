#!/usr/bin/env bash
# Runs the whole doctest set, [slow] cases included, on the head of the work
# branch, once per head. Ticks merge on the fast set plus the [slow] cases of the
# test files they change, so this is where every other [slow] case is read. A red
# run files (or updates) one issue naming the failing cases and the merges since
# the last green head. Scheduled from cron; one run at a time.
set -u
HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# shellcheck source=/dev/null
[ -r "$HARNESS_HOME/config/harness.env" ] && . "$HARNESS_HOME/config/harness.env"
: "${HARNESS_STATE:=$HARNESS_HOME/state}"
: "${TARGET_REPO:?}"; : "${GH_REPO:?}"
: "${WORK_BRANCH:=ai/manure-mpm}"
: "${SLOW_SET_WORKTREE:=$(dirname "$TARGET_REPO")/ai-worktrees/slowset}"
: "${BUILDER_IMAGE:=worv-builder:isaac6}"
export PATH="${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"
S="$HARNESS_STATE/slow-set"; mkdir -p "$S/logs" "$HARNESS_STATE/locks"
exec 7>"$HARNESS_STATE/locks/slowset.lock"; flock -n 7 || exit 0
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
            -lc "mkdir -p /out/$name && bash /w/tools/test/run_doctest_sharded.sh /w/extensions/env/worv.env.manure/bin/tests/$name" \
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
# Each failing case runs once more alone: one that passes alone is load-sensitive,
# one that fails alone is a regression, and the issue says which.
failing=""
for f in "$out"/*.txt; do
    [ -e "$f" ] || continue
    name=$(basename "$f" .txt)
    cases=$(awk '/^TEST CASE:/{sub(/^TEST CASE: +/,""); c=$0} /ERROR:|FATAL ERROR|TIMEOUT/{if(c!="") print c}' "$f" | sort -u)
    while IFS= read -r c; do
        [ -n "$c" ] || continue
        docker run --rm --runtime=runc -e CUDA_VISIBLE_DEVICES= -e NVIDIA_VISIBLE_DEVICES=void \
            -v "$SLOW_SET_WORKTREE":/w:ro --entrypoint bash "$BUILDER_IMAGE" \
            -lc "/w/extensions/env/worv.env.manure/bin/tests/$name -tc=\"$c\"" >"$out/alone.txt" 2>&1 \
            && verdict="passes alone: load-sensitive" || verdict="fails alone: regression"
        failing="$failing$name :: $c  [$verdict]\n$(grep -E 'ERROR|values:' "$f" | grep -A1 -F "" | head -4)\n"
    done <<< "$cases"
done
[ -z "$failing" ] && failing=$(cat "$out"/*.txt 2>/dev/null | grep -E 'FAILED|TIMEOUT|ERROR' | sort -u | head -40)
[ "$build_rc" -ne 0 ] && failing="build failed:\n$(tail -30 "$log")"
body=$(printf 'Operator: **take this before any other claimable issue.**\n\nThe whole doctest set, `[slow]` cases included, is red on `%s` at `%s` (`bin/slow_set.sh`, host-only; device cases skip).\n\n**Failing:**\n```\n%b\n```\n\n**Merges since the last green head%s:**\n%s\n\nFind which merge turned it red (run the failing case with `-tc=` on each merge in the range), fix it in that area, and prove it with the same case. Full per-binary output is in the harness state, `%s`.\n' \
    "$WORK_BRANCH" "$short" "$failing" "${green:+ \`${green:0:8}\`}" "${range:-"(no green head recorded yet)"}" "$out")
open=$(gh issue list --repo "$GH_REPO" --state open --search 'in:title "the [slow] set is red"' --json number -q '.[0].number' 2>/dev/null)
if [ -n "$open" ]; then
    gh issue comment "$open" --repo "$GH_REPO" --body "$body" >>"$log" 2>&1
else
    gh issue create --repo "$GH_REPO" --label ai --title "test: the [slow] set is red on $WORK_BRANCH" --body "$body" >>"$log" 2>&1
fi
echo "RED $sha" >>"$log"
