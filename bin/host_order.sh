#!/usr/bin/env bash
# The host order policy of bin/host_health.sh: reads no host, so it is tested on its own.
#
#   host_order.sh <preference> <streaks> <results>
#     preference  one host a line, best first: the operator's order
#     streaks     "<host> <ok|demoted> <bad> <good> <since>" a line, the previous run's output
#                 (absent on the first run); since is the epoch of the host's last demotion or promotion
#     results     "<host> <ok|slow|down>" a line: this probe's verdicts
#
# Prints one "streak <host> <state> <bad> <good> <since>" line a host, then
# "order <host> ...": the preference with every demoted host moved to the end, in
# preference order. A demoted host stays in the order, so it still takes work
# that copies little. A host is demoted after HOST_DEMOTE_AFTER (2) bad verdicts
# in a row and promoted after HOST_PROMOTE_AFTER (3) good ones; a host without a
# verdict this run keeps its streaks.
set -u
[ "$#" -eq 3 ] || { echo "usage: host_order.sh <preference> <streaks> <results>" >&2; exit 2; }
pref_file=$1; streak_file=$2; result_file=$3
: "${HOST_DEMOTE_AFTER:=2}"
: "${HOST_PROMOTE_AFTER:=3}"
now=${HOST_HEALTH_NOW:-$(date +%s)}

declare -A state=() bad=() good=() since=() known=()
hosts=()
remember() { [ -n "${known[$1]:-}" ] || { known[$1]=1; hosts+=("$1"); }; }
num() { [[ "$1" =~ ^[0-9]+$ ]] && echo "$1" || echo 0; }

if [ -r "$streak_file" ]; then
  while read -r h s b g t _; do
    [[ -n "${h:-}" && "${h:0:1}" != "#" ]] || continue
    remember "$h"
    [ "$s" = demoted ] && state[$h]=demoted || state[$h]=ok
    bad[$h]=$(num "${b:-}"); good[$h]=$(num "${g:-}"); since[$h]=$(num "${t:-}")
  done < "$streak_file"
fi

while read -r h verdict _; do
  [[ -n "${h:-}" && "${h:0:1}" != "#" ]] || continue
  remember "$h"
  : "${state[$h]:=ok}" "${bad[$h]:=0}" "${good[$h]:=0}" "${since[$h]:=0}"
  if [ "${verdict:-}" = ok ]; then
    bad[$h]=0; good[$h]=$(( ${good[$h]} + 1 ))
    if [ "${state[$h]}" = demoted ] && [ "${good[$h]}" -ge "$HOST_PROMOTE_AFTER" ]; then state[$h]=ok; since[$h]=$now; fi
  else
    good[$h]=0; bad[$h]=$(( ${bad[$h]} + 1 ))
    if [ "${state[$h]}" = ok ] && [ "${bad[$h]}" -ge "$HOST_DEMOTE_AFTER" ]; then state[$h]=demoted; since[$h]=$now; fi
  fi
done < "$result_file"

for h in "${hosts[@]}"; do
  printf 'streak %s %s %s %s %s\n' "$h" "${state[$h]}" "${bad[$h]}" "${good[$h]}" "${since[$h]}"
done

first=(); last=(); declare -A listed=()
if [ -r "$pref_file" ]; then
  while read -r h _; do
    [[ -n "${h:-}" && "${h:0:1}" != "#" && -z "${listed[$h]:-}" ]] || continue
    listed[$h]=1
    if [ "${state[$h]:-ok}" = demoted ]; then last+=("$h"); else first+=("$h"); fi
  done < "$pref_file"
fi
echo "order ${first[*]} ${last[*]}" | sed 's/ *$//; s/  */ /g'
