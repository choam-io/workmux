#!/bin/bash
# Test the execution targets feature with a real codespace
#
# Usage:
#   ./scripts/test-targets.sh                    # Run unit/mock tests only
#   ./scripts/test-targets.sh github/some-repo   # Run with real codespace

set -e

cd "$(dirname "$0")/.."

echo "Building workmux..."
cargo build

echo ""
echo "Running integration tests..."

if [ -n "$1" ]; then
    echo ""
    echo "🚀 Running REAL codespace test with repo: $1"
    echo "   This will create/reuse a codespace and may take a while..."
    echo ""
    WORKMUX_TEST_REAL_CODESPACE="$1" cargo test --test target_integration -- --include-ignored
else
    cargo test --test target_integration
fi

echo ""
echo "✅ All tests passed!"
