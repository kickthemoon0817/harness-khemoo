#!/usr/bin/env bash
# Exercises `harness claimable`'s priority gate with a fake gh that answers from fixture files.
set -u
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
mkdir -p "$H/state/locks" "$H/fake" "$H/fx"
: "${HARNESS_BIN:?run through tests/run.sh, which builds the harness binary}"
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
export PATH="$H/fake:$PATH" FX="$H/fx" HARNESS_HOME="$H" GH_REPO=o/r ISSUE_LABEL=ai:plan LANE_LABEL=ai:lane
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
issue() {  # number, comma-separated labels, optional body
  local n=$1 labels=$2 body=${3:-} out="" l
  for l in ${labels//,/ }; do out+="${out:+,}{\"name\":\"$l\"}"; done
  printf '{"number":%s,"body":"%s","labels":[%s]}' "$n" "$body" "$out"
}
plan() { local IFS=,; echo "[$*]" > "$FX/ai_plan.json"; }
firsts() { local IFS=,; echo "[$*]" > "$FX/ai_first.json"; }
lanes() { local IFS=,; echo "[$*]" > "$FX/ai_lane.json"; }
count() { "$HARNESS_BIN" claimable; }
reset() { rm -f "$FX"/*; }

reset; plan "$(issue 1 ai,ai:plan)" "$(issue 2 ai,ai:plan,ai:low)"
out=$(count); check "a low issue does not count while another is claimable" '[ "$out" = 1 ]' "$out"

reset; plan "$(issue 2 ai,ai:plan,ai:low)" "$(issue 3 ai,ai:plan,ai:low)"
out=$(count); check "low issues count when nothing else is claimable" '[ "$out" = 2 ]' "$out"

reset; plan "$(issue 2 ai,ai:plan,ai:low)" "$(issue 4 ai,ai:plan,ai:first,ai:operator)"
out=$(count); check "a held first issue does not keep the low queue waiting" '[ "$out" = 1 ]' "$out"

reset; plan "$(issue 5 ai,ai:plan,ai:lane)" "$(issue 6 ai,ai:plan,ai:first)"
out=$(count); check "a lane issue without first waits while a first issue is claimable" '[ "$out" = 1 ]' "$out"

reset; plan "$(issue 5 ai,ai:plan,ai:lane)" "$(issue 7 ai,ai:plan,ai:lane,ai:first)"
out=$(count); check "a first lane issue counts beside the gate" '[ "$out" = 1 ]' "$out"

reset; plan "$(issue 5 ai,ai:plan,ai:lane)" "$(issue 8 ai,ai:plan)"
out=$(count); check "with no first issue, lane issues count" '[ "$out" = 2 ]' "$out"

# The model lane lists only lane issues, so a first issue elsewhere is read from its own list.
reset; lanes "$(issue 5 ai,ai:plan,ai:lane)"; firsts "$(issue 6 ai,ai:plan,ai:first)"
out=$(ISSUE_LABEL=ai:lane count); check "the model lane waits on a first issue of the other lane" '[ "$out" = 0 ]' "$out"

reset; lanes "$(issue 5 ai,ai:plan,ai:lane)"; firsts "$(issue 6 ai,ai:plan,ai:first,ai:signoff)"
out=$(ISSUE_LABEL=ai:lane count); check "a first issue awaiting sign-off does not hold the model lane" '[ "$out" = 1 ]' "$out"

reset; lanes "$(issue 5 ai,ai:plan,ai:lane)"; firsts "$(issue 6 ai,ai:plan,ai:first 'Depends on: #99')"
out=$(ISSUE_LABEL=ai:lane count); check "a first issue blocked by an open dependency does not hold the model lane" '[ "$out" = 1 ]' "$out"
touch "$FX/closed.99"
out=$(ISSUE_LABEL=ai:lane count); check "once the dependency closes, the first issue holds it again" '[ "$out" = 0 ]' "$out"

reset; plan "$(issue 1 ai,ai:plan)" "$(issue 9 ai,ai:plan,ai:signoff)" "$(issue 10 ai,ai:plan,ai:operator)"
out=$(count); check "sign-off and operator holds are left out" '[ "$out" = 1 ]' "$out"

reset; plan "$(issue 5 ai,ai:plan,ai:lane)" "$(issue 6 ai,ai:plan,ai:first)"
out=$(LANE_LABEL= count); check "with the lane off, a lane-labelled issue counts like any other" '[ "$out" = 2 ]' "$out"

# A claim lock under HARNESS_STATE hides its issue from the count.
reset; plan "$(issue 1 ai,ai:plan)" "$(issue 2 ai,ai:plan)"
mkdir -p "$H/st/locks"; echo "2 $$ 2026-09-28T00:00:00Z" > "$H/st/locks/claim.2"
out=$(HARNESS_STATE="$H/st" count); check "a live local claim under HARNESS_STATE is left out" '[ "$out" = 1 ]' "$out"

# The open queue issues, for the supervisor's queue event: a claim and its label leave the number
# as it was, though the claimable count falls.
reset; plan "$(issue 1 ai,ai:plan)" "$(issue 2 ai,ai:plan)" "$(issue 3 ai,ai:plan)"
before=$(QUEUE_TOTAL_FILE="$H/total" count); total_before=$(cat "$H/total")
plan "$(issue 1 ai,ai:plan)" "$(issue 2 ai,ai:plan,ai:wip)" "$(issue 3 ai,ai:plan)"
echo "2 $$ 2026-09-28T00:00:00Z" > "$H/st/locks/claim.2"
after=$(HARNESS_STATE="$H/st" QUEUE_TOTAL_FILE="$H/total" count); total_after=$(cat "$H/total")
check "QUEUE_TOTAL_FILE counts the open issues, a claimed and labelled one included" \
    '[ "$before $total_before" = "3 3" ] && [ "$after $total_after" = "2 3" ]' "before=$before/$total_before after=$after/$total_after"
rm -f "$H/st/locks/claim.2" "$H/total"
out=$(count); check "without QUEUE_TOTAL_FILE nothing else is written" '[ ! -e "$H/total" ]' "$(ls "$H")"

reset; touch "$FX/fail"
out=$(count 2>/dev/null); rc=$?
check "a failed issue list prints no count and exits non-zero" '[ -z "$out" ] && [ "$rc" -ne 0 ]' "rc=$rc out=$out"
reset; out=$(GH_REPO= count 2>&1); rc=$?
check "an unset GH_REPO prints no count and exits non-zero" '[ "$rc" -ne 0 ] && ! [[ "$out" =~ ^[0-9]+$ ]]' "rc=$rc out=$out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
