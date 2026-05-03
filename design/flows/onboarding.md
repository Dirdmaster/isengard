---
type: design
kind: flow
status: stable
created: 2026-05-03
updated: 2026-05-03
tags:
  - design
  - flow
  - onboarding
  - welcome
  - wizard
---

# Onboarding flow

The first-run experience. Goal: a brand-new install reaches a healthy `Hosts` page with at least one connected host in under five minutes, without reading docs.

Success criteria:

- User runs the controller via `docker run` and lands on a wizard, not a blank dashboard.
- User exits the wizard with one host successfully heartbeating in.
- The wizard never lies about state — if the host is silent, it says so plainly.
- The wizard is skippable at any step (for users importing existing hosts via API).

## Entry conditions

The wizard fires on first load when **all** of the following hold:

- `0` hosts in `hosts` table
- `0` rows in `wizard_state` table (or `wizard_state.completed_at IS NULL`)
- The dashboard URL is reached at root (`/`) — direct deep links to other routes bypass the wizard

Subsequent visits skip the wizard. A user who deletes their last host does NOT re-trigger the wizard — they see the empty state on `/hosts` instead, with a link to "re-run setup wizard" if they want it back.

## Step map

| Step | Concept | Purpose | Exit condition |
|---|---|---|---|
| 1 — Welcome | `welcome/1-welcome-v1.html` | Explain what Isengard is in 3 sentences. Set expectation: this takes one host with Docker, ~2 minutes. | Click "Get started" |
| 2 — Add host | `welcome/2-add-host-v1.html` | Name a fleet, copy the `docker run` install command, run it on the target host. | Token displayed, command copied |
| 3 — Listening | `welcome/3-listening-v1.html` | Live polling state. Shows "Waiting for first heartbeat from this token..." with what to check if nothing happens (firewall, network, docker logs). | First heartbeat arrives |
| 4 — Connected | `welcome/4-connected-v1.html` | Confirmation: host is live, version detected, container count, what the user can do next. Earned moment with a check disc. | Click "Go to Hosts" |

After step 4 the user lands on `/hosts` with their newly-enrolled host highlighted briefly (subtle pulse on the row, fades after 2s).

## State machine

The wizard is driven by a tiny `wizard_state` row + the `hosts` table:

```
                  ┌──────────────┐
                  │   step:1     │   user lands on /
                  │   welcome    │
                  └──────┬───────┘
                         │ click "Get started"
                         ▼
                  ┌──────────────┐
                  │   step:2     │   token generated, fleet input,
                  │   add-host   │   docker run command shown
                  └──────┬───────┘
                         │ click "I've run the command"
                         ▼
                  ┌──────────────┐
                  │   step:3     │   poll /api/v1/hosts every 2s
                  │   listening  │   for any host with this token
                  └──────┬───────┘
                         │ heartbeat received
                         ▼
                  ┌──────────────┐
                  │   step:4     │   show host details
                  │   connected  │   wizard_state.completed_at = now
                  └──────┬───────┘
                         │ click "Go to Hosts"
                         ▼
                       /hosts
```

The wizard never auto-advances on a back-button — once the user steps back from `connected` to `listening`, the listener resumes polling but the heartbeat row is still there (user just sees connected immediately again). The transitions are user-driven, the underlying state is server-driven.

## Token lifecycle

The wizard generates a single-use enrollment token on entry to step 2:

- 32 bytes random, base32-encoded (display-friendly, no ambiguous chars)
- Stored in `enrollment_tokens(token, fleet, expires_at, used_at)`
- Expires in 60 minutes if unused
- Marked `used_at` on first heartbeat — subsequent heartbeats with same token are rejected
- After step 4 completes the token is deleted

If the user backtracks from step 3 to step 2, the same token is re-shown (not regenerated) so the docker command they may have already run still validates. If the user backs all the way out to step 1 and re-enters, a fresh token is generated and the old one revoked.

## Failure modes (step 3)

The "listening" step is where reality intrudes. The UI must give the user enough signal to debug without reading docs:

| Symptom | What the UI says | Action surfaced |
|---|---|---|
| No heartbeat after 30s | "Still waiting. Most installs take 5–10 seconds." | (no action — keep polling) |
| No heartbeat after 2 min | Yellow banner: "Nothing yet. Common causes:" + 3-bullet list | "Check docker logs" copyable command, "Verify outbound to controller" command |
| Token expired (60 min) | Red banner: "Token expired. Generate a new one." | "← Back" to regenerate |
| Heartbeat from different fleet | Yellow banner: "Got a heartbeat — but on fleet X, not Y. Did you set ISENGARD_FLEET correctly?" | "Use this anyway" / "Edit fleet" |
| Multiple heartbeats with same token | Take the first one, ignore subsequent. (User likely re-ran the command.) | Silent |

The "Listening" concept's bullseye glyph telegraphs "actively listening, not stuck" without lying about progress. The 30s/2min thresholds are visible to the user via `step3.listenedSince` so they can self-diagnose timing.

## Skip paths

- **API-first users** can `POST /api/v1/wizard/skip` (also accessible via a small "skip wizard" link in step 1 footer). This sets `wizard_state.completed_at = now` with `skip_reason: "user_initiated"` and routes to `/hosts` empty state.
- **Tooling users** importing many hosts via Terraform/Ansible never see the wizard if they hit the API before opening the UI — by the time the UI loads, hosts > 0 so the wizard doesn't fire.

## Telemetry to capture (controller-local, not external)

- Time spent on each step
- Whether the user back-navigated
- Token TTL at heartbeat (how long it took)
- Skip reason if any

This is for the operator to debug their own onboarding pain, not for us to phone home. Surfaced in `events` with kind `onboarding.step_completed`.

## Open questions

- ❓ Optional step 5: "Add your first stack"? — defer per [[Stack Deployment]] note. The loop is closed even without it; deferring keeps the wizard short.
- ❓ Multi-host enrollment in one wizard pass? — no, one host per wizard run. Subsequent hosts use the `+ Add host` button on `/hosts`, which reuses welcome/2 + welcome/3 in modal form.
- ❓ Auth setup as part of onboarding? — no, defaults to None mode (homelab-friendly). User configures auth later in Settings → Authentication when ready.
- ❓ Fleet rename after enrollment? — yes, but not in the wizard. Edit on the host row in `/hosts`.

## Related

- Concepts: `concepts/welcome/1-welcome-v1.html`, `concepts/welcome/2-add-host-v1.html`, `concepts/welcome/3-listening-v1.html`, `concepts/welcome/4-connected-v1.html`
- Page specs: [[hosts]], [[welcome]]
- Source design: [[Context]] (controller-first install model)
- Adjacent flow: stacks-add wizard reuses the same step-indicator + footer pattern → `flows/stack-add.md` (TODO when needed)
