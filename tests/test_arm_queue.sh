#!/usr/bin/env bash
# Exercises bin/arm_queue.sh in a scratch harness home: a fake lease.sh answers from a script of
# replies, two fake trees carry an iter.sh and a scenario runner that act on control files, and
# fake docker, nvidia-smi and ssh on PATH stand in for the container, the card and the remote
# host. Every fake logs its calls, so each case reads what the queue asked for and in what order.
# check() evaluates each condition string later, so its variables expand then, not where it is written.
# shellcheck disable=SC2016,SC2034
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
H=$T/h CTR=$T/ctr CALLS=$T/calls.log
mkdir -p "$H/bin" "$T/fake" "$CTR/tmp" "$T/ev"
cp "$here/bin/arm_queue.sh" "$H/bin/"
export T CTR CALLS HARNESS_HOME=$H

# lease.sh: each acquire takes the next line of $T/lease.seq, else $T/lease.default.
#   grant[:host]  a remote slot on worv@<host> (default hostA), container iter-<host>
#   local         a local slot (no DOCKER_HOST); busy / yield  a refusal (exit 1)
cat > "$H/bin/lease.sh" <<'EOF'
#!/usr/bin/env bash
echo "lease $* want=${LEASE_WANT_HOST:-} holder=${LEASE_HOLDER_PID:-}" >> "$CALLS"
case "$1" in
  acquire)
    reply=$(head -1 "$T/lease.seq" 2>/dev/null)
    if [ -n "$reply" ]; then sed -i 1d "$T/lease.seq"; else reply=$(cat "$T/lease.default"); fi
    case "$reply" in
      grant*)
        h=${reply#grant}; h=${h#:}; h=${h:-hostA}
        echo "ACQUIRED slot=4 file=$T/lease.4 container=iter-$h domain=80 remote=worv@$h exclusive=all-remote"
        echo "export MANURE_GATE_LEASE_FILE=$T/lease.4 WORV_ITER_CONTAINER=iter-$h ROS_DOMAIN_ID=80 DOCKER_HOST=ssh://worv@$h PATH=$T/shim:\$PATH" ;;
      local)
        echo "ACQUIRED slot=1 file=$T/lease.1 container=iter-local domain=77"
        echo "export MANURE_GATE_LEASE_FILE=$T/lease.1 WORV_ITER_CONTAINER=iter-local ROS_DOMAIN_ID=77" ;;
      yield) echo "YIELDED slot=4"; exit 1 ;;
      *) echo "BUSY slot 4 held by 9999"; exit 1 ;;
    esac ;;
  release) echo "RELEASED slot=4" ;;
  status) echo "slot 4 HELD 2464 ${LEASE_HOLDER_PID:-} (alive)" ;;
esac
exit 0
EOF
cat > "$H/bin/nucleus_ok.sh" <<'EOF'
#!/usr/bin/env bash
echo "nucleus FAIL at probe 1"; exit 1
EOF

