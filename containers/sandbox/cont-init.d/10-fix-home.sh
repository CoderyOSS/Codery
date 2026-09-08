#!/command/with-contenv bash
set -e
# BuildKit COPY does not reliably preserve the sticky bit on /tmp (the old
# entrypoint.sh carried this same workaround). Without it, gem-user services
# crash on mkdir /tmp/* (opencode, opendesign uploads).
chmod 1777 /tmp
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
# ssh private keys must not be group/world-readable; the nix build
# normalizes store modes and the Dockerfile u+w pass leaves them 644
if [ -f "/home/${USER}/.ssh/id_codery_apps" ]; then
    chmod 600 "/home/${USER}/.ssh/id_codery_apps"
fi
if [ -f "/home/${USER}/.ssh/config" ]; then
    chmod 600 "/home/${USER}/.ssh/config"
fi
echo "[sandbox] Fixed ownership of /home/${USER}"
