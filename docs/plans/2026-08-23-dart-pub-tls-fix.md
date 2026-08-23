# Dart/pub TLS Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. **Update checkboxes + Progress Log as you go — this file is the resume point if blocked.**

**Goal:** Make `dart pub get` / `flutter pub get` work directly against pub.dev from the sandbox, forever, and codify the knowledge so no agent ever debugs this again.

**Architecture:** Dart's BoringSSL probes a fixed CA path list (ignores `SSL_CERT_FILE`/`SSL_CERT_DIR` env — proven empirically). The nix rootfs only ships `ca-bundle.crt`, which is NOT in dart's probe list. Fix = provide `/etc/ssl/certs/ca-certificates.crt` (which IS probed) in the image, plus a boot-time self-heal script, plus a persistent `PUB_CACHE` on the projects bind mount so redeploys stop wiping the pub cache.

**Tech Stack:** Nix rootfs builder (`containers/sandbox/nixos/configuration.nix`), docker-entrypoint.d shell script, Dockerfile ENV.

## Evidence (2026-08-23, live)

- `SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt` IS set in env; dart default context still fails: `HandshakeException ... CERTIFICATE_VERIFY_FAILED: unable to get local issuer certificate`
- Same bundle loaded via explicit `SecurityContext().setTrustedCertificates('/etc/ssl/certs/ca-bundle.crt')` → **200 OK**. Bundle content is fine; dart just can't FIND it.
- `grep -aoh` on `~/projects/flutter/bin/cache/dart-sdk/bin/dart` — embedded probe list:
  `/etc/openssl/certs`, `/etc/pki/tls/cacert.pem`, `/etc/pki/tls/certs`, `/etc/pki/tls/certs/ca-bundle.crt`, `/etc/security/cacerts`, `/etc/ssl/cert.pem`, `/etc/ssl/certs`, **`/etc/ssl/certs/ca-certificates.crt`**
  → `ca-bundle.crt` under `/etc/ssl/certs/` is NOT probed. `ca-certificates.crt` IS.
