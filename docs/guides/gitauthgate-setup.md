# GitAuthGate — Setup and Deployment

## What it is

GitAuthGate is an opt-in tools-layer service that puts every agent GitHub
action behind a gate you control: a machine account's token is held in
encrypted custody and agents reach GitHub only through the gate's git and
API proxies, with human approval (over Telegram) for dangerous operations
(force pushes, branch/tag deletions, held API mutations). Design and
decisions live in the ADRs — start at `docs/adr/GATE-001` (placement and
agent wiring), then `GATE-009` (build/config), `GATE-004` (token custody),
`GATE-005` (policy), `GATE-006` (Telegram control channel), `GATE-011`
(first-run setup).

The service is defined in `stacks/tools/devtools/compose.yml` with
`profiles: [gag]` — off by default, and when off, zero residue: no
container, no route, and paseo's gate env vars interpolate to empty.

## Deploy

1. **Build the image on the host** (on-host builds, no registry CI — same
   model as the other tool images):

   ```
   docker build -t codery-gate:$(date +%Y%m%d) /opt/codery/projects/Codery/images/gate
   ```

2. **Stage secrets in `/opt/codery/.env`** (the core+tools secrets file;
   see `host/templates/env.example`):

   | Key | Value |
   |---|---|
   | `GATE_TOKEN_ENC_KEY` | `openssl rand -hex 32` — AES key for token-at-rest encryption. Keep it here, never next to the ciphertext (`/opt/codery/data/gate/`). Changing it later fails closed — re-run setup. |
   | `GATE_MACHINE_LOGIN` | machine account login (you pick it during setup; must match the account the gate verifies tokens against) |
   | `TELEGRAM_BOT_TOKEN` | token from [@BotFather](https://t.me/BotFather) — the control channel for approvals and setup |
   | `TELEGRAM_ALLOWLIST` | Telegram chat/user IDs allowed to talk to the bot |
   | `TELEGRAM_CHAT_ID` | numeric chat id approval asks are delivered to (see below) |

   How to obtain the Telegram values — see **Telegram control channel
   wiring** below.

3. **Stage routing + image keys in the stack `.env`**
   (`/opt/codery/stacks/devtools/.env` — compose interpolation reads this
   file, the same place `PASEO_IMAGE`/`OPENDESIGN_IMAGE` live):

   | Key | Value |
   |---|---|
   | `COMPOSE_PROFILES` | add `gag` to the existing list |
   | `GATE_IMAGE` | the tag from step 1, e.g. `codery-gate:20261117` |
   | `GATE_AGENT_HOST` | `gag.<domain>` (leave unset until step 5 mints the token) |
   | `GATE_AGENT_TOKEN` | set after first-run setup (step 5) |

   `GATE_PUBLIC_URL` needs no staging — the compose service derives it
   from `DOMAIN_NAME` (`https://gag.$DOMAIN_NAME`).

4. **Create the policy file** (the gate refuses to start without it):

   ```
   mkdir -p /opt/codery/data/gate
   cat > /opt/codery/data/gate/policy.toml <<'EOF'
   [defaults]
   push = "allow"
   force-push = "approve"
   branch-delete = "approve"
   tag-delete = "approve"
   EOF
   ```

   Tune per repo later (`[repos."owner/name"]` overrides) — edits are
   picked up on mtime change; the effective policy is visible at
   `/status` (GATE-005).

5. **Deploy via Dockhand** (repo copy and `/opt/codery/stacks/` copy of
   `compose.yml` updated together).

## Telegram control channel wiring

The control plane (first-run setup, approvals, grants, elevation,
reminders) talks over the Telegram Bot API. Three keys in
`/opt/codery/.env`, staged with the other gate secrets, then **Deploy** —
the gate service picks them up via `env_file`:

| Key | How to obtain |
|---|---|
| `TELEGRAM_BOT_TOKEN` | Message [@BotFather](https://t.me/BotFather) → `/newbot` → copy the token it issues (`123456:ABC-…`). |
| `TELEGRAM_ALLOWLIST` | Comma-separated Telegram **user IDs** allowed to talk to the bot — the operator(s). Find yours: message [@userinfobot](https://t.me/userinfobot); it replies with your `Id`. |
| `TELEGRAM_CHAT_ID` | The numeric chat id **asks are delivered to** (approval prompts, elevation consents, expiry reminders). Usually your own user id — the same number as your allowlist entry. Negative numbers are group chats. |

Missing-key behavior is fail closed, by design:

- **No bot token** → the control-plane poll loop never spawns; git/API
  proxying still works, but there is no setup, no approvals (held
  mutations are denied), no grants/elevation.
- **Empty allowlist** → every update is dropped.
- **Unset/invalid `TELEGRAM_CHAT_ID`** → the bot runs, but no ask can be
  delivered: approval present errors and held mutations are denied.

## Zoraxy route (operator applies)

Request a proxy host in the Zoraxy UI (agents request, never edit):

- **Host:** `gag.<domain>` → **target:** `gitauthgate:8080` (codery-net
  container name; no host port involved — the `127.0.0.1:23080` publish
  is for host-side curl checks only).
- **Timeout must exceed 300 s.** Approval holds block the request for up
  to 300 s (`APPROVAL_TIMEOUT_SECS`, GATE-005); a shorter Zoraxy timeout
  turns every held mutation into a gateway error before the human can
  answer.

**No public URL?** Fallback mode (GATE-001): leave `GATE_PUBLIC_URL`
empty (remove it from the compose `environment` block) — agents use the
internal alias `http://gitauthgate:8080` (codery-net only), the token
intake falls back to paste-in-Telegram, and the agent wiring instructions
are delivered over Telegram instead of `GET /`.

## Host-side verification checklist

Run these on the VPS host after staging `.env` keys:

```bash
cd /opt/codery/stacks/devtools

# renders WITH the profile (gag listed in COMPOSE_PROFILES) — gitauthgate present
docker compose config

# renders WITHOUT the profile (remove gag) — service absent, no errors
docker compose config
```

Both must succeed; the `:?` guards (`GATE_IMAGE`, `DOMAIN_NAME`) fire on
missing keys, which is the intended failure mode.

```bash
# image build succeeds (fresh checkout path on the host)
docker build -t codery-gate:check /opt/codery/projects/Codery/images/gate

# DNS + TLS from inside codery-net reach the gate (after the Zoraxy route exists):
docker run --rm --network codery-net curlimages/curl -sI https://gag.<domain>/
```

**Empty-GH_HOST check** (GATE-001 spike deliverable) — inside the paseo
container:

```bash
GH_HOST='' gh auth status
```

`gh` must treat an empty `GH_HOST` as unset and **not** attempt the gate.
If it does try `gag.<domain>`, flip to the commented-env-block fallback
documented in GATE-001: move the `GH_HOST`/`GITHUB_ENTERPRISE_TOKEN`
lines out of the paseo service's `environment` into comments, and uncom-
ment them during opt-in. Note the outcome here when you do.

## First-run setup

With the gate deployed, the Zoraxy route live, and the Telegram keys
staged (see **Telegram control channel wiring** above):

1. Message your Telegram bot (from an allowlisted chat) and start the
   guided walkthrough (GATE-011) — eight steps, one message each: machine
   account email alias, sign-up (private window), profile, private-email
   settings, 2FA, classic PAT creation, token intake via the one-time
   HTTPS form (`{GATE_PUBLIC_URL}/setup/token?nonce=…`), repo access
   grants. The bot verifies what it can via the API and never touches
   your browser.
2. Mint an agent token: `/token mint paseo` (GATE-003).
3. Put the minted value into the stack `.env` as `GATE_AGENT_TOKEN` (and
   confirm `GATE_AGENT_HOST=gag.<domain>`), then Deploy again — paseo
   picks up its gate wiring on recreate.

## Ruleset backstop (do this per protected branch)

The gate classifies pushes at the proxy, but GitHub itself should make
the harmful operations impossible server-side. For **each protected
branch** (e.g. `main` in every repo the agent works on):

1. Repo → **Settings** → **Rules** → **Rulesets** → **New branch
   ruleset**.
2. **Ruleset name:** e.g. `protect-main`; **Enforcement status:**
   **Active**.
3. **Add target** → **Include by pattern** → `~DEFAULT_BRANCH` (or the
   branch name).
4. **Add rules** → enable **Restrict deletions** and **Block force
   pushes**.
5. **Bypass list:** leave the machine account (and everyone else) **out**.
   A bypass entry would let the exact actor the gate exists to constrain
   skip the protection.

The post-setup Telegram message reminds you of this (GATE-011 step 8);
this section is the full walkthrough it points to.

## Agent wiring recap

When the `gag` profile is enabled, the paseo service carries:

| Env var | Effect |
|---|---|
| `GH_HOST=gag.<domain>` | `gh` and git-in-`gh` target the gate instead of github.com |
| `GITHUB_ENTERPRISE_TOKEN=<minted>` | bearer token the gate authenticates agents with (GATE-003) |
| `GIT_TERMINAL_PROMPT=0` | git never hangs on a credential prompt — set unconditionally |

Both gate vars interpolate empty when `GATE_AGENT_HOST`/`GATE_AGENT_TOKEN`
are unset, so a disabled gate degrades nothing (GATE-001). Agents in the
apps container are wired manually per instance.

`GET /` on the gate renders the agent instruction page: copy-paste clone
URLs (public + private repos), commit identity, API env — with the live
gate host, and the minted token when you open it with `?token=…`.

## Acceptance QA checklist (manual, post-deploy)

Run once against a **scratch GitHub account + repo** — never production
repos. This checklist extends, not replaces, the host-side checks above:
compose config with and without the `gag` profile, the image build, the
codery-net DNS/TLS probe, and the empty-`GH_HOST` check all still apply
(run those first).

1. **First-run setup, Telegram only.** From the allowlisted chat, run
   `/setup` and complete the whole walkthrough (machine-account email
   alias, sign-up in a private window, profile, private-email setting,
   2FA, classic PAT, token intake form, repo grants).
   **Expected:** the bot verifies each step via the API, the token lands
   in custody, the repo grant lands, and a working agent exists in one
   repo — no page visited that the walkthrough didn't link.
2. **Wrong-account token rejected by name.** Paste a PAT from your main
   account into the intake form.
   **Expected:** rejected naming the login — "wrong account — the token
   belongs to `<main>`, not the machine account `<machine>`" — and
   custody unchanged.
3. **Force-push hold → approve → forwarded.** With `force-push =
   "approve"` (the default policy), `git push --force` from the agent.
   **Expected:** the push blocks for up to 300 s while an ask arrives in
   `TELEGRAM_CHAT_ID`; **Approve** → the push forwards upstream and
   succeeds. Deny/timeout → rejected. Either outcome is audited.
4. **`/block` kill switch.** `/block <owner>/<repo>`, then any git/API
   request against that repo.
   **Expected:** GitHub-shaped 429 immediately — no ask, no upstream
   forwarding. `/unblock <owner>/<repo>` restores service.
5. **Role-gap action ⇒ permission_pending with elevation.** Attempt an
   action the agent's role lacks (e.g. an admin-only mutation from a
   `write` agent).
   **Expected:** fail-fast pre-flight errors `permission_pending`
   before anything is forwarded; ONE combined consent message arrives;
   approve → temp grant, retry succeeds; deny → denied.
6. **Role change detected without user confirmation.** Change the
   agent's collaborator role upstream (or remove it) and wait one sweep
   (≤ 30 min).
   **Expected:** the registry sweep reconciles the role map on its own —
   no prompt — and elevated grants are walked back per policy; the
   change lands in the audit log.
7. **"Grant until done, then remove."** `/grant <owner>/<repo>
   maintain` → do the elevated thing → `/revoke <owner>/<repo>`.
   **Expected:** the grant lands (invitation auto-accepted), the
   elevated action works while held, and revoke walks the agent back to
   its base role (e.g. `write`).
8. **Expiry reminders + `/rotate` with no failed requests.** With a
   token expiring inside the reminder window, confirm the 14/3/1-day
   reminders each fire exactly once with a `[Start /rotate]` button;
   then `/rotate` (or the button).
   **Expected:** the intake mints the form, custody swaps to the new
   token once verified, and agents keep working with zero failed
   requests during and after the swap.
9. **Log hygiene.**
   `grep -rE "ghp_|Authorization" /opt/codery/data/gate/audit.jsonl`
   **Expected:** only redactions — no full token strings, no raw
   Authorization values.

## Known operational notes

Watch items from the build ledger — not bugs, but know them before you
operate:

- **Shared rate-limit bucket behind Zoraxy.** The auth-failure limiter
  keys on source IP, and behind Zoraxy every agent arrives from one
  source — one agent's repeated auth failures can 429 *all* agents for
  60 s. Chase repeated 401s quickly.
- **Approval holds need a Zoraxy timeout ≥ 300 s.** Held mutations
  block the HTTP request until answered (up to 300 s); a shorter router
  timeout turns every held mutation into a gateway error before the
  human can answer (see the route section above).
- **Startup verify runs before bind.** The gate verifies custody before
  the listener binds — no request is ever served while the credential
  state is unverified. A bad `GATE_TOKEN_ENC_KEY` or unreadable state
  fails boot rather than serving half-configured.
- **Git-read deny audit asymmetry.** Allowed reads and all mutations
  are audited; reads that fail *locally* (forwarding flag off, no
  custody credential, malformed path) return before the audit call and
  are **not** in `audit.jsonl`. Upstream-unreachable read failures are
  audited.
- **`policy.toml` unknown keys abort boot.** The policy parser rejects
  unknown keys/sections (`deny_unknown_fields`) — a typo stops startup
  instead of being ignored. Grep the live `/opt/codery/data/gate/
  policy.toml` before every image upgrade: keys an older build tolerated
  may be fatal now.
