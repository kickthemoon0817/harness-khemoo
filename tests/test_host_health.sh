#!/usr/bin/env bash
# Exercises bin/host_order.sh on its own, then bin/host_health.sh in a scratch harness home
# with a fake ssh (FAKE_DOWN and FAKE_SLOW name the hosts that fail or upload slowly) and a
# copy of bin/lease.sh behind a wrapper that logs every call.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d); trap 'rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/state/locks" "$H/fake" "$H/p"
cp "$here/bin/host_health.sh" "$here/bin/host_order.sh" "$here/bin/lease.sh" "$H/bin/"
L="$H/state/locks"; S="$H/state"
cat > "$H/fake/ssh" <<'EOF'
#!/usr/bin/env bash
host=""; cmd=""; op=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) shift ;;
    -O) op=$2; shift ;;
    -*) ;;
    *) if [ -z "$host" ]; then host=$1; else cmd+="$1 "; fi ;;
  esac
  shift
done
[ -n "$op" ] && exit 0
case " ${FAKE_DOWN:-} " in *" $host "*) exit 255 ;; esac
case "$cmd" in
  *loadavg*) echo "1.50 16 ${FAKE_GPU_UTIL:-0} 1234" ;;
  *"cat > /dev/null"*) cat > /dev/null; case " ${FAKE_SLOW:-} " in *" $host "*) sleep "${FAKE_SLOW_S:-1.5}" ;; esac ;;
esac
exit 0
EOF
cat > "$H/fake/lease" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "$H/lease.calls"
exec "$H/bin/lease.sh" "\$@"
EOF
chmod +x "$H/fake/"*
export HARNESS_HOME="$H" HARNESS_STATE="$S" HARNESS_PATH="$H/fake:$PATH" HOST_HEALTH_LEASE="$H/fake/lease"
export HOST_MIN_UPLOAD_MB_S=1 HOST_UPLOAD_TIMEOUT_S=5
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }

# --- bin/host_order.sh on its own -------------------------------------------------------
POL="$H/bin/host_order.sh"; P="$H/p"
printf 'a\nb\n' > "$P/pref"
policy() {  # <verdict for a> [verdict for b]: one run over $P/streaks, which it rewrites
  printf 'a %s\nb %s\n' "$1" "${2:-ok}" > "$P/results"
  HOST_HEALTH_NOW=${NOW:-1000} "$POL" "$P/pref" "$P/streaks" "$P/results" > "$P/out"
  sed -n 's/^streak //p' "$P/out" > "$P/streaks"
}
order() { sed -n 's/^order //p' "$P/out"; }
rm -f "$P/streaks"; policy slow
check "one bad probe does not demote" '[ "$(order)" = "a b" ] && grep -qx "a ok 1 0 0" "$P/streaks"' "$(cat "$P/out")"
NOW=2000 policy down
check "a second bad probe in a row demotes and moves the host last" '[ "$(order)" = "b a" ] && grep -qx "a demoted 2 0 2000" "$P/streaks"' "$(cat "$P/out")"
policy slow
check "a demoted host stays last and keeps its demotion time" '[ "$(order)" = "b a" ] && grep -qx "a demoted 3 0 2000" "$P/streaks"' "$(cat "$P/out")"
policy ok; policy ok
check "two good probes do not promote" '[ "$(order)" = "b a" ] && grep -qx "a demoted 0 2 2000" "$P/streaks"' "$(cat "$P/out")"
NOW=3000 policy ok
check "the third good probe in a row promotes" '[ "$(order)" = "a b" ] && grep -qx "a ok 0 3 3000" "$P/streaks"' "$(cat "$P/out")"
rm -f "$P/streaks"; policy slow; policy ok; policy slow
check "a good probe between two bad ones resets the bad streak" '[ "$(order)" = "a b" ] && grep -qx "a ok 1 0 0" "$P/streaks"' "$(cat "$P/out")"
rm -f "$P/streaks"; policy slow slow; policy slow slow
check "with every host degraded the preference order stands" '[ "$(order)" = "a b" ]' "$(cat "$P/out")"
rm -f "$P/streaks"; policy ok slow; policy ok slow
check "a degraded host already last leaves the order as it was" '[ "$(order)" = "a b" ] && grep -qx "b demoted 2 0 1000" "$P/streaks"' "$(cat "$P/out")"
printf 'a demoted 4 0 900\n' > "$P/streaks"; printf 'b ok\n' > "$P/results"
"$POL" "$P/pref" "$P/streaks" "$P/results" > "$P/out"
check "a host without a verdict this run keeps its streaks" '[ "$(order)" = "b a" ] && grep -qx "streak a demoted 4 0 900" "$P/out"' "$(cat "$P/out")"
printf 'a garbage x y z\n' > "$P/streaks"; printf 'a slow\n' > "$P/results"
"$POL" "$P/pref" "$P/streaks" "$P/results" > "$P/out"
check "an unreadable streak line starts over instead of failing" 'grep -qx "streak a ok 1 0 0" "$P/out"' "$(cat "$P/out")"

# --- bin/host_health.sh -------------------------------------------------------------------
HH="$H/bin/host_health.sh"
printf 'fast@one\nslow@two\n' > "$L/remote-card"
printf 'slow@two\nfast@one\n' > "$L/harness-first-remote-only"
calls() { cat "$H/lease.calls" 2>/dev/null | grep -c harness-only; }
only() { paste -sd' ' "$L/harness-first-remote-only"; }

