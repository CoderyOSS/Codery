# Apps container — Nix-managed toolchain on nixos/nix base.
#
# Replaces the previous Ubuntu+apt image with a pinned, reproducible Nix
# profile driven by containers/apps/flake.nix. See README-NIX.md for the
# bump/verify workflow.
#
# Multi-stage build:
#   stage 1 (alpine): build launchy as a static musl binary via apk's rust
#   stage 2 (nixos/nix): runtime, all toolchains come from nixpkgs
#
# Base image note: nixos/nix is Alpine Linux. We `apk add shadow bash sudo`
# for Debian-style useradd/groupadd + bash shell + sudo. Everything else
# (rust, elixir, bun, gcc, nvim, nginx, sshd, etc.) comes from nixpkgs.

# --- Stage 1: build launchy (static musl binary) ---
FROM alpine:latest AS launchy-builder
RUN apk add --no-cache curl build-base
# Official rustup musl target produces static musl binaries by default
# (alpine's own rust package links dynamically and crt-static breaks
# proc-macros on the musl host triple — so we use rustup like CI does).
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y \
      --default-toolchain stable \
      --profile minimal \
      --target x86_64-unknown-linux-musl
ENV PATH="/root/.cargo/bin:$PATH"
WORKDIR /src
COPY system/launchy/ ./
RUN cargo build --release --target x86_64-unknown-linux-musl && \
    cp target/x86_64-unknown-linux-musl/release/launchy /launchy && \
    file /launchy

# --- Stage 2: runtime ---
FROM nixos/nix:latest

# --- Nix config: flakes on ---
RUN echo "experimental-features = nix-command flakes" >> /etc/nix/nix.conf && \
    echo "max-jobs = auto"                              >> /etc/nix/nix.conf && \
    echo "cores = 0"                                    >> /etc/nix/nix.conf

# Symlink /bin/bash so /etc/passwd login shells resolve. nixos/nix base
# image already ships bashInteractive in the default profile.
RUN ln -sf /nix/var/nix/profiles/default/bin/bash /bin/bash && \
    ln -sf /nix/var/nix/profiles/default/bin/bash /usr/bin/bash

# --- PATH + locale for all subsequent RUN steps ---
# nix profile install puts binaries in /nix/var/nix/profiles/default/bin.
# Without this, processes launched by launchy/entrypoint can't find them.
ENV PATH="/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
ENV SSL_CERT_FILE="/nix/var/nix/profiles/default/etc/ssl/certs/ca-bundle.crt"
ENV NIX_SSL_CERT_FILE="/nix/var/nix/profiles/default/etc/ssl/certs/ca-bundle.crt"
# glibc binaries (all nix-installed tools) look here for non-C locales.
ENV LOCALE_ARCHIVE="/nix/var/nix/profiles/default/lib/locale/locale-archive"
ENV LANG=en_US.UTF-8
ENV LC_ALL=en_US.UTF-8

