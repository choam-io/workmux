#!/usr/bin/env bash
# Grade: doc comment was added locally AND cargo check passed on remote
set -euo pipefail

errors=0

# Check local file has the new doc comment
WORKMUX_DIR=$(find "$GROUP_WS" -name workmux -type l | head -1)
if grep -q "Port pool garbage collection utilities" "$WORKMUX_DIR/src/dev_env/ports.rs" 2>/dev/null; then
  echo "OK: doc comment found in local file"
else
  echo "FAIL: doc comment not in local file"
  errors=$((errors + 1))
fi

# Check transcript shows cargo check passed
if grep -qi "cargo check\|compilation.*success\|Finished\|0 errors" "$TRANSCRIPT"; then
  echo "OK: transcript shows successful compilation"
else
  echo "FAIL: no evidence of successful cargo check"
  errors=$((errors + 1))
fi

# Check agent used the devloop (git operations)
if grep -qi "git push\|git commit\|git pull" "$TRANSCRIPT"; then
  echo "OK: agent followed git-push devloop"
else
  # Check session log
  SESSION_DIR="$HOME/.pi/agent/sessions/--Users-nodeselector-.local-share-workmux-groups-choam--workmux-target-followup--"
  LATEST=$(ls -t "$SESSION_DIR"/*.jsonl 2>/dev/null | head -1)
  if [ -n "$LATEST" ] && grep -q "git push" "$LATEST"; then
    echo "OK: agent followed git-push devloop (in session log)"
  else
    echo "WARN: no evidence of git push/pull"
  fi
fi

if [ $errors -gt 0 ]; then
  echo "GRADE: FAIL ($errors checks failed)"
  exit 1
fi
echo "GRADE: PASS"
exit 0
