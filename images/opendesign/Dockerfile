# OpenDesign (nexu-io/open-design) — collaborative AI design workspace.
# Thin wrapper over the upstream image: pins the version and adds the
# OpenCode CLI so OpenDesign's runtime adapters can spawn it (agent spawn
# inside the design workspace, as the legacy agent container offered).
# Data (.od project store) lives on the host at /opt/codery/data/opendesign
# and is bind-mounted at the upstream image's expected path.
FROM ghcr.io/nexu-io/od:0.17.0

USER root
RUN npm install -g opencode-ai@1.18.31 \
 && npm cache clean --force
USER open-design
