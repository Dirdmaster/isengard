<script setup lang="ts">
import { computed, onMounted } from 'vue'
import {
  useServiceDeployStrategy,
  type DeployStrategyChoice,
  type ServiceDeployStrategyDto,
} from '~/composables/useServiceDeployStrategy'

const { items, loading, error, refresh, setOverride } = useServiceDeployStrategy()
const toast = useToast()

onMounted(refresh)

/** Per-row UI value: `auto` when there is no persisted override. */
function currentChoice(row: ServiceDeployStrategyDto): DeployStrategyChoice {
  const v = row.override_value
  if (v === 'blue-green' || v === 'in-place') return v
  return 'auto'
}

async function onChange(row: ServiceDeployStrategyDto, choice: DeployStrategyChoice) {
  if (choice === currentChoice(row)) return
  try {
    await setOverride(row.service_id, choice)
    const label =
      choice === 'auto'
        ? 'Cleared override'
        : `Set ${row.service_name} to ${choice}`
    toast.success(label)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    toast.error(`Save failed: ${msg}`)
  }
}

const grouped = computed(() => {
  // Group rows by stack name for nicer display. Services without a stack go
  // under a synthetic "(no stack)" bucket.
  const buckets = new Map<string, ServiceDeployStrategyDto[]>()
  for (const row of items.value) {
    const key = row.stack_name ?? '(no stack)'
    const arr = buckets.get(key) ?? []
    arr.push(row)
    buckets.set(key, arr)
  }
  return Array.from(buckets.entries()).sort(([a], [b]) => a.localeCompare(b))
})

const choices: { key: DeployStrategyChoice; label: string }[] = [
  { key: 'auto', label: 'Auto' },
  { key: 'blue-green', label: 'Blue-green' },
  { key: 'in-place', label: 'In-place' },
]
</script>

<template>
  <SettingsSection
    title="Deploy strategy"
    description="Override how each service is updated when a new image lands. Auto picks blue-green for HTTP-routed services and in-place for everything else."
  >
    <div v-if="loading && items.length === 0" class="text-iso-text-muted text-sm py-4">
      Loading services...
    </div>
    <div v-else-if="error" class="text-iso-error text-sm py-4">
      {{ error }}
    </div>
    <div v-else-if="items.length === 0" class="text-iso-text-muted text-sm py-4">
      No services discovered yet. Once an agent reports its containers, they appear here.
    </div>
    <div v-else class="space-y-6">
      <div v-for="[stackName, rows] in grouped" :key="stackName">
        <div class="text-xs uppercase tracking-wider text-iso-text-faint mb-2">
          {{ stackName }}
        </div>
        <table class="w-full text-xs">
          <thead class="text-iso-text-muted">
            <tr>
              <th class="text-left pb-2 font-medium">Service</th>
              <th class="text-left pb-2 font-medium">Strategy</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="row in rows"
              :key="row.service_id"
              class="border-t border-iso-border"
            >
              <td class="py-2 font-mono text-iso-text-primary">{{ row.service_name }}</td>
              <td class="py-2">
                <div class="flex gap-1">
                  <button
                    v-for="c in choices"
                    :key="c.key"
                    :class="[
                      'px-3 py-1 rounded text-xs border transition-colors',
                      currentChoice(row) === c.key
                        ? 'border-iso-info text-iso-text-primary bg-iso-info/10'
                        : 'border-iso-border-subtle text-iso-text-muted hover:text-iso-text-primary hover:border-iso-border',
                    ]"
                    :aria-pressed="currentChoice(row) === c.key"
                    @click="onChange(row, c.key)"
                  >
                    {{ c.label }}
                  </button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>
  </SettingsSection>
</template>
