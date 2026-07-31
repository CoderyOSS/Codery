# Apps container — Nix-managed toolchain + s6-overlay process supervision.
#
# Pinned, reproducible Nix profile driven by containers/apps/flake.nix.
# Process supervision via s6-overlay (s6-rc) — replaces the previous
# Launchy-based process manager. See README-NIX.md for the bump/verify workflow.
#
# Base image note: nixos/nix is Alpine Linux. Everything else
# (rust, elixir, bun, gcc, nvim, nginx, sshd, etc.) comes from nixpkgs.

# --- Runtime ---
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
ENV PATH="/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
ENV SSL_CERT_FILE="/nix/var/nix/profiles/default/etc/ssl/certs/ca-bundle.crt"
ENV NIX_SSL_CERT_FILE="/nix/var/nix/profiles/default/etc/ssl/certs/ca-bundle.crt"
# glibc binaries (all nix-installed tools) look here for non-C locales.
ENV LOCALE_ARCHIVE="/nix/var/nix/profiles/default/lib/locale/locale-archive"
ENV LANG=C.UTF-8
ENV LC_ALL=C.UTF-8
# Erlang/BEAM filename encoding. nixos/nix (Alpine) base lacks a generated
# UTF-8 locale in the glibc sense, so BEVM defaults to latin1 — Elixir then
# warns and may mangle non-ASCII paths. +fnu forces Unicode filename mode
# regardless of locale detection. See Elixir "Unicode" docs.
ENV ELIXIR_ERL_OPTIONS="+fnu"

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
    grep -q '^gem:' /etc/shadow     || echo 'gem:*:19000::::::'       >> /etc/shadow && \
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

# --- sudo setuid: nix store binaries lack setuid bit ---
# home-manager-only container (no NixOS modules) means security.wrappers
# never runs. The nix profile bin is first in PATH and has a non-setuid
# sudo symlink. Remove that symlink, install a real setuid copy at
# /usr/bin/sudo (in runtime PATH after nix profile bin). Write a custom
# sudoers with nix profile bin in secure_path (default sudoers only has
# FHS paths, so sudo can't find nix-installed commands).
RUN src="$(readlink -f "$(command -v sudo)")" && \
    rm -f /nix/var/nix/profiles/default/bin/sudo && \
    cp "$src" /usr/bin/sudo && \
    chmod 4755 /usr/bin/sudo && \
    printf 'Defaults secure_path="/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"\nroot ALL=(ALL:ALL) ALL\n@includedir /etc/sudoers.d\n' \
      > /etc/sudoers && \
    chmod 440 /etc/sudoers

# --- PAM config for sudo ---
# sudoers.so plugin links libpam — PAM IS used. nix linux-pam reads
# config from /usr/lib/pam.d/ (compiled-in path) and modules from the
# nix store security dir. Create permissive config (sudoers already
# grants NOPASSWD:ALL to gem).
RUN mkdir -p /etc/pam.d /usr/lib/pam.d && \
    printf 'auth sufficient pam_permit.so\naccount sufficient pam_permit.so\nsession optional pam_permit.so\n' \
      > /etc/pam.d/sudo && \
    ln -sf /etc/pam.d/sudo /usr/lib/pam.d/sudo && \
    chmod 644 /etc/pam.d/sudo

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

