#!/usr/bin/env bash
# Grade: agent ran gh codespace code with the correct codespace name
set -euo pipefail

errors=0

SESSION_DIR="$HOME/.pi/agent/sessions/--Users-nodeselector-.local-share-workmux-groups-choam--workmux-target-followup--"

# Check transcript text first
if grep -qi "gh codespace code\|gh cs code" "$TRANSCRIPT"; then
  echo "OK: agent ran gh codespace code (in transcript)"
else
  # Check session JSONL (tool calls aren't in --print output)
  LATEST_SESSION=$(ls -t "$SESSION_DIR"/*.jsonl 2>/dev/null | head -1)
  if [ -n "$LATEST_SESSION" ] && grep -q "gh codespace code\|gh cs code" "$LATEST_SESSION"; then
    echo "OK: agent ran gh codespace code (in session log)"
  else
    echo "FAIL: agent didn't run gh codespace code"
    errors=$((errors + 1))
  fi
fi

if [ $errors -gt 0 ]; then
  echo "GRADE: FAIL ($errors checks failed)"
  exit 1
fi
echo "GRADE: PASS"
exit 0
