#!/usr/bin/env bash
# Exercises bin/public_record_lint.py on bodies written here: every blocking rule fires on a body
# written for the dev host, a body written to prompts/public-record.md is clean, a comment keeps
# its protocol line and its Harness trace block, and an installation's rules file adds to the core.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LINT="$here/bin/public_record_lint.py"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
# The core rules alone, whatever instance the checkout holds.
mkdir -p "$T/no-instance"
export HARNESS_INSTANCE_DIR="$T/no-instance"
unset PUBLIC_RECORD_RULES WORKTREE_ROOT HARNESS_HOME HARNESS_STATE
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }

out=$(python3 "$LINT" --self-test 2>&1); rc=$?
check "the built-in samples pass" '[ "$rc" -eq 0 ]' "$out"

cat >"$T/good-pr.md" <<'BODY'
## Summary
The handler parses the config once per change instead of once per request. No behaviour changes:
every existing test passes unchanged. Lands as version 2.4.0.1. Fixes #1234.

## Why
Each request parsed the whole config again: 57 ms of an 84 ms handler, with the sample config.

## What changed
- `ConfigCache.get` keeps the parsed config keyed on the file's hash.
- No public signature changes; no new setting.

## How to verify
```
make test
```
`test_config_cache.py`'s new case reads red at 4f04f3f1f and green at 8f4e54b3d.

## Evidence
| Reading, 1000 requests, no other process on the machine | head 4f04f3f1f | change 8f4e54b3d |
|---|---|---|
| handler time, p50 (ms) | 84.0 | 27.0 |

## Scope and limits
Measured on one host; it waited on ai:ready before the merge.
BODY
out=$(python3 "$LINT" --kind pr "$T/good-pr.md" 2>&1); rc=$?
check "a PR body written to the contract is clean" '[ "$rc" -eq 0 ]' "$out"
check "its label in prose is a warning, not a block" 'grep -q "warn label-mention" <<<"$out"' "$out"

cat >"$T/bad-pr.md" <<'BODY'
Results: `~/work/runs/1234/INDEX.md`, run on 192.0.2.178 with `claim.sh acquire 1234`. Then `ai:ready` (mine).
BODY
out=$(python3 "$LINT" --kind pr "$T/bad-pr.md" 2>&1); rc=$?
check "a host path, an address and a harness file block" \
    '[ "$rc" -eq 1 ] && grep -q "BLOCK host-path" <<<"$out" && grep -q "BLOCK host-address" <<<"$out" && grep -q "BLOCK harness-file" <<<"$out"' "$out"
check "the missing sections are named" 'grep -q "BLOCK section: .## Summary" <<<"$out"' "$out"

printf 'Claimed. The cache is built at 249643293; the suite is running.\n\n<details><summary>Harness trace</summary>\n\nharness tick 2026-11-03T04:30:40Z pid 2421513 claiming, budget 120 min\nhost host-a, worktree /home/you/worktrees/app-1234, claim.sh acquire 1234\n</details>\n' >"$T/comment.md"
out=$(python3 "$LINT" --kind comment "$T/comment.md" 2>&1); rc=$?
check "a comment whose protocol line sits inside its trace block is clean" '[ "$rc" -eq 0 ]' "$out"

printf 'harness tick 2026-11-03T09:06:50Z pid 205984 claiming (host host-a), budget 120 min (labels: ai,ai:wip). Claim time 2026-11-03T09:06:50Z.\nWorktree /home/you/worktrees/app-1234.\n' >"$T/comment-bad.md"
out=$(python3 "$LINT" --kind comment "$T/comment-bad.md" 2>&1); rc=$?
check "the old claim line in the open is blocked for its pid, host and budget, and the path after it" \
    '[ "$rc" -eq 1 ] && grep -q "L1 BLOCK host-address" <<<"$out" && grep -q "L1 BLOCK process-id" <<<"$out" && grep -q "L1 BLOCK tick-aside" <<<"$out" && grep -q "L2 BLOCK host-path" <<<"$out"' "$out"

{ cat "$T/good-pr.md"; printf '\n<details><summary>Harness trace</summary>\npid 1\n</details>\n'; } >"$T/trace-in-body.md"
out=$(python3 "$LINT" --kind pr "$T/trace-in-body.md" 2>&1); rc=$?
check "a trace block in a body is blocked" '[ "$rc" -eq 1 ] && grep -q "trace-in-body" <<<"$out"' "$out"