# --- Treesitter grammars + queries (bypass broken nvim-treesitter installer) ---
# See docs/treesitter-nix-fix.md for full explanation.
# Short version: lazy's nvim-treesitter (master) requires neovim 0.12+; container
# has 0.11.5 (nixos-25.05). Installer crashes on vim.list API, so :TSInstall is
# broken for all languages. home.nix ships nvim-treesitter.withAllGrammars but
# lazy shadows it. Plugin also nests queries under runtime/queries/ while
# treesitter scans <rtp>/queries/ (one level shallower); installer normally
# bridges that gap.
# Fix: symlink nix-built parsers + nix-shipped queries into stdpath('data')/site/
# (on runtimepath by default). Bypasses installer entirely. Auto-includes any
# future grammar nix ships — no edit needed on nixpkgs bumps.
RUN SITE=/home/gem/.local/share/nvim/site && \
    mkdir -p "$SITE/parser" "$SITE/queries" && \
    for so in /nix/store/*-vimplugin-treesitter-grammar-*/parser/*.so; do \
      [ -f "$so" ] || continue; \
      lang=$(basename "$so" .so); \
      ln -sf "$so" "$SITE/parser/$lang.so"; \
    done && \
    TS_NIX=$(ls -d /nix/store/*-vimplugin-nvim-treesitter-2* 2>/dev/null | head -1) && \
    if [ -n "$TS_NIX" ] && [ -d "$TS_NIX/queries" ]; then \
      for q in "$TS_NIX"/queries/*; do \
        [ -d "$q" ] || continue; \
        lang=$(basename "$q"); \
        ln -sf "$q" "$SITE/queries/$lang"; \
      done; \
    fi && \
    ls "$SITE/parser/" | wc -l && \
    ls "$SITE/queries/" | wc -l

# Elixir LSP — elixir-ls on PATH via flake.nix (beam.packages.erlang_27.elixir-ls).
# NvChad's defaults() sets capabilities globally via vim.lsp.config("*", ...);
# lspconfig auto-registers the elixirls.lua default config, so just enable it.
RUN printf '\nvim.lsp.enable("elixirls")\n' >> /home/gem/.config/nvim/lua/configs/lspconfig.lua

# Silence nvim-treesitter installer noise. Parsers + queries are wired manually
# above; the plugin's installer is redundant. Without this override, NvChad's
# default ensure_installed = {"lua","luadoc",...} triggers a build task on every
# nvim launch that can't compile (no cc at runtime) and spams output. Setting
# ensure_installed = {} skips it entirely. Highlighting still works via
# neovim's built-in vim.treesitter.start() + the symlinked parsers/queries.
RUN cat > /home/gem/.config/nvim/lua/plugins/init.lua <<'NVIM_PLUGINS'
return {
  {
    "stevearc/conform.nvim",
    opts = require "configs.conform",
  },

  {
    "neovim/nvim-lspconfig",
    config = function()
      require "configs.lspconfig"
    end,
  },

  {
    "nvim-treesitter/nvim-treesitter",
    opts = {
      ensure_installed = {},
      auto_install = false,
      sync_install = false,
    },
  },
}
NVIM_PLUGINS

# NOTE: NvChad plugin bootstrap (Lazy sync + TSUpdateSync) is NOT done at
# build time. Reason: lazy.nvim clones from various forges (codeberg, etc.)
# that rate-limit / 504 mid-build, hanging docker build. First interactive
# `nvim` launch will bootstrap plugins — accepts ~30s one-time delay.
USER root

# --- Backwards-compat symlinks ---
# Ensure nix-installed binaries are at expected paths.
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
# Locale loader sourced by every s6-rc run script (s6-rc doesn't pass container
# ENV to longruns). Pulls LANG/LC_ALL/ELIXIR_ERL_OPTIONS/LOCALE_ARCHIVE from
# /run/s6/container_environment so beam.smp gets +fnu and Elixir stops warning.
COPY containers/apps/scripts/s6-load-locale-env.sh /usr/local/bin/s6-load-locale-env
RUN chmod +x /usr/local/bin/github-app-token /usr/local/bin/ssh-agent-add-keys.sh /usr/local/bin/s6-load-locale-env

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

# --- s6-overlay (process supervision) ---
ARG S6_OVERLAY_VERSION=3.2.3.2
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz /tmp
RUN tar -C / -Jxpf /tmp/s6-overlay-noarch.tar.xz
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-x86_64.tar.xz /tmp
RUN tar -C / -Jxpf /tmp/s6-overlay-x86_64.tar.xz

# --- s6-rc service definitions ---
COPY containers/apps/s6-overlay/ /etc/s6-overlay/
RUN mkdir -p /etc/s6-overlay/apps.d

# --- Healthcheck ---
COPY containers/apps/scripts/healthcheck.sh /usr/local/bin/healthcheck
RUN chmod +x /usr/local/bin/healthcheck

HEALTHCHECK --interval=10s --timeout=5s --start-period=90s --retries=3 \
    CMD /usr/local/bin/healthcheck

ENTRYPOINT ["/init"]
