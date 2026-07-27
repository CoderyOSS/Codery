{
  description = "Codery sandbox container — declarative package set and rootfs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    # Repo content (scripts, configs, launchy binary, SSH keys). The Docker
    # build context is the repo root; this input makes it visible to the flake.
    # NOTE: .git must be excluded from the Docker context (.dockerignore),
    # otherwise nix treats this as a git repo and uncommitted files vanish.
    repo = {
      url = "path:../..";
      flake = false;
    };
  };

  outputs =
    { nixpkgs, repo, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      cfg = import ./configuration.nix { inherit pkgs repo; };
    in
    {
      packages.${system} = {
        inherit (cfg) toolEnv rootfs;
        default = cfg.rootfs;
      };
    };
}
