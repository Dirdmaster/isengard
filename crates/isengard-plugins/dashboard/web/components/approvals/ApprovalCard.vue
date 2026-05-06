<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import type {
  ApprovalDto,
  ApprovalState,
  DecisionKind,
} from '~/composables/useApprovals'

interface Props {
  approval: ApprovalDto
  /** True when a decision is in flight; disables every action button. */
  busy?: boolean
}

const props = withDefaults(defineProps<Props>(), { busy: false })

const emit = defineEmits<{
  (
    e: 'decide',
    payload: { id: string; decision: DecisionKind; snoozeHours?: number },
  ): void
}>()

const SNOOZE_OPTIONS: Array<{ hours: number; label: string }> = [
  { hours: 6, label: '6h' },
  { hours: 12, label: '12h' },
  { hours: 24, label: '24h' },
  { hours: 72, label: '3d' },
  { hours: 168, label: '7d' },
]

// `now` ticks every 30s so relative-time text refreshes without forcing a
// full data refetch.
const now = ref(Date.now())
let nowInterval: ReturnType<typeof setInterval> | null = null
onMounted(() => {
  nowInterval = setInterval(() => {
    now.value = Date.now()
  }, 30_000)
})
onBeforeUnmount(() => {
  if (nowInterval !== null) {
    clearInterval(nowInterval)
    nowInterval = null
  }
})

const snoozeOpen = ref(false)
function toggleSnooze() {
  snoozeOpen.value = !snoozeOpen.value
}
function closeSnooze() {
  snoozeOpen.value = false
}

const isDecided = computed(() => props.approval.state !== 'pending_open')

const scopePath = computed(
  () => `${props.approval.stack} / ${props.approval.service}`,
)

function shortDigest(d: string): string {
  if (!d) return ''
  // GHCR-style digests look like `sha256:0123abcd...`; show 12 chars after
  // the colon. Bare hex strings get the first 12 chars too.
  const colon = d.indexOf(':')
  if (colon >= 0) return d.slice(colon + 1, colon + 13)
  return d.slice(0, 12)
}

const currentShort = computed(() => shortDigest(props.approval.currentDigest))
const proposedShort = computed(() => shortDigest(props.approval.proposedDigest))

