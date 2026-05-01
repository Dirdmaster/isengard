<script setup lang="ts">
import { ref } from 'vue'
import { useFleetsStore } from '~/stores/fleets'

defineEmits<{ close: [] }>()

const fleetsStore = useFleetsStore()
if (!fleetsStore.loaded) await fleetsStore.load()

const toast = useToast()
const fleet = ref('default')
const hostname = ref('')
const installCommand = ref('')
const loading = ref(false)
const error = ref('')

async function generate() {
  loading.value = true
  error.value = ''
  try {
    const api = useApi()
    const dto = await api.post<{ install_command: string }>('/hosts', {
      fleet: fleet.value,
      hostname: hostname.value || undefined,
    })
    installCommand.value = dto.install_command
    toast.info('Install command generated. Token expires in 30 min.')
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    error.value = msg
    toast.error(`Generate failed: ${msg}`)
  } finally {
    loading.value = false
  }
}

async function copy() {
  try {
    await navigator.clipboard.writeText(installCommand.value)
    toast.success('Install command copied to clipboard')
  } catch {
    toast.error('Copy failed — please copy manually')
  }
}
</script>

<template>
  <div class="fixed inset-0 bg-black/60 flex items-center justify-center z-50" @click.self="$emit('close')">
    <div class="bg-iso-bg-base border border-iso-border-subtle rounded-lg w-[640px] max-w-full p-6 space-y-4">
      <header class="flex items-center justify-between">
        <h2 class="font-mono text-lg">Add host</h2>
        <button class="text-iso-text-muted hover:text-iso-text-primary" @click="$emit('close')">
          <Icon name="lucide:x" class="w-5 h-5" />
        </button>
      </header>

      <div v-if="!installCommand" class="space-y-3">
        <label class="block">
          <span class="text-xs uppercase tracking-wider text-iso-text-faint">Fleet</span>
          <select
            v-model="fleet"
            class="mt-1 w-full bg-iso-bg-elevated border border-iso-border-subtle rounded px-3 py-2 text-sm font-mono"
          >
            <option v-for="f in fleetsStore.fleets" :key="f.name" :value="f.name">{{ f.name }}</option>
          </select>
        </label>

        <label class="block">
          <span class="text-xs uppercase tracking-wider text-iso-text-faint">Hostname (optional)</span>
          <input
            v-model="hostname"
            type="text"
            placeholder="e.g. prod-04"
            class="mt-1 w-full bg-iso-bg-elevated border border-iso-border-subtle rounded px-3 py-2 text-sm font-mono"
          />
        </label>

        <button
          class="px-4 py-2 rounded border border-iso-border-subtle hover:border-iso-success hover:text-iso-success disabled:opacity-50"
          :disabled="loading"
          @click="generate"
        >
          {{ loading ? 'Generating...' : 'Generate install command' }}
        </button>

        <p v-if="error" class="text-xs text-iso-error">{{ error }}</p>
      </div>

      <div v-else class="space-y-3">
        <p class="text-sm text-iso-text-muted">
          Run this on the host you want to enroll. It will install the agent and contact this controller.
        </p>
        <pre class="text-xs font-mono bg-iso-bg-elevated rounded p-3 overflow-x-auto whitespace-pre-wrap">{{ installCommand }}</pre>
        <div class="flex items-center gap-2">
          <button class="px-3 py-1.5 text-sm rounded border border-iso-border-subtle hover:border-iso-success" @click="copy">
            Copy
          </button>
          <button class="px-3 py-1.5 text-sm rounded border border-iso-border-subtle" @click="$emit('close')">
            Done
          </button>
        </div>
      </div>
    </div>
  </div>
</template>
