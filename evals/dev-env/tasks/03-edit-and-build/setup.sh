#!/usr/bin/env bash
# Reset the file to known state on both sides
cd "$GROUP_WS/workmux" 2>/dev/null || cd "$(find "$GROUP_WS" -name workmux -type l | head -1)"
git checkout -- src/dev_env/ports.rs 2>/dev/null || true
ssh "$SSH_HOST" "cd $REMOTE_WORKDIR && git checkout -- src/dev_env/ports.rs" 2>/dev/null || true
