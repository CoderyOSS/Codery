#!/bin/bash

# ssh-agent-add-keys.sh - Add SSH keys to the running ssh-agent
# This is called by supervisord after ssh-agent starts

USER="${USER:-gem}"
SSH_DIR="/home/${USER}/.ssh"

# Wait for ssh-agent socket to be ready
for i in {1..30}; do
  if [ -S /tmp/ssh-agent.sock ]; then
    break
  fi
  sleep 0.1
done

if [ ! -S /tmp/ssh-agent.sock ]; then
  echo "[ssh-agent-keys] SSH agent socket not found, skipping key loading"
  exit 0
fi

# Find and add private keys
FIRST_KEY=$(find "$SSH_DIR" -maxdepth 1 -type f \
  ! -name "*.pub" ! -name "authorized_keys" ! -name "known_hosts" ! -name "config" \
  | sort | head -1)

if [ -n "$FIRST_KEY" ]; then
  export SSH_AUTH_SOCK=/tmp/ssh-agent.sock
  if ssh-add "$FIRST_KEY" 2>/dev/null; then
    echo "[ssh-agent-keys] Loaded key: $(basename "$FIRST_KEY")"
  else
    echo "[ssh-agent-keys] Warning: Could not load key (passphrase-protected?): $(basename "$FIRST_KEY")"
  fi
fi

# Keep the script running so supervisord doesn't restart it
# or we can exit 0 if we want it to be a one-shot
# For now, let's make it a one-shot that supervisord doesn't restart
exit 0