# docker: a container's /tmp is $CTR/tmp; grep, test and rm run there, pkill acts on nothing.
cat > "$T/fake/docker" <<'EOF'
#!/usr/bin/env bash
echo "docker $*" >> "$CALLS"
case "$1" in
  inspect) if [ -e "$T/no_sidecar" ]; then echo false; else echo true; fi ;;
  cp) cp -r "$CTR${2#*:}" "$3" ;;
  exec)
    shift; [ "$1" = -d ] && shift; shift
    cmd=$1; shift
    args=(); for a in "$@"; do case "$a" in /tmp/*) args+=("$CTR$a") ;; *) args+=("$a") ;; esac; done
    case "$cmd" in
      grep|test|rm) "$cmd" "${args[@]}" ;;
      pgrep) exit 1 ;;
    esac ;;
esac
EOF
cat > "$T/fake/nvidia-smi" <<'EOF'
#!/usr/bin/env bash
case "$*" in *memory.used*) echo 1234 ;; *compute-apps*) echo "4242, 5000 MiB" ;; esac
EOF
cat > "$T/fake/ssh" <<'EOF'
#!/usr/bin/env bash
echo "ssh $*" >> "$CALLS"
case "$*" in *loadavg*) echo "0.50 0.40 0.30 1/100 999" ;; esac
EOF

# The runner reads $T/arm.<arm>: its first word is the mode (ok, rc<N>, hang, crash-once,
# solver), then write:<container path>=<text> and stop (touch the packet's STOP) at its end.
cat > "$T/runner.py" <<'EOF'
import os, pathlib, sys, time
argv = sys.argv[1:]
out = argv[argv.index("--out") + 1]
arm = os.path.basename(out)
T, CTR = os.environ["T"], os.environ["CTR"]
with open(os.environ["CALLS"], "a") as log:
    log.write("runner %s %s container=%s\n" % (os.path.basename(sys.argv[0]), " ".join(argv), os.environ.get("WORV_ITER_CONTAINER", "")))
ctl = pathlib.Path(T, "arm." + arm)
words = ctl.read_text().split() if ctl.exists() else ["ok"]
if os.path.exists(out):
    sys.exit(9)
os.makedirs(out)
def kit_log(line):
    pathlib.Path(CTR, "tmp/kit.log").write_text("Logging to file /tmp/kit-%s-%d.log\n%s\n" % (arm, time.time_ns(), line))
if words[0] == "crash-once":
    ctl.write_text("ok")
    kit_log("A crash has occurred")
    time.sleep(60)
elif words[0] == "solver":
    kit_log("[Error] the zone could not start its solver on the device")
    time.sleep(60)
elif words[0] == "hang":
    time.sleep(60)
for w in words[1:]:
    if w.startswith("write:"):
        path, text = w[len("write:"):].split("=", 1)
        p = pathlib.Path(CTR + path)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
    elif w == "stop":
        pathlib.Path(os.environ["PKT"], "STOP").touch()
pathlib.Path(out, "run.json").write_text("{}")
sys.exit(int(words[0][2:]) if words[0].startswith("rc") else 0)
EOF
for t in A B; do
  mkdir -p "$T/trees/$t/tools/dev" "$T/trees/$t/tools/eval"
  cat > "$T/trees/$t/tools/dev/iter.sh" <<'EOF'
#!/usr/bin/env bash
tree=$(basename "$(cd "$(dirname "$0")/../.." && pwd)")
echo "iter $* tree=$tree container=${WORV_ITER_CONTAINER:-} docker_host=${DOCKER_HOST:-} issue=${MANURE_GATE_LEASE_ISSUE:-}" >> "$CALLS"
[ "$1" = up ] && [ -e "$T/fail.up" ] && exit 1
exit 0
EOF
  cp "$T/runner.py" "$T/trees/$t/tools/eval/scenario_run.py"
  cp "$T/runner.py" "$T/trees/$t/tools/eval/pbd_baseline.py"
done
chmod +x "$H/bin/"* "$T/fake/"* "$T"/trees/*/tools/dev/iter.sh
A=$T/trees/A B=$T/trees/B
unset -f docker nvidia-smi ssh 2>/dev/null
unset DEADLINE DOCKER_HOST
export PATH="$T/fake:$PATH" ARM_QUEUE_POLL_S=0.2 ARM_QUEUE_WATCH_S=0.2 ARM_QUEUE_SAMPLE_S=0.2

pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
new_case() {  # a fresh packet, call log and control files; the queue lines are the arguments
  P=$(mktemp -d -p "$T" pkt.XXXX); export PKT=$P
  : > "$CALLS"; rm -f "$T"/arm.* "$T/lease.seq" "$T/fail.up" "$T/no_sidecar"
  echo grant > "$T/lease.default"
  printf 'Logging to file /tmp/kit-old.log\n' > "$CTR/tmp/kit.log"
  printf '%s\n' "$@" > "$P/arms.queue"
}
run_q() { "$H/bin/arm_queue.sh" --issue 2464 --packet "$P" --queue "$P/arms.queue" --deadline +3600 --lease-margin 0 "$@" > "$P/launch.out" 2>&1; }
st() { cat "$P/runs/status" 2>/dev/null; }
last_st() { tail -1 "$P/runs/status" 2>/dev/null; }
calls() { cat "$CALLS"; }
n_calls() { grep -c -- "$1" "$CALLS"; }
at() { grep -n -- "$1" "$CALLS" | sed -n "${2:-1}p" | cut -d: -f1; }
holder() { awk '/^holder/ {print $2}' "$P/runs/lease.log" | tail -1; }
one_holder() { [ -n "$(holder)" ] && [ "$(grep '^lease ' "$CALLS" | grep -vc "holder=$(holder)$")" -eq 0 ]; }

