#!/usr/bin/env bash
# Runs every tests/test_*.sh, the core's and any the instance adds under $HARNESS_INSTANCE_DIR/tests,
# and exits non-zero when any suite fails. A suite pattern with no file (an instance without tests)
# is skipped, not failed. The suites run the harness binary this builds, unless HARNESS_BIN names one.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(dirname "$here")
inst="${HARNESS_INSTANCE_DIR:-$root/instance}"
if [ -z "${HARNESS_BIN:-}" ]; then
    # The workspace pins its toolchain in rust-toolchain.toml, which rustup reads from the cwd.
    (cd "$root" && cargo build --quiet --locked --bin harness) || { echo "FAILED: cargo build --bin harness"; exit 1; }
    HARNESS_BIN="$(cd "$root" && cargo metadata --format-version 1 --no-deps | jq -r .target_directory)/debug/harness"
fi
export HARNESS_BIN
failed=()
for t in "$here"/test_*.sh "$inst"/tests/test_*.sh; do
    [ -f "$t" ] || continue
    echo "=== ${t#"$root"/}"
    bash "$t" || failed+=("${t##*/}")
done
if [ "${#failed[@]}" -gt 0 ]; then
    echo "FAILED: ${failed[*]}"
    exit 1
fi
echo "all suites passed"