"$HH"
check "the preference is seeded from the harness-only list when absent" '[ "$(paste -sd" " "$L/harness-host-preference")" = "slow@two fast@one" ] && grep -q "preference seeded" "$S/logs/host_health.log"' "$(cat "$L/harness-host-preference" "$S/logs/host_health.log" 2>&1)"
check "a row a host is appended under a header, nine columns each" '[ "$(wc -l < "$S/host_health.tsv")" -eq 3 ] && [ "$(head -1 "$S/host_health.tsv" | cut -f1)" = time ] && awk -F"\t" "NF != 9 { exit 1 }" "$S/host_health.tsv"' "$(cat "$S/host_health.tsv")"
check "a healthy host reads ok with its load, cores and card" 'awk -F"\t" "\$2 == \"fast@one\" && \$5 == \"1.50\" && \$6 == 16 && \$7 == 0 && \$8 == 1234 && \$9 == \"ok\" && \$4 + 0 >= 1 { f = 1 } END { exit !f }" "$S/host_health.tsv"' "$(cat "$S/host_health.tsv")"
check "with every host healthy and the order as preferred nothing is applied" '[ "$(calls)" -eq 0 ]' "$(cat "$H/lease.calls" 2>/dev/null)"

FAKE_SLOW=slow@two "$HH"
check "one slow probe leaves the order alone" '[ "$(calls)" -eq 0 ] && [ "$(only)" = "slow@two fast@one" ]' "$(only)"
check "a slow upload is recorded as slow with its rate" 'tail -2 "$S/host_health.tsv" | awk -F"\t" "\$2 == \"slow@two\" && \$9 == \"slow\" && \$4 + 0 < 1 { f = 1 } END { exit !f }"' "$(tail -2 "$S/host_health.tsv")"
FAKE_SLOW=slow@two "$HH"
check "the second slow probe applies the new order through lease.sh" '[ "$(calls)" -eq 1 ] && grep -qx "harness-only --on fast@one slow@two" "$H/lease.calls" && [ "$(only)" = "fast@one slow@two" ]' "$(cat "$H/lease.calls" 2>/dev/null) / $(only)"
check "the change is logged once with its reason" '[ "$(grep -c " order " "$S/logs/host_health.log")" -eq 1 ] && grep -q "order slow@two fast@one -> fast@one slow@two; slow@two degraded after 2 bad probes (slow, upload" "$S/logs/host_health.log"' "$(cat "$S/logs/host_health.log")"
check "the preference is left as the operator's" '[ "$(paste -sd" " "$L/harness-host-preference")" = "slow@two fast@one" ]' "$(cat "$L/harness-host-preference")"
FAKE_SLOW=slow@two HOST_UPLOAD_TIMEOUT_S=1 "$HH"
check "an upload past its timeout reads timeout and keeps the host last, applying nothing" '[ "$(calls)" -eq 1 ] && tail -2 "$S/host_health.tsv" | grep -q "slow@two.*timeout.*slow$"' "$(tail -2 "$S/host_health.tsv")"
"$HH"; "$HH"
check "two good probes do not bring the host back" '[ "$(calls)" -eq 1 ] && [ "$(only)" = "fast@one slow@two" ]' "$(only)"
"$HH"
check "the third good probe restores the preference" '[ "$(calls)" -eq 2 ] && [ "$(only)" = "slow@two fast@one" ] && grep -q "slow@two healthy after 3 good probes" "$S/logs/host_health.log"' "$(cat "$S/logs/host_health.log")"

FAKE_DOWN=fast@one "$HH"
check "a host that does not answer reads down with no measurements" 'tail -2 "$S/host_health.tsv" | grep -qP "^\S+\tfast@one\t-\t-\t-\t-\t-\t-\tdown$"' "$(tail -2 "$S/host_health.tsv")"

printf 'fast@one\nslow@two\n' > "$L/harness-host-preference"; rm -f "$S/host_health.streaks"
"$HH"
check "an operator's new preference is applied on the next probe" '[ "$(calls)" -eq 3 ] && [ "$(only)" = "fast@one slow@two" ] && grep -q "the harness-only list was not the preference" "$S/logs/host_health.log"' "$(cat "$S/logs/host_health.log")"

rm -f "$L/harness-first-remote-only" "$S/host_health.streaks" "$L/harness-host-preference"
FAKE_SLOW=fast@one "$HH"; FAKE_SLOW=fast@one "$HH"
check "with harness-only off nothing is applied or seeded" '[ "$(calls)" -eq 3 ] && [ ! -e "$L/harness-first-remote-only" ] && [ ! -e "$L/harness-host-preference" ]' "$(ls "$L")"
check "with harness-only off the probe still records rows and streaks" 'grep -q "^fast@one demoted 2 0 " "$S/host_health.streaks" && [ "$(tail -2 "$S/host_health.tsv" | grep -c slow$)" -eq 1 ]' "$(cat "$S/host_health.streaks")"

HOST_HEALTH_KEEP_ROWS=4 "$HH"
check "the table keeps its header and the last rows only" '[ "$(wc -l < "$S/host_health.tsv")" -eq 5 ] && [ "$(head -1 "$S/host_health.tsv" | cut -f1)" = time ]' "$(cat "$S/host_health.tsv")"

exec 7>"$L/host_health.lock"; flock 7
"$HH"; rc=$?
check "a run that finds another running exits at once without a row" '[ "$rc" -eq 0 ] && [ "$(wc -l < "$S/host_health.tsv")" -eq 5 ]' "rc=$rc"
exec 7>&-

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
