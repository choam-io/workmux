#!/usr/bin/env bash
# Grade: transcript must contain the key dev_env fields
set -euo pipefail

TRANSCRIPT="$TRANSCRIPT"
errors=0

check() {
  if ! grep -qi "$1" "$TRANSCRIPT"; then
    echo "FAIL: transcript missing '$1'"
    errors=$((errors + 1))
  else
    echo "OK: found '$1'"
  fi
}

# Must mention the codespace name
check "$CODESPACE_NAME"
# Must mention the SSH host
check "$SSH_HOST"
# Must mention the remote workdir
check "/workspaces/workmux"
# Must mention port mappings
check "8080"
check "10000"
# Must mention sync strategy
check "git.push\|git-push\|GitPush"

if [ $errors -gt 0 ]; then
  echo "GRADE: FAIL ($errors checks failed)"
  exit 1
fi
echo "GRADE: PASS"
exit 0
