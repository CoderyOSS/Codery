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

  # --- Shell environment ---
  home.sessionVariables = {
    EDITOR = "nvim";
    VISUAL = "nvim";
    PAGER = "less";
    LANG = "en_US.UTF-8";
    LC_ALL = "en_US.UTF-8";
  };

  programs.bash = {
    enable = true;
    # bashrcExtra runs at the TOP of .bashrc (before the interactive guard) so
    # PATH is set for non-interactive SSH commands too (bash reads ~/.bashrc
    # when invoked by sshd, even non-interactively).
    bashrcExtra = ''
      export PATH="/nix/var/nix/profiles/default/bin:$HOME/.local/bin:$PATH"
      export SSL_CERT_FILE="/nix/var/nix/profiles/default/etc/ssl/certs/ca-bundle.crt"
      export NIX_SSL_CERT_FILE="$SSL_CERT_FILE"
    '';
  };

  home.packages = [ ];
}
