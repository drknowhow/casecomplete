#!/usr/bin/env bash
# Runs tools/e2e/targets.ps1 for each target under each correction-injection mode.
# Usage: tools/e2e/target_matrix.sh "<modes>" "<targets>"   e.g. "fast keys" "chrome cmd"
cd "$(dirname "$0")/../.." || exit 1
modes=${1:-"fast keys"}
targets=${2:-"chrome cmd"}
for mode in $modes; do
  taskkill //F //IM casecomplete.exe >/dev/null 2>&1
  sleep 1
  CC_ACCEPT_INJECTED=1 CC_FIX_INJECT="$mode" RUST_LOG=info ./target/release/casecomplete.exe > "/tmp/cc-t-$mode.log" 2>&1 &
  sleep 6
  for t in $targets; do
    echo "=== mode=$mode target=$t"
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File tools/e2e/targets.ps1 -Target "$t" 2>&1 | grep -a "RESULT"
  done
  taskkill //F //IM casecomplete.exe >/dev/null 2>&1
  sleep 1
done
