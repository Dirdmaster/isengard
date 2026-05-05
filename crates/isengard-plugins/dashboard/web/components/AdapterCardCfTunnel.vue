<template>
  <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-5">
    <div class="flex items-center justify-between mb-3">
      <h3 class="text-sm font-semibold text-iso-text-primary">cf-tunnel</h3>
      <StatusPill v-if="config?.enabled" state="success" label="enabled" size="xs" />
      <StatusPill v-else state="neutral" label="disabled" size="xs" />
    </div>

    <p class="text-xs text-iso-text-muted mb-4">
      Cloudflare Tunnel: outbound-only ingress through Cloudflare's edge. Provide an API token with
      Tunnel + Zone DNS edit scopes.
    </p>

    <div v-if="loading" class="text-xs text-iso-text-muted">Loading...</div>

    <div v-else class="space-y-3">
      <div>
        <label class="text-xs text-iso-text-muted">API token</label>
        <div class="flex gap-2">
          <input
            v-model="form.api_token"
            :type="showToken ? 'text' : 'password'"
            placeholder="cloudflare api token"
            class="flex-1 font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-base"
          />
          <button
            type="button"
            class="px-2 py-1 text-xs border border-iso-border rounded text-iso-text-muted"
            @click="showToken = !showToken"
          >
            {{ showToken ? 'hide' : 'show' }}
          </button>
        </div>
      </div>

      <div class="grid grid-cols-2 gap-3">
        <div>
          <label class="text-xs text-iso-text-muted">Account ID</label>
          <input
            v-model="form.account_id"
            type="text"
            class="w-full font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-base"
          />
        </div>
        <div>
          <label class="text-xs text-iso-text-muted">Zone ID</label>
          <input
            v-model="form.zone_id"
            type="text"
            class="w-full font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-base"
          />
        </div>
      </div>

      <div class="grid grid-cols-2 gap-3">
        <div>
          <label class="text-xs text-iso-text-muted">Tunnel name</label>
          <input
            v-model="form.tunnel_name"
            type="text"
            placeholder="isengard-prod"
            class="w-full font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-base"
          />
        </div>
        <div>
          <label class="text-xs text-iso-text-muted">Tunnel ID</label>
          <input
            v-model="form.tunnel_id"
            type="text"
            :readonly="!!form.tunnel_id"
            class="w-full font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-base read-only:opacity-70"
          />
        </div>
      </div>

      <div>
        <label class="text-xs text-iso-text-muted">Tunnel token</label>
        <input
          v-model="form.tunnel_token"
          :type="showToken ? 'text' : 'password'"
          class="w-full font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-base"
        />
      </div>

      <label class="flex items-center gap-2 text-xs">
        <input v-model="form.enabled" type="checkbox" />
        Enabled on this host
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

const { config, loading, error, testResult, testing, load, save, test } = useAdapterConfig(props.hostId, 'cf-tunnel')

const form = reactive({
  api_token: '',
  account_id: '',
  zone_id: '',
  tunnel_name: '',
  tunnel_id: '',
  tunnel_token: '',
  enabled: false,
})
const showToken = ref(false)
const saving = ref(false)

function syncForm() {
  const cj = config.value?.config_json ?? {}
  form.api_token = cj.api_token ?? ''
  form.account_id = cj.account_id ?? ''
  form.zone_id = cj.zone_id ?? ''
  form.tunnel_name = cj.tunnel_name ?? ''
  form.tunnel_id = cj.tunnel_id ?? ''
  form.tunnel_token = cj.tunnel_token ?? ''
  form.enabled = config.value?.enabled ?? false
}

watch(config, syncForm)

onMounted(async () => {
  await load()
  syncForm()
})

async function onSave() {
  saving.value = true
  try {
    await save(
      {
        api_token: form.api_token,
        account_id: form.account_id,
        zone_id: form.zone_id,
        tunnel_name: form.tunnel_name,
        tunnel_id: form.tunnel_id,
        tunnel_token: form.tunnel_token,
      },
      form.enabled,
    )
  } finally {
    saving.value = false
  }
}
</script>
