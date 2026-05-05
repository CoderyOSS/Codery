#!/bin/bash
# Generate ED25519 keypair for sandbox→apps SSH into the shared projects volume.
# Apps sshd reads .pub via AuthorizedKeysCommand on each connection — no stale auth.

KEY_DIR="/home/gem/projects/.codery"
KEY="$KEY_DIR/sandbox-key"

mkdir -p "$KEY_DIR"
chown gem:gem "$KEY_DIR"
chmod 700 "$KEY_DIR"

rm -f "$KEY" "$KEY.pub"
ssh-keygen -t ed25519 -f "$KEY" -N "" -C "codery-sandbox" -q
chown gem:gem "$KEY" "$KEY.pub"
chmod 600 "$KEY"
chmod 644 "$KEY.pub"   # world-readable: sshd AuthorizedKeysCommandUser=nobody reads this

echo "[sandbox] Generated sandbox→apps SSH keypair"

# Write SSH client config so `ssh gem@apps` works from sandbox with no flags.
SSH_DIR="/home/gem/.ssh"
mkdir -p "$SSH_DIR"
chown gem:gem "$SSH_DIR"
chmod 700 "$SSH_DIR"

cat > "$SSH_DIR/config" <<'EOF'
Host apps
    HostName apps
    User gem
    IdentityFile /home/gem/projects/.codery/sandbox-key
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    LogLevel ERROR
EOF
chown gem:gem "$SSH_DIR/config"
chmod 600 "$SSH_DIR/config"

echo "[sandbox] SSH client config written — use: ssh gem@apps"
