#!/bin/bash
set -e
USER="gem"
PROJECTS_DIR="/home/${USER}/projects"
mkdir -p "${PROJECTS_DIR}"
# Ownership may fail on named volumes where CAP_CHOWN is not available; that's fine.
chown "${USER}:${USER}" "${PROJECTS_DIR}" 2>/dev/null || true
mkdir -p /var/log/supervisor
echo "[sandbox] Projects directory ready at ${PROJECTS_DIR}"
