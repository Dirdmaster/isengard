<script setup lang="ts">
import { computed } from 'vue'
import type {
  FailureHandling,
  PolicyDto,
  UpdateGate,
  UpdateStrategy,
} from '~/composables/usePolicies'

interface Props {
  policy: PolicyDto
  /**
   * When true, this row is the implicit Global Default rendered before any
   * row exists. The action cluster collapses to "Edit" only and the body
   * shows the resolver fall-back values rather than the row's overrides.
   */
  implicitDefault?: boolean
}

const props = withDefaults(defineProps<Props>(), { implicitDefault: false })

defineEmits<{
  (e: 'edit', policy: PolicyDto): void
  (e: 'remove', policy: PolicyDto): void
  (e: 'resume', policy: PolicyDto): void
}>()

/** Scope label rendered in the row's header, e.g. `STACK . prod / blog`. */
const scopeLabel = computed<string>(() => {
  if (props.implicitDefault || props.policy.scopeType === 'global') {
    return 'GLOBAL DEFAULT'
  }
  const t = props.policy.scopeType.toUpperCase()
  return `${t} . ${props.policy.scopeKey}`
})

/**
 * Indent classes mirror the design concept's hierarchy:
 *   global    -> 0
 *   fleet     -> ml-6
 *   stack     -> ml-12
 *   service   -> ml-[72px]
 *   container -> ml-24
 */
const indentClass = computed<string>(() => {
  if (props.implicitDefault) return ''
  switch (props.policy.scopeType) {
    case 'global':
      return ''
    case 'fleet':
      return 'ml-6'
    case 'stack':
      return 'ml-12'
    case 'service':
      return 'ml-[72px]'
    case 'container':
      return 'ml-24'
  }
})

/** Effective values shown in the body: prefer overrides, fall back to defaults
 *  for the implicit Global Default row. */
const effectiveStrategy = computed<UpdateStrategy | null>(() => {
  if (props.implicitDefault) return 'tag-only'
  return props.policy.body.strategy ?? null
})
const effectiveGate = computed<UpdateGate | null>(() => {
  if (props.implicitDefault) return 'auto'
  return props.policy.body.gate ?? null
})
const effectiveOnFailure = computed<FailureHandling | null>(() => {
  if (props.implicitDefault) return 'notify'
  return props.policy.body.on_failure ?? null
})

/** A row counts as "paused" when paused_until is set and in the future. */
const isPaused = computed<boolean>(() => {
  const ts = props.policy.body.paused_until
  if (!ts) return false
  const t = Date.parse(ts)
  if (Number.isNaN(t)) return false
  return t > Date.now()
})

/** Phase 9b.1: container-scope rows are discovered from compose labels.
 *  They carry a "from labels" pill and have Edit / Remove suppressed since
 *  the source of truth is the compose file, not this UI. */
const isContainerScope = computed<boolean>(
  () => props.policy.scopeType === 'container',
)

interface SummaryLine {
  key: string
  text: string
  override: boolean
}

/**
 * Human-readable summary of overridden fields, one line per non-None field.
 * For the implicit Global Default we render the resolver defaults instead so
 * the user sees the baseline they're inheriting from.
 */
const summary = computed<SummaryLine[]>(() => {
  const out: SummaryLine[] = []
  const b = props.policy.body

  if (props.implicitDefault) {
    out.push({
      key: 'strategy',
      text: 'Strategy: tag-only (apply same-tag digest updates).',
      override: false,
    })
    out.push({
      key: 'gate',
      text: 'Gate: auto (apply without approval).',
      override: false,
    })
    out.push({
      key: 'on_failure',
      text: 'On failure: notify and leave previous container in place.',
      override: false,
    })
    return out
  }

  if (b.strategy) {
    out.push({
      key: 'strategy',
      text: `Override strategy: ${strategyDescription(b.strategy)}`,
      override: true,
    })
  }
  if (b.gate) {
    out.push({
      key: 'gate',
      text: `Override gate: ${gateDescription(b.gate)}`,
      override: true,
    })
  }
  if (b.paused_until) {
    out.push({
      key: 'paused_until',
      text: `Pause until: ${formatPaused(b.paused_until)}`,
      override: true,
    })
  }
  if (b.on_failure) {
    out.push({
      key: 'on_failure',
      text: `On failure: ${failureDescription(b.on_failure)}`,
      override: true,
    })
  }
  if (b.approver_channel) {
    out.push({
      key: 'approver',
      text: `Approver: ${b.approver_channel}`,
      override: true,
    })
  }

  if (out.length === 0) {
    out.push({
      key: 'noop',
      text: 'No fields overridden at this level.',
      override: false,
    })
  }
  return out
})

function strategyDescription(s: UpdateStrategy): string {
  switch (s) {
    case 'pinned': return 'do not update.'
    case 'tag-only': return 'apply same-tag digest updates.'
    case 'minor': return 'allow patch and minor bumps.'
    case 'any': return 'allow any registry-served update for the configured tag.'
  }
}

