<script setup lang="ts">
/**
 * Events page empty state — mirrors `design/concepts/events/empty-v1.html`.
 *
 * Centred clock icon + headline + 4 example chips so first-time operators
 * understand what kinds of things will appear here. Renders inside a dashed
 * elevated tile so it reads as "this is where the list will go."
 */

defineProps<{
  hostCount: number
}>()
</script>

<template>
  <div class="flex-1 rounded-iso-xl border border-dashed border-iso-border-subtle bg-iso-bg-elevated/40 flex items-center justify-center min-h-0">
    <div class="w-[600px] flex flex-col items-center gap-6 p-10 text-center">
      <div class="w-14 h-14 rounded-full border border-iso-border-subtle bg-iso-bg-base flex items-center justify-center">
        <Icon name="lucide:clock" class="w-7 h-7 text-iso-text-faint" stroke-width="1.5" />
      </div>

      <div class="flex flex-col gap-2">
        <h2 class="text-xl font-semibold tracking-tight text-iso-text-primary">No events yet</h2>
        <p class="text-sm text-iso-text-muted leading-relaxed">
          Events are recorded as your fleet runs: container updates, healthcheck failures, deploys, policy decisions, approvals.
          They'll appear here in real-time<span v-if="hostCount > 0"> ({{ hostCount }} {{ hostCount === 1 ? 'host' : 'hosts' }} heartbeating)</span>.
        </p>
      </div>

      <div class="w-full grid grid-cols-2 gap-2 text-left">
        <div class="px-3 py-2 rounded-iso-sm bg-iso-bg-base border border-iso-border-subtle flex items-center gap-2 text-[11px] text-iso-text-muted">
          <span class="px-1.5 py-px rounded-iso-sm bg-iso-success-soft font-mono text-[10px] text-iso-success">update.success</span>
          container redeployed
        </div>
        <div class="px-3 py-2 rounded-iso-sm bg-iso-bg-base border border-iso-border-subtle flex items-center gap-2 text-[11px] text-iso-text-muted">
          <span class="px-1.5 py-px rounded-iso-sm bg-iso-warn-soft font-mono text-[10px] text-iso-warn">approval.pending</span>
          human review needed
        </div>
        <div class="px-3 py-2 rounded-iso-sm bg-iso-bg-base border border-iso-border-subtle flex items-center gap-2 text-[11px] text-iso-text-muted">
          <span class="px-1.5 py-px rounded-iso-sm bg-iso-error-soft font-mono text-[10px] text-iso-error">healthcheck.failed</span>
          container marked unhealthy
        </div>
        <div class="px-3 py-2 rounded-iso-sm bg-iso-bg-base border border-iso-border-subtle flex items-center gap-2 text-[11px] text-iso-text-muted">
          <span class="px-1.5 py-px rounded-iso-sm bg-iso-info-soft font-mono text-[10px] text-iso-info">stack.deployed</span>
          new stack created
        </div>
      </div>
    </div>
  </div>
</template>