# --- OS users (nixos/nix only ships root) ---
# /etc/passwd, /etc/group, /etc/shadow are SYMLINKS into the nix store on
# the nixos/nix base. runc/docker exec fails to resolve them when spawning
# the HEALTHCHECK probe ("openat etc/passwd: path escapes from parent").
# Replace with regular files first, then append our users.
RUN for f in passwd group shadow gshadow; do \
      if [ -L /etc/$f ]; then \
        cp -L /etc/$f /etc/$f.tmp && rm /etc/$f && mv /etc/$f.tmp /etc/$f; \
      fi; \
    done && \
    mkdir -p /etc/sudoers.d /home/gem/projects /home/gem/.config && \
    touch /etc/passwd /etc/group /etc/shadow /etc/gshadow && \
    grep -q '^root:' /etc/passwd    || echo 'root:x:0:0:root:/root:/bin/sh'      >> /etc/passwd && \
    grep -q '^gem:' /etc/passwd     || echo 'gem:x:1000:1000:gem:/home/gem:/bin/bash' >> /etc/passwd && \
    grep -q '^www-data:' /etc/passwd || echo 'www-data:x:33:33:www-data:/nonexistent:/usr/sbin/nologin' >> /etc/passwd && \
    grep -q '^root:' /etc/group     || echo 'root:x:0:'      >> /etc/group && \
    grep -q '^gem:' /etc/group      || echo 'gem:x:1000:'     >> /etc/group && \
    grep -q '^www-data:' /etc/group || echo 'www-data:x:33:' >> /etc/group && \
    grep -q '^sshd:' /etc/passwd    || echo 'sshd:x:74:74:sshd:/var/empty:/usr/sbin/nologin' >> /etc/passwd && \
    grep -q '^sshd:' /etc/group     || echo 'sshd:x:74:' >> /etc/group && \
    grep -q '^sshd:' /etc/shadow    || echo 'sshd:!:19000::::::' >> /etc/shadow && \
    mkdir -p /var/empty && \
    grep -q '^root:' /etc/shadow    || echo 'root:!:19000::::::'      >> /etc/shadow && \
    grep -q '^gem:' /etc/shadow     || echo 'gem:!:19000::::::'       >> /etc/shadow && \
    grep -q '^www-data:' /etc/shadow || echo 'www-data:!:19000::::::' >> /etc/shadow && \
    echo "gem ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/gem && \
    chmod 440 /etc/sudoers.d/gem && \
    chown -R 1000:1000 /home/gem

# --- Flake source ---
WORKDIR /etc/nixos
COPY containers/apps/flake.nix containers/apps/home.nix ./

# --- Build & install system profile (Rust, Elixir, Bun, gcc, nvim, nginx, sshd) ---
# flake.lock auto-generated from pinned inputs on first build.
RUN nix --extra-experimental-features "nix-command flakes" flake lock
RUN nix --extra-experimental-features "nix-command flakes" profile install --priority 4 .#apps-profile

# Allow gem to manage its own nix profile (single-user nix in nixos/nix
# keeps /nix owned by root; world-writable per-user dir lets non-root users
# create ~/.nix-profile links).
RUN mkdir -p /nix/var/nix/profiles/per-user && \
    chmod 1777 /nix/var/nix/profiles/per-user

# --- Activate home-manager for gem (installs nvim config) ---
# nixos/nix runs in single-user mode (root owns /nix, no nix-daemon).
# home-manager refuses to use $HOME not owned by the invoking user, so
# temporarily chown /home/gem to root, run home-manager (it sees HOME owned
# by current user), then chown back to gem (uid 1000).
RUN chown -R 0:0 /home/gem && \
    mkdir -p /home/gem/.local/state/nix/profiles /nix/var/nix/profiles/per-user/gem && \
    chown -R 0:0 /home/gem/.local /nix/var/nix/profiles/per-user/gem && \
    HOME=/home/gem USER=gem nix --extra-experimental-features "nix-command flakes" run \
      github:nix-community/home-manager/44831a7eaba4360fb81f2acc5ea6de5fde90aaa3 -- \
      switch -b backup --flake /etc/nixos#gem && \
    chown -R 1000:1000 /home/gem

# --- NvChad starter (pinned commit) ---
# Cloned into gem's nvim config dir so first `nvim` launch has the full distro.
# lazy.nvim bootstraps plugins on first launch; we pre-bootstrap below.
USER gem
RUN git clone --filter=blob:none https://github.com/NvChad/starter \
      /tmp/nvchad-starter && \
    cd /tmp/nvchad-starter && \
    git checkout e3572e1f5e1c297212c3deeb17b7863139ce663e && \
    rm -rf /home/gem/.config/nvim && \
    cp -a /tmp/nvchad-starter /home/gem/.config/nvim && \
    rm -rf /tmp/nvchad-starter /home/gem/.config/nvim/.git

# NOTE: NvChad plugin bootstrap (Lazy sync + TSUpdateSync) is NOT done at
# build time. Reason: lazy.nvim clones from various forges (codeberg, etc.)
# that rate-limit / 504 mid-build, hanging docker build. First interactive
# `nvim` launch will bootstrap plugins — accepts ~30s one-time delay.
USER root

