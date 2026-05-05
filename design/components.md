---
type: design
kind: component-inventory
status: stable
created: 2026-05-03
updated: 2026-05-05
tags:
  - design
  - components
---

# Components

Actual inventory of what exists in `crates/isengard-plugins/dashboard/web/components/`. Promotion path: design concepts in `design/concepts/` use these by name; implementation Vue files match the props documented here.

If a component is listed here, it ships in the Vue tree. Aspirational components live in concept HTML, not in this list — promote them only after the Vue file lands.

## Layout chrome

- **AppShell** (`AppShell.vue`) — top-level wrapper that renders `<TopBar />` plus a default slot. No props. Used by `pages/*` route shells.
- **TopBar** (`TopBar.vue`) — h-14 global nav: brand cluster + `<FleetPicker />`, route-aware tabs (`Home / Hosts / Stacks / Events / Settings`), search button (opens cmd pane), settings icon. No props; reads `useUiStore` and `$route`.
- **BottomStatusBar** (`BottomStatusBar.vue`) — h-9 status strip. Props: `{ connectionState: 'connecting' | 'live' | 'reconnecting' | 'offline'; eventCount: number }`. Renders dot + label + key hints.
- **PageHeader** (`PageHeader.vue`) — title + optional subtitle row with `meta` and `actions` named slots. Props: `{ title: string; subtitle?: string }`.
- **WizardShell** (`WizardShell.vue`) — fixed-position chrome for `/welcome`. Props: `{ step: 1 | 2 | 3 | 4 }`. Renders brand mark, step counter, default slot.
- **WizardCard** (`WizardCard.vue`) — card chrome inside the wizard. Props: `{ width?: number = 580; contentGap?: number = 18; padding?: string = 'p-10' }`. Default + named `actions` slots.

## Tables and lists

- **HostsTable** (`HostsTable.vue`) — props: `{ hosts: Host[]; sparklines: Record<string, number[]>; stackCounts: Record<string, { stacks: number; services: number }>; latestEvents: Record<string, { kind: string; summary: string } | null>; selectedId: string | null }`. Emits `select(host)`, `action('force-update'|'shell'|'menu', host)`. Falls back to `<EmptyState>` when empty.
- **HostRow** (`HostRow.vue`) — single row inside HostsTable. Props: `{ host; sparkline: number[]; stackCount: number; serviceCount: number; latestEvent; lastSeenRelative: string; agentVersionWarn: boolean; selected? }`. Emits `click(host)`, `action(action, host)`.
- **StacksTable** (`StacksTable.vue`) — props: `{ rows: Array<{ stack, hostHostname, fleet, serviceCount, latestEvent }> }`. Routes on row click. Empty-state fallback explains compose label discovery.
- **StackRow** (`StackRow.vue`) — compact row used outside StacksTable (e.g. host-detail context). Props: `{ stack: Stack; services: { name: string; state? }[] }`. Emits `click(stack)`.
- **EventTimeline** (`EventTimeline.vue`) — full feed grouped by day. No props; reads `useEventsStore` + `useUiStore`. Renders `<DayLabel>` + `<EventRow>` groups.
- **EventRow** (`EventRow.vue`) — props: `{ event: EventType; selected: boolean }`. Emits `select`. Color-codes kind (success/error/warn/info).
- **DayLabel** (`DayLabel.vue`) — uppercase date separator. Props: `{ label: string }`.
- **EventFilterChip** (`EventFilterChip.vue`) — pill toggle for event kind filters. Props: `{ label: string; active?: boolean; count?: number }`. Emits `toggle`.
- **RoutingRulesTable** (`RoutingRulesTable.vue`) — table of routing rules with edit/delete actions. No props (uses `useRoutingRules` composable). Emits `add`, `edit(rule)`.
- **TableSkeleton** (`TableSkeleton.vue`) — shimmer placeholder. Props: `{ rows?: number = 6; columns?: number[] = [170, 70, 130, 80, 400, 90, 60] }`.

## Inspectors and modals

