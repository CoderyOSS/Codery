FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    git \
    gnupg \
    jq \
    locales \
    python3 \
    python3-pip \
    supervisor \
    openssh-client \
    openssh-server \
    sudo \
    tmux \
    ttyd \
    unzip \
    && locale-gen en_US.UTF-8 \
    && update-locale LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 \
    && rm -rf /var/lib/apt/lists/*

ENV LANG=en_US.UTF-8
ENV LC_ALL=en_US.UTF-8

RUN (groupadd -f -g 1000 gem || true) && \
    useradd -m -u 1000 -g 1000 -s /bin/bash gem || \
    useradd -m -u 1000 -o -g 1000 -s /bin/bash gem && \
    usermod -p '*' gem

# Install Node.js 20+
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# Install bun
RUN curl -fsSL https://bun.sh/install | bash \
    && mv /root/.bun/bin/bun /usr/local/bin/bun \
    && ln -s /usr/local/bin/bun /usr/local/bin/bunx
ENV BUN_INSTALL_BIN=/usr/local/bin

# Install Rust
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/usr/local/cargo/bin:${PATH}"

# Install GitHub CLI
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y gh \
    && rm -rf /var/lib/apt/lists/*

# Install OpenCode
RUN npm install -g opencode-ai @anthropic-ai/claude-code openspec-extensions

# Install Archon (custom build with modelMap support)
COPY containers/sandbox/bin/archon-linux-x64 /usr/local/bin/archon
RUN chmod +x /usr/local/bin/archon
# Archon's bundled claude-agent-sdk extracts to /tmp/claude-agent-sdk-* at runtime.
# The directory is created root-owned; the gem user can't write temp files there.
# Pre-create a writable TMPDIR so the SDK extraction succeeds.
RUN mkdir -p /home/gem/.cache/archon-tmp \
    && chown gem:gem /home/gem/.cache/archon-tmp
ENV TMPDIR=/home/gem/.cache/archon-tmp

# Pre-trust GitHub host key
RUN ssh-keyscan github.com >> /etc/ssh/ssh_known_hosts 2>/dev/null

# sshd config — key auth only, gem user only, no PAM
RUN mkdir -p /var/run/sshd && \
    printf '%s\n' \
        'Port 22' \
        'Protocol 2' \
        'PasswordAuthentication no' \
        'ChallengeResponseAuthentication no' \
        'PubkeyAuthentication yes' \
        'AuthorizedKeysFile .ssh/authorized_keys /run/secrets/authorized_keys' \
        'AllowUsers gem' \
        'UsePAM no' \
        'PrintMotd no' \
        'PrintLastLog no' \
        'Subsystem sftp /usr/lib/openssh/sftp-server' \
    > /etc/ssh/sshd_config.d/sandbox.conf

# Install code-server
RUN curl -fsSL https://code-server.dev/install.sh | bash

# GitHub App token and push helpers
COPY containers/sandbox/scripts/github-app-token.sh /usr/local/bin/github-app-token
RUN chmod +x /usr/local/bin/github-app-token
COPY containers/sandbox/scripts/github-push.sh /usr/local/bin/github-push
RUN chmod +x /usr/local/bin/github-push

# Allow gem user to run github-push without password
RUN echo "gem ALL=(ALL) NOPASSWD: /usr/local/bin/github-push" > /etc/sudoers.d/github-push

# Supervisord configuration
COPY containers/sandbox/supervisor/supervisord.conf /etc/supervisor/supervisord.conf
COPY containers/sandbox/supervisor/conf.d/ /etc/supervisor/conf.d/

# AGENTS.md — copied to home dir so OpenCode finds it (not projects/, which is bind-mounted at runtime)
COPY AGENTS.md /home/gem/AGENTS.md
RUN chown gem:gem /home/gem/AGENTS.md

# OpenCode config + caveman
RUN mkdir -p /home/gem/.config/opencode
COPY opencode.json /home/gem/.config/opencode/config.json

# Archon config
RUN mkdir -p /home/gem/.archon
COPY containers/sandbox/archon/config.yaml /home/gem/.archon/config.yaml
RUN chown -R gem:gem /home/gem/.archon
# Global AGENTS.md — auto-loaded by OpenCode, contains caveman always-on instructions
COPY containers/sandbox/opencode-global-agents.md /home/gem/.config/opencode/AGENTS.md
# Vendored caveman skills — COPY instead of npx (network-free, no silent failures)
# Update: re-vendor from JuliusBrussee/caveman via `npx skills add JuliusBrussee/caveman --copy`
# then copy containers/sandbox/agents-skills/ contents over
COPY containers/sandbox/agents-skills/ /home/gem/.agents/skills/
RUN chown -R gem:gem /home/gem/.config /home/gem/.agents

# tmux config — enable mouse scrolling
COPY containers/sandbox/tmux.conf /home/gem/.tmux.conf
RUN chown gem:gem /home/gem/.tmux.conf

# code-server settings (theme, etc.)
RUN mkdir -p /home/gem/.local/share/code-server/User
COPY containers/sandbox/code-server-settings.json /home/gem/.local/share/code-server/User/settings.json

# Modular entrypoint scripts
COPY containers/sandbox/docker-entrypoint.d/ /docker-entrypoint.d/
RUN chmod +x /docker-entrypoint.d/*.sh

# Entrypoint wrapper
COPY containers/sandbox/scripts/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Create persistent directories with correct ownership
RUN mkdir -p /home/gem/projects \
    /home/gem/.local/share/opencode \
    /var/log/supervisor \
    && chown -R gem:gem /home/gem /var/log/supervisor

# Security hardening: remove root secrets
RUN rm -rf /root/.ssh /root/.cache /root/.config 2>/dev/null || true

# NOTE: Do NOT set USER gem here. The entrypoint runs as root so it can
# read /run/secrets/github-app.pem (root-owned) and authenticate gh as gem.
# supervisord.conf has `user=gem` so all supervised processes run as gem.
WORKDIR /home/gem/projects

ENTRYPOINT ["/entrypoint.sh"]
