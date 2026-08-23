# Codery sandbox — package set + root filesystem layout.
#
# toolEnv: every runtime tool, one declarative list. Add a package here and
#          rebuild — no more curl|bash chains in the Dockerfile.
# rootfs:  FHS skeleton (/bin, /etc, /usr/sbin/sshd, entrypoint, launchy,
#          home skeleton) as symlinks into the nix store closure.
{ pkgs, repo }:

let
  toolEnv = pkgs.buildEnv {
    name = "sandbox-tools";
    paths = with pkgs; [
      # shell + core utilities
      bashInteractive
      coreutils
      findutils
      gnugrep
      gnused
      gawk
      which
      procps
      less
      file
      diffutils
      gnutar
      gzip
      xz
      # system-ish
      openssl
      cacert
      glibcLocales
      shadow # su, useradd (su copied setuid during image assembly)
      sudo # copied setuid during image assembly
      # user tools
      curl
      git
      gnupg
      jq
      nano
      vim
      neovim
      tmux
      unzip
      openssh # ssh client + sshd + ssh-keygen
      gh
      ripgrep
      fd
      poppler_utils # Open Design PDF export
      # language toolchains
      nodejs_24
      bun
      rustup
      python3
      uv
    ];
    pathsToLink = [ "/bin" "/share" ];
  };

  # sshd/su/sudo PAM stack: permit-all. The container has no passwords and
  # sshd uses UsePAM no; su/sudo only gate via setuid + sudoers rules.
  pamPermit = ''
    auth sufficient pam_permit.so
    account sufficient pam_permit.so
    session required pam_permit.so
  '';

  rootfs = pkgs.runCommand "sandbox-rootfs" { } ''
    mkdir -p $out/{bin,sbin,etc,tmp,run,proc,sys,dev}
    mkdir -p $out/lib64
    mkdir -p $out/usr/{bin,sbin,lib/locale}
    mkdir -p $out/usr/local/bin
    mkdir -p $out/var/run/sshd
    mkdir -p $out/var/empty
    mkdir -p $out/home/gem
    chmod 1777 $out/tmp
    chmod 700 $out/var/empty

    # Every tool binary reachable from both /bin and /usr/bin
    for f in ${toolEnv}/bin/*; do
      ln -s "$f" "$out/bin/$(basename "$f")"
      ln -s "$f" "$out/usr/bin/$(basename "$f")"
    done
    ln -sf ${toolEnv}/bin/bash $out/bin/sh

    # FHS dynamic loader: non-nix ELF binaries (opencode/claude npm bins,
    # Playwright browsers) hardcode /lib64/ld-linux-x86-64.so.2 as PT_INTERP.
    # Missing loader surfaces as execve ENOENT ("No such file or directory").
    ln -sf ${pkgs.glibc}/lib/ld-linux-x86-64.so.2 $out/lib64/ld-linux-x86-64.so.2

    # sshd canonical path (devcontainer.json launches /usr/sbin/sshd)
    ln -sf ${toolEnv}/bin/sshd $out/usr/sbin/sshd

    # ── /etc ──────────────────────────────────────────────────────────
    cat > $out/etc/passwd <<'EOF'
    root:x:0:0:root:/root:/bin/bash
    gem:x:1000:1000:gem:/home/gem:/bin/bash
    sshd:x:74:74:sshd privilege separation:/var/empty:/bin/false
    EOF

    cat > $out/etc/group <<'EOF'
    root:x:0:
    gem:x:1000:
    sshd:x:74:
    EOF

    cat > $out/etc/nsswitch.conf <<'EOF'
    hosts: files dns
    passwd: files
    group: files
    EOF

    mkdir -p $out/etc/ssh
    cat > $out/etc/ssh/sshd_config <<'EOF'
    Port 22
    Protocol 2
    PasswordAuthentication no
    ChallengeResponseAuthentication no
    PubkeyAuthentication yes
    AuthorizedKeysFile .ssh/authorized_keys /run/secrets/authorized_keys
    AllowUsers gem
    UsePAM no
    PrintMotd no
    PrintLastLog no
    Subsystem sftp internal-sftp
    EOF
    # Populated at image build time (network needed for ssh-keyscan)
    touch $out/etc/ssh/ssh_known_hosts

    # Launchy service definitions (rendered by 15-render-domain.sh, which
    # edits /etc/launchy.json — entrypoint execs launchy with that path)
    cp ${repo}/.devcontainer/devcontainer.json $out/etc/launchy.json

    # sudo: only github-push, passwordless (reads root-owned PEM)
    mkdir -p $out/etc/sudoers.d
    cat > $out/etc/sudoers <<'EOF'
    root ALL=(ALL) ALL
    #includedir /etc/sudoers.d
    EOF
    chmod 440 $out/etc/sudoers
    echo 'gem ALL=(ALL) NOPASSWD: /usr/local/bin/github-push' > $out/etc/sudoers.d/github-push
    chmod 440 $out/etc/sudoers.d/github-push

    # PAM permit-all stack for su/sudo
    mkdir -p $out/etc/pam.d
    for f in su sudo other; do
      printf '%s' '${pamPermit}' > $out/etc/pam.d/$f
    done

    # TLS certs (OpenSSL/GnuTLS lookups) + locale archive, stable paths.
    # Real dir, NOT a symlink to the read-only store: dart's BoringSSL
    # probes a fixed CA path list (ignores SSL_CERT_FILE/SSL_CERT_DIR env)
    # that includes /etc/ssl/certs/ca-certificates.crt but NOT
    # ca-bundle.crt — nixpkgs cacert only ships the latter, so every
    # dart/pub TLS op failed while curl worked. Link both names.
    mkdir -p $out/etc/ssl/certs
    ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/ssl/certs/ca-bundle.crt
    ln -s ca-bundle.crt $out/etc/ssl/certs/ca-certificates.crt
    ln -s ${pkgs.glibcLocales}/lib/locale/locale-archive \
      $out/usr/lib/locale/locale-archive

    # ── Entrypoint + helper scripts ───────────────────────────────────
    cp -r ${repo}/containers/sandbox/docker-entrypoint.d $out/docker-entrypoint.d
    chmod +x $out/docker-entrypoint.d/*.sh
    cp ${repo}/containers/sandbox/scripts/entrypoint.sh $out/entrypoint.sh
    chmod +x $out/entrypoint.sh

    # Launchy PID 1 (static binary)
    cp ${repo}/containers/sandbox/bin/launchy $out/sbin/launchy
    chmod +x $out/sbin/launchy

    cp ${repo}/containers/sandbox/scripts/github-app-token.sh $out/usr/local/bin/github-app-token
    cp ${repo}/containers/sandbox/scripts/github-push.sh $out/usr/local/bin/github-push
    cp ${repo}/containers/sandbox/scripts/prune-opencode-diffs.sh $out/usr/local/bin/prune-opencode-diffs
    cp ${repo}/containers/sandbox/scripts/opencode-serve-guard.sh $out/usr/local/bin/opencode-serve-guard
    cp ${repo}/containers/sandbox/scripts/github-app-permissions-mcp.ts $out/usr/local/bin/github-app-permissions-mcp.ts
    chmod +x $out/usr/local/bin/github-app-token $out/usr/local/bin/github-push \
      $out/usr/local/bin/prune-opencode-diffs $out/usr/local/bin/opencode-serve-guard

    # ── Home skeleton (ownership fixed to 1000:1000 in Dockerfile) ────
    cp ${repo}/containers/sandbox/agents_file $out/home/gem/AGENTS.md
    mkdir -p $out/home/gem/.config/opencode
    cp ${repo}/opencode.json $out/home/gem/.config/opencode/config.json
    cp ${repo}/containers/sandbox/opencode-global-agents.md $out/home/gem/.config/opencode/AGENTS.md
    mkdir -p $out/home/gem/.agents
    cp -r ${repo}/containers/sandbox/agents-skills/. $out/home/gem/.agents/skills/
    cp ${repo}/containers/sandbox/tmux.conf $out/home/gem/.tmux.conf

    mkdir -p $out/home/gem/.ssh
    cp ${repo}/containers/sandbox/ssh/sandbox-to-apps $out/home/gem/.ssh/id_codery_apps
    chmod 600 $out/home/gem/.ssh/id_codery_apps
    cat > $out/home/gem/.ssh/config <<'EOF'
    Host apps
        HostName apps
        User gem
        IdentityFile /home/gem/.ssh/id_codery_apps
        StrictHostKeyChecking no
        UserKnownHostsFile /dev/null
        LogLevel ERROR
    EOF
    chmod 600 $out/home/gem/.ssh/config

    mkdir -p $out/home/gem/projects \
      $out/home/gem/.local/share/opencode \
      $out/home/gem/.claude \
      $out/home/gem/open-design
  '';

in
{
  inherit toolEnv rootfs;
}
