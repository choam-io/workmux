#!/usr/bin/env bash
# Grade: agent started server, used correct port, got response through tunnel
set -euo pipefail

errors=0

# The agent should have mentioned port 10000 (the local tunnel port for 8080)
if grep -qi "10000\|localhost:10000" "$TRANSCRIPT"; then
  echo "OK: agent used correct local tunnel port 10000"
else
  echo "FAIL: agent didn't identify local port 10000 for remote 8080"
  errors=$((errors + 1))
fi

# The agent should have gotten the response
if grep -qi "hello-eval" "$TRANSCRIPT"; then
  echo "OK: agent got 'hello-eval' response"
else
  echo "FAIL: 'hello-eval' not in transcript"
  errors=$((errors + 1))
fi

# Verify the server is actually running
if ssh "$SSH_HOST" "ss -tlnp 2>/dev/null | grep -q 8080"; then
  echo "OK: HTTP server running on codespace port 8080"
else
  echo "FAIL: HTTP server not running on codespace"
  errors=$((errors + 1))
fi

if [ $errors -gt 0 ]; then
  echo "GRADE: FAIL ($errors checks failed)"
  exit 1
fi

# Cleanup (best-effort, don't affect grade)
ssh "$SSH_HOST" "pkill -f 'http.server' 2>/dev/null" || true
echo "GRADE: PASS"
exit 0
