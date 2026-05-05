<template>
  <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-5">
    <div class="flex items-center justify-between mb-3">
      <h3 class="text-sm font-semibold text-iso-text-primary">tailscale</h3>
      <StatusPill v-if="config?.enabled" state="success" label="enabled" size="xs" />
      <StatusPill v-else state="neutral" label="disabled" size="xs" />
    </div>

    <p class="text-xs text-iso-text-muted mb-4">
      Expose services on your tailnet, optionally publish to the public internet via Funnel.
    </p>

    <div v-if="loading" class="text-xs text-iso-text-muted">Loading...</div>

    <div v-else class="space-y-3">
      <label class="flex items-center gap-2 text-xs">
        <input v-model="form.enabled" type="checkbox" />
        Enabled on this host
      </label>

      <label class="flex items-center gap-2 text-xs">
        <input v-model="form.funnel" type="checkbox" />
        Funnel (public ingress via tailnet)
      </label>

      <div v-if="error" class="text-xs text-iso-error">{{ error }}</div>

      <div class="flex items-center gap-2 pt-2">
        <button
          class="px-3 py-1.5 text-xs bg-iso-info text-iso-bg-base rounded font-medium disabled:opacity-50"
          :disabled="saving"
          @click="onSave"
        >
          Save
        </button>
        <button
          class="px-3 py-1.5 text-xs border border-iso-border rounded text-iso-text-primary disabled:opacity-50"
          :disabled="testing || !config"
          @click="test"
        >
          Test
        </button>
        <span v-if="testResult" class="text-xs" :class="testResult.ok ? 'text-iso-success' : 'text-iso-error'">
          {{ testResult.ok ? 'ok' : (testResult.error || 'error') }}
        </span>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { reactive, ref, watch, onMounted } from 'vue'
import { useAdapterConfig } from '~/composables/useAdapterConfig'
import StatusPill from '~/components/StatusPill.vue'

const props = defineProps<{ hostId: string }>()

const { config, loading, error, testResult, testing, load, save, test } = useAdapterConfig(props.hostId, 'tailscale')

const form = reactive({
  enabled: false,
  funnel: false,
})
const saving = ref(false)

function syncForm() {
  form.enabled = config.value?.enabled ?? false
  form.funnel = !!config.value?.config_json?.funnel
}

watch(config, syncForm)

onMounted(async () => {
  await load()
  syncForm()
})

async function onSave() {
  saving.value = true
  try {
    await save({ funnel: form.funnel }, form.enabled)
  } finally {
    saving.value = false
  }
}
</script>
