---
type: decision
status: decided
date: 2026-05-07
tags:
  - design
  - decision
  - home
  - pencil-override
---

# Home page: rebuild from v1 concept, override Pencil-locked layout

## Context

Two designs exist for the dashboard root (`/`):

- **`design/concepts/home/v1.html`**: 4-cell StatRow at the top, page header with relative-time subtitle, two-column body (ActivityCard 3fr, ActiveDeploysCard + HealthSnapshotCard stacked 2fr).
- **`design/app.pen`** (Pencil-locked, frame `k3bOyE`): per-fleet FleetGroupCard list, NeedsAttentionRow strip, full-width EventTimeline, 340px Inspector rail. Strictly less information per square inch.

The v1.html surface shipped briefly via PR #31 (`e6ca0a2` "Concept fidelity: home page rebuild"). It was then replaced by the Pencil-locked simpler version in `27ef61d` "Home rebuild: match k3bOyE locked Pencil design", because the project rule is "Pencil is canonical when both .pen and concept HTML exist" (see project memory `feedback_pencil_is_canonical.md`).

After living with the simpler surface for ~36h, the operator's feedback: too bare. The Pencil layout works for a healthy homelab (fleet cards as the answer to "which boxes are up?"), but doesn't surface deploy progress, pending approvals, or service-level health: the questions that drive a daily check.

## Decision

Rebuild `/` from `design/concepts/home/v1.html`. Drop FleetGroupCard and NeedsAttentionRow (those move conceptually to the Hosts page, where fleet roll-ups make more sense). Keep the Inspector rail because event detail is still load-bearing for activity drill-downs.

This **explicitly overrides** the "Pencil is canonical" rule for this one page. The rule still holds elsewhere; v1.html is canonical for `/` until the next .pen edit reconciles.

## Specific divergences from app.pen

- StatRow (4 cells: HOSTS / STACKS / APPROVALS / DEPLOYS) replaces the FleetGroupCard list as the primary scan target.
- ActivityCard (12-event card) replaces the full-bleed EventTimeline.
- ActiveDeploysCard and HealthSnapshotCard fill the right column; previously empty real estate.
- Page header gains a relative-time subtitle ("last updated …").
- NeedsAttentionRow is dropped (its signal is absorbed by the APPROVALS cell + degraded count in HealthSnapshotCard).
- Inspector rail stays at 340px on the far right, beside the new right column cards. Not in v1.html; kept because event drill-down is still useful.

## Reconciliation plan

- Next time the .pen file is opened for any reason, edit the Home frame to match v1.html. Until then, treat v1.html as canonical for `/`.
- The Pencil-canonical rule continues to apply to every other page; this is a one-page exception, not a policy rollback.
- If the operator decides to revert to the simpler surface later, `27ef61d` is the reference commit (FleetGroupCard + NeedsAttentionRow code is preserved in git history).

## Consequences

- Home re-ships closer to "operator dashboard" instead of "fleet inventory".
- One outstanding source-of-truth conflict between `design/app.pen` (Home frame) and `design/concepts/home/v1.html`. Documented here so it doesn't surprise a future Claude reading `feedback_pencil_is_canonical.md` and silently rebuilding the simpler version a third time.

## Reference

- Reverted-then-restored commit: `e6ca0a2` (PR #31).
- Pencil-locked rebuild that this ADR overrides: `27ef61d` (PR #32, since-merged).
- This change ships in branch `chore/polish-home`.