- `/etc/ssl/certs` is a symlink into read-only nix store (`nss-cacert-3.117`), 148-cert bundle, no hashed dir.
- Old playwright-based sandbox had this exact fix codified in `containers/sandbox/Dockerfile.base:22-29` — LOST in the nix migration. This plan restores it for the nix rootfs.
- Apps container (`nixos/nix` base): glibc dart binary cannot execute (no `/lib64/ld-linux-x86-64.so.2`). Flutter/dart run ONLY in the sandbox. Not fixable without ditching the nixos/nix builder base — out of scope; codify as documentation instead.
- History: agents burned 4+ sessions (Aug 2, 15-16, 20, 23) on this. Ad-hoc workarounds: `node /tmp/opencode/mirror2.cjs` proxy on :8123 + `PUB_HOSTED_URL=http://localhost:8123` (unsupervised, in /tmp, doesn't rewrite `archive_url`, half-broken), manual curl+untar cache population ×3, 5-way cache-dir zoo.
- Live-fix from inside the current container is impossible: `sudo` blocked by `no-new-privileges`, store dirs root-owned 555. The image rebuild IS the fix.

## Global Constraints

- Never edit `/nix/store` contents at runtime — rootfs layout changes belong in `configuration.nix`.
- Entrypoint scripts run as root at boot (PID 1 chain) — self-heal belongs there, not in agent sessions.
- Sandbox redeploys wipe everything not under `/home/gem/projects` (bind mount).
- Deploy path: local build via MCP `codery_exec` (`build` → `deploy-preview`), cutover by human on host. Never `gh workflow run` unless asked.
- Commit message style: concise, imperative. Push via `github-push` only.

---

### Task 1: Record evidence [DONE pre-plan]

- [x] Live reproduction + binary probe-list extraction + bundle validity proof (see Evidence above).

### Task 2: configuration.nix — provide `/etc/ssl/certs/ca-certificates.crt`

**Files:**
- Modify: `containers/sandbox/nixos/configuration.nix:149-151`

**Change** — replace the single certs-dir symlink with a real dir carrying both names:

```nix
    # TLS certs (OpenSSL/GnuTLS lookups) + locale archive, stable paths.
    # Real dir, NOT a symlink to the read-only store: dart's BoringSSL
    # probes a fixed CA path list (ignores SSL_CERT_FILE/SSL_CERT_DIR env)
    # that includes /etc/ssl/certs/ca-certificates.crt but NOT
    # ca-bundle.crt — nixpkgs cacert only ships the latter, so every
    # dart/pub TLS op failed while curl worked. Link both names.
    mkdir -p $out/etc/ssl/certs
    ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/ssl/certs/ca-bundle.crt
    ln -s ca-bundle.crt $out/etc/ssl/certs/ca-certificates.crt
```

- [x] Edit applied
- [x] `nix` syntax sanity (balanced braces; relies on existing `pkgs.cacert` in scope — same as before)

### Task 3: Boot-time self-heal — `55-dart-ca.sh`

**Files:**
- Create: `containers/sandbox/docker-entrypoint.d/55-dart-ca.sh`

Belt-and-suspenders: even if the rootfs regresses, boot recreates the link (runs as root; root CAN write the layer's /etc).

```sh
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
```

Note: `configuration.nix:156-157` copies `docker-entrypoint.d/*.sh` + chmod +x automatically — no other wiring needed.

- [x] File created
- [x] Executable bit not needed in repo (chmod happens at build), but keep `0755` in git for local testing

### Task 4: Persistent `PUB_CACHE`

**Files:**
- Modify: `examples/Dockerfile.sandbox` ENV block (~line 158-165) — add `PUB_CACHE=/home/gem/projects/.pub-cache`
- Modify: `examples/Dockerfile.sandbox` `.bashrc` printf block (~line 123-135) — add `export PUB_CACHE=/home/gem/projects/.pub-cache` (sshd strips env; .bashrc re-exports)

Survives sandbox redeploys (projects dir is the host bind mount). Ends the "pub cache wiped, deps gone" rediscovery loop. Old `~/.pub-cache` cache-zoo (`localhost%3A8123`, `%588123`, `%2588123`, …) dies with the old container — intentional, clean slate with working TLS.

- [x] ENV added
- [x] .bashrc export added

### Task 5: Codify for agents — `agents_file` Flutter/Dart section

**Files:**
- Modify: `containers/sandbox/agents_file` (becomes `/home/gem/AGENTS.md` in the image, per `configuration.nix:174`)

Add a section (after "## Environment"):

```markdown
## Flutter / Dart

Flutter SDK lives at `/home/gem/projects/flutter` (bind-mounted, survives redeploys).
`PUB_CACHE=/home/gem/projects/.pub-cache` — persistent, never re-download cached packages.

Rules:
- Run flutter/dart ONLY in the sandbox. The apps container is `nixos/nix` base —
  glibc binaries like `bin/cache/dart-sdk/bin/dart` CANNOT execute there
  ("required file not found"). Never run flutter on the apps side.
- `pub get` works directly against pub.dev. Do NOT set `PUB_HOSTED_URL`, do NOT
  use a local mirror — the dart CA fix is baked into the image
  (/etc/ssl/certs/ca-certificates.crt; dart ignores SSL_CERT_FILE env).
- Always use a timeout: `timeout 180 flutter pub get` / `timeout 600 flutter test`.
  Hung pub/flutter_tester processes have burned 50-minute agent sessions.
- If dart TLS ever fails again: check `ls -la /etc/ssl/certs/` for the
  ca-certificates.crt link first — that is the known failure mode, not network.
```

- [x] Section added

### Task 6: Commit

- [x] `git add` plan + all changed files, commit to master

### Task 7: Local build

- [x] `codery_exec ["build", "sandbox", "dart-ca-fix"]` → poll `codery_exec_status` until done (nix build — expect 10-30 min)

### Task 8: Preview deploy

- [x] `codery_exec ["deploy-preview", "sandbox", "dart-ca-fix"]` → container boots + health check passes (proves rootfs builder change didn't break the image)

### Task 9: Cutover [DONE]

- [x] User ran `codery-ci cutover sandbox` — complete.

### Task 10: Post-cutover verification (from inside new container)

- [x] `ls -la /etc/ssl/certs/` → `ca-certificates.crt -> ca-bundle.crt` present; `55-dart-ca.sh` in `/docker-entrypoint.d/`
- [x] dart default-context TLS test → `OK 200` (no SecurityContext override, no env tricks)
- [x] `timeout 300 flutter pub get` in CartaClient/frontend → "Changed 76 dependencies!", exit 0, direct pub.dev, no `PUB_HOSTED_URL`
- [x] `PUB_CACHE=/home/gem/projects/.pub-cache`, cache populated there, single clean `pub.dev` dir (old 5-dir zoo died with green container)

### Task 11: Cleanup [DONE]

- [x] Mirror killed, `mirror2.cjs` + `pub_mirror*.{js,cjs}` removed, port 8123 closed
- [x] CartaClient/AGENTS.md updated — stale 8123 instruction replaced with fixed-at-image-level note
- [ ] Commit Codery plan file final state + push; commit CartaClient AGENTS.md

## Fallback

If Task 10 dart TLS STILL fails (mechanism wrong): mirror becomes permanent infra — bake supervised mirror (v3: rewrites `archive_url` in JSON, disk cache on projects mount, launchy service) + `pub-get` wrapper. Escalate to user before building this.

## Progress Log

- 2026-08-23 — COMPLETE. Cutover done; all verification green (dart TLS 200, flutter pub get 76 deps exit 0, PUB_CACHE persistent, mirror removed, docs updated). Follow-ups left open: host swap (8GB RAM, none present — OOM'd during first build), apps nginx `getgrnam("nogroup")` reload failure (pre-existing, apps image).

- 2026-08-23 ~07:30 — Research complete, evidence recorded (Task 1). Plan written.
- 2026-08-23 ~07:45 — Tasks 2-6 done: configuration.nix + 55-dart-ca.sh + PUB_CACHE + agents_file. Commit `733a9cd` on master (not pushed yet — push after cutover verification).
- 2026-08-23 ~07:48 — Build started: `codery_exec ["build","sandbox","dart-ca-fix"]` job `5f240006870f`, log `/var/log/codery-ci-mcp/exec-1787470710-build-5f240006870f.log`. Note: older commit fd4670d fixed this same bug in `Dockerfile.base` — dead file, the nix build never reads it; that is how the regression survived.
- 2026-08-23 (same day, later) — **HOST OOM HARD CRASH** during first rebuild attempt (job `5f3ee8240f3d` died with the box; 8GB RAM, no swap). Power-cycled by user. Post-reboot: services healthy, mirror process dead (restarted from surviving `/tmp/opencode/mirror2.cjs`), docker layer cache partially intact. **Caution protocol for this host: no swap, 8GB RAM — monitor `MemAvailable` via `ssh gem@apps 'grep MemAvailable /proc/meminfo'` during any build; alert user if < 1.5GB.**
- Retry build job `8764f2ebb24f`: od-build re-ran (~RAM peak fine, 3.2GB floor), nix-builder layers CACHED from pre-crash run, export done. exit 0, 324s. Image `ghcr.io/coderyoss/codery:sandbox-dart-ca-fix`.
- deploy-preview job `87b19bf8682e`: blue container started, health check PASSED (entrypoint chain incl. new 55-dart-ca.sh ran clean — launchy up, port 3000 listening). Preview: https://sandbox-preview.rancidgrandmas.online. Note: logs show `nginx: [emerg] getgrnam("nogroup") failed` on apps reload — apps-side nix image issue, possibly pre-existing, NOT caused by this change. Port 8080 still listening.
- **Awaiting cutover.** Verify checklist = Task 10 below.
