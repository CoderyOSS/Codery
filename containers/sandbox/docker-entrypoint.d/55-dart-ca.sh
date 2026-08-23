#!/bin/sh
# Dart's BoringSSL probes a fixed CA path list (ignores SSL_CERT_FILE env):
# /etc/ssl/certs/ca-certificates.crt is probed; ca-bundle.crt is not.
# Without this link every dart/pub TLS op fails while curl works.
# configuration.nix bakes it; this heals any regression at boot.
set -e
if [ ! -e /etc/ssl/certs/ca-certificates.crt ]; then
    if [ -L /etc/ssl/certs ]; then
        # /etc/ssl/certs is a symlink into the read-only store — rebuild
        # as a real dir with both cert names pointing at the store file.
        target="$(readlink -f /etc/ssl/certs)"
        rm /etc/ssl/certs
        mkdir /etc/ssl/certs
        ln -s "$target/ca-bundle.crt" /etc/ssl/certs/ca-bundle.crt
        ln -s ca-bundle.crt /etc/ssl/certs/ca-certificates.crt
    else
        ln -sf ca-bundle.crt /etc/ssl/certs/ca-certificates.crt
    fi
fi
