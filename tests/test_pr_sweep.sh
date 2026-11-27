#!/usr/bin/env bash
# Exercises bin/pr_sweep.sh with a fake gh that serves an open pull request list and issue
# states from fixtures and logs every write.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/fake" "$T/h/instance" "$T/h/state/logs"
echo "GH_REPO=org/repo" > "$T/h/instance/harness.env"
export CALLS="$T/calls.log" FX="$T/fx"; mkdir -p "$FX"
cat > "$T/fake/gh" <<'G'
#!/usr/bin/env bash
case "$*" in
  *"-X POST"*|*"-X PATCH"*) echo "$*" >> "$CALLS"; [ -e "$FX/fail_write" ] && exit 1; echo 1 ;;
  *"pulls?state=open"*) cat "$FX/prs" ;;
  *"repos/org/repo/issues/"*) a="$*"; n=${a##*issues/}; n=${n%% *}; [ -e "$FX/issue.$n" ] && cat "$FX/issue.$n" || exit 1 ;;
  *) exit 1 ;;
esac
G
chmod +x "$T/fake/gh"; export PATH="$T/fake:$PATH" HARNESS_HOME="$T/h"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
printf '%s\n' "2386 ai/2382-converted-push" "2472 ai/2464-unit-rest-host" "2030 ai/2026-still-island-sleep" > "$FX/prs"
echo "closed 2026-11-23T19:59:09Z" > "$FX/issue.2382"
echo "open -" > "$FX/issue.2464"
echo "closed 2026-11-25T06:31:33Z" > "$FX/issue.2026"

out=$(bash "$here/bin/pr_sweep.sh" --dry-run)
check "--dry-run names each pull request whose issue is closed" '[ "$(echo "$out" | grep -c "would close")" = 2 ] && echo "$out" | grep -q "PR #2386" && echo "$out" | grep -q "PR #2030"' "$out"
check "--dry-run writes nothing" '[ ! -s "$CALLS" ]' "$(cat "$CALLS" 2>/dev/null)"

: > "$CALLS"; bash "$here/bin/pr_sweep.sh"
check "a pull request whose issue is closed gets a comment and is closed" 'grep -q "POST repos/org/repo/issues/2386/comments" "$CALLS" && grep -q "PATCH repos/org/repo/pulls/2386 -f state=closed" "$CALLS"' "$(cat "$CALLS")"
check "the other one is closed too" 'grep -q "PATCH repos/org/repo/pulls/2030 -f state=closed" "$CALLS"' "$(cat "$CALLS")"
check "a pull request whose issue is open is left alone" '! grep -q "2472" "$CALLS"' "$(cat "$CALLS")"
check "each close is logged with its issue" '[ "$(grep -c "closed PR #" "$T/h/state/logs/pr_sweep.log")" = 2 ] && grep -q "closed PR #2386 (ai/2382-converted-push): #2382 closed" "$T/h/state/logs/pr_sweep.log"' "$(cat "$T/h/state/logs/pr_sweep.log")"

: > "$CALLS"; printf '%s\n' "1 ai/manure" "2 ai/9999-gone" > "$FX/prs"
bash "$here/bin/pr_sweep.sh"
check "a branch without an issue number, or an issue that cannot be read, is left alone" '[ ! -s "$CALLS" ]' "$(cat "$CALLS")"

: > "$CALLS"; : > "$T/h/state/logs/pr_sweep.log"; touch "$FX/fail_write"; printf '%s\n' "2386 ai/2382-converted-push" > "$FX/prs"
bash "$here/bin/pr_sweep.sh"
check "a failed comment leaves the pull request open and says so" '! grep -q PATCH "$CALLS" && grep -q "PR #2386 comment failed" "$T/h/state/logs/pr_sweep.log"' "$(cat "$CALLS") / $(cat "$T/h/state/logs/pr_sweep.log")"

rm -f "$FX/fail_write"; : > "$CALLS"; : > "$T/h/state/logs/pr_sweep.log"
printf '%s\n' "2386 ai/2382-converted-push" "2030 bot/2026-still-island-sleep" > "$FX/prs"
BRANCH_PREFIX=bot/ bash "$here/bin/pr_sweep.sh"
check "BRANCH_PREFIX names the branches swept" 'grep -q "pulls/2030 -f state=closed" "$CALLS" && ! grep -q 2386 "$CALLS"' "$(cat "$CALLS")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