- **Inspector** (`Inspector.vue`) — right rail for the selected event on the events page. No props; reads `useUiStore` + `useEventsStore`. Renders kvrows + quick-action buttons.
- **HostInspector** (`HostInspector.vue`) — slide-over for a host (decommission, revoke cert, fleet change, force update). Props: `{ host: Host }`. Emits `close`, `changed`.
- **AddHostModal** (`AddHostModal.vue`) — Dialog with fleet input + install-command result. Emits `close`. No props.
- **MintTokenModal** (`MintTokenModal.vue`) — Dialog for minting an enrollment token; shows docker run snippet on success. Emits `close`, `minted`.
- **ServiceExposeModal** (`ServiceExposeModal.vue`) — thin wrapper around RoutingRuleEditModal pre-filling service + port. Props: `{ open: boolean; hostId: string; serviceName: string; containerPort: number }`. Emits `update:open`.
- **RoutingRuleEditModal** (`RoutingRuleEditModal.vue`) — Dialog form for create/edit of a routing rule. Props: `{ open: boolean; rule?: RoutingRule | null; defaultHostId?: string }`. Emits `update:open`.
- **ConfirmDialogShell** (`ConfirmDialogShell.vue`) — generic AlertDialog driven by `useConfirm()`. No props. Mounted once globally; pages call `confirm({ title, description, confirmText, danger })`.
- **HelpOverlay** (`HelpOverlay.vue`) — keyboard shortcut reference card. Props: `{ open: boolean }`. Emits `close`.

## Empty states and status

- **EmptyState** (`EmptyState.vue`) — circular-icon empty state. Props: `{ icon: string; title: string; description?: string }`. Slots: default (description override), `cta`. Used by HostsTable, StacksTable.
- **StatusPill** (`StatusPill.vue`) — colored pill. Props: `{ state: 'success' | 'warn' | 'error' | 'info' | 'neutral'; label: string; size?: 'xs' | 'sm' = 'sm'; icon?: string }`.
- **StateStrip** (`StateStrip.vue`) — per-fleet aggregate status block: header (failed/updating counts, host count, last activity) + up to 5 issue rows. Props: `{ fleet: Fleet }`.
- **Sparkline** (`Sparkline.vue`) — bar-chart SVG. Props: `{ data: number[]; color?: 'success'|'warn'|'error'|'info' = 'info'; width?: number = 130; height?: number = 24 }`.
- **ServiceChip** (`ServiceChip.vue`) — service-state pill with dot. Props: `{ name: string; state?: 'running'|'stopped'|'restarting'|'unknown' = 'unknown' }`.

## Cmd palette

- **CmdPane** (`CmdPane.vue`) — root ⌘K palette teleported to body. Two modes: `navigator` (search hosts/stacks/events/actions via Fuse.js) and `terminal` (xterm log streaming). No props; reads `useUiStore` + entity stores.
- **CmdInput** (`CmdInput.vue`) — large search input with `esc` kbd hint. Props: `{ modelValue: string }`. Emits `update:modelValue`, `keydown(KeyboardEvent)`.
- **CmdSection** (`CmdSection.vue`) — uppercase section label inside the navigator. Props: `{ label: string }`.
- **CmdResultRow** (`CmdResultRow.vue`) — single result row. Props: `{ icon: string; label: string; meta?: string; highlighted?: boolean }`. Emits `select`.
- **CmdBreadcrumb** (`CmdBreadcrumb.vue`) — header for terminal mode. Props: `{ segments: string[]; connected: boolean }`. Emits `toggle-position`, `close`.
- **CmdTerminal** (`CmdTerminal.vue`) — xterm-backed log viewer. Props: `{ serviceId: string; serviceName: string; hostHostname: string; fleet: string; stackName? }`. Emits `toggle-position`, `close`.

## Stack detail

- **StackHeader** (`StackHeader.vue`) — page header for `/stacks/[id]` with back link + force-update CTA. Props: `{ stack: Stack; hostHostname: string; fleet: string }`. Emits `force-update`.
- **DeploymentInProgressPanel** (`DeploymentInProgressPanel.vue`) — blue/green progress timeline. Props: `{ deployment: DeploymentDto }`. Inline abort button hits `POST /deployments/:id/abort`.
- **DeploymentAbortedPanel** (`DeploymentAbortedPanel.vue`) — failure/abort summary with retry. Props: `{ deployment: DeploymentDto }`. Emits `dismiss`.

## Settings tab content

These all wrap `<SettingsSection>` and live as direct children of `pages/settings/index.vue`.

