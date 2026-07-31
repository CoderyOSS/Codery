# home-manager module for the `gem` user in the apps container.
#
# Activated by: nix run home-manager -- switch -b backup --flake .#gem
#
# NvChad config is cloned separately in the Dockerfile (avoids fetchFromGitHub
# hash-pinning cycle — git clone with --branch <tag> is reproducible enough
# for a base image and lets you bump by editing one Dockerfile line).
{
  config, pkgs, lib, ...
}:

{
  home.username = "gem";
  home.homeDirectory = "/home/gem";
  home.stateVersion = "25.05";

  # --- Neovim ---
  # nvim itself comes from the system profile (flake.nix apps-profile).
  # home-manager just wires the editor + pre-ships treesitter parsers.
  programs.neovim = {
    enable = true;
    defaultEditor = true;
    vimAlias = true;
    viAlias = true;
    withPython3 = true;
    withNodeJs = true;
    # Ship treesitter + lspconfig ahead of lazy.nvim bootstrap so first
    # launch isn't blocked on network. NvChad still loads its own copies
    # via lazy — these are the floor.
    plugins = with pkgs.vimPlugins; [
      nvim-treesitter.withAllGrammars
      nvim-lspconfig
    ];
  };

  # Put the home-manager profile itself on PATH (starship, nvim wrappers).
  home.sessionPath = [ "$HOME/.nix-profile/bin" ];

  # --- Shell environment ---
  home.sessionVariables = {
    EDITOR = "nvim";
    VISUAL = "nvim";
    PAGER = "less";
    # Locale for SSH login shells (sourced via .profile → hm-session-vars.sh).
    # sshd does NOT inherit Docker ENV (UsePAM no, no /etc/environment,
    # AcceptEnv commented), so locale must be re-exported here or BEAM falls
    # back to latin1 filename encoding and Elixir warns on every startup.
    LANG = "C.UTF-8";
    LC_ALL = "C.UTF-8";
    LOCALE_ARCHIVE = "/nix/var/nix/profiles/default/lib/locale/locale-archive";
    ELIXIR_ERL_OPTIONS = "+fnu";
  };

  programs.bash = {
    enable = true;
    shellAliases = {
      ll = "ls -alF --color=auto";
      la = "ls -A --color=auto";
      ls = "ls --color=auto";
      grep = "grep --color=auto";
    };
    # bashrcExtra runs at the TOP of .bashrc (before the interactive guard) so
    # PATH is set for non-interactive SSH commands too (bash reads ~/.bashrc
    # when invoked by sshd, even non-interactively).
    bashrcExtra = ''
      export PATH="/nix/var/nix/profiles/default/bin:$HOME/.nix-profile/bin:$HOME/.local/bin:$PATH"
      export SSL_CERT_FILE="/nix/var/nix/profiles/default/etc/ssl/certs/ca-bundle.crt"
      export NIX_SSL_CERT_FILE="$SSL_CERT_FILE"
      export LANG=C.UTF-8
      export LC_ALL=C.UTF-8
      export LOCALE_ARCHIVE=/nix/var/nix/profiles/default/lib/locale/locale-archive
      export ELIXIR_ERL_OPTIONS=+fnu
    '';
  };

  # LS_COLORS for directory listings (symlinks ls --color above).
  programs.dircolors.enable = true;

  # Starship: git-aware colored prompt, single Rust binary, zero config.
  programs.starship.enable = true;

  home.packages = [ ];
}
