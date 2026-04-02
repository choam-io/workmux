#!/usr/bin/env bash
# Grade: agent must have run cargo test and reported results
set -euo pipefail

errors=0

check() {
  if ! grep -qi "$1" "$TRANSCRIPT"; then
    echo "FAIL: transcript missing '$1'"
    errors=$((errors + 1))
  else
    echo "OK: found '$1'"
  fi
}

# Must show test results - "14" and "passed" somewhere
check "14.*pass\|pass.*14"
# Must have referenced the remote somehow
check "ssh\|codespace\|remote"
# Must NOT show actual test failures (but "0 failed" is fine)
if grep -qi "test result.*FAILED\|FAILED.*[1-9]" "$TRANSCRIPT"; then
  echo "FAIL: tests failed on codespace"
  errors=$((errors + 1))
else
  echo "OK: no test failures"
fi

if [ $errors -gt 0 ]; then
  echo "GRADE: FAIL ($errors checks failed)"
  exit 1
fi
echo "GRADE: PASS"
exit 0
