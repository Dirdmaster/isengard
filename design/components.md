# Components

Inventory of reusable UI components in the Isengard dashboard. Update when adding, removing, or deprecating a component.

Source location: `crates/isengard-plugins/dashboard/web/components/`

## Chrome

### TopBar
**Source:** `components/TopBar.vue`
**Props:** `{ activeTab, fleetName }`
**Variants:** default
**Used by:** Hosts, Stacks, Stack detail, Settings, Approvals, Events
**Status:** stable

### BottomBar
**Source:** `components/BottomBar.vue`
**Props:** `{ liveStatus, hostsUp, totalHosts, pendingApprovals }`
**Variants:** idle, typing, action-confirm
**Used by:** all main pages
**Status:** experimental — settled on always-on cmdK design 2026-05-02

### PageShell
**Source:** `components/PageShell.vue` (planned)
**Props:** `{ title }`
**Slots:** default
**Used by:** all main pages — wraps TopBar + main + BottomBar
**Status:** not yet built

## Atomics

### KbdChip
**Source:** `components/KbdChip.vue`
**Props:** `{ keys }` (e.g. `["⌘", "K"]`)
**Used by:** BottomBar, Help overlay, search box
**Status:** stable

### StatusDot
**Source:** `components/StatusDot.vue`
**Props:** `{ status }` (success / warn / error / info / neutral)
**Used by:** host rows, stack rows, BottomBar, anywhere live state matters
**Status:** stable

### StatusChip
**Source:** `components/StatusChip.vue` (planned)
**Props:** `{ status, label, withIcon }`
**Variants:** success / warn / error / info, with optional icon
**Status:** in pencil mocks, not yet built

### Button
**Source:** `components/Button.vue`
**Props:** `{ variant, size, icon, disabled }`
**Variants:** primary, secondary, ghost, danger, icon-only
**Used by:** everywhere
**Status:** stable

## Composites

### ModalShell
**Source:** `components/ModalShell.vue` (planned)
**Props:** `{ title, size }` (size: sm 560px / lg 760px)
**Slots:** header, body, footer
**Status:** in pencil mocks, not yet built

### EmptyState
**Source:** `components/EmptyState.vue` (planned)
**Props:** `{ icon, title, body, ctaLabel }`
**Slots:** body, cta
**Status:** in pencil mocks, not yet built

### TableRow
**Source:** `components/TableRow.vue` (planned)
**Props:** `{ cells }`
**Status:** not yet built

### MetricCard
**Source:** `components/MetricCard.vue` (planned)
**Props:** `{ label, value, sparkline, trend }`
**Status:** in pencil mocks, not yet built

## Wizard

### WizardShell
**Source:** `components/WizardShell.vue`
**Props:** `{ currentStep, totalSteps }`
**Slots:** default
**Status:** stable

### WizardStep1Welcome / WizardStep2AddHost / WizardStep3Listening / WizardStep4Connected
**Source:** `components/WizardStep*.vue`
**Status:** stable, in production

## Conventions

- One Vue file per component
- Component name = filename = export default name
- Props typed via `defineProps<{...}>()`
- Tailwind iso-* classes only (no arbitrary hex values)
- Composables in `composables/`, stores in `stores/`
