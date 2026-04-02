#!/usr/bin/env bash
# Kill any existing HTTP servers on the codespace
ssh "$SSH_HOST" "pkill -f 'http.server' 2>/dev/null || true"
sleep 1