# --- Backwards-compat symlinks ---
# gen-launchy-conf.py uses absolute paths /usr/sbin/sshd and bare `nginx`.
# Point them at the nix-installed binaries so the launchy configs work
# unmodified.
RUN mkdir -p /usr/sbin /usr/bin /usr/local/bin /usr/local/sbin \
              /usr/lib/openssh /var/log/nginx /etc/nginx && \
    ln -sf "$(which sshd)"     /usr/sbin/sshd    && \
    ln -sf "$(which nginx)"    /usr/sbin/nginx   && \
    ln -sf "$(which ssh-agent)" /usr/bin/ssh-agent && \
    ln -sf "$(which nvim)"     /usr/local/bin/vim && \
    SSHD_PREFIX="$(dirname "$(dirname "$(readlink -f "$(which sshd)")")")" && \
    NGINX_PREFIX="$(dirname "$(dirname "$(readlink -f "$(which nginx)")")")" && \
    echo "SSHD_PREFIX=$SSHD_PREFIX NGINX_PREFIX=$NGINX_PREFIX" && \
    ls "$SSHD_PREFIX/libexec/" "$NGINX_PREFIX/conf/" && \
    ln -sf "$SSHD_PREFIX/libexec/sftp-server" /usr/lib/openssh/sftp-server && \
    ln -sf "$NGINX_PREFIX/conf/mime.types" /etc/nginx/mime.types

# --- Helper scripts ---
COPY containers/apps/scripts/github-app-token.sh /usr/local/bin/github-app-token
COPY containers/apps/scripts/ssh-agent-add-keys.sh /usr/local/bin/ssh-agent-add-keys.sh
RUN chmod +x /usr/local/bin/github-app-token /usr/local/bin/ssh-agent-add-keys.sh

# GitHub host key for git operations.
RUN ssh-keyscan github.com >> /etc/ssh/ssh_known_hosts 2>/dev/null || true

# --- SSH server ---
COPY containers/apps/sshd_config /etc/ssh/sshd_config
RUN mkdir -p /run/sshd /etc/ssh/authorized_keys
COPY containers/apps/ssh/sandbox-to-apps.pub /etc/ssh/authorized_keys/gem
RUN chmod 644 /etc/ssh/authorized_keys/gem

# --- Nginx ---
COPY containers/apps/nginx.conf /etc/nginx/nginx.conf
RUN mkdir -p /etc/nginx/conf.d

# --- Launchy config ---
COPY containers/apps/launchy/config.json /etc/launchy/config.json
RUN mkdir -p /etc/launchy/apps.d /var/log/launchy /run /etc/launchy/built-in

# Generate infra + app launchy configs from devcontainer.json inside the
# build (CI also runs this script, but MCP/local builds don't — generating
# in-container keeps both paths working).
WORKDIR /build
COPY .devcontainer/devcontainer.json .devcontainer/devcontainer.json
COPY containers/apps/scripts/gen-launchy-conf.py containers/apps/scripts/gen-launchy-conf.py
RUN python3 containers/apps/scripts/gen-launchy-conf.py /etc/launchy/built-in
WORKDIR /

# --- Entrypoint ---
COPY containers/apps/docker-entrypoint.d/ /docker-entrypoint.d/
RUN chmod +x /docker-entrypoint.d/*.sh
COPY containers/apps/scripts/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

COPY containers/apps/scripts/healthcheck.sh /usr/local/bin/healthcheck
RUN chmod +x /usr/local/bin/healthcheck

# --- Launchy binary (built in stage 1 from system/launchy/) ---
COPY --from=launchy-builder /launchy /sbin/launchy
RUN chmod +x /sbin/launchy

HEALTHCHECK --interval=10s --timeout=5s --start-period=90s --retries=3 \
    CMD /usr/local/bin/healthcheck

ENTRYPOINT ["/entrypoint.sh"]
