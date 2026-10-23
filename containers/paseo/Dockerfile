# Paseo control plane (paseo.sh) — the iPad Paseo app talks to this daemon;
# it spawns agent CLIs (opencode) as its own subprocesses. Child of the
# official image per paseo.sh/docs/docker: adds the OpenCode agent CLI and
# git tooling for GitHub App auth. Base pinned to the iPad app's protocol
# version; bump deliberately together with app expectations.
FROM ghcr.io/getpaseo/paseo:0.11.4

USER root
# OpenCode agent CLI — prebuilt binary via npm wrapper (glibc base image,
# so the wrapper's platform detection picks correctly). Version matches the
# sandbox's opencode for on-disk project state compatibility.
RUN npm install -g opencode-ai@1.18.31 \
 && npm cache clean --force \
 && apt-get update && apt-get install -y --no-install-recommends \
      git curl jq openssl python3 python3-pip python3-venv ripgrep \
 && rm -rf /var/lib/apt/lists/*

# GitHub App credential machinery — same scripts the sandbox uses: fresh
# installation token per git operation via the credential helper. Needs
# GITHUB_APP_ID + GITHUB_APP_PRIVATE_KEY_PATH env (service.yml wires them)
# and resolves the token's org from the credential request path.
COPY containers/sandbox/scripts/github-app-token.sh /usr/local/bin/github-app-token
COPY containers/sandbox/scripts/git-credential-codery.sh /usr/local/bin/git-credential-codery
COPY containers/sandbox/scripts/github-push.sh /usr/local/bin/github-push
RUN chmod +x /usr/local/bin/github-app-token /usr/local/bin/git-credential-codery /usr/local/bin/github-push

# github-app permissions MCP — bun-only script (Bun.spawnSync / Bun.stdin),
# same one the sandbox runs. Bun pinned to the sandbox's version.
COPY containers/sandbox/scripts/github-app-permissions-mcp.ts /usr/local/bin/github-app-permissions-mcp.ts
COPY --from=oven/bun:1.2.13 /usr/local/bin/bun /usr/local/bin/bun

# git identity + credential helper for the paseo user (uid 1000).
# /home/paseo is a named volume — this file seeds it on first boot and
# persists across recreates.
RUN printf '%s\n' \
      '[user]' \
      '    name = Codery Bot' \
      '    email = codery-bot@users.noreply.github.com' \
      '[credential "https://github.com"]' \
      '    helper = /usr/local/bin/git-credential-codery' \
    > /home/paseo/.gitconfig

# Shared-project compatibility shim: /opt/codery/projects is bind-mounted at
# /workspace, and the repo's committed per-project opencode.json (shared with
# the sandbox container, which reads the same tree at ~/projects) references
# /home/gem/... paths. This symlink keeps ONE canonical project config valid
# in both containers; the paseo global config uses honest /home/paseo paths.
RUN ln -s /home/paseo /home/gem

# Pre-create opencode state with correct ownership so a fresh named volume
# never seeds root-owned dirs — root-owned locks/ made the codery MCP client
# fail with EACCES in every paseo-user session (incident 2026-10-19; the
# existing volume was chowned in place). Also: never run opencode as root in
# this container — it recreates exactly this bug.
RUN mkdir -p /home/paseo/.local/state/opencode/locks /home/paseo/.config/opencode \
 && chown -R 1000:1000 /home/paseo

# Base entrypoint: root only for first-run mounted-volume setup, then drops
# the daemon and launched agents to the non-root `paseo` user (uid 1000).
