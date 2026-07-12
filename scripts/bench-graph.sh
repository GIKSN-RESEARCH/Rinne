#!/usr/bin/env bash
set -euo pipefail
# A/B benchmark for the code graph (#11). Runs one fixed task twice on THIS repo
# and reports prompt tokens + wall time from the usage ledger. No per-language
# fixtures: it only toggles --no-graph and reads the recorded usage.

TASK="${1:-Add a doc comment to the atomic_write function in crates/rinne-core/src/blackboard.rs}"
DB=".rinne/state.db"

measure() {
  local label="$1"; shift
  local start end
  start=$(date +%s)
  "$@" >/dev/null 2>&1 || true
  end=$(date +%s)
  local tokens
  tokens=$(sqlite3 "$DB" "SELECT COALESCE(SUM(prompt_tokens),0) FROM usage_ledger;" 2>/dev/null || echo 0)
  printf '%s: prompt_tokens=%s wall_s=%s\n' "$label" "$tokens" "$((end - start))"
}

echo "== graph OFF =="
rm -rf .rinne
measure "no-graph" rinne --no-graph -p "$TASK"

echo "== graph ON =="
rm -rf .rinne
measure "graph" rinne -p "$TASK"
