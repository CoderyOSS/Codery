#!/bin/bash
set -e
SSH_DIR="${SSH_DIR:-/home/gem/.ssh}"
if [ ! -d "$SSH_DIR" ]; then
  echo "[apps] No .ssh directory, skipping SSH agent setup"
  exit 0
fi
echo "[apps] SSH directory present — Launchy will manage ssh-agent"
