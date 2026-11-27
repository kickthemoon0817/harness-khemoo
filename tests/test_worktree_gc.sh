#!/usr/bin/env bash
# Exercises bin/worktree_gc.sh's dry run against real worktrees of a scratch checkout: which issue
# a worktree belongs to, and a worktree-busy hook that hangs. A fake gh lists the open issues.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
mkdir -p "$H/instance/hooks" "$H/fake" "$H/trees"
cat > "$H/fake/gh" <<'G'
#!/usr/bin/env bash
printf '%s\n' 99
G
chmod +x "$H/fake/gh"
git init -q --bare "$H/origin.git"
git clone -q "$H/origin.git" "$H/target" 2>/dev/null
g() { git -C "$H/target" -c user.name=t -c user.email=t@example.invalid "$@"; }
g commit -q --allow-empty -m base
g push -q origin HEAD:main
tree() {  # directory, branch
    g worktree add -q -b "$2" "$H/trees/$1"
    g -C "$H/trees/$1" commit -q --allow-empty -m "$2"
    g -C "$H/trees/$1" push -q origin "$2"
}
tree my-app-42 topic-short
tree scratch ai/7-named-by-branch
tree app-99 ai/99-still-open
tree notes other-branch
cat > "$H/instance/harness.env" <<H
TARGET_REPO=$H/target
WORKTREE_ROOT=$H/trees
GH_REPO=example-org/example-repo
HARNESS_PATH=$H/fake:$PATH
H
export HARNESS_HOME="$H"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }

out=$(bash "$here/bin/worktree_gc.sh" 2>&1)
check "an issue below 100 is read from the trailing number of a hyphenated directory name" 'echo "$out" | grep -qx "remove my-app-42 (issue #42 closed)"' "$out"
check "the branch names the issue of a worktree whose directory does not" 'echo "$out" | grep -qx "remove scratch (issue #7 closed)"' "$out"
check "an open issue's worktree and one with no issue are kept" '! echo "$out" | grep -q "app-99\|notes"' "$out"

printf '#!/usr/bin/env bash\nsleep 30\n' > "$H/instance/hooks/worktree-busy"; chmod +x "$H/instance/hooks/worktree-busy"
echo "WORKTREE_BUSY_TIMEOUT_S=1" >> "$H/instance/harness.env"
start=$(date +%s); out=$(bash "$here/bin/worktree_gc.sh" 2>&1); rc=$?; took=$(( $(date +%s) - start ))
check "a busy hook that hangs removes nothing" '[ "$rc" -ne 0 ] && [ "$took" -lt 10 ] && ! echo "$out" | grep -q "^remove" && echo "$out" | grep -q "worktree-busy failed"' "rc=$rc took=${took}s $out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