# Each pair: a near miss a reader's prose may hold is clean, and the form the rule exists for blocks.
# $1 name, $2 rule, $3 clean comment text, $4 blocking comment text.
near_miss() {
    rule=$2
    out=$(printf '%b' "$3" | python3 "$LINT" --kind comment - 2>&1); rc=$?
    check "$1: the near miss is clean" '[ "$rc" -eq 0 ]' "$out"
    out=$(printf '%b' "$4" | python3 "$LINT" --kind comment - 2>&1); rc=$?
    check "$1: the real form blocks" '[ "$rc" -eq 1 ] && grep -q "BLOCK $rule" <<<"$out"' "$out"
}
near_miss "a tmp folder inside the repository" container-tmp \
    'The log is `_build/x/release/tmp/a.log` at 8f4e54b3d.\n' 'The log is at /tmp/mg/run.log.\n'
near_miss "a four-part version" host-address \
    'Built against version 106.5.0.2.\n' 'Built on 203.0.113.12.\n'
near_miss "an address with a port" host-address \
    'Built against Version 106.5.0.2.\n' 'Copied over 198.51.100.5:22.\n'
near_miss "an address in a CGNAT range" host-address \
    'Built against v 4.5.0.1.\n' 'Reached the build host at 100.101.5.7.\n'
near_miss "a link-local address" host-address \
    'Built against v1.2.3.4 and v-1.2.3.5.\n' 'The host answered on 169.254.1.1.\n'
near_miss "a version word inside a longer word" host-address \
    'Built against version 1.2.3.4.\n' 'The dev 10.0.0.1 answered.\n'
near_miss "harness words in a closed fence" tick-aside \
    'The run printed:\n```\nharness tick 2 starts, this tick waits\n```\n' \
    'The run printed:\n```\nharness tick 2 starts\n~~~\nThen this tick waited.\n'
near_miss "a fence closed by a shorter run" tick-aside \
    '````\nthis tick waits\n````\n' '````\nthis tick waits\n```\n'
out=$(printf 'The run printed:\n```\nwrote /home/ci/out/run.log as pid 4242 on 203.0.113.12\n```\n' \
    | python3 "$LINT" --kind comment - 2>&1); rc=$?
check "a host path, a pid and an address inside a fence still block" \
    '[ "$rc" -eq 1 ] && grep -q "BLOCK host-path" <<<"$out" && grep -q "BLOCK process-id" <<<"$out" && grep -q "BLOCK host-address" <<<"$out"' "$out"
out=$(printf 'The run printed:\n```\nfree\nThen this tick took it.\n' | python3 "$LINT" --kind comment - 2>&1); rc=$?
check "an unclosed fence exempts nothing" '[ "$rc" -eq 1 ] && grep -q "L4 BLOCK tick-aside" <<<"$out"' "$out"

printf '## Summary\nx\n## Ask\ny\n' >"$T/issue.md"
out=$(python3 "$LINT" --kind issue "$T/issue.md" 2>&1); rc=$?
check "an issue needs Summary and Ask only" '[ "$rc" -eq 0 ]' "$out"
out=$(PUBLIC_RECORD_ISSUE_SECTIONS="Summary,Ask,Evidence" python3 "$LINT" --kind issue "$T/issue.md" 2>&1); rc=$?
check "the required sections come from the environment" '[ "$rc" -eq 1 ] && grep -q "## Evidence" <<<"$out"' "$out"
out=$(printf 'Waiting on ai:hold.\n' | LOW_LABEL=ai:hold python3 "$LINT" --kind comment - 2>&1); rc=$?
check "the labels come from the environment" '[ "$rc" -eq 0 ] && grep -q "warn label-mention" <<<"$out"' "$out"

# The template instance's rules: a blocking rule, a prose rule, a warning, a version word, a label.
out=$(printf 'Kept under scratch-runs/12/.\n' | PUBLIC_RECORD_RULES="$here/templates/instance/lint_rules.json" \
    python3 "$LINT" --kind comment - 2>&1); rc=$?
check "the template rules file adds its blocking rule" '[ "$rc" -eq 1 ] && grep -q "BLOCK scratch-path" <<<"$out"' "$out"
cp "$here/templates/instance/lint_rules.json" "$T/no-instance/lint_rules.json"
out=$(printf 'Built against SDK 106.5.0.2 for T1, waiting on ai:plan.\n```\ndevice slot 2 free\n```\n' \
    | python3 "$LINT" --kind comment - 2>&1); rc=$?
check "the instance's rules file loads by default: version word, warning, label and fenced prose rule" \
    '[ "$rc" -eq 0 ] && grep -q "warn shorthand-target" <<<"$out" && grep -q "warn label-mention: .ai:plan" <<<"$out"' "$out"