new_case "$A|RS1-c|RS1||/exts/a=1,/exts/b=2|60||--no-moments,--through-sim-s,13" "$B|RS2-c|scen/RS2.toml|worv.robots.lifecycle||||"
run_q; rc=$?
check "a queue that runs to its end exits 0" '[ "$rc" -eq 0 ]' "rc=$rc $(cat "$P/launch.out")"
check "it acquires, brings tree A up, runs, renews, brings tree B up, runs, stops the kit and releases, in that order" \
  '[ "$(at "lease acquire 2464")" -lt "$(at "iter up .* tree=A")" ] && [ "$(at "iter up .* tree=A")" -lt "$(at "runner .*RS1-c")" ] &&
   [ "$(at "runner .*RS1-c")" -lt "$(at "lease acquire 2464" 2)" ] && [ "$(at "lease acquire 2464" 2)" -lt "$(at "iter up .* tree=B")" ] &&
   [ "$(at "iter up .* tree=B")" -lt "$(at "runner .*RS2-c")" ] && [ "$(at "runner .*RS2-c")" -lt "$(at "iter down")" ] &&
   [ "$(at "iter down")" -lt "$(at "lease release 2464")" ]' "$(calls)"
check "the lease is held as the queue's own PID on every call" 'one_holder' "holder=$(holder) $(grep '^lease ' "$CALLS")"
check "iter.sh runs under the export line, with the issue exported" \
  'grep -q "iter up .* tree=A container=iter-hostA docker_host=ssh://worv@hostA issue=2464" "$CALLS" && grep -q "iter sidecar tree=A container=iter-hostA" "$CALLS"' "$(calls)"
check "a bare scenario name, the flags and each kit setting reach the runner" \
  'grep -qx "runner scenario_run.py run tools/eval/scenarios/RS1.toml --run-mode lockstep_sync --no-moments --through-sim-s 13 --kit-setting /exts/a=1 --kit-setting /exts/b=2 --out $P/runs/RS1-c container=iter-hostA" "$CALLS"' "$(calls)"
check "a scenario path is passed as written, and the extra extension is mounted beside the base set" \
  'grep -q "runner scenario_run.py run scen/RS2.toml --run-mode lockstep_sync --out $P/runs/RS2-c" "$CALLS" && grep -q "iter up worv.core.runtime .*worv.robots.teleport worv.robots.lifecycle tree=B" "$CALLS"' "$(calls)"
check "status carries the grant, both arms start and end, ALLDONE, and RELEASED last" \
  'st | grep -q "^granted: export MANURE_GATE_LEASE_FILE=" && st | grep -q "^RS1-c start .* slot=iter-hostA domain=80 scenario=RS1 .*setting=\[/exts/a=1,/exts/b=2\] flags=\[--no-moments,--through-sim-s,13\] host=worv@hostA" &&
   st | grep -q "^RS1-c rc=0 end " && st | grep -q "^RS2-c rc=0 end " && st | grep -q "^ALLDONE arms.queue " && [[ "$(last_st)" == "RELEASED arms.queue "* ]]' "$(st)"
check "each arm has its output folder, runner log and card-memory log" \
  '[ -f "$P/runs/RS1-c/run.json" ] && [ -f "$P/runs/RS1-c.out" ] && grep -q "card_used_mib=1234" "$P/runs/RS1-c-gpu.log"' "$(ls "$P/runs")"
check "without --host-load no host is reached over ssh" '[ "$(n_calls "^ssh ")" -eq 0 ]' "$(calls)"

new_case "$A|RS1-c|RS1|||||"
echo busy > "$T/lease.default"
start=$(date +%s); run_q --deadline +2; rc=$?
check "no lease by the deadline exits 4 after polling more than once" '[ "$rc" -eq 4 ] && [ "$(n_calls "lease acquire")" -ge 2 ] && [ $(( $(date +%s) - start )) -le 6 ]' "rc=$rc $(calls)"
check "it records NO-LEASE, starts no container or arm, and still releases" \
  'st | grep -q "^NO-LEASE by deadline " && [ "$(n_calls "^iter ")" -eq 0 ] && [ "$(n_calls "^runner ")" -eq 0 ] && grep -q "lease release 2464" "$CALLS" && [[ "$(last_st)" == RELEASED* ]]' "$(st) $(calls)"

new_case "$A|RS1-c|RS1|||||"
touch "$P/STOP"; run_q; rc=$?
check "a STOP present at launch asks for no lease and exits 4" '[ "$rc" -eq 4 ] && [ "$(n_calls "lease acquire")" -eq 0 ] && st | grep -q "^STOP while waiting "' "rc=$rc $(st)"
check "the STOP exit still releases" 'grep -q "lease release 2464" "$CALLS" && [[ "$(last_st)" == RELEASED* ]]' "$(calls)"

