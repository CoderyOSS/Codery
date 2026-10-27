# Paseo control plane (paseo.sh) — the iPad Paseo app talks to this daemon;
# it spawns agent CLIs (opencode) as its own subprocesses. Child of the
# official image per paseo.sh/docs/docker: adds the OpenCode agent CLI,
# git tooling for GitHub App auth, and a hardened sshd (tailnet-only :2222
# via CoderyCI's stable-port proxy). Base pinned to the iPad app's protocol
# version; bump deliberately together with app expectations.
FROM ghcr.io/getpaseo/paseo:0.11.4

USER root
# OpenCode agent CLI — prebuilt binary via npm wrapper (glibc base image,
# so the wrapper's platform detection picks correctly). Version pinned for
# on-disk project state compatibility across Codery agents.
RUN npm install -g opencode-ai@1.18.31 \
 && npm cache clean --force \
 && apt-get update && apt-get install -y --no-install-recommends \
      git curl jq openssl python3 python3-pip python3-venv ripgrep \
      openssh-server openssh-client \
 && rm -rf /var/lib/apt/lists/*

# GitHub App credential machinery — fresh installation token per git
# operation via the credential helper. Needs GITHUB_APP_ID +
# GITHUB_APP_PRIVATE_KEY_PATH env (service.yml wires them) and resolves the
# token's org from the credential request path.
COPY containers/paseo/scripts/github-app-token.sh /usr/local/bin/github-app-token
COPY containers/paseo/scripts/git-credential-codery.sh /usr/local/bin/git-credential-codery
COPY containers/paseo/scripts/github-push.sh /usr/local/bin/github-push
RUN chmod +x /usr/local/bin/github-app-token /usr/local/bin/git-credential-codery /usr/local/bin/github-push

# github-app permissions MCP — bun-only script (Bun.spawnSync / Bun.stdin).
COPY containers/paseo/scripts/github-app-permissions-mcp.ts /usr/local/bin/github-app-permissions-mcp.ts
COPY --from=oven/bun:1.2.13 /usr/local/bin/bun /usr/local/bin/bun

# sshd: hardened config + entrypoint wrapper (starts sshd as root, then execs
# the base entrypoint, which drops everything else to the paseo user).
COPY containers/paseo/sshd_config /etc/ssh/sshd_config
COPY containers/paseo/entrypoint-with-sshd /usr/local/bin/entrypoint-with-sshd
RUN chmod 644 /etc/ssh/sshd_config && chmod 755 /usr/local/bin/entrypoint-with-sshd
# Host keys are generated on first boot into /etc/ssh/host_keys (named volume
# — stable fingerprints). AuthorizedKeysFile is /run/secrets/authorized_keys
# (host-managed file, bind-mounted read-only by service.yml — append a device
# pubkey on the host, no rebuild needed).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint-with-sshd"]

# git identity + credential helper + ssh client config for the paseo user
# (uid 1000). /home/paseo is a named volume — these seed it on first boot and
# persist across recreates.
RUN printf '%s\n' \
      '[user]' \
      '    name = Codery Bot' \
      '    email = codery-bot@users.noreply.github.com' \
      '[credential "https://github.com"]' \
      '    helper = /usr/local/bin/git-credential-codery' \
    > /home/paseo/.gitconfig \
 && mkdir -p /home/paseo/.ssh \
 && printf '%s\n' \
      'Host apps' \
      '    User gem' \
      '    IdentityFile ~/.ssh/agent-to-apps' \
      '    IdentitiesOnly yes' \
      '    StrictHostKeyChecking accept-new' \
    > /home/paseo/.ssh/config \
 && chown -R 1000:1000 /home/paseo/.ssh \
 && chmod 700 /home/paseo/.ssh \
 && chmod 600 /home/paseo/.ssh/config

# Shared-project compatibility shim: /opt/codery/projects is bind-mounted at
# /workspace, and the repo's committed per-project opencode.json references
# /home/gem/... paths (historical layout shared by every Codery agent
# container). This symlink keeps ONE canonical project config valid; the
# paseo global config uses honest /home/paseo paths.
RUN ln -s /home/paseo /home/gem

# Pre-create opencode state with correct ownership so a fresh named volume
# never seeds root-owned dirs — root-owned locks/ made the codery MCP client
# fail with EACCES in every paseo-user session (incident 2026-10-19; the
# existing volume was chowned in place). Also: never run opencode as root in
# this container — it recreates exactly this bug.
RUN mkdir -p /home/paseo/.local/state/opencode/locks /home/paseo/.config/opencode \
 && chown -R 1000:1000 /home/paseo
