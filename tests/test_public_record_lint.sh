#!/usr/bin/env bash
# Exercises bin/public_record_lint.py on bodies written here: every blocking rule fires on a body
# written for the dev host, a body written to prompts/public-record.md is clean, and a comment keeps
# its protocol line and its Harness trace block.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LINT="$here/bin/public_record_lint.py"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }

out=$(python3 "$LINT" --self-test 2>&1); rc=$?
check "the built-in samples pass" '[ "$rc" -eq 0 ]' "$out"

cat >"$T/good-pr.md" <<'EOF'
## Summary
The return walk keeps a candidate's borne verdict while every input it reads is unchanged. No behaviour
changes (class A: the replay of every review scenario is identical). Lands as worv.env.manure 0.3.569.
Fixes #2137.

## Why
Each exchange read every column again: 57 ms of an 84 ms stage on RS6, the pushed-crate scenario, at rest.

## What changed
- `ManureZone::_markBorneByStillSteel` keeps each candidate's verdict by row, keyed on what the verdict reads.
- No conservation path changes; no new constant.

## How to verify
```
tools/dev/iter.sh build --test --fast worv.env.manure
```
`test_exchange_convert.cpp`'s new case reads red at 4f04f3f1f and green at 8f4e54b3d.

## Evidence
Packet `docs/manure-plan/evidence/2026-11-03-2137-walk-hold-at-rest/INDEX.md`, committed here.

| Reading, RS6 at rest, no other process on the GPU | head 4f04f3f1f | change 8f4e54b3d |
|---|---|---|
| engine update interval, p50 over the last two windows (ms) | 311.1 | 253.9 |

## Principle checks
P1 (the engine names no body role): the diff's one hit, `wheel`, is in a test comment.

## Scope and limits
The RS3 reading moved host mode between arms and is not attributed.
EOF
out=$(python3 "$LINT" --kind pr "$T/good-pr.md" 2>&1); rc=$?
check "a PR body written to the contract is clean" '[ "$rc" -eq 0 ]' "$out"
check "its shorthand is a warning, not a block" 'grep -q "warn shorthand-scenario" <<<"$out"' "$out"

cat >"$T/bad-pr.md" <<'EOF'
Packet: `~/tmp_workspace/artifacts/manure-plan/evidence/2026-11-03-2137-walk-hold-at-rest/INDEX.md`,
run on slot 1 of 10.50.2.178 with `lease.sh acquire 2137`, cubin b034f353dfa4. Then `ai:ready` (mine).
EOF
out=$(python3 "$LINT" --kind pr "$T/bad-pr.md" 2>&1); rc=$?
check "a host path, a slot, an address and a harness file block" \
    '[ "$rc" -eq 1 ] && grep -q "BLOCK host-path" <<<"$out" && grep -q "BLOCK gpu-slot" <<<"$out" && grep -q "BLOCK host-address" <<<"$out" && grep -q "BLOCK harness-file" <<<"$out"' "$out"
check "the missing sections are named" 'grep -q "BLOCK section: .## Summary" <<<"$out"' "$out"

printf 'Claimed. The walk hold is built at 249643293; the replays are queued.\n\n<details><summary>Harness trace</summary>\n\nharness tick 2026-11-03T04:30:40Z pid 2421513 claiming, budget 120 min\nhost khemoo, slot 1, worktree /home/khemoo/tmp_workspace/ai-worktrees/mpm-2137, lease.sh acquire 2137\n</details>\n' >"$T/comment.md"
out=$(python3 "$LINT" --kind comment "$T/comment.md" 2>&1); rc=$?
check "a comment whose protocol line sits inside its trace block is clean" '[ "$rc" -eq 0 ]' "$out"

printf 'harness tick 2026-11-03T09:06:50Z pid 205984 claiming (host khemoo), budget 120 min (labels: ai,ai:plan). Claim time 2026-11-03T09:06:50Z.\nWorktree /home/khemoo/tmp_workspace/ai-worktrees/mpm-2137.\n' >"$T/comment-bad.md"
out=$(python3 "$LINT" --kind comment "$T/comment-bad.md" 2>&1); rc=$?
check "the old claim line in the open is blocked for its pid, host and budget, and the path after it" \
    '[ "$rc" -eq 1 ] && grep -q "L1 BLOCK host-address" <<<"$out" && grep -q "L1 BLOCK process-id" <<<"$out" && grep -q "L1 BLOCK tick-aside" <<<"$out" && grep -q "L2 BLOCK host-path" <<<"$out"' "$out"

{ cat "$T/good-pr.md"; printf '\n<details><summary>Harness trace</summary>\npid 1\n</details>\n'; } >"$T/trace-in-body.md"
out=$(python3 "$LINT" --kind pr "$T/trace-in-body.md" 2>&1); rc=$?
check "a trace block in a body is blocked" '[ "$rc" -eq 1 ] && grep -q "trace-in-body" <<<"$out"' "$out"

printf '## Summary\nx\n## Ask\ny\n' >"$T/issue.md"
out=$(python3 "$LINT" --kind issue "$T/issue.md" 2>&1); rc=$?
check "an issue needs Summary and Ask only" '[ "$rc" -eq 0 ]' "$out"
out=$(PUBLIC_RECORD_ISSUE_SECTIONS="Summary,Ask,Evidence" python3 "$LINT" --kind issue "$T/issue.md" 2>&1); rc=$?
check "the required sections come from the environment" '[ "$rc" -eq 1 ] && grep -q "## Evidence" <<<"$out"' "$out"

out=$(python3 "$LINT" --kind pr "$T/good-pr.md" - 2>&1 </dev/null); rc=$?
check "an extra argument is a usage error" '[ "$rc" -eq 2 ]' "rc=$rc $out"
out=$(python3 "$LINT" 2>&1 </dev/null); rc=$?
check "no body and no kind is a usage error" '[ "$rc" -eq 2 ]' "rc=$rc $out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
