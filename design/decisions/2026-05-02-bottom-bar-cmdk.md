---
type: decision
status: accepted
date: 2026-05-02
tags:
  - design
  - decision
  - bottom-bar
  - keyboard
---

# BottomBar: always-on cmdK over status chips

## Context

The original BottomBar (h-32, text-xs) was "almost unusable" per the user — too small, too quiet, didn't earn its real estate. We sketched 4 alternative directions in Pencil:

A. **Activity ticker** — live event scrolls through, prev/next/pause
B. **Always-on ⌘K** — cmd input never closes, type to filter / jump / act
C. **Collapsible drawer** — tabs (approvals/events/deploys/errors), click to slide up
D. **Mission control** — sparklines for events, agents, deploys, errors

The product is a developer tool aimed at keyboard-first operators. The BottomBar gets ~120px of vertical screen real estate; whatever lives there should *earn* it on every page.

## Options considered

### A — Activity ticker
**Pros:** "system has a heartbeat" feel; latest event always visible.
**Cons:** Pull-rather-than-push for action; user must wait for the right event; no way to *do* anything from the bar.

### B — Always-on ⌘K
**Pros:** Dominant input invites typing; the bar IS the action surface; aligns with keyboard-first product positioning. Direct path to every command without modal pop-up.
**Cons:** Persistent input might feel busy on every page; bar height has to accommodate text input.

### C — Collapsible drawer
**Pros:** "Pending approvals" becomes actionable inline; deepest information density when needed.
**Cons:** Too modal — user has to expand to act. Hides the bar's value behind a click. Easy to ignore when collapsed.

### D — Mission control sparklines
**Pros:** Operators love metrics; concrete and impressive.
**Cons:** Tells you "stuff is happening" but not *what to do next*. Pretty without being useful.

## Decision

We chose **Option B — always-on ⌘K** because:
1. **Aligns with product positioning.** Isengard is for keyboard-first operators ("hates clicking around" per the [[Positioning]] doc). The bar should reflect that culture.
2. **Bar becomes the primary action surface.** Tap ⌘K once and you're in flight. No modal, no context switch.
3. **Verb-first parsing matches mental model.** "deploy web-app to prod" reads as a sentence; the bar parses verb + tokens; ↵ runs.
4. **Status doesn't disappear.** Compact "● 4 / 4 hosts" on the left preserves the live signal. Pending approvals can show as an amber border on the focus hint (when count > 0).

## Consequences

- BottomBar height: 56px (up from 32px). Tradeoff: less main content area, but the bar earns it.
- Three zones: `[live + N/N hosts]` `[cmd input — fill]` `[↵ run, esc clear]`.
- Cmd parsing grammar TBD (separate ADR). Initial verbs: `deploy`, `restart`, `rollback`, `go`, `show`, `filter`, `approve`, `drain`.
- All other pages inherit this BottomBar via the shared component. No per-page bottom bars.
- The chip cluster idea (V1, "1 deploying / 3 pending approvals / 4/4 hosts up") is parked. We can layer status chips back in later as the cmd's *context* (hint area when not focused) but the input stays primary.

## See also

- Concepts: `concepts/2026-05-02-hosts-v1.html` (shows the bar in typing state)
- Future ADRs: `2026-05-XX-cmd-grammar.md` (what verbs and parameters mean)
- Implementation: `crates/isengard-plugins/dashboard/web/components/BottomBar.vue` (planned)
