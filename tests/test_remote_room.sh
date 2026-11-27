#!/usr/bin/env bash
# Exercises bin/remote_room.sh's quiet checks with a fake ssh that answers as a remote host would.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/fakebin"
cat > "$T/fakebin/ssh" <<'S'
#!/usr/bin/env bash
case "$*" in
  *compute-apps*) echo "${FAKE_APPS:-0} ${FAKE_LOAD:-1.0} ${FAKE_CORES:-24} ${FAKE_BUILDERS:-0}" ;;
  *MemAvailable*) echo "60000 30000" ;;
esac
S
chmod +x "$T/fakebin/ssh"
export PATH="$T/fakebin:$PATH" REMOTE_CARD_HOST=fake@host
R="$here/bin/remote_room.sh"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
out=$("$R" --alone); check "a quiet host with no container is alone" '[[ "$out" == ok* ]]' "$out"
out=$(FAKE_BUILDERS=2 "$R" --alone); check "a build or test container keeps a cost read waiting" '[[ "$out" == "busy 2 build or test container(s) on the host" ]]' "$out"
out=$(FAKE_BUILDERS=2 "$R" --alone-card); check "a run that times nothing does not wait for one" '[[ "$out" == ok* ]]' "$out"
out=$(FAKE_LOAD=13 "$R" --alone); check "load over half the cores still keeps it waiting" '[[ "$out" == busy\ load1=13* ]]' "$out"
out=$(FAKE_APPS=1 "$R" --alone); check "a compute process on the card keeps it waiting" '[[ "$out" == "busy 1 compute process(es) on the card" ]]' "$out"
echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
