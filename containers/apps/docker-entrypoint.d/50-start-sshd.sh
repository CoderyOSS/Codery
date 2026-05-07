#!/bin/bash
ssh-keygen -A -q
mkdir -p /var/log/sshd
/usr/sbin/sshd -f /etc/ssh/sshd_config -E /var/log/sshd/debug.log -o "LogLevel=DEBUG2" \
  || { echo "[apps] WARNING: sshd failed to start (exit $?)"; exit 0; }
echo "[apps] sshd started on port 22 (debug log: /var/log/sshd/debug.log)"
