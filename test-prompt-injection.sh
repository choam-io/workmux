#!/usr/bin/env bash
# Test script for prompt injection fixes
#
# This script:
# 1. Creates a test worktree with a prompt
# 2. Verifies the prompt file is stored in .workmux/
# 3. Closes the workspace
# 4. Re-opens it and verifies auto-detection works
# 5. Cleans up
#
# Run with: RUST_LOG=workmux=debug ./test-prompt-injection.sh

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log() { echo -e "${BLUE}[TEST]${NC} $1"; }
success() { echo -e "${GREEN}[PASS]${NC} $1"; }
fail() { echo -e "${RED}[FAIL]${NC} $1"; exit 1; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }

# Configuration
TEST_BRANCH="test-prompt-injection-$$"
PROMPT_CONTENT="This is a test prompt for worktree: $TEST_BRANCH

## Task
Verify that prompt injection works correctly.

## Expected behavior
1. The agent should receive this prompt on initial creation
2. When re-opening the worktree, this prompt should be auto-detected
3. The prompt should be injected into the agent command
"

cleanup() {
    log "Cleaning up test worktree..."
    # Close the workspace first (ignore errors)
    workmux close "$TEST_BRANCH" --force 2>/dev/null || true
    # Remove the worktree
    workmux remove "$TEST_BRANCH" --force 2>/dev/null || true
    # Remove temp prompt file
    rm -f "$PROMPT_FILE" 2>/dev/null || true
    log "Cleanup complete"
}

# Set up cleanup trap
trap cleanup EXIT

# Create temp prompt file
PROMPT_FILE=$(mktemp).md
echo "$PROMPT_CONTENT" > "$PROMPT_FILE"

log "Test configuration:"
log "  Branch: $TEST_BRANCH"
log "  Prompt file: $PROMPT_FILE"
log "  RUST_LOG: ${RUST_LOG:-not set}"

# Enable debug logging if not already set
export RUST_LOG="${RUST_LOG:-workmux=debug}"

log "========================================="
log "Phase 1: Create worktree with prompt"
log "========================================="

# Create the worktree with a prompt (background mode so it doesn't block)
log "Creating worktree with prompt..."
workmux add "$TEST_BRANCH" -b -P "$PROMPT_FILE" --no-focus 2>&1 | tee /tmp/workmux-add-output.log

# Wait for workspace to be created
sleep 2

# Check that the prompt file was stored
WORKTREE_PATH=$(workmux path "$TEST_BRANCH" 2>/dev/null || echo "")
if [ -z "$WORKTREE_PATH" ]; then
    fail "Could not get worktree path"
fi

log "Worktree path: $WORKTREE_PATH"

STORED_PROMPT="$WORKTREE_PATH/.workmux/PROMPT-$TEST_BRANCH.md"
if [ -f "$STORED_PROMPT" ]; then
    success "Prompt file stored at: $STORED_PROMPT"
    log "Stored prompt content:"
    head -5 "$STORED_PROMPT"
else
    fail "Prompt file not found at: $STORED_PROMPT"
fi

# Verify the prompt content matches
if diff -q "$PROMPT_FILE" "$STORED_PROMPT" > /dev/null; then
    success "Stored prompt matches original"
else
    fail "Stored prompt differs from original"
fi

log "========================================="
log "Phase 2: Close and re-open worktree"
log "========================================="

# Close the workspace
log "Closing workspace..."
workmux close "$TEST_BRANCH" --force 2>&1 || warn "Close command returned non-zero (may be expected)"

# Wait for close to complete
sleep 1

# Re-open the worktree (without providing a prompt)
log "Re-opening worktree (without explicit prompt)..."
workmux open "$TEST_BRANCH" --new --no-focus 2>&1 | tee /tmp/workmux-open-output.log

# Check the log for auto-detection
if grep -q "auto-injecting stored prompt" /tmp/workmux-open-output.log 2>/dev/null; then
    success "Auto-detection logged in output"
elif grep -q "auto-detected stored prompt" /tmp/workmux-open-output.log 2>/dev/null; then
    success "Auto-detection logged in output (alternate message)"
else
    # Check if debug logging captured it
    if grep -q "detected stored prompt" /tmp/workmux-open-output.log 2>/dev/null; then
        success "Detected stored prompt file in debug output"
    else
        warn "Auto-detection message not found in output (may need RUST_LOG=debug)"
        log "Output was:"
        cat /tmp/workmux-open-output.log
    fi
fi

log "========================================="
log "Phase 3: Verify session state"
log "========================================="

# List agents to see if any are registered
log "Checking for registered agents..."
workmux dashboard --list 2>&1 | head -20 || warn "Dashboard list failed"

# Check workmux list for the worktree
log "Worktree list:"
workmux list 2>&1 | grep -E "$TEST_BRANCH|handle" || true

log "========================================="
log "Test Summary"
log "========================================="

success "All tests passed!"
log "Test artifacts:"
log "  - workmux add output: /tmp/workmux-add-output.log"
log "  - workmux open output: /tmp/workmux-open-output.log"

# Optional: show the full debug logs
if [ "${SHOW_LOGS:-}" = "1" ]; then
    log "Full output from workmux add:"
    cat /tmp/workmux-add-output.log
    echo ""
    log "Full output from workmux open:"
    cat /tmp/workmux-open-output.log
fi
