<template>
  <div class="m-3 p-4 rounded-iso-lg bg-iso-bg-elevated border border-iso-border-subtle">
    <!-- Header row -->
    <div class="flex items-center gap-3.5 mb-3.5">
      <div
        class="w-6 h-6 rounded-full flex items-center justify-center"
        :style="{ backgroundColor: iconBg }"
      >
        <Icon :name="iconName" class="w-3.5 h-3.5" :style="{ color: iconColor }" />
      </div>

      <div>
        <h4 class="font-semibold text-iso-md text-iso-text-primary">Fleet · {{ fleet.name }}</h4>
        <div class="flex gap-3 mt-1 text-iso-sm">
          <span v-if="healthyCount > 0" class="text-iso-success">{{ healthyCount }} healthy</span>
          <span v-if="updatingCount > 0" class="text-iso-warn">{{ updatingCount }} updating</span>
          <span v-if="failedCount > 0" class="text-iso-error">{{ failedCount }} failed</span>
        </div>
        <div class="text-iso-xs text-iso-text-muted mt-1">
          {{ totalServices }} services · {{ fleet.host_count }} hosts · last cycle {{ lastCycleAgo }}
        </div>
      </div>

      <div class="flex-1"></div>

      <div class="flex gap-1">
        <div v-for="(s, i) in serviceStates" :key="i" class="w-3.5 h-3.5 rounded-sm" :style="{ backgroundColor: stateColor(s) }"></div>
      </div>
    </div>

    <div v-if="issues.length > 0">
      <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-2">{{ issues.length }} need attention</div>
      <div class="space-y-1">
        <div v-for="issue in issues" :key="issue.id" class="flex items-center gap-2.5 py-1.5 border-b border-iso-border-subtle last:border-b-0 text-iso-sm">
          <div class="w-2 h-2 rounded-full" :style="{ backgroundColor: stateColor(issue.state), boxShadow: `0 0 6px ${stateColor(issue.state)}66` }"></div>
          <span class="text-iso-text-primary">
            {{ issue.container_name }}
            <span class="text-iso-text-faint font-mono text-iso-xs">on {{ issue.host_name }}</span>
          </span>
          <div class="flex-1"></div>
          <span class="text-iso-text-muted font-mono text-iso-xs">{{ issue.detail }}</span>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import type { Fleet } from '~/stores/fleets'

defineProps<{
  fleet: Fleet
}>()

const eventsStore = useEventsStore()

const totalServices = computed(() => 7) // TODO 5d: real count from API

const issues = computed(() => {
  const recent = eventsStore.events.slice(0, 50)
  return recent
    .filter((e: any) => ['update.failed', 'update.pulling', 'agent.disconnect_long'].includes(e.kind))
    .slice(0, 5)
    .map((e: any) => ({
      id: e.id,
      container_name: e.container_name ?? '?',
      host_name: '?', // TODO 5d: lookup from hosts store
      detail: e.summary,
      state: kindToState(e.kind),
    }))
})

const serviceStates = computed<Array<'success'|'warn'|'error'|'info'>>(() => {
  const states: Array<'success'|'warn'|'error'|'info'> = []
  for (let i = 0; i < totalServices.value; i++) states.push('success')
  let idx = 0
  for (const issue of issues.value) {
    states[idx % states.length] = issue.state as 'success'|'warn'|'error'|'info'
    idx++
  }
  return states
})

function kindToState(kind: string): 'success'|'warn'|'error'|'info' {
  if (kind === 'update.failed') return 'error'
  if (kind === 'update.pulling') return 'warn'
  if (kind === 'agent.disconnect_long') return 'info'
  return 'success'
}

const updatingCount = computed(() => issues.value.filter((i: any) => i.state === 'warn').length)
const failedCount = computed(() => issues.value.filter((i: any) => i.state === 'error').length)
const healthyCount = computed(() => Math.max(0, totalServices.value - updatingCount.value - failedCount.value))

const iconName = computed(() => {
  if (failedCount.value > 0 || updatingCount.value > 0) return 'lucide:triangle-alert'
  return 'lucide:check'
})

const iconBg = computed(() => {
  if (failedCount.value > 0) return '#f8717126'
  if (updatingCount.value > 0) return '#fbbf2426'
  return '#4ade8026'
})

const iconColor = computed(() => {
  if (failedCount.value > 0) return '#f87171'
  if (updatingCount.value > 0) return '#fbbf24'
  return '#4ade80'
})

const lastCycleAgo = computed(() => '22s ago') // TODO 5d: derive from latest CHECKED event

function stateColor(s: string) {
  const map: Record<string, string> = {
    success: '#4ade80',
    warn: '#fbbf24',
    error: '#f87171',
    info: '#c084fc',
  }
  return map[s] ?? '#94a3b8'
}
</script>
