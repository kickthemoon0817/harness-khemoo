#!/usr/bin/env bash
# Exercises bin/remote-docker/docker and bin/remote-env.sh with fake ssh, rsync and
# docker that log every call, so each case reads what the shim copied and ran.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$HOME/.cache"; T=$(mktemp -d -p "$HOME/.cache"); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/fake" "$T/run" "$T/ws/trees/w1/ext" "$T/ws/trees/w1/out" "$T/ws/other" "$T/state/locks"
LOG="$T/calls.log"; export LOG
cat > "$T/fake/ssh" <<'EOF'
#!/usr/bin/env bash
echo "ssh $*" >> "$LOG"
case "$*" in *"id -u"*) echo "1001:1001" ;; esac
exit 0
EOF
cat > "$T/fake/rsync" <<'EOF'
#!/usr/bin/env bash
echo "rsync $*" >> "$LOG"
exit 0
EOF
cat > "$T/fake/docker" <<'EOF'
#!/usr/bin/env bash
echo "docker DOCKER_HOST=${DOCKER_HOST:-} $*" >> "$LOG"
exit "${FAKE_DOCKER_RC:-0}"
EOF
chmod +x "$T/fake/"*
SHIM="$here/bin/remote-docker/docker"
export PATH="$T/fake:$PATH" XDG_RUNTIME_DIR="$T/run" REMOTE_DOCKER_LOCK_PARENTS="$T/ws/trees"
RO="$T/ws/trees/w1/ext"; RW="$T/ws/trees/w1/out"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
calls() { cat "$LOG" 2>/dev/null; }
n_lines() { grep -c -- "$1" "$LOG" 2>/dev/null || true; }

: > "$LOG"; DOCKER_HOST="" "$SHIM" run --rm -v "$RW:/b" img true
check "without an ssh DOCKER_HOST the real docker runs alone" '[ "$(calls)" = "docker DOCKER_HOST= run --rm -v $RW:/b img true" ]' "$(calls)"

export DOCKER_HOST=ssh://fake@host
: > "$LOG"; "$SHIM" ps -q
check "a command that mounts nothing goes straight to the remote daemon" '[ "$(calls)" = "docker DOCKER_HOST=ssh://fake@host ps -q" ]' "$(calls)"

: > "$LOG"; "$SHIM" run --rm -v "$RO:/a:ro" -v "$RW:/b" -v named:/c -v /tmp/x:/d img make; rc=$?
check "a foreground run copies each host path it mounts to the remote" '[ "$(n_lines "rsync -a --delete")" -eq 2 ] && grep -q -- "--delete -e ssh -o BatchMode=yes $RO fake@host:$T/ws/trees/w1/" "$LOG" && grep -q -- "--delete -e ssh -o BatchMode=yes $RW fake@host:$T/ws/trees/w1/" "$LOG"' "$(calls)"
check "named volumes and /tmp paths are not copied" '! grep -q "rsync.*named" "$LOG" && ! grep -q "rsync.*/tmp/x" "$LOG"' "$(calls)"
check "the run itself goes to the remote daemon with its arguments as given" 'grep -qx "docker DOCKER_HOST=ssh://fake@host run --rm -v $RO:/a:ro -v $RW:/b -v named:/c -v /tmp/x:/d img make" "$LOG"' "$(calls)"
check "after the run the writable mount is handed to the remote user" 'grep -q "run --rm -v $RW:/w --entrypoint chown .* -R 1001:1001 /w" "$LOG" && ! grep -q "run --rm -v $RO:/w --entrypoint chown" "$LOG"' "$(calls)"
check "only the writable mount is copied back, without deleting" 'grep -q "rsync -a --update -e ssh -o BatchMode=yes fake@host:$RW $T/ws/trees/w1/" "$LOG" && [ "$(n_lines "rsync -a --update")" -eq 1 ]' "$(calls)"
check "the copy back follows the run" '[ "$(grep -n "img make" "$LOG" | cut -d: -f1)" -lt "$(grep -n "rsync -a --update" "$LOG" | cut -d: -f1)" ]' "$(calls)"
check "the run's exit code is the shim's" '[ "$rc" -eq 0 ]' "rc=$rc"

: > "$LOG"; FAKE_DOCKER_RC=3 "$SHIM" run --rm -v "$RW:/b" img false; rc=$?
check "a failed run still copies its outputs back and keeps its exit code" '[ "$rc" -eq 3 ] && grep -q "rsync -a --update" "$LOG"' "rc=$rc $(calls)"

: > "$LOG"; "$SHIM" run -d --name kit -v "$RW:/b" img serve
check "a detached run is copied over and started, and nothing is copied back" 'grep -q "rsync -a --delete" "$LOG" && grep -q "run -d --name kit" "$LOG" && ! grep -q "rsync -a --update" "$LOG" && ! grep -q "entrypoint chown" "$LOG"' "$(calls)"

