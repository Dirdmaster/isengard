---
type: design
kind: page-spec
status: stable
created: 2026-05-03
updated: 2026-05-03
tags:
  - design
  - page
  - wizard
---

# Welcome (onboarding wizard)

Full-screen, no-chrome 4-step wizard. Replaces the "+ Add a host" empty-state CTA on first boot. The dashboard auto-redirects to `/welcome` when 0 hosts AND no skip/complete flag.

Source design: [[Onboarding Wizard]].

## Route

`/welcome` (with `?step=N` for refresh recovery and `?fresh=1` to reset state)

## Audience

Brand-new user who just `docker run`'d the controller, opened the dashboard, and is staring at an empty fleet. They need a 60-second path from "page loaded" to "first host enrolled."

## Steps

| Step | Title | Goal | Concept |
|---|---|---|---|
| 1 | Welcome | Brand intro + 3-feature framing + Get started | `wizard-1-welcome-v1.html` |
| 2 | Add your first host | Name fleet + render docker-run install command + wait for check-in | `wizard-2-add-host-v1.html` |
| 3 | Listening for your agent… | Status panel (token, fleet, elapsed, last check-in) + troubleshooting links + 60s/600s message stages | `wizard-3-listening-v1.html` |
| 4 | Connected! | Discovery summary (hostname, fleet, OS, stacks, services) + Open dashboard CTA | `wizard-4-connected-v1.html` |

## Key interactions

- **Step dots** — top of every card, 4 dots showing progress
- **Skip for now** — exits to /hosts (sets `wizard.skipped=true` in localStorage)
- **Continue** — advances when current step is satisfied
- **Add another host** (step 4) — restarts wizard at step 2 with `?fresh=1`
- **Open dashboard** (step 4) — primary CTA, exits to /home

## Layout

All 4 steps share:
- 1440x900 frame
- Centered card (~720px wide), `bg-iso-bg-elevated`, `rounded-iso-xl`, `border-iso-border`
- Step dot row at top
- Title (text-2xl or text-3xl semibold)
- Body content varies per step
- Action cluster at bottom (centered)

Only step 4 carries a hero icon (green check disc) — see `decisions/2026-05-03-wizard-decorative-icons.md` (TODO).

## States (per step)

- **Step 2** — empty fleet input → disabled Continue + greyed install command box; filled fleet → focused green border + populated command + Skip/Continue actions
- **Step 3** — 0-60s "Listening for your agent…"; 60s+ "Still waiting… we'll keep listening" + warm pulse; 600s+ "No check-in after 10 minutes. Run `docker logs isengard-agent` to see what happened." with troubleshoot CTA
- **Step 4** — discovery counts may be 0 if the agent hasn't run a scan yet; field shows `—` not 0

## Open questions

- ❓ Mobile layout? — out of scope, dashboard is desktop-first
- ❓ Pre-fill fleet name suggestion based on hostname? — could be nice; defer
- ❓ Skip-to-end button for re-onboarding? — `?step=4&fresh=1` URL trick exists, no UI

## Related

- Concepts: `concepts/2026-05-02-wizard-{1,2,3,4}-*-v1.html`
- Source: [[Onboarding Wizard]]
- Implementation: `crates/isengard-plugins/dashboard/web/pages/welcome.vue` (committed on `next`)