function gateDescription(g: UpdateGate): string {
  switch (g) {
    case 'auto': return 'apply without approval.'
    case 'approval': return 'ask before applying.'
    case 'never': return 'block updates entirely.'
  }
}

function failureDescription(f: FailureHandling): string {
  switch (f) {
    case 'rollback': return 'roll back to the previous image.'
    case 'keep': return 'keep the broken container for inspection.'
    case 'notify': return 'notify and leave previous container in place.'
  }
}

function formatPaused(iso: string): string {
  const t = Date.parse(iso)
  if (Number.isNaN(t)) return iso
  const d = new Date(t)
  return d.toISOString().slice(0, 10)
}

/** Tailwind classes for the strategy chip. */
function strategyChipClass(s: UpdateStrategy): string {
  switch (s) {
    case 'pinned': return 'bg-iso-error-soft border-iso-error/40 text-iso-error'
    case 'tag-only': return 'bg-iso-info-soft border-iso-info/40 text-iso-info'
    case 'minor': return 'bg-iso-info-soft border-iso-info/40 text-iso-info'
    case 'any': return 'bg-iso-warn-soft border-iso-warn/40 text-iso-warn'
  }
}

/** Tailwind classes for the gate badge. */
function gateBadgeClass(g: UpdateGate): string {
  switch (g) {
    case 'auto': return 'bg-iso-success-soft border-iso-success/40 text-iso-success'
    case 'approval': return 'bg-iso-warn-soft border-iso-warn/40 text-iso-warn'
    case 'never': return 'bg-iso-error-soft border-iso-error/40 text-iso-error'
  }
}
</script>

<template>
  <div
    :class="[
      'rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-4 flex flex-col gap-2.5',
      indentClass,
    ]"
  >
    <div class="flex items-center justify-between gap-3">
      <div class="flex items-center gap-3 flex-wrap min-w-0">
        <span class="text-[10px] font-semibold text-iso-text-muted tracking-widest font-mono shrink-0">
          {{ scopeLabel }}
        </span>
        <span
          v-if="isContainerScope"
          class="px-1.5 py-0.5 rounded-iso-sm border border-iso-border-subtle bg-iso-bg-base text-[10px] text-iso-text-muted font-mono inline-flex items-center gap-1"
          title="Discovered from compose labels (isengard.policy.*). Edit the compose file to change."
        >
          <span aria-hidden="true">[label]</span>
          <span>from labels</span>
        </span>
        <span
          v-if="effectiveStrategy"
          :class="[
            'px-2 py-0.5 rounded-iso-sm border font-mono text-[11px]',
            strategyChipClass(effectiveStrategy),
          ]"
        >{{ effectiveStrategy }}</span>
        <span
          v-if="effectiveGate"
          :class="[
            'px-2 py-0.5 rounded-iso-sm border font-mono text-[11px] inline-flex items-center gap-1',
            gateBadgeClass(effectiveGate),
          ]"
        >
          {{ effectiveGate }}
          <span
            v-if="effectiveGate === 'approval'"
            title="Approval gate is data-modeled but not yet enforced. Phase 9e wires the enforcement path."
            class="text-[9px] text-iso-text-muted"
          >(Phase 9e)</span>
        </span>
        <span
          v-if="isPaused"
          class="px-2 py-0.5 rounded-iso-sm border border-iso-border-strong bg-iso-bg-base font-mono text-[11px] text-iso-text-secondary"
        >paused</span>
      </div>

      <div class="flex items-center gap-1 shrink-0">
        <span
          v-if="isContainerScope"
          class="px-2 py-1 rounded-iso-sm text-[11px] text-iso-text-faint italic"
          title="Edit the compose file's isengard.policy.* labels and redeploy to change."
        >read-only</span>
        <button
          v-else
          class="px-2 py-1 rounded-iso-sm text-[11px] text-iso-text-secondary hover:bg-iso-bg-base hover:text-iso-text-primary"
          @click="$emit('edit', policy)"
        >Edit</button>
        <button
          v-if="isPaused && !isContainerScope"
          class="px-2 py-1 rounded-iso-sm text-[11px] text-iso-info hover:bg-iso-info/10"
          @click="$emit('resume', policy)"
        >Resume now</button>
        <button
          v-if="!implicitDefault && !isContainerScope"
          class="px-2 py-1 rounded-iso-sm text-[11px] text-iso-text-faint hover:bg-iso-error/10 hover:text-iso-error"
          @click="$emit('remove', policy)"
        >Remove</button>
      </div>
    </div>

    <div class="flex flex-col gap-1">
      <p
        v-for="line in summary"
        :key="line.key"
        :class="[
          'text-xs',
          line.override ? 'text-iso-text-secondary' : 'text-iso-text-muted',
        ]"
      >
        {{ line.text }}
      </p>
    </div>
  </div>
</template>
