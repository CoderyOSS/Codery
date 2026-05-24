#!/bin/bash
set -e
ssh-keygen -A -q 2>/dev/null || true
mkdir -p /var/log/sshd
echo "[apps] SSH host keys generated — Launchy will manage sshd"
