# Apps container — Nix system profile + home-manager.
#
# Pinned to nixos-25.05 stable. Bump by running:
#   nix flake update --flake containers/apps/
# then commit the regenerated flake.lock.
#
# Build the system profile:
#   nix profile install path:containers/apps#apps-profile
#
# Activate user home (gem):
#   nix run home-manager -- switch -b backup --flake containers/apps#gem
{
  description = "Codery apps container toolchain";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/ac62194c3917d5f474c1a844b6fd6da2db95077d";
  inputs.home-manager = {
    url = "github:nix-community/home-manager/44831a7eaba4360fb81f2acc5ea6de5fde90aaa3";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, home-manager, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      # Runtime toolchains installed system-wide into /nix/var/nix/profiles/default.
      # Healthcheck requires bun, node, git, python3 on PATH — all present here.
      systemTools = with pkgs; [
        # --- Rust ---
        rustc
        cargo
        rustfmt
        clippy
        rust-analyzer
        cargo-edit
        cargo-nextest
        pkg-config
        openssl.dev

        # --- C compiler chain (needed for tree-sitter parsers + native deps) ---
        gcc
        binutils
        gnumake
        cmake
        autoconf
        automake
        libtool
        gnum4

        # --- Elixir / Erlang (latest on nixos-25.05: OTP 27, Elixir 1.18) ---
        beam.interpreters.erlang_27
        beam.packages.erlang_27.elixir
        beam.packages.erlang_27.elixir-ls
        beam.packages.erlang_27.rebar3

        # --- TypeScript / Bun (Bun 1.1.x, Node 22.x on 25.05) ---
        bun
        nodejs_22
        pnpm
        yarn

        # --- Editor + tree-sitter (nvim 0.11.x on 25.05) ---
        neovim
        tree-sitter
        ripgrep
        fd
        fzf

        # --- Infra used by entrypoint / launchy-managed services ---
        nginx
        openssh
        git
        jq
        # python3 with pyjwt so github-app-token.sh's `import jwt` works.
        # `python3.withPackages` wraps the interpreter to see the listed pkgs.
        (python3.withPackages (p: [ p.pyjwt ]))
        curl
        gnupg
        unzip
        gettext
        sudo
        cacert

        # --- Misc dev ---
        less
        which
        file
        # util-linux: `script` (PTY wrapper Blink/Mosh expect), flock, nsenter
        util-linux

        # --- Locale data for nix (glibc) binaries ---
        # Alpine base uses musl; nix-installed binaries are glibc-linked and
        # need LOCALE_ARCHIVE pointing here for non-C locales to work.
        glibcLocales
      ];
    in {
      packages.${system} = {
        apps-profile = pkgs.buildEnv {
          name = "apps-system-profile";
          paths = systemTools;
          extraOutputsToInstall = [ "bin" "lib" "man" "doc" ];
        };
        default = self.packages.${system}.apps-profile;
      };

      homeConfigurations.gem = home-manager.lib.homeManagerConfiguration {
        inherit pkgs;
        modules = [ ./home.nix ];
      };
    };
}
