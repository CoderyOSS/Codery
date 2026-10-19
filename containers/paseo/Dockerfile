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
      git curl jq openssl \
 && rm -rf /var/lib/apt/lists/*

# GitHub App credential machinery — same scripts the sandbox uses: fresh
# installation token per git operation via the credential helper. Needs
# GITHUB_APP_ID + GITHUB_APP_PRIVATE_KEY_PATH env (service.yml wires them)
# and resolves the token's org from the credential request path.
COPY containers/sandbox/scripts/github-app-token.sh /usr/local/bin/github-app-token
COPY containers/sandbox/scripts/git-credential-codery.sh /usr/local/bin/git-credential-codery
RUN chmod +x /usr/local/bin/github-app-token /usr/local/bin/git-credential-codery

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

# Base entrypoint: root only for first-run mounted-volume setup, then drops
# the daemon and launched agents to the non-root `paseo` user (uid 1000).