out=$(printf 'Measured on device slot 2.\n' | python3 "$LINT" --kind comment - 2>&1); rc=$?
check "the instance's prose rule blocks outside a fence" '[ "$rc" -eq 1 ] && grep -q "BLOCK device-slot" <<<"$out"' "$out"
rm "$T/no-instance/lint_rules.json"
out=$(printf 'Built against SDK 106.5.0.2.\n' | python3 "$LINT" --kind comment - 2>&1); rc=$?
check "without the rules file its version word is an address again" '[ "$rc" -eq 1 ] && grep -q "BLOCK host-address" <<<"$out"' "$out"

out=$(printf 'x\n' | python3 "$LINT" --kind comment - 2>&1 >/dev/null); rc=$?
check "an instance without lint_rules.json says only the core rules applied" \
    '[ "$rc" -eq 0 ] && grep -q "only the core rules applied" <<<"$out"' "rc=$rc $out"
out=$(printf 'x\n' | HARNESS_INSTANCE_DIR="$T/absent" python3 "$LINT" --kind comment - 2>&1 >/dev/null); rc=$?
check "no instance dir at all prints no warning" '[ "$rc" -eq 0 ] && [ -z "$out" ]' "rc=$rc $out"

# The harness's own trees: whatever the operator named them, a path under one blocks, fenced or not.
roots='WORKTREE_ROOT=/srv/wt HARNESS_HOME=/opt/harness HARNESS_STATE=/var/harness-state'
for path in /srv/wt/app-1234/src/a.py /opt/harness/bin/x /var/harness-state/logs/t.log; do
    out=$(printf 'The run printed:\n```\nwrote %s\n```\n' "$path" | env $roots python3 "$LINT" --kind comment - 2>&1); rc=$?
    check "a path under a harness root blocks inside a fence: $path" '[ "$rc" -eq 1 ] && grep -q "BLOCK harness-path" <<<"$out"' "$out"
done
out=$(printf 'Compared /srv/wtx/a and the repository path src/wt/a.py.\n' | env $roots python3 "$LINT" --kind comment - 2>&1); rc=$?
check "a path that only shares a root's prefix is clean" '[ "$rc" -eq 0 ]' "$out"
out=$(printf 'Wrote /srv/wt/app-1234/a.py.\n' | python3 "$LINT" --kind comment - 2>&1); rc=$?
check "with no roots set the rule is off" '[ "$rc" -eq 0 ]' "$out"

cat >"$T/custom.json" <<'JSON'
{"version_words": ["Runtime"],
 "rules": [{"id": "container-tmp", "pattern": "(?<![\\w./-])/tmp/(?!app\\.log\\b)", "hint": "a path in a container"},
           {"id": "ticket-ref", "pattern": "\\bTICKET-\\d+\\b", "hint": "link the issue instead"}]}
JSON
out=$(printf 'Runtime 106.5.0.2 wrote /tmp/app.log.\n' | PUBLIC_RECORD_RULES="$T/custom.json" \
    python3 "$LINT" --kind comment - 2>&1); rc=$?
check "PUBLIC_RECORD_RULES loads a custom file whose rule replaces the core one of its id" '[ "$rc" -eq 0 ]' "$out"
out=$(printf 'See TICKET-12 and /tmp/other.log.\n' | PUBLIC_RECORD_RULES="$T/custom.json" \
    python3 "$LINT" --kind comment - 2>&1); rc=$?
check "the custom file's own rule and its replacement both block" \
    '[ "$rc" -eq 1 ] && grep -q "BLOCK ticket-ref" <<<"$out" && grep -q "BLOCK container-tmp" <<<"$out"' "$out"
printf '{"rules": [{"id": "x", "pattern": "(", "hint": "y"}]}' >"$T/bad.json"
out=$(printf 'x\n' | PUBLIC_RECORD_RULES="$T/bad.json" python3 "$LINT" --kind comment - 2>&1); rc=$?
check "a rules file with a bad pattern is a usage error" '[ "$rc" -eq 2 ] && grep -q "bad pattern" <<<"$out"' "rc=$rc $out"
out=$(printf 'x\n' | PUBLIC_RECORD_RULES="$T/missing.json" python3 "$LINT" --kind comment - 2>&1); rc=$?
check "a named rules file that is missing is a usage error" '[ "$rc" -eq 2 ]' "rc=$rc $out"

out=$(python3 "$LINT" --kind pr "$T/good-pr.md" - 2>&1 </dev/null); rc=$?
check "an extra argument is a usage error" '[ "$rc" -eq 2 ]' "rc=$rc $out"
out=$(python3 "$LINT" 2>&1 </dev/null); rc=$?
check "no body and no kind is a usage error" '[ "$rc" -eq 2 ]' "rc=$rc $out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