- **FleetsSettings** (`FleetsSettings.vue`) — list/create/delete fleets via `useFleetsStore`.
- **EnrollmentSettings** (`EnrollmentSettings.vue`) — wizard re-entry, advanced AddHostModal, active token table with revoke (Phase 14).
- **NetworkingSettings** (`NetworkingSettings.vue`) — per-host adapter cards + RoutingRulesTable + edit modal (Phase 8).
- **NotifierSettings** (`NotifierSettings.vue`) — Telegram/Discord toggles (Phase 4).
- **DeploymentsSettings** (`DeploymentsSettings.vue`) — per-service deploy strategy override grouped by stack (Phase 10).
- **AdapterCardNone** (`AdapterCardNone.vue`) — static info card. No props.
- **AdapterCardTailscale** (`AdapterCardTailscale.vue`) — enabled/funnel toggles + save/test. Props: `{ hostId: string }`.
- **AdapterCardCfTunnel** (`AdapterCardCfTunnel.vue`) — Cloudflare credentials form + save/test. Props: `{ hostId: string }`.
- **SettingsSection** (`SettingsSection.vue`) — `<section>` wrapper with title, optional description, default slot, trailing `Separator`. Props: `{ title: string; description?: string }`.
- **settings/SettingsTabs** (`settings/SettingsTabs.vue`) — query-string-driven tab nav. Props: `{ tabs: { key, label }[]; defaultTab?: string }`. Slot receives `{ activeTab }`.

## Wizard steps

- **WizardStep1Welcome** (`WizardStep1Welcome.vue`) — feature-card hero. Emits `getStarted`, `skip`.
- **WizardStep2AddHost** (`WizardStep2AddHost.vue`) — fleet/hostname inputs + tokenized install-command snippet. Emits `back`, `next`. Reads `useWizardStore`.
- **WizardStep3Listening** (`WizardStep3Listening.vue`) — spinner with normal/slow/stuck stages keyed off `wizard.elapsedSeconds`. Emits `back`, `cancel`.
- **WizardStep4Connected** (`WizardStep4Connected.vue`) — success card + "what's next" actions. Emits `done`.

## Entry-point atoms

- **AddHostButton** (`AddHostButton.vue`) — outline `<Button>` that opens AddHostModal. No props. Used in HostsTable empty state.
- **FleetPicker** (`FleetPicker.vue`) — dropdown for active fleet (lives inside TopBar). No props; mutates `useUiStore.activeFleet`.
- **KvRow** (`KvRow.vue`) — label/value row used in Inspector. Props: `{ label: string; value: string; mono?: boolean; valueClass?: string }`.

## Inlined patterns (intentionally not extracted)

These show up across the tree but are kept as inline markup for now. Tracked here so a future "promote inline → component" pass can find them:

- **Status dot** — `<span class="w-2 h-2 rounded-full bg-iso-{state}">` — used in TopBar brand cluster, BottomStatusBar, EventRow, FleetPicker, HostRow, HostInspector, StateStrip, deployment panels (~10+ places). Promote when sizes/colors start drifting.
- **Kbd chip** — `<kbd class="px-1.5 py-0.5 rounded text-[11px] font-mono border …">⌘K</kbd>` — used in TopBar, CmdInput, CmdResultRow, CmdTerminal footer, HelpOverlay (multiple flavors). Promote when accessibility or styling consistency becomes a concern.

Cost > benefit at current scale. Not worth refactoring just to pad the inventory.

## Components removed in this rewrite

These were listed as "planned" or "stable" in the prior `components.md` but never shipped. They are intentionally not on the roadmap:

- ~~**PageShell**~~ — superseded by AppShell.
- ~~**KbdChip**~~ — kept inline (see above).
- ~~**StatusDot**~~ — kept inline (see above).
- ~~**StatusChip**~~ — superseded by StatusPill (different prop names; rename was implicit during build).
- ~~**Button**~~ — superseded by shadcn `components/ui/button`.
- ~~**ModalShell**~~ — superseded by ConfirmDialogShell + per-modal components built on shadcn `Dialog`.
- ~~**TableRow**~~ — never needed; tables roll their own row components inline (HostRow, StackRow, EventRow).
- ~~**MetricCard**~~ — Home page redesign deferred; no consumer.

## shadcn primitives (imported, not authored)

Used directly from `components/ui/`:

- `alert-dialog`, `badge`, `button`, `card`, `dialog`, `input`, `label`, `select`, `separator`, `sonner`, `switch`

Don't redocument these; refer to shadcn-vue docs for API.

## Conventions

- One Vue file per component, filename = component name.
- Props typed via `defineProps<{...}>()` or `withDefaults(defineProps<...>(), {...})`.
- Tailwind `iso-*` classes only (see `design/tokens.md`); no arbitrary hex outside StateStrip's color map.
- Composables in `composables/`, stores in `stores/`.