new_case "$A|RS1-c|RS1|||||" "$A|RS2-c|RS2|||||"
echo "ok stop" > "$T/arm.RS1-c"; run_q; rc=$?
check "a STOP made between arms ends the queue before the next arm, exit 3" \
  '[ "$rc" -eq 3 ] && st | grep -q "^RS1-c rc=0 end" && st | grep -q "^STOP before RS2-c " && [ "$(n_calls "runner .*RS2-c")" -eq 0 ] && st | grep -q "^ALLDONE "' "rc=$rc $(st)"
check "the early end stops the kit and releases" '[ "$(at "iter down")" -lt "$(at "lease release")" ] && [[ "$(last_st)" == RELEASED* ]]' "$(calls)"

new_case "$A|RS1-c|RS1|||||" "$A|RS2-c|RS2|||||" "$A|RS3-p|RS3||||$T/ev/parent-abc|" "$A|RS4-p|RS4||||$T/ev/parent-abc|"
mkdir -p "$P/runs/RS1-c" "$T/ev/parent-abc/RS3-p" "$T/ev/parent-abc/RS4-p"; echo "{}" > "$P/runs/RS1-c/run.json"; echo "{}" > "$T/ev/parent-abc/RS4-p/run.json"
run_q; rc=$?
check "an arm whose folder holds a run is skipped and never run" \
  'st | grep -q "^RS1-c skipped: $P/runs/RS1-c exists" && [ "$(n_calls "runner .*RS1-c")" -eq 0 ] && st | grep -q "^RS2-c rc=0 end"' "$(st)"
check "a skipped first arm costs no renewal: one acquire per arm that ran" '[ "$(n_calls "lease acquire")" -eq 2 ]' "$(calls)"
check "an empty parent folder is this tick's claim: the arm runs into it" \
  '[ -f "$T/ev/parent-abc/RS3-p/run.json" ] && grep -q "^RS3-p rc=0 end" "$T/ev/parent-abc/status" && st | grep -q "^RS3-p rc=0 end"' "$(cat "$T/ev/parent-abc/status" 2>/dev/null)"
check "a finished parent arm is skipped in the parent's status" 'grep -q "^RS4-p skipped:" "$T/ev/parent-abc/status" && [ "$(n_calls "runner .*RS4-p")" -eq 0 ]' "$(cat "$T/ev/parent-abc/status")"
rm -rf "$T/ev/parent-abc"

new_case "$A|RS1-c|RS1|||||" "$A|RS2-c|RS2||||$T/ev/parent-new|"
run_q; rc=$?
check "a missing parent folder is made and claimed for the arm" '[ "$rc" -eq 0 ] && [ -f "$T/ev/parent-new/RS2-c/run.json" ] && grep -q "^RS2-c rc=0 end" "$T/ev/parent-new/status"' "rc=$rc $(st)"

new_case "$A|RS1-c|RS1|||||"
echo crash-once > "$T/arm.RS1-c"; run_q; rc=$?
check "a crash line in the new kit's log voids the arm and keeps it as void1" \
  'st | grep -q "^RS1-c void1: kit GPU out-of-memory or crash" && [ -d "$P/runs/RS1-c-void1" ] && [ -f "$P/runs/RS1-c-void1.out" ]' "$(st) $(ls "$P/runs")"
check "the void arm runs once more, after a renewal, and that run counts" \
  '[ "$(n_calls "runner .*RS1-c")" -eq 2 ] && [ "$(n_calls "lease acquire")" -eq 2 ] && st | grep -q "^RS1-c rc=0 end" && [ -f "$P/runs/RS1-c/run.json" ] && [ "$rc" -eq 0 ]' "rc=$rc $(st)"
check "a crash writes no STOP" '[ ! -e "$P/runs/STOP" ]' "$(cat "$P/runs/STOP" 2>/dev/null)"

new_case "$A|RS1-c|RS1|||||" "$A|RS2-c|RS2|||||"
printf 'Logging to file /tmp/kit-old.log\nA crash has occurred\n' > "$CTR/tmp/kit.log"; run_q; rc=$?
check "a crash line left by an earlier kit does not void the next arm" '[ "$rc" -eq 0 ] && ! st | grep -q void && [ "$(n_calls "^runner ")" -eq 2 ]' "rc=$rc $(st)"

