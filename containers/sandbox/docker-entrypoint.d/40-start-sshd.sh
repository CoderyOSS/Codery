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

# Pass container environment through sshd to login shells. sshd sanitizes
# the environment for SSH sessions (compiled-in defaults), which drops
# /usr/local/bin from PATH and the GITHUB_APP_* vars needed by
# github-push / github-app-token. Idempotent; rolls back if sshd -t fails.
if ! grep -q "# codery-env-passthrough" /etc/ssh/sshd_config; then
    cp /etc/ssh/sshd_config /etc/ssh/sshd_config.codery-bak
    BASELINE_OK=0
    sshd -t 2>/dev/null && BASELINE_OK=1
    {
        echo "# codery-env-passthrough"
        echo "SetEnv PATH=/home/gem/.local/bin:/home/gem/.npm-global/bin:/usr/local/cargo/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        [ -n "${GITHUB_APP_ID:-}" ] && echo "SetEnv GITHUB_APP_ID=${GITHUB_APP_ID}"
        [ -n "${GITHUB_APP_SLUG:-}" ] && echo "SetEnv GITHUB_APP_SLUG=${GITHUB_APP_SLUG}"
        [ -n "${GITHUB_APP_PRIVATE_KEY_PATH:-}" ] && echo "SetEnv GITHUB_APP_PRIVATE_KEY_PATH=${GITHUB_APP_PRIVATE_KEY_PATH}"
    } >> /etc/ssh/sshd_config
    if [ "$BASELINE_OK" = "1" ] && ! sshd -t 2>/dev/null; then
        echo "[sandbox] WARNING: sshd config invalid after env passthrough — rolling back"
        mv /etc/ssh/sshd_config.codery-bak /etc/ssh/sshd_config
    else
        rm -f /etc/ssh/sshd_config.codery-bak
        echo "[sandbox] sshd env passthrough configured (PATH + GITHUB_APP_*)"
    fi
fi

# Belt-and-suspenders: export GITHUB_APP_* in ~/.bashrc too. SetEnv in sshd_config
# handles PATH reliably but some sshd builds/flags drop non-PATH vars; .bashrc is
# sourced by every interactive shell. Values are non-secret (PEM stays file-mounted).
if ! grep -q "# codery-github-env" /home/gem/.bashrc; then
    {
        echo "# codery-github-env"
        [ -n "${GITHUB_APP_ID:-}" ] && echo "export GITHUB_APP_ID=${GITHUB_APP_ID}"
        [ -n "${GITHUB_APP_SLUG:-}" ] && echo "export GITHUB_APP_SLUG=${GITHUB_APP_SLUG}"
        [ -n "${GITHUB_APP_PRIVATE_KEY_PATH:-}" ] && echo "export GITHUB_APP_PRIVATE_KEY_PATH=${GITHUB_APP_PRIVATE_KEY_PATH}"
    } >> /home/gem/.bashrc
    chown gem:gem /home/gem/.bashrc
    echo "[sandbox] GITHUB_APP_* exported in ~/.bashrc"
fi
