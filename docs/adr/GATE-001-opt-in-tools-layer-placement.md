---
id: GATE-001
title: "GitAuthGate is an opt-in tools-layer service"
domain: gate
rules: false
---

# GATE-001: GitAuthGate is an opt-in tools-layer service

## Context

Codery separates core (dockhand, zoraxy, tailscale), tools (paseo, apps,
playwright, opendesign, dbgate), and projects. Tools are opt-in: each has a
compose `profile`, and users enable it by adding the profile name to
`COMPOSE_PROFILES` in the stack `.env`. Users may not want a GitHub gate;
absent tools must simply not exist and degrade nothing.

The gate exists because managing a GitHub App's credentials and per-org
permissions is clunky and the operator wants every agent action behind a
gate they control. It serves agent instances; it is developer-facing, not
infrastructure.

## Decision

GitAuthGate follows the OpenDesign placement model exactly:

- One service `gitauthgate` in `stacks/tools/devtools/compose.yml` with
  `profiles: [gag]`.
- Image built on-host from `images/gate/` (`docker build -t
  codery-gate:<date> images/gate`), selected via `GATE_IMAGE` in the stack
  `.env`.
- Attached to `codery-net` with container name/alias `gitauthgate`.
- Secrets come from `/opt/codery/.env` via the compose `env_file`; per-user
  state lives in a bind mount at `/opt/codery/data/gate`.
- Loopback port publish only (`127.0.0.1:<port>`), like opendesign.
- A Zoraxy proxy-host route `gag.<domain>` → `gitauthgate:<port>` is
  **required**, not optional: GATE-004's one-time token-intake form must be
  reachable over HTTPS by the user's browser. The user applies the route in
  the Zoraxy UI; agents request it, never create it.

**Agent env wiring** (builder check settled): compose profiles cannot
conditionally add environment to a *different* service, so the paseo
service carries gate-related vars as empty-default interpolations — e.g.
`GH_HOST: ${GATE_AGENT_HOST:-}`, `GITHUB_ENTERPRISE_TOKEN:
${GATE_AGENT_TOKEN:-}` — which the stack `.env` sets only when the `gag`
profile is enabled. `GIT_TERMINAL_PROMPT=0` is set unconditionally
(harmless without the gate). Whether `gh` treats an *empty* `GH_HOST` as
unset is verified in the M1 spike; if not, the fallback is a commented
env block flipped during opt-in (documented, zero image changes). P0
wires **paseo only**; agents in the apps container are wired manually per
the setup guide. Deployment verification (spike deliverables): containers
must actually reach `gag.{domain}` (DNS + TLS from codery-net), and the
Zoraxy route's timeout must exceed the approval-hold timeout (GATE-005) —
else the hold timeout is lowered below Zoraxy's limit. Fallback when no
public URL exists: agents use the internal alias (`http://gitauthgate:<port>`,
codery-net only), setup falls back to paste-in-Telegram, and wiring
instructions are delivered over Telegram instead of `GET /`.

## Do's and Don'ts

- Do add the service and its env keys to the canonical mirror in this repo
  and to the deployed copy in `/opt/codery/stacks/` together.
- Do degrade gracefully: with the `gag` profile off, no paseo env var may
  point at a dead gate (see GATE-002/GATE-007 wiring notes).
- Don't publish the gate on a public host port or route it through Zoraxy
  to the public internet; the tailnet + codery-net boundary is assumed by
  GATE-006's threat model.
- Don't store secrets in the repo; the secrets contract lives in
  `host/templates/env.example`.

## Consequences

- Positive: one-profile opt-in; removing it leaves zero residue; multiple
  instances (one per user) are possible by stacking profiles in private
  deployments.
- Trade-off: the Zoraxy route is a manual step outside the agent's reach —
  accepted, it happens once per instance.
- Risk: users forget the route exists and the setup form 502s. Mitigation:
  the setup walkthrough (GATE-011) detects an unreachable public URL and
  says so in plain words.

## Compliance and Enforcement

Enforced by review: the compose diff must show `profiles: [gag]`, no
`0.0.0.0` port publishes, and env keys matching GATE-009.

## References

- GATE-002, GATE-004, GATE-009, GATE-011
- `stacks/tools/devtools/compose.yml` (opendesign service as the model)
