#!/bin/bash
set -e
USER="gem"
USER_UID="1000"
USER_GID="1000"
mkdir -p "/home/${USER}"
chown "${USER_UID}:${USER_GID}" "/home/${USER}"
CODE_SERVER_DIR="/home/${USER}/.config/code-server/vscode"
mkdir -p "${CODE_SERVER_DIR}"/{extensions,User/globalStorage,User/History,Machine}
chown -R "${USER_UID}:${USER_GID}" "/home/${USER}/.config"
mkdir -p "/home/${USER}/projects"
chown "${USER_UID}:${USER_GID}" "/home/${USER}/projects"
mkdir -p "/home/${USER}/.local/share/opencode"
chown "${USER_UID}:${USER_GID}" "/home/${USER}/.local/share/opencode"
mkdir -p "/home/${USER}/.claude"
chown "${USER_UID}:${USER_GID}" "/home/${USER}/.claude"
echo "[sandbox] Fixed ownership of /home/${USER}"
