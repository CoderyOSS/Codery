---
id: GATE-009
title: "Build, image, and configuration"
domain: gate
rules: false
---

# GATE-009: Build, image, and configuration

## Context

Codery builds tool images on-host (no registry CI): `docker build -t
codery-gate:<date> images/gate`, then point `GATE_IMAGE` at the tag and
Deploy. The gate needs no git binary — it streams git smart-HTTP bytes,
parses pkt-lines, and calls upstream HTTP APIs.

## Decision

- **Language:** Rust (project preference order; confirmed with the user).
  Single binary: axum (HTTP server), reqwest (upstream client), tokio
  (tasks: Telegram long-poll, registry re-checks, expiry reminders, daily
  token verification), rusqlite (state), serde/serde_json.
- **Image:** small static build (musl or distroless-style); no git, no
  shell dependencies at runtime.
- **Stack `.env` keys** (all documented in `host/templates/env.example`):
  - `GATE_IMAGE` — image tag
  - `GATE_PUBLIC_URL` — e.g. `https://gag.{domain}` (one-time form links,
    instruction page content)
  - `GATE_TOKEN_ENC_KEY` — AES-256-GCM key for token-at-rest encryption
    (never co-located with ciphertext; GATE-004)
  - `TELEGRAM_BOT_TOKEN`, `TELEGRAM_ALLOWLIST` — control channel
    (GATE-006)
  - `DOMAIN_NAME` — already in the stack env
- **State layout** (bind mount `/opt/codery/data/gate`): SQLite DB
  (registry, flows, setup progress, pending approvals, kill-switch state,
  hashed agent tokens) + append-only audit JSONL. Both restart-resilient;
  setup and elevation flows resume from durable records (GATE-011/GATE-010).
- **Policy config:** bind-mounted file (e.g. `/opt/codery/data/gate/
  policy.toml`), re-read on mtime change; effective policy visible via
  `/status` (GATE-005).
- **Testing:** integration tests run a real `git` client against the gate
  with a stub upstream (bare repo + wiremock); the approval backend sits
  behind a trait so tests use an auto-approve/auto-deny stub; Telegram is
  never contacted in CI.

## Do's and Don'ts

- Do keep the service single-binary and stateless except for the bind
  mount — recreating the container must lose nothing.
- Do derive all URLs from `GATE_PUBLIC_URL`/`DOMAIN_NAME`; no hardcoded
  hostnames.
- Don't publish host ports beyond loopback (GATE-001).
- Don't add a web UI, metrics stack, or admin panel in P0.

## Consequences

- Positive: cheap on-host builds; the whole system is one binary + SQLite
  + JSONL + two env secrets; recreates are safe.
- Trade-off: SQLite over Postgres — right-sized for one instance, one
  user; a multi-tenant future would revisit (unlikely for Codery tools).
- Risk: bind-mount permission drift. Mitigation: container runs with a
  fixed UID/GID documented in the compose service comments, matching
  other tools' data-dir conventions.

## Compliance and Enforcement

Review-verified: image builds via the documented command; no runtime
dependencies beyond the static binary; `.env` keys match this list exactly.

## References

- GATE-001 (placement), GATE-004 (encryption key discipline), GATE-005
  (policy file), GATE-006 (Telegram), GATE-010/GATE-011 (durable flows)
