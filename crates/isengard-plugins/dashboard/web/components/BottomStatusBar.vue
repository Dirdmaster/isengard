<template>
  <div class="h-14 border-t border-iso-border-subtle bg-iso-bg-elevated flex items-stretch text-xs shrink-0">
    <!-- Left cluster: live indicator + sparkline + event count -->
    <div class="flex items-center gap-3.5 px-4 border-r border-iso-border-subtle">
      <span class="flex items-center gap-2">
        <span class="w-2 h-2 rounded-full" :class="stateLabel.dot"></span>
        <span class="flex flex-col leading-tight">
          <span class="font-medium text-[13px]" :class="stateLabel.color">{{ stateLabel.text }}</span>
          <span class="font-mono text-[10px] text-iso-text-faint">{{ stateLabel.subtext }}</span>
        </span>
      </span>

      <!-- Sparkline: 12 mini bars derived from event volume per 2-hour bucket -->
      <div class="flex items-end gap-[2px] h-6">
        <div
          v-for="(bar, i) in sparkline"
          :key="i"
          class="w-[3px] rounded-[1px]"
          :style="{ height: `${bar.height}px`, backgroundColor: bar.color }"
        ></div>
      </div>

      <span class="font-mono text-[11px] text-iso-text-muted">{{ eventCount }} events / 24h</span>
    </div>

    <!-- Middle cluster: aggregate fleet stats -->
    <div class="flex items-center gap-3 px-4">
      <button
        type="button"
        class="flex items-center gap-1.5 px-2.5 py-1 rounded-iso-md bg-iso-bg-base border border-iso-border-subtle hover:border-iso-border-strong transition-colors"
        title="Active deployments"
        @click="$router.push('/stacks')"
      >
        <Icon name="lucide:zap" class="w-3 h-3" :style="{ color: deploysInProgress > 0 ? '#fbbf24' : 'var(--iso-text-muted)' }" />
        <span class="font-mono text-[11px] font-medium text-iso-text-secondary">
          {{ deploysInProgress }} {{ deploysInProgress === 1 ? 'deploying' : 'deploying' }}
        </span>
      </button>

      <div
        class="flex items-center gap-1.5 px-2.5 py-1 rounded-iso-md bg-iso-bg-base border opacity-60"
        :class="'border-iso-border-subtle'"
        title="Approvals coming with Phase 9"
      >
        <Icon name="lucide:check-square" class="w-3 h-3 text-iso-text-faint" />
        <span class="font-mono text-[11px] text-iso-text-faint">0 pending approvals</span>
      </div>

      <button
        type="button"
        class="flex items-center gap-1.5 px-2.5 py-1 rounded-iso-md bg-iso-bg-base border border-iso-border-subtle hover:border-iso-border-strong transition-colors"
        title="Hosts up vs total"
        @click="$router.push('/hosts')"
      >
        <Icon name="lucide:server" class="w-3 h-3" :style="{ color: hostsAllUp ? '#4ade80' : 'var(--iso-text-muted)' }" />
        <span class="font-mono text-[11px] text-iso-text-secondary">{{ hostsUp }} / {{ hostsTotal }} hosts up</span>
      </button>
    </div>

    <div class="flex-1"></div>

    <!-- Right cluster: kbd buttons + version (separator at left edge) -->
    <div class="flex items-center gap-3 px-4 border-l border-iso-border-subtle">
      <button
        type="button"
        class="flex items-center gap-1.5 hover:opacity-90 transition-opacity"
        title="Open command pane"
        @click="ui.openCmdPane('navigator')"
      >
        <span class="px-2 py-px rounded-iso-sm bg-iso-bg-base border border-iso-border-strong font-mono text-[11px] text-iso-text-secondary">⌘K</span>
        <span class="text-[11px] text-iso-text-muted">command</span>
      </button>
      <button
        type="button"
        class="flex items-center gap-1.5 hover:opacity-90 transition-opacity"
        title="Show keyboard shortcuts"
        @click="ui.helpOpen = true"
      >
        <span class="px-2 py-px rounded-iso-sm bg-iso-bg-base border border-iso-border-strong font-mono text-[11px] text-iso-text-secondary">?</span>
        <span class="text-[11px] text-iso-text-muted">help</span>
      </button>
      <span class="font-mono text-[11px] text-iso-text-faint">v{{ version }}</span>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import type { DeploymentDto } from '~/composables/useDeployments'

