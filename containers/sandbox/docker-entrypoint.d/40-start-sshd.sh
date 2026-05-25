#!/bin/bash
set -e

# Generate SSH host keys if not already present (fast no-op on rebuild)
ssh-keygen -A -q

# Set up authorized_keys for gem from the bind-mounted secret.
# /run/secrets/authorized_keys is mounted read-only from the host;
# we copy it so we can set strict permissions sshd requires.
mkdir -p /home/gem/.ssh
chmod 700 /home/gem/.ssh
chown gem:gem /home/gem/.ssh

if [ -f /run/secrets/authorized_keys ]; then
    cp /run/secrets/authorized_keys /home/gem/.ssh/authorized_keys
    chmod 600 /home/gem/.ssh/authorized_keys
    chown gem:gem /home/gem/.ssh/authorized_keys
    echo "[sandbox] Installed authorized_keys for gem ($(wc -l < /home/gem/.ssh/authorized_keys) key(s))"
else
    echo "[sandbox] WARNING: /run/secrets/authorized_keys not found — SSH will reject all connections"
    echo "[sandbox]   Put your public key in /opt/codery/ssh/authorized_keys on the host"
fi

# sshd is managed by launchy (devcontainer.json) — not started here.
# This script only prepares host keys and authorized_keys.
