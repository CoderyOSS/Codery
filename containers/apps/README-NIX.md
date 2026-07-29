# Apps Container — Nix Build

Apps container uses [Nix](https://nixos.org) for reproducible, declarative toolchains.
All packages come from a pinned nixpkgs revision; NvChad is cloned at a pinned commit.

## Files

| File | Purpose |
|------|---------|
| `containers/apps/Dockerfile` | `nixos/nix` base image, runs `nix profile install` + `home-manager switch` |
| `containers/apps/flake.nix` | System profile (Rust, Elixir, Bun, gcc, nvim, nginx, sshd, infra) |
| `containers/apps/home.nix` | home-manager module for `gem` — neovim + treesitter parsers |
| `containers/apps/scripts/entrypoint.sh` | Unchanged — runs `/docker-entrypoint.d/*.sh` then `launchy` |
| `containers/apps/service.yml` | Unchanged — volumes, ports, network alias |

## Pinned versions (nixos-25.05)

- **nixpkgs**: `ac62194c3917d5f474c1a844b6fd6da2db95077d` (nixos-25.05 HEAD as of Jan 2026)
- **home-manager**: `44831a7eaba4360fb81f2acc5ea6de5fde90aaa3` (release-25.05)
- **NvChad starter**: `e3572e1f5e1c297212c3deeb17b7863139ce663e` (cloned in Dockerfile)

Resolved package versions on this pin (approximate — check via `nix profile list` inside container):

| Tool | Version |
|------|---------|
| Rust (rustc/cargo) | 1.83.x |
| Erlang/OTP | 27.x |
| Elixir | 1.18.x |
| Bun | 1.1.x |
| Node.js | 22.x |
| GCC | 14.x |
| Neovim | 0.11.x |
| tree-sitter | 0.25.x |

## How versions get selected

`flake.nix` pins a specific nixpkgs commit. nixpkgs is one giant repo where every package version is fixed per commit. So "latest" means "whatever 25.05 ships at the pinned commit". To move forward, bump the pin:

```bash
# From a machine with nix installed (sandbox doesn't have it; use host or CI runner)
nix flake update --flake containers/apps/
git add containers/apps/flake.lock
git commit -m "apps: bump nixpkgs"
github-push
```

## What's installed

System profile (`/nix/var/nix/profiles/default/bin`):

- **Rust**: rustc, cargo, rustfmt, clippy, rust-analyzer, cargo-edit, cargo-nextest
- **C chain**: gcc, binutils, gnumake, cmake, autoconf, automake, libtool, m4, pkg-config, openssl.dev
- **Elixir/OTP**: erlang_27, elixir, elixir_ls, rebar3
- **TypeScript/Bun**: bun, nodejs_22, pnpm, yarn
- **Editor**: neovim, tree-sitter, ripgrep, fd, fzf
- **Infra**: nginx, openssh (sshd), git, jq, python3, pyjwt, curl, gnupg, unzip, gettext, sudo, ca-certificates

gem user home (`home-manager`):

- `programs.neovim` with `nvim-treesitter.withAllParsedGrammars` + `nvim-lspconfig` pre-installed
- `~/.config/nvim` populated with NvChad starter (cloned in Dockerfile, plugins bootstrapped at build)

## Building

CI workflow `deploy-apps.yml` builds unchanged — `docker/build-push-action` runs the Dockerfile inside the GitHub runner; the `nixos/nix` base image brings its own nix, so no nix setup on the runner is needed.

To build locally on the host:

```bash
# From Codery repo root
codery-ci build apps nix-test
# → docker build -t ghcr.io/coderyoss/codery:apps-nix-test \
#                 -f containers/apps/Dockerfile .
```

First build takes ~10-15 min (nix downloads ~1.5GB of packages from cache.nixos.org). Subsequent builds reuse the nix store layer.

## Verifying

```bash
codery-ci deploy-preview apps nix-test

ssh gem@apps 'rustc --version && cargo --version'
ssh gem@apps 'elixir --version && iex --version'
ssh gem@apps 'bun --version && node --version'
ssh gem@apps 'gcc --version | head -1'
ssh gem@apps 'tree-sitter --version'
ssh gem@apps 'nvim --version | head -2'
ssh gem@apps 'nginx -v 2>&1'
ssh gem@apps 'sshd -V 2>&1 | head -1'

# NvChad + plugins present
ssh gem@apps 'ls /home/gem/.local/share/nvim/lazy | head'

# Tree-sitter parsers compiled
ssh gem@apps 'ls /home/gem/.local/share/nvim/lazy/nvim-treesitter/parser | head'

# Healthcheck passes
ssh gem@apps '/usr/local/bin/healthcheck'

# Launchy running everything
ssh gem@apps 'cat /run/launchy-status.json | jq'
```

Then on the host:

```bash
codery-ci cutover apps
```

## Installing extra apps in the container

The system profile is image-baked. To install ad-hoc packages at runtime (e.g. `htop`):

```bash
ssh gem@apps
# As gem user — installs into /home/gem/.nix-profile
nix profile install nixpkgs#htop
```

For project-local dev shells, drop a `flake.nix` in `/home/gem/projects/<proj>/` and run `nix develop`.

## Disk usage

Nix store grows. Inside the container:

```bash
du -sh /nix/store
nix-collect-garbage --delete-old
```

For the image itself, `docker image ls ghcr.io/coderyoss/codery` — expect ~3-4GB (vs ~1.5GB for the old Ubuntu image).

## Rollback

`codery-ci rollback apps` redeploys the previous cached image. Original Ubuntu+apt image remains in GHCR until pruned.

## Removing Nix / reverting

`git revert <commit>` and push, then `gh workflow run deploy-apps.yml`. Old Dockerfile is preserved in git history.