: > "$LOG"; "$SHIM" run -dit -v "$RW:/b" img serve
check "a combined short flag with d counts as detached" '! grep -q "rsync -a --update" "$LOG"' "$(calls)"

: > "$LOG"; "$SHIM" run --rm --mount "type=bind,source=$RO,target=/a,readonly" --mount "type=bind,src=$RW,dst=/b" img make
check "--mount sources are copied, and only the writable one comes back" '[ "$(n_lines "rsync -a --delete")" -eq 2 ] && [ "$(n_lines "rsync -a --update")" -eq 1 ] && grep -q "fake@host:$RW " "$LOG"' "$(calls)"

: > "$LOG"; "$SHIM" run --rm -v "$T/ws/trees/w1:/w" -v "$RO:/e:ro" img make
check "a path under one already copied is not copied twice" '[ "$(n_lines "rsync -a --delete")" -eq 1 ]' "$(calls)"

: > "$LOG"; ( docker() { echo "LOOPED" >> "$LOG"; }; export -f docker; "$SHIM" run --rm -v "$RW:/b" img make )
check "an exported docker function does not loop back into the shim" '! grep -q LOOPED "$LOG" && grep -q "img make" "$LOG"' "$(calls)"

# The writable run holds its tree's lock: a second run of the same tree waits for it.
cat > "$T/fake/docker" <<'EOF'
#!/usr/bin/env bash
echo "docker DOCKER_HOST=${DOCKER_HOST:-} $*" >> "$LOG"
case "$*" in *"slow"*) sleep 2 ;; esac
exit 0
EOF
: > "$LOG"
"$SHIM" run --rm -v "$RW:/b" img slow & S1=$!
sleep 0.5
start=$(date +%s.%N); "$SHIM" run --rm -v "$RO:/a:ro" img quick; waited=$(echo "$(date +%s.%N) - $start" | bc)
wait $S1
check "a read-only run of a tree waits while another run writes it" '[ "$(echo "$waited > 1.0" | bc)" -eq 1 ]' "waited=$waited"
"$SHIM" run --rm -v "$T/ws/other:/o" img slow & S2=$!
sleep 0.5
start=$(date +%s.%N); "$SHIM" run --rm -v "$RW:/b" img quick; waited=$(echo "$(date +%s.%N) - $start" | bc)
wait $S2
check "runs of different trees do not wait on each other" '[ "$(echo "$waited < 1.0" | bc)" -eq 1 ]' "waited=$waited"

# bin/remote-env.sh: the function exists only while the local host is off and a remote host is named.
out=$(HARNESS_HOME="$here" HARNESS_STATE="$T/state" bash -c '. "$HARNESS_HOME/bin/remote-env.sh"; remote_host_env; type -t docker || echo none')
check "with the local host on, no docker function is set" '[ "$out" = none ] || [ "$out" = file ]' "$out"
out=$(env -u DOCKER_HOST HARNESS_HOME="$here" HARNESS_STATE="$T/state" bash -c '. "$HARNESS_HOME/bin/remote-env.sh"; remote_host_env; echo "${DOCKER_HOST:-unset}"')
check "with the local host on, DOCKER_HOST is left alone" '[ "$out" = unset ]' "$out"
echo fake@host > "$T/state/locks/remote-card"; touch "$T/state/locks/local-card-off"
out=$(HARNESS_HOME="$here" HARNESS_STATE="$T/state" bash -c '. "$HARNESS_HOME/bin/remote-env.sh"; remote_host_env; bash -c "export PATH=/usr/bin:/bin; type -t docker"')
check "with the local host off, a child bash with its own PATH still runs docker through the shim" '[ "$out" = function ]' "$out"
: > "$LOG"
HARNESS_HOME="$here" HARNESS_STATE="$T/state" PATH="$T/fake:$PATH" bash -c '. "$HARNESS_HOME/bin/remote-env.sh"; remote_host_env; unset DOCKER_HOST; bash -c "docker run --rm -v '"$RW"':/b img make"'
check "the function sends the run to the named host through the shim" 'grep -q "docker DOCKER_HOST=ssh://fake@host run --rm -v $RW:/b img make" "$LOG" && grep -q "rsync -a --update" "$LOG"' "$(calls)"
: > "$LOG"
HARNESS_HOME="$here" HARNESS_STATE="$T/state" PATH="$T/fake:$PATH" bash -c '. "$HARNESS_HOME/bin/remote-env.sh"; remote_host_env; timeout 5 docker ps; xargs docker ps <<< "-q"'
check "a docker binary reached past the function finds no daemon on this host" '[ "$(grep -c "^docker DOCKER_HOST=unix:///run/local-host-off/" "$LOG")" -eq 2 ] && ! grep -q "DOCKER_HOST= ps" "$LOG"' "$(calls)"
: > "$LOG"
HARNESS_HOME="$here" HARNESS_STATE="$T/state" PATH="$T/fake:$PATH" bash -c '. "$HARNESS_HOME/bin/remote-env.sh"; remote_host_env; bash -c "docker run --rm -v '"$RW"':/b img make"'
check "the function takes that inherited DOCKER_HOST as unset and sends the run to the remote host" 'grep -q "docker DOCKER_HOST=ssh://fake@host run --rm -v $RW:/b img make" "$LOG"' "$(calls)"
: > "$LOG"
HARNESS_HOME="$here" HARNESS_STATE="$T/state" PATH="$T/fake:$PATH" bash -c '. "$HARNESS_HOME/bin/remote-env.sh"; remote_host_env; DOCKER_HOST=ssh://other@h2 docker ps; timeout 5 bash -c "docker ps -a"'
check "an explicit ssh DOCKER_HOST wins, and bash -c under timeout keeps the function" 'grep -q "docker DOCKER_HOST=ssh://other@h2 ps$" "$LOG" && grep -q "docker DOCKER_HOST=ssh://fake@host ps -a" "$LOG"' "$(calls)"


