#!/usr/bin/env bash
# Ensure the codespace has the latest code and Rust toolchain
ssh "$SSH_HOST" "source ~/.cargo/env 2>/dev/null; which cargo" || {
  echo "Rust not installed, installing..."
  ssh "$SSH_HOST" "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
}
# Make sure code is up to date
ssh "$SSH_HOST" "cd /workspaces/workmux && git pull --ff-only 2>/dev/null || true"
