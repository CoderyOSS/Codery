#!/bin/bash
set -e
SSH_DIR="/home/gem/.ssh"
if [ ! -d "$SSH_DIR" ]; then
  echo "[system] No .ssh directory, skipping SSH agent setup"
  exit 0
fi
FIRST_KEY=$(find "$SSH_DIR" -maxdepth 1 -type f \
  ! -name "*.pub" ! -name "authorized_keys" ! -name "known_hosts" ! -name "config" \
  | sort | head -1)
if [ -z "$FIRST_KEY" ]; then
  echo "[system] No private SSH keys found, skipping"
  exit 0
fi
ssh-agent -s > /tmp/ssh-agent.env
eval "$(cat /tmp/ssh-agent.env)"
echo "${SSH_AUTH_SOCK}" > /tmp/ssh-auth-sock-path
if ssh-add "${FIRST_KEY}" 2>/dev/null; then
  echo "[system] SSH agent started, loaded key: $(basename "${FIRST_KEY}")"
else
  echo "[system] Warning: Could not load key: $(basename "${FIRST_KEY}")"
fi