# The cost-read guard, read through a fake harness tree: a tick may not start a container on a
# host another issue holds whole, unless that hold is marked no-cost.
G="$T/guard"; mkdir -p "$G/bin/remote-docker"; cp "$SHIM" "$G/bin/remote-docker/docker"
printf '#!/usr/bin/env bash\necho 999999\n' > "$G/bin/tick-pid.sh"
cat > "$G/bin/lease.sh" <<'EOF'
#!/usr/bin/env bash
echo "remote card: fake@host (slots 4-5)"
echo "slot 4 HELD 2490 1 2026-11-26T00:00:00Z domain=80 remote-all${GUARD_MARK:-} render (alive)"
EOF
chmod +x "$G/bin/"*.sh "$G/bin/remote-docker/docker"
: > "$LOG"; DOCKER_HOST=ssh://fake@host "$G/bin/remote-docker/docker" run --rm img true 2> "$T/guard.err"; rc=$?
check "a whole-card cost hold refuses another tick's container on that host" '[ "$rc" = 75 ] && grep -q "held whole by #2490" "$T/guard.err" && ! grep -q " run " "$LOG"' "rc=$rc $(cat "$T/guard.err") $(calls)"
: > "$LOG"; GUARD_MARK=" no-cost" DOCKER_HOST=ssh://fake@host "$G/bin/remote-docker/docker" run --rm img true 2> "$T/guard.err"; rc=$?
check "a whole-card no-cost hold lets the host's builds go on" '[ "$rc" = 0 ] && grep -q "docker DOCKER_HOST=ssh://fake@host run --rm img true" "$LOG"' "rc=$rc $(cat "$T/guard.err") $(calls)"

# remote_room.sh: --alone reads the host's load; --alone-card does not.
R="$T/room"; mkdir -p "$R/bin" "$R/fake"; cp "$here/bin/remote_room.sh" "$R/bin/"
cat > "$R/fake/ssh" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  *query-compute-apps*) printf '%s %s %s\n' "${ROOM_APPS:-0}" "${ROOM_LOAD:-20.0}" 24 ;;
  *) echo "60000 20000" ;;
esac
EOF
chmod +x "$R/fake/ssh"
out=$(PATH="$R/fake:$PATH" REMOTE_CARD_HOST=fake@host bash "$R/bin/remote_room.sh" --alone); rc=$?
check "--alone refuses a loaded host" '[ "$rc" = 1 ] && [[ "$out" == "busy load1=20.0"* ]]' "rc=$rc $out"
out=$(PATH="$R/fake:$PATH" REMOTE_CARD_HOST=fake@host bash "$R/bin/remote_room.sh" --alone-card); rc=$?
check "--alone-card takes a loaded host whose card is free" '[ "$rc" = 0 ] && [[ "$out" == ok* ]]' "rc=$rc $out"
out=$(ROOM_APPS=1 PATH="$R/fake:$PATH" REMOTE_CARD_HOST=fake@host bash "$R/bin/remote_room.sh" --alone-card); rc=$?
check "--alone-card still refuses a card that runs another process" '[ "$rc" = 1 ] && [[ "$out" == "busy 1 compute"* ]]' "rc=$rc $out"
out=$(ROOM_LOAD=2.0 PATH="$R/fake:$PATH" REMOTE_CARD_HOST=fake@host bash "$R/bin/remote_room.sh" --alone); rc=$?
check "--alone takes a quiet host whose card is free" '[ "$rc" = 0 ] && [[ "$out" == ok* ]]' "rc=$rc $out"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
