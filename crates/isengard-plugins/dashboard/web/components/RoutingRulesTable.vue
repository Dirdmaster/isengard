<template>
  <div>
    <div class="flex items-center justify-between mb-4">
      <h3 class="text-sm font-semibold text-iso-text-primary">Routing rules</h3>
      <button
        class="px-3 py-1.5 text-xs font-medium bg-iso-info text-iso-bg-base rounded"
        @click="emit('add')"
      >
        + Add rule
      </button>
    </div>

    <div v-if="loading" class="text-iso-text-muted text-sm py-4">Loading…</div>
    <div v-else-if="error" class="text-iso-error text-sm py-4">Error: {{ error }}</div>
    <div v-else-if="rules.length === 0" class="text-iso-text-muted text-sm py-4">
      No routing rules. Add one above, or apply <code>isengard.expose</code> labels to your containers.
    </div>
    <table v-else class="w-full text-xs">
      <thead class="text-iso-text-muted">
        <tr>
          <th class="text-left pb-2 font-medium">Hostname</th>
          <th class="text-left pb-2 font-medium">Target</th>
          <th class="text-left pb-2 font-medium">Adapter</th>
          <th class="text-left pb-2 font-medium">TLS</th>
          <th class="text-left pb-2 font-medium">State</th>
          <th class="text-left pb-2 font-medium">Source</th>
          <th class="pb-2"></th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="r in rules" :key="r.id" class="border-t border-iso-border">
          <td class="py-2 font-mono">{{ r.public_hostname }}</td>
          <td class="py-2 font-mono">{{ r.service_name }}:{{ r.container_port }}</td>
          <td class="py-2">{{ r.adapter }}</td>
          <td class="py-2">{{ r.tls_mode }}</td>
          <td class="py-2">{{ r.state }}</td>
          <td class="py-2">
            <span v-if="r.source === 'label'">label · <code class="text-iso-text-muted">{{ r.source_container_id?.slice(0, 8) }}</code></span>
            <span v-else-if="r.source === 'imported'">imported</span>
            <span v-else>ui</span>
          </td>
          <td class="py-2 text-right">
            <button class="text-iso-text-muted hover:text-iso-text-primary px-2" @click="emit('edit', r)">
              Edit
            </button>
            <button class="text-iso-error hover:underline px-2" @click="onDelete(r)">
              Delete
            </button>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<script setup lang="ts">
import { useRoutingRules, type RoutingRule } from '~/composables/useRoutingRules'

const { rules, loading, error, deleteRule } = useRoutingRules()
const emit = defineEmits<{ (e: 'add'): void; (e: 'edit', rule: RoutingRule): void }>()

async function onDelete(r: RoutingRule) {
  if (!confirm(`Delete rule for ${r.public_hostname}?`)) return
  await deleteRule(r.id)
}
</script>