new_case "$A|RS1-c|RS1|||||" "$A|RS2-c|RS2|||||"
echo solver > "$T/arm.RS1-c"; run_q; rc=$?
check "a solver that could not start writes STOP, and the retry and the next arm do not run" \
  'grep -q "could not start its solver" "$P/runs/STOP" && st | grep -q "^STOP before RS1-c" && [ "$(n_calls "^runner ")" -eq 1 ] && [ "$rc" -eq 3 ]' "rc=$rc $(st)"
check "the solver stop releases" 'grep -q "lease release 2464" "$CALLS" && [[ "$(last_st)" == RELEASED* ]]' "$(st)"

new_case "$A|RS1-c|RS1|||1||" "$A|RS2-c|RS2|||100000||"
run_q; rc=$?
check "an arm that would end past the deadline is not started, exit 3" \
  '[ "$rc" -eq 3 ] && st | grep -q "^RS1-c rc=0 end" && st | grep -q "^DEADLINE before RS2-c " && [ "$(n_calls "runner .*RS2-c")" -eq 0 ]' "rc=$rc $(st)"
check "the deadline stop releases" '[ "$(at "iter down")" -lt "$(at "lease release")" ] && [[ "$(last_st)" == RELEASED* ]]' "$(calls)"

new_case "$A|RS1-c|RS1|||||" "$A|RS2-c|RS2|||||"
echo stale > "$CTR/tmp/census.csv"; mkdir -p "$CTR/tmp/probe_dir"; echo old > "$CTR/tmp/probe_dir/old.txt"
echo "ok write:/tmp/census.csv=fresh write:/tmp/probe_dir/x.txt=hello" > "$T/arm.RS1-c"
run_q --collect /tmp/census.csv,/tmp/probe_dir; rc=$?
check "collected files and folders are copied into the arm's folder" \
  '[ "$(cat "$P/runs/RS1-c/census.csv")" = fresh ] && [ "$(cat "$P/runs/RS1-c/probe_dir/x.txt")" = hello ]' "$(ls -R "$P/runs/RS1-c")"
check "what an earlier run left is cleared before the arm, never collected" \
  '[ ! -e "$P/runs/RS1-c/probe_dir/old.txt" ] && [ ! -e "$P/runs/RS2-c/census.csv" ] && [ ! -e "$P/runs/RS2-c/probe_dir" ]' "$(ls -R "$P/runs/RS2-c")"
new_case "$A|RS1-c|RS1|||||"
run_q; rc=$?
check "without --collect nothing is cleared or copied" '[ "$(n_calls "docker cp")" -eq 0 ] && [ "$(n_calls "docker exec .* rm -rf")" -eq 0 ]' "$(calls)"

new_case "$A|RS1-c|RS1|||||" "$B|RS2-c|RS2|||||"
printf 'grant\nyield\nbusy\ngrant:hostB\n' > "$T/lease.seq"; run_q; rc=$?
check "a refused renewal stops the container at once and polls again" \
  'st | grep -q "^YIELD before RS2-c " && [ "$(at "lease acquire" 2)" -lt "$(at "iter down")" ] && [ "$(at "iter down")" -lt "$(at "lease acquire" 3)" ]' "$(st) $(calls)"
check "after the new grant the arm runs on the new slot and host" \
  '[ "$rc" -eq 0 ] && grep -q "iter up .* tree=B container=iter-hostB docker_host=ssh://worv@hostB" "$CALLS" && st | grep -q "^RS2-c start .*slot=iter-hostB .*host=worv@hostB" && st | grep -q "^RS2-c rc=0 end"' "rc=$rc $(st)"

new_case "$A|RS1-c|RS1|||||" "$B|RS2-c|RS2|||||"
printf 'grant\nyield\nlocal\n' > "$T/lease.seq"; run_q; rc=$?
check "a new grant on the local card drops the remote host the last export line set" \
  '[ "$rc" -eq 0 ] && grep -q "iter up .* tree=B container=iter-local docker_host= issue=2464" "$CALLS" && st | grep -q "^RS2-c start .*host=local"' "rc=$rc $(calls)"

new_case "$A|RS1-c|RS1|||||"
printf 'grant:hostB\ngrant:hostA\n' > "$T/lease.seq"; run_q --want-host worv@hostA; rc=$?
check "--want-host releases a grant on another host and keeps polling" \
  '[ "$rc" -eq 0 ] && grep -q "grant on worv@hostB, not worv@hostA: released" "$P/runs/lease.log" && [ "$(at "lease release")" -lt "$(at "lease acquire" 2)" ] && grep -q "container=iter-hostA" "$CALLS" && [ "$(n_calls "iter-hostB")" -eq 0 ]' "rc=$rc $(calls)"
