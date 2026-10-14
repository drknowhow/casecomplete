#!/usr/bin/env bash
# Runs tools/e2e/corrections.ps1 under each correction-injection mode and prints the results.
# Usage: tools/e2e/fix_matrix.sh [mode ...]   (default: keys keys-slow paste)
cd "$(dirname "$0")/../.." || exit 1
modes=("$@")
[ ${#modes[@]} -eq 0 ] && modes=(keys keys-slow paste)
for mode in "${modes[@]}"; do
  taskkill //F //IM casecomplete.exe >/dev/null 2>&1
  sleep 1
  CC_ACCEPT_INJECTED=1 CC_FIX_INJECT="$mode" RUST_LOG=info ./target/release/casecomplete.exe > "/tmp/cc-fix-$mode.log" 2>&1 &
  sleep 6
  echo "=== mode=$mode gap=${CC_FIX_KEY_GAP_MS:-default} pre=${CC_FIX_PRE_DELAY_MS:-default}"
  for i in 1 2; do
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File tools/e2e/corrections.ps1 2>&1 | grep -v '^\s*$' | grep -vE '_BEGIN|_END'
  done
  taskkill //F //IM casecomplete.exe >/dev/null 2>&1
  sleep 1
done
