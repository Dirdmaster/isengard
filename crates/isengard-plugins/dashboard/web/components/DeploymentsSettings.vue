<script setup lang="ts">
import { computed, onMounted, reactive, ref, watch } from 'vue'
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

// ---------------------------------------------------------------------------
// Phase 10c (T5 refs #50): per-stack parallelism dropdown
// ---------------------------------------------------------------------------
//
// One dropdown per stack header. Persists to
// `/api/v1/stacks/:id/deployment-parallelism`. `null` means default (rolling,
// 1 host at a time).

type Parallelism = '1' | '2' | '3' | 'all'
const parallelismChoices: { key: Parallelism; label: string }[] = [
  { key: '1', label: 'Rolling (1)' },
  { key: '2', label: 'Parallel 2' },
  { key: '3', label: 'Parallel 3' },
  { key: 'all', label: 'All at once' },
]

const stackParallelism = reactive<Record<string, Parallelism>>({})
const parallelismLoading = ref(false)

/**
 * Distinct (stack_id, stack_name) pairs from the row list. Stacks without
 * a `stack_id` (orphan services) are skipped: parallelism only applies to
 * real stack rows.
 */
const stackList = computed(() => {
  const m = new Map<number, string>()
  for (const r of items.value) {
    if (typeof r.stack_id === 'number' && r.stack_name) {
      m.set(r.stack_id, r.stack_name)
    }
  }
  return Array.from(m.entries()).map(([id, name]) => ({ id, name }))
})

async function loadParallelism() {
  if (!stackList.value.length) return
  parallelismLoading.value = true
  const api = useApi()
  try {
    await Promise.all(
      stackList.value.map(async (s) => {
        try {
          const dto = await api.get<{ stack_id: number; parallelism: string | null }>(
            `/stacks/${s.id}/deployment-parallelism`,
          )
          stackParallelism[String(s.id)] = (dto.parallelism as Parallelism | null) ?? '1'
        } catch {
          stackParallelism[String(s.id)] = '1'
        }
      }),
    )
  } finally {
    parallelismLoading.value = false
  }
}

watch(stackList, loadParallelism, { immediate: true })

async function onParallelismChange(stackId: number, value: Parallelism) {
  const api = useApi()
  try {
    await api.post(`/stacks/${stackId}/deployment-parallelism`, {
      parallelism: value === '1' ? null : value,
    })
    stackParallelism[String(stackId)] = value
    toast.success(`Saved parallelism: ${value}`)
  } catch (e) {
    toast.error(`Save failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

function stackIdForName(name: string): number | null {
  const found = stackList.value.find((s) => s.name === name)
  return found ? found.id : null
}
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
        <div class="flex items-center justify-between mb-2 gap-3">
          <div class="text-xs uppercase tracking-wider text-iso-text-faint">
            {{ stackName }}
          </div>
          <!-- Per-stack parallelism dropdown. Hidden for orphan services. -->
          <div
            v-if="stackIdForName(stackName) !== null"
            class="flex items-center gap-2 text-xs"
          >
            <span
              class="text-iso-text-faint"
              title="How many hosts deploy in lockstep when this stack runs on multiple hosts. Defaults to rolling (1 at a time)."
            >
              Multi-host:
            </span>
            <select
              :value="stackParallelism[String(stackIdForName(stackName))] ?? '1'"
              class="bg-iso-bg-elevated border border-iso-border-subtle rounded px-2 py-0.5 text-iso-text-primary"
              @change="
                onParallelismChange(
                  stackIdForName(stackName)!,
                  ($event.target as HTMLSelectElement).value as Parallelism,
                )
              "
            >
              <option v-for="c in parallelismChoices" :key="c.key" :value="c.key">
                {{ c.label }}
              </option>
            </select>
          </div>
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