check "--want-host asks the lease for that host alone" \
  'grep -q "lease acquire 2464 .*want=worv@hostA" "$CALLS" && ! grep "lease acquire" "$CALLS" | tail -1 | grep -q "want= "' "$(calls)"

new_case "$A|RS1-c|RS1|||||"
run_q --runner baseline --host-load; rc=$?
check "--runner baseline runs pbd_baseline.py with the engine arm" \
  'grep -q "runner pbd_baseline.py run tools/eval/scenarios/RS1.toml --arm engine --run-mode lockstep_sync --out" "$CALLS"' "$(calls)"
check "--host-load samples the remote host and records the load and the lease" \
  'grep -q "^ssh .*worv@hostA cat /proc/loadavg" "$CALLS" && st | grep -q "^RS1-c start .*load=0.50,0.40,0.30 card_apps=\[4242, 5000 MiB;\]" && st | grep -q "^RS1-c lease slot 4 HELD" && grep -q "0.50 0.40 0.30;4242" "$P/runs/host-load.log"' "$(st)"

new_case "$A|RS1-c|RS1|||||"
run_q --nucleus-check; rc=$?
check "--nucleus-check ends the queue before the bring-up when Nucleus fails" \
  '[ "$rc" -eq 3 ] && st | grep -q "^nucleus not ok before RS1-c" && [ "$(n_calls "^iter up")" -eq 0 ] && [ ! -e "$P/runs/RS1-c" ] && [[ "$(last_st)" == RELEASED* ]]' "rc=$rc $(st)"

new_case "$A|RS1-c|RS1|||||" "$A|RS2-c|RS2|||||"
touch "$T/fail.up"; run_q; rc=$?
check "a failed bring-up is tried once more, then the queue goes on to the next arm" \
  '[ "$(n_calls "^iter up")" -eq 4 ] && [ "$(st | grep -c "up failed")" -eq 4 ] && [ "$(n_calls "^runner ")" -eq 0 ] && [ "$rc" -eq 0 ] && grep -q "iter down" "$CALLS"' "rc=$rc $(st)"

new_case "$A|RS1-c|RS1|||||"
echo hang > "$T/arm.RS1-c"
"$H/bin/arm_queue.sh" --issue 2464 --packet "$P" --queue "$P/arms.queue" --deadline +3600 > "$P/launch.out" 2>&1 & Q=$!
for _ in $(seq 1 50); do st | grep -q "^RS1-c start" && break; sleep 0.2; done
sleep 0.5; kill -TERM "$Q"; wait "$Q"; rc=$?
check "a TERM mid-arm ends the arm, stops the container and releases" \
  '[ "$rc" -eq 143 ] && grep -q "iter down" "$CALLS" && grep -q "lease release 2464" "$CALLS" && [[ "$(last_st)" == RELEASED* ]]' "rc=$rc $(st) $(calls)"
check "no runner is left behind" '! pgrep -f "out $P/runs/RS1-c" >/dev/null' "$(pgrep -af "$P/runs/RS1-c")"

new_case "$A|RS1|RS1|--no-moments|/exts/a=1|"
run_q; rc=$?
check "a line in the old six-field order is refused before any lease" \
  '[ "$rc" -eq 2 ] && grep -q "field 4 is extra_exts" "$P/launch.out" && [ ! -s "$CALLS" ]' "rc=$rc $(cat "$P/launch.out")"
new_case "$T/trees/none|RS1-c|RS1|||||"
run_q; rc=$?
check "a missing tree is refused before any lease" '[ "$rc" -eq 2 ] && grep -q "has no tools/dev/iter.sh" "$P/launch.out" && [ ! -s "$CALLS" ]' "rc=$rc $(cat "$P/launch.out")"
new_case "$A|RS1-c|RS1|||||"
"$H/bin/arm_queue.sh" --issue 2464 --packet "$P" --queue "$P/arms.queue" > "$P/launch.out" 2>&1; rc=$?
check "a queue without a deadline is refused" '[ "$rc" -eq 2 ] && grep -q "deadline" "$P/launch.out" && [ ! -s "$CALLS" ]' "rc=$rc $(cat "$P/launch.out")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
