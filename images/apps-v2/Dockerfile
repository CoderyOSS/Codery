# Apps container v2 (slim) — Debian + nix + devbox + s6-overlay.
#
# Slim architecture: the image carries ONLY nix (~250 MB store) + devbox +
# apt glue. ALL dev toolchains (rust, elixir, bun, node, nvim, ~9 GB) live
# in the persistent /nix volume, installed by the s6 `devbox-replay` oneshot
# (`devbox install` from the committed devbox.json + devbox.lock) at every
# boot — no-op when the volume is already populated; on a FRESH volume the
# first boot downloads them (network required, ~10-15 min). Runtime installs:
#
#   ssh gem@apps
#   cd ~/projects/Codery/images/apps-v2 && devbox add <pkg>   (no sudo)
#
# apt owns system services (sshd, mosh, tmux) and the native build chain;
# devbox owns versioned dev toolchains. Dropped vs v1: nginx (layer
# violation), locale hacks (glibc base), ijq, rustup (devbox pins rustc).

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# --- System layer (apt): services + native build chain + small CLI ---
RUN apt-get update && apt-get install -y --no-install-recommends \
      autoconf automake bash-completion binutils build-essential ca-certificates \
      cloc cmake curl file gettext git git-lfs gnupg jq less libssl-dev libtool \
      m4 mosh openssh-client openssh-server pkg-config procps python3 \
      python3-jwt python3-pip sqlite3 sudo tmux unzip vim xz-utils \
    && rm -rf /var/lib/apt/lists/*

# --- Nix (single-user, later gem-owned) + devbox CLI ---
# Pinned binary tarball, single-user, no daemon. Running as root, nix
# defaults build-users-group to 'nixbld' and aborts ("has no members") —
# override it to empty for the install itself, then persist the override
# plus sandbox=false in /etc/nix/nix.conf (namespaces are unavailable in
# this runtime; nix auto-falls back anyway). /nix becomes a named volume
# at runtime (fresh volume seeded from this image on first mount).
RUN curl -fsSL https://releases.nixos.org/nix/nix-2.35.6/nix-2.35.6-x86_64-linux.tar.xz -o /tmp/nix.tar.xz \
    && cd /tmp \
    && tar xf nix.tar.xz \
    && cd nix-2.35.6-x86_64-linux \
    && NIX_CONFIG="build-users-group =" ./install --no-daemon \
    && cd / \
    && rm -rf /tmp/nix.tar.xz /tmp/nix-2.35.6-x86_64-linux \
    && mkdir -p /etc/nix \
    && printf 'build-users-group =\nsandbox = false\n' > /etc/nix/nix.conf
ENV PATH="/nix/var/nix/profiles/default/bin:${PATH}"
RUN curl -fsSL https://get.jetify.com/devbox | bash -s -- -f \
    && chmod 755 /usr/local/bin/devbox

# --- gem user (uid 1000 matches host bind-mount ownership) ---
# -p '*': useradd's default '!' leaves the account LOCKED — Debian sshd
# denies pubkey auth to locked accounts even with NOPASSWD sudoers.
RUN useradd -u 1000 -m -s /bin/bash gem \
    && usermod -p '*' gem \
    && echo "gem ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/gem \
    && chmod 440 /etc/sudoers.d/gem \
    && git lfs install --system \
    && chown -R 1000:1000 /nix

# --- Devbox manifest (baked for reference/repair) ---
# NOT installed into the image — the /nix volume holds the toolchains.
# devbox-replay installs from the committed manifest at every boot.
COPY devbox.json* devbox.lock* /usr/local/share/apps-devbox/
RUN chown -R 1000:1000 /usr/local/share/apps-devbox

USER gem
# --- NvChad starter (pinned) — first `nvim` launch bootstraps lazy plugins ---
# mkdir -p: nothing in the slim build creates ~/.config (devbox's gem-level
# install used to); cp -a needs the parent to exist.
RUN mkdir -p /home/gem/.config \
    && git clone --filter=blob:none https://github.com/NvChad/starter /tmp/nvchad-starter \
    && cd /tmp/nvchad-starter \
    && git checkout e3572e1f5e1c297212c3deeb17b7863139ce663e \
    && rm -rf /home/gem/.config/nvim \
    && cp -a /tmp/nvchad-starter /home/gem/.config/nvim \
    && rm -rf /tmp/nvchad-starter /home/gem/.config/nvim/.git
# elixir-ls: NvChad defaults() sets capabilities globally; lspconfig
# auto-registers elixirls defaults — just enable it.
RUN printf '\nvim.lsp.enable("elixirls")\n' >> /home/gem/.config/nvim/lua/configs/lspconfig.lua
# Silence the nvim-treesitter auto-installer: parsers compile at runtime
# via the first-launch TSUpdate path (gcc is on PATH); avoid build-time
# network flakiness, same rationale as v1.
RUN cat > /home/gem/.config/nvim/lua/plugins/init.lua <<'NVIM_PLUGINS'
return {
  {
    "stevearc/conform.nvim",
    opts = require "configs.conform",
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

# --- Shell environment for gem (login + non-interactive ssh) ---
USER root
RUN printf '%s\n' \
      '# devbox profile first: runtime-installed tools shadow system ones' \
      'export PATH="$HOME/projects/Codery/images/apps-v2/.devbox/nix/profile/default/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$HOME/.local/bin:$PATH"' \
      'export LANG=C.UTF-8 LC_ALL=C.UTF-8' \
      'export EDITOR=nvim VISUAL=nvim PAGER=less' \
      'command -v starship >/dev/null 2>&1 && eval "$(starship init bash)"' \
      'alias ll="ls -alF --color=auto"' \
      'alias la="ls -A --color=auto"' \
      'alias ls="ls --color=auto"' \
      'alias grep="grep --color=auto"' \
      > /home/gem/.bashrc \
    && printf '%s\n' \
      'export PATH="$HOME/projects/Codery/images/apps-v2/.devbox/nix/profile/default/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH"' \
      'export LANG=C.UTF-8 LC_ALL=C.UTF-8' \
      > /etc/profile.d/zz-devbox.sh \
    && echo "set -g mouse on" > /home/gem/.tmux.conf \
    && chown 1000:1000 /home/gem/.bashrc /home/gem/.tmux.conf \
    && chmod 644 /home/gem/.bashrc /home/gem/.tmux.conf /etc/profile.d/zz-devbox.sh

# --- Helper scripts ---
COPY scripts/github-app-token.sh /usr/local/bin/github-app-token.sh
COPY scripts/ssh-agent-add-keys.sh /usr/local/bin/ssh-agent-add-keys.sh
COPY scripts/runtime-apps-restore.sh /usr/local/bin/runtime-apps-restore
COPY scripts/devbox-replay /usr/local/bin/devbox-replay
COPY scripts/ssh-prep /usr/local/bin/ssh-prep
COPY scripts/healthcheck.sh /usr/local/bin/healthcheck
RUN chmod +x /usr/local/bin/github-app-token.sh /usr/local/bin/ssh-agent-add-keys.sh \
      /usr/local/bin/runtime-apps-restore /usr/local/bin/devbox-replay \
      /usr/local/bin/ssh-prep /usr/local/bin/healthcheck

# --- sshd_config: (re)installed into /etc/ssh by ssh-prep at every boot ---
# /etc/ssh is a host-key volume mount; the config lives in the volume, so
# each boot re-installs this file to keep config and image in sync.
COPY sshd_config /usr/local/share/apps-etc/sshd_config
RUN mkdir -p /etc/s6-overlay/apps.d

# --- s6-overlay (process supervision) ---
ARG S6_OVERLAY_VERSION=3.2.3.2
RUN for f in noarch x86_64; do \
      curl -fsSL --retry 5 --retry-delay 3 --retry-all-errors --connect-timeout 20 \
        -o /tmp/s6-overlay-$f.tar.xz \
        "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-$f.tar.xz" \
      && tar -C / -Jxf /tmp/s6-overlay-$f.tar.xz \
      && rm /tmp/s6-overlay-$f.tar.xz; \
    done
COPY s6-overlay/ /etc/s6-overlay/

# PATH for root processes: s6 services, HEALTHCHECK (docker exec has no
# login shell). The repo-dir devbox profile is created by devbox-replay at
# boot (bind-mounted, persists); devbox 0.22 puts profiles under
# nix/profile/<profile-name>.
ENV PATH="/home/gem/projects/Codery/images/apps-v2/.devbox/nix/profile/default/bin:/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
ENV LANG=C.UTF-8

# start-period 600s: tolerates fresh-volume first boots, where
# devbox-replay downloads the whole manifest before tools appear.
HEALTHCHECK --interval=10s --timeout=5s --start-period=600s --retries=3 \
    CMD /usr/local/bin/healthcheck

ENTRYPOINT ["/init"]
