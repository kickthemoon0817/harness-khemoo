#!/usr/bin/env bash
# Runs every tests/test_*.sh and tests/test_*.py, the core's and any the instance adds under
# $HARNESS_INSTANCE_DIR/tests, and exits non-zero when any suite fails. A suite pattern with no
# file (an instance without tests) is skipped, not failed.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
inst="${HARNESS_INSTANCE_DIR:-$(dirname "$here")/instance}"
failed=()
for t in "$here"/test_*.sh "$here"/test_*.py "$inst"/tests/test_*.sh "$inst"/tests/test_*.py; do
    [ -f "$t" ] || continue
    echo "=== ${t#"$(dirname "$here")"/}"
    case "$t" in
        *.sh) bash "$t" ;;
        *.py) python3 "$t" ;;
    esac || failed+=("${t##*/}")
done
if [ "${#failed[@]}" -gt 0 ]; then
    echo "FAILED: ${failed[*]}"
    exit 1
fi
echo "all suites passed"
