#!/usr/bin/env bash
# Exercises bin/claimable.sh's priority gate with a fake gh that answers from fixture files.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/state/locks" "$H/fake" "$H/fx"
cp "$here/bin/claimable.sh" "$H/bin/claimable.sh"
# gh issue list --label L ... prints $H/fx/<L>.json (empty list when absent);
# gh issue view N --json state prints OPEN unless $H/fx/closed.<N> exists; comments print nothing.
cat > "$H/fake/gh" <<'EOF'
#!/usr/bin/env bash
fx="$FX"
if [ "$1 $2" = "issue list" ]; then
  label=""; prev=""
  for a in "$@"; do [ "$prev" = "--label" ] && label=$a; prev=$a; done
  [ -e "$fx/fail" ] && exit 1
  f="$fx/$(printf '%s' "$label" | tr ':' '_').json"
  if [ -e "$f" ]; then cat "$f"; else echo "[]"; fi
  exit 0
fi
if [ "$1 $2" = "issue view" ]; then
  case "$*" in
    *state*) if [ -e "$fx/closed.$3" ]; then echo CLOSED; else echo OPEN; fi ;;
    *) echo "" ;;
  esac
  exit 0
fi
exit 0
EOF
chmod +x "$H/fake/gh"
export PATH="$H/fake:$PATH" FX="$H/fx" HARNESS_HOME="$H" GH_REPO=o/r ISSUE_LABEL=ai:plan
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
issue() {  # number, comma-separated labels, optional body
  local n=$1 labels=$2 body=${3:-} out="" l
  for l in ${labels//,/ }; do out+="${out:+,}{\"name\":\"$l\"}"; done
  printf '{"number":%s,"body":"%s","labels":[%s]}' "$n" "$body" "$out"
}
plan() { local IFS=,; echo "[$*]" > "$FX/ai_plan.json"; }
firsts() { local IFS=,; echo "[$*]" > "$FX/ai_first.json"; }
fables() { local IFS=,; echo "[$*]" > "$FX/ai_fable.json"; }
count() { "$H/bin/claimable.sh"; }
reset() { rm -f "$FX"/*; }

reset; plan "$(issue 1 ai,ai:plan)" "$(issue 2 ai,ai:plan,ai:low)"
out=$(count); check "a low issue does not count while another is claimable" '[ "$out" = 1 ]' "$out"

reset; plan "$(issue 2 ai,ai:plan,ai:low)" "$(issue 3 ai,ai:plan,ai:low)"
out=$(count); check "low issues count when nothing else is claimable" '[ "$out" = 2 ]' "$out"

reset; plan "$(issue 2 ai,ai:plan,ai:low)" "$(issue 4 ai,ai:plan,ai:first,ai:operator)"
out=$(count); check "a held first issue does not keep the low queue waiting" '[ "$out" = 1 ]' "$out"

reset; plan "$(issue 5 ai,ai:plan,ai:fable)" "$(issue 6 ai,ai:plan,ai:first)"
out=$(count); check "a design issue without first waits while a first issue is claimable" '[ "$out" = 1 ]' "$out"

reset; plan "$(issue 5 ai,ai:plan,ai:fable)" "$(issue 7 ai,ai:plan,ai:fable,ai:first)"
out=$(count); check "a first design issue counts beside the gate" '[ "$out" = 1 ]' "$out"

reset; plan "$(issue 5 ai,ai:plan,ai:fable)" "$(issue 8 ai,ai:plan)"
out=$(count); check "with no first issue, design issues count as before" '[ "$out" = 2 ]' "$out"

# The fable lane lists only fable issues, so a first issue elsewhere is read from its own list.
reset; fables "$(issue 5 ai,ai:plan,ai:fable)"; firsts "$(issue 6 ai,ai:plan,ai:first)"
out=$(ISSUE_LABEL=ai:fable count); check "the fable lane waits on a first issue of the other lane" '[ "$out" = 0 ]' "$out"

reset; fables "$(issue 5 ai,ai:plan,ai:fable)"; firsts "$(issue 6 ai,ai:plan,ai:first,ai:signoff)"
out=$(ISSUE_LABEL=ai:fable count); check "a first issue awaiting sign-off does not hold the fable lane" '[ "$out" = 1 ]' "$out"

reset; fables "$(issue 5 ai,ai:plan,ai:fable)"; firsts "$(issue 6 ai,ai:plan,ai:first 'Depends on: #99')"
out=$(ISSUE_LABEL=ai:fable count); check "a first issue blocked by an open dependency does not hold the fable lane" '[ "$out" = 1 ]' "$out"
touch "$FX/closed.99"
out=$(ISSUE_LABEL=ai:fable count); check "once the dependency closes, the first issue holds it again" '[ "$out" = 0 ]' "$out"

reset; plan "$(issue 1 ai,ai:plan)" "$(issue 9 ai,ai:plan,ai:signoff)" "$(issue 10 ai,ai:plan,ai:operator)"
out=$(count); check "sign-off and operator holds are left out as before" '[ "$out" = 1 ]' "$out"

reset; touch "$FX/fail"
out=$(count); check "a failed issue list still prints 1" '[ "$out" = 1 ]' "$out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
