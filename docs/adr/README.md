# Architecture Decision Records — GitAuthGate (`gag`)

ADRs for **GitAuthGate**, the Codery tools-layer gate that sits between coding
agents and GitHub. The agent never holds a GitHub credential; every request
flows through the gate, which classifies the action, enforces policy, manages
the machine account's access lifecycle, and injects the held credential.

## Conventions

- Domain: `gate`. IDs: `GATE-NNN`.
- All ADRs ship `rules: false`: this project is Rust, and Archgate's
  automated rule checks are TS/AST-based. Enforcement is agent-read-context
  plus human review; text-based rules may be added later.
- Product shorthand: the gate service is **the gate**; the coding agent is
  **the agent**; the dedicated GitHub account the agent acts as is the
  **machine account**; the human operator is **the user**.

## Index

| ID | Decision |
|---|---|
| GATE-001 | Opt-in tools-layer placement |
| GATE-002 | Remote model: canonical `origin` + `gag` remote |
| GATE-003 | Agent→gate authentication: bearer tokens |
| GATE-004 | Machine account credential custody |
| GATE-005 | Push classification and fail-closed policy |
| GATE-006 | Telegram as the P0 control channel |
| GATE-007 | GitHub API proxy surface |
| GATE-008 | Platform trait; GitHub adapter only in P0 |
| GATE-009 | Build, image, and configuration |
| GATE-010 | Access registry, granting, and elevation |
| GATE-011 | Guided Telegram setup |
