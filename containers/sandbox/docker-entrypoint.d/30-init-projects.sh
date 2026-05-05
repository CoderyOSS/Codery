#!/bin/bash
set -e
USER="gem"
PROJECTS_DIR="/home/${USER}/projects"
mkdir -p "${PROJECTS_DIR}"
chown "${USER}:${USER}" "${PROJECTS_DIR}" 2>/dev/null || true
echo "[sandbox] Projects directory ready at ${PROJECTS_DIR}"