function relativeAgo(iso: string, nowMs: number): string {
  const t = Date.parse(iso)
  if (Number.isNaN(t)) return iso
  const ms = nowMs - t
  const s = Math.max(0, Math.floor(ms / 1000))
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ago`
  return `${Math.floor(h / 24)}d ago`
}

function relativeUntil(iso: string, nowMs: number): { text: string; expired: boolean; soon: boolean } {
  const t = Date.parse(iso)
  if (Number.isNaN(t)) return { text: iso, expired: false, soon: false }
  const ms = t - nowMs
  if (ms <= 0) return { text: 'expired', expired: true, soon: false }
  const s = Math.floor(ms / 1000)
  if (s < 60) return { text: `in ${s}s`, expired: false, soon: true }
  const m = Math.floor(s / 60)
  if (m < 60) {
    return { text: `in ${m}m`, expired: false, soon: m <= 30 }
  }
  const h = Math.floor(m / 60)
  if (h < 24) {
    return { text: `in ${h}h`, expired: false, soon: h <= 2 }
  }
  return { text: `in ${Math.floor(h / 24)}d`, expired: false, soon: false }
}

const requestedAgo = computed(() => relativeAgo(props.approval.createdAt, now.value))
const expiresIn = computed(() => relativeUntil(props.approval.expiresAt, now.value))

const decidedAgo = computed(() => {
  if (!props.approval.decidedAt) return null
  return relativeAgo(props.approval.decidedAt, now.value)
})

interface DecidedChip {
  label: string
  cls: string
  dot: string
}

const decidedChip = computed<DecidedChip | null>(() => {
  switch (props.approval.state) {
    case 'pending_approved':
      return {
        label: 'Approved',
        cls: 'bg-iso-success-soft border-iso-success/40 text-iso-success',
        dot: 'bg-iso-success',
      }
    case 'pending_rejected':
      return {
        label: 'Rejected',
        cls: 'bg-iso-error-soft border-iso-error/40 text-iso-error',
        dot: 'bg-iso-error',
      }
    case 'pending_snoozed':
      return {
        label: 'Snoozed',
        cls: 'bg-iso-info-soft border-iso-info/40 text-iso-info',
        dot: 'bg-iso-info',
      }
    case 'pending_expired':
      return {
        label: 'Expired',
        cls: 'bg-iso-bg-base border-iso-border-strong text-iso-text-muted',
        dot: 'bg-iso-text-muted',
      }
    default:
      return null
  }
})

const cardBorderClass = computed(() => {
  if (isDecided.value) return 'border-iso-border-subtle'
  if (expiresIn.value.expired) return 'border-iso-error/60'
  if (expiresIn.value.soon) return 'border-iso-warn/60'
  return 'border-iso-warn/40'
})

function emitDecide(decision: DecisionKind, snoozeHours?: number) {
  emit('decide', { id: props.approval.actionId, decision, snoozeHours })
}

function onApprove() {
  if (props.busy || isDecided.value) return
  emitDecide('approve')
}

function onReject() {
  if (props.busy || isDecided.value) return
  emitDecide('reject')
}

function onSnoozeChoose(hours: number) {
  closeSnooze()
  if (props.busy || isDecided.value) return
  emitDecide('snooze', hours)
}

function onKeySnoozeBtn(e: KeyboardEvent) {
  if (e.key === 'Escape' && snoozeOpen.value) {
    e.preventDefault()
    closeSnooze()
  }
}

const stateLabel = (state: ApprovalState): string => {
  switch (state) {
    case 'pending_open': return 'Open'
    case 'pending_approved': return 'Approved'
    case 'pending_rejected': return 'Rejected'
    case 'pending_snoozed': return 'Snoozed'
    case 'pending_expired': return 'Expired'
  }
}
</script>

<template>
  <article
    :class="[
      'rounded-iso-lg border bg-iso-bg-elevated p-4 flex flex-col gap-3',
      cardBorderClass,
    ]"
    :aria-label="`Approval ${approval.actionId}: ${stateLabel(approval.state)}`"
  >
    <!-- Header: scope path + state chip -->
    <div class="flex items-start justify-between gap-3 flex-wrap">
      <div class="flex flex-col gap-1 min-w-0">
        <div class="flex items-center gap-2 text-sm flex-wrap">
          <span class="font-mono text-iso-text-secondary text-[11px] uppercase tracking-wider shrink-0">
            {{ approval.hostId.slice(0, 12) }}
          </span>
          <span class="text-iso-text-faint">/</span>
          <span class="font-medium text-iso-text-primary truncate">{{ scopePath }}</span>
        </div>
        <span class="text-[11px] text-iso-text-muted font-mono truncate">
          {{ approval.containerName }}
        </span>
      </div>

      <div class="flex items-center gap-2 shrink-0">
        <span
          v-if="decidedChip"
          :class="[
            'px-2 py-0.5 rounded-iso-sm border font-mono text-[11px] inline-flex items-center gap-1.5',
            decidedChip.cls,
          ]"
        >
          <span :class="['inline-block w-1.5 h-1.5 rounded-full', decidedChip.dot]"></span>
          {{ decidedChip.label }}
        </span>
        <span
          v-else-if="expiresIn.expired"
          class="px-2 py-0.5 rounded-iso-sm border font-mono text-[11px] bg-iso-error-soft border-iso-error/40 text-iso-error inline-flex items-center gap-1.5"
        >
          <span class="inline-block w-1.5 h-1.5 rounded-full bg-iso-error"></span>
          Expiring
        </span>
        <span
          v-else
          class="px-2 py-0.5 rounded-iso-sm border font-mono text-[11px] bg-iso-warn-soft border-iso-warn/40 text-iso-warn inline-flex items-center gap-1.5"
        >
          <span class="inline-block w-1.5 h-1.5 rounded-full bg-iso-warn"></span>
          Pending
        </span>
      </div>
    </div>

    <!-- Image change -->
    <div class="flex items-center gap-2 flex-wrap text-xs">
      <span class="font-mono text-iso-text-secondary truncate max-w-full">
        {{ approval.image }}
      </span>
      <span class="font-mono text-iso-text-muted">:{{ currentShort }}</span>
      <span class="text-iso-text-faint">to</span>
      <span class="font-mono font-medium text-iso-success">:{{ proposedShort }}</span>
    </div>

    <!-- Meta line: requested + expires + diff link -->
    <div class="flex items-center gap-3 text-[11px] text-iso-text-muted flex-wrap">
      <span>
        Requested {{ requestedAgo }}
      </span>
      <span class="text-iso-text-faint">.</span>
      <span v-if="!isDecided">
        Expires
        <span :class="expiresIn.soon ? 'text-iso-warn' : ''">{{ expiresIn.text }}</span>
      </span>
      <template v-if="isDecided">
        <span>
          {{ stateLabel(approval.state) }}
          <template v-if="approval.decidedBy"> by {{ approval.decidedBy }}</template>
          <template v-if="decidedAgo"> {{ decidedAgo }}</template>
        </span>
      </template>
      <template v-if="approval.diffUrl">
        <span class="text-iso-text-faint">.</span>
        <a
          :href="approval.diffUrl"
          target="_blank"
          rel="noopener noreferrer"
          class="text-iso-info hover:underline inline-flex items-center gap-1"
        >
          View diff
          <Icon name="lucide:external-link" class="w-3 h-3" />
        </a>
      </template>
    </div>

    <!-- Action row: Approve / Reject / Snooze (open rows only) -->
    <div v-if="!isDecided" class="flex items-center gap-2 flex-wrap pt-1">
      <button
        type="button"
        :disabled="busy"
        class="px-3 py-1.5 rounded-iso-md bg-iso-success text-iso-bg-base text-xs font-medium hover:opacity-90 disabled:opacity-50 disabled:cursor-not-allowed transition-opacity focus:outline-none focus-visible:ring-2 focus-visible:ring-iso-success focus-visible:ring-offset-2 focus-visible:ring-offset-iso-bg-elevated"
        @click="onApprove"
        @keydown.enter.prevent="onApprove"
      >
        Approve
      </button>
      <button
        type="button"
        :disabled="busy"
        class="px-3 py-1.5 rounded-iso-md bg-iso-bg-base border border-iso-border-strong text-xs font-medium text-iso-error hover:bg-iso-error/10 hover:border-iso-error/60 disabled:opacity-50 disabled:cursor-not-allowed transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-iso-error focus-visible:ring-offset-2 focus-visible:ring-offset-iso-bg-elevated"
        @click="onReject"
        @keydown.enter.prevent="onReject"
      >
        Reject
      </button>

      <div class="relative" @click.stop>
        <button
          type="button"
          :disabled="busy"
          :aria-expanded="snoozeOpen"
          aria-haspopup="menu"
          class="px-3 py-1.5 rounded-iso-md bg-iso-bg-base border border-iso-border-strong text-xs font-medium text-iso-text-secondary hover:text-iso-text-primary hover:border-iso-info/60 disabled:opacity-50 disabled:cursor-not-allowed transition-colors inline-flex items-center gap-1 focus:outline-none focus-visible:ring-2 focus-visible:ring-iso-info focus-visible:ring-offset-2 focus-visible:ring-offset-iso-bg-elevated"
          @click="toggleSnooze"
          @keydown="onKeySnoozeBtn"
        >
          Snooze
          <Icon name="lucide:chevron-down" class="w-3 h-3" />
        </button>
        <div
          v-if="snoozeOpen"
          role="menu"
          class="absolute top-full left-0 mt-1.5 min-w-[120px] bg-iso-bg-overlay border border-iso-border-strong rounded-iso-md shadow-xl shadow-black/40 z-30 py-1"
        >
          <button
            v-for="opt in SNOOZE_OPTIONS"
            :key="opt.hours"
            type="button"
            role="menuitem"
            class="w-full px-3 py-1.5 text-xs text-left text-iso-text-secondary hover:bg-iso-bg-row-hover hover:text-iso-text-primary transition-colors focus:outline-none focus-visible:bg-iso-bg-row-hover"
            @click="onSnoozeChoose(opt.hours)"
            @keydown.enter.prevent="onSnoozeChoose(opt.hours)"
          >
            Snooze {{ opt.label }}
          </button>
        </div>
      </div>

      <span v-if="busy" class="text-[11px] text-iso-text-muted ml-1">
        Submitting...
      </span>
    </div>
  </article>
</template>
