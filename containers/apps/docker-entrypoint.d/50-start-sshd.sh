#!/bin/bash
ssh-keygen -A -q
/usr/sbin/sshd -f /etc/ssh/sshd_config \
  || { echo "[apps] WARNING: sshd failed to start (exit $?)"; exit 0; }
echo "[apps] sshd started on port 22"