/**
 * Bottom status bar · matches Pencil k3bOyE.
 *
 * Three clusters in a 56-px high strip:
 *  - Left   : live indicator + 12-bar sparkline + 24h event count
 *  - Middle : aggregate fleet stats (deploys / approvals / hosts up)
 *  - Right  : ⌘K / ? kbd buttons + controller version
 *
 * Approvals stays at "0 pending · phase 9" until Phase 9 lands. Deploys come
 * from `/deployments?state=active`; hosts from the hosts store. The sparkline
 * buckets the last 24h of events into twelve 2-hour windows and tints any
 * bucket containing a `*.failed` event amber-ish.
 */

const props = defineProps<{
  connectionState: 'connecting' | 'live' | 'reconnecting' | 'offline'
  eventCount: number
}>()

const ui = useUiStore()
const eventsStore = useEventsStore()
const hostsStore = useHostsStore()
const api = useApi()

const version = '0.1.0-alpha'

// ─── Active deployments (best-effort) ────────────────────────────────────
const activeDeploys = ref<DeploymentDto[]>([])

onMounted(async () => {
  try {
    activeDeploys.value = await api.get<DeploymentDto[]>('/deployments', { state: 'active' })
  } catch {
    activeDeploys.value = []
  }
})

const deploysInProgress = computed(() => activeDeploys.value.length)

// ─── Hosts up ────────────────────────────────────────────────────────────
const hostsUp = computed(() => {
  const cutoff = Date.now() - 5 * 60 * 1000
  return hostsStore.hosts.filter(h => {
    if (!h.last_seen_at) return false
    return new Date(h.last_seen_at).getTime() >= cutoff
  }).length
})
const hostsTotal = computed(() => hostsStore.hosts.length)
const hostsAllUp = computed(() => hostsTotal.value > 0 && hostsUp.value === hostsTotal.value)

// ─── Sparkline ────────────────────────────────────────────────────────────
//
// 12 buckets × 2h each = last 24h. Bar height scales linearly to a 22px max.
// Buckets containing any failure event get tinted warn; otherwise success.
const sparkline = computed(() => {
  const now = Date.now()
  const bucketMs = 2 * 60 * 60 * 1000
  const buckets = new Array(12).fill(0).map(() => ({ count: 0, hadFail: false }))
  for (const e of eventsStore.events) {
    const t = new Date(e.occurred_at).getTime()
    if (!Number.isFinite(t)) continue
    const ago = now - t
    if (ago < 0 || ago > bucketMs * 12) continue
    const idx = 11 - Math.floor(ago / bucketMs)
    if (idx < 0 || idx > 11) continue
    buckets[idx].count++
    if (e.kind?.endsWith('.failed') || e.kind === 'routing.degraded') buckets[idx].hadFail = true
  }
  const max = Math.max(1, ...buckets.map(b => b.count))
  return buckets.map(b => {
    const h = Math.max(2, Math.round((b.count / max) * 22))
    const color = b.count === 0
      ? 'var(--iso-border-subtle)'
      : b.hadFail
        ? 'var(--iso-accent-warn)'
        : 'var(--iso-accent-success)'
    return { height: h, color }
  })
})

// ─── Connection label ────────────────────────────────────────────────────
const stateLabel = computed(() => {
  const liveText = 'live'
  const lastEv = eventsStore.events[0]
  const subtext = lastEv ? `last cycle ${relativeTime(lastEv.occurred_at)}` : 'no activity yet'
  const map: Record<string, { dot: string; text: string; color: string; subtext: string }> = {
    connecting:   { dot: 'bg-iso-warn',    text: 'connecting…',  color: 'text-iso-warn',    subtext },
    live:         { dot: 'bg-iso-success', text: liveText,       color: 'text-iso-success', subtext },
    reconnecting: { dot: 'bg-iso-warn',    text: 'reconnecting…', color: 'text-iso-warn',   subtext },
    offline:      { dot: 'bg-iso-error',   text: 'offline',      color: 'text-iso-error',   subtext },
  }
  return map[props.connectionState]
})

function relativeTime(iso: string): string {
  const ms = Date.now() - new Date(iso).getTime()
  const s = Math.floor(ms / 1000)
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ago`
  return `${Math.floor(h / 24)}d ago`
}
</script>
