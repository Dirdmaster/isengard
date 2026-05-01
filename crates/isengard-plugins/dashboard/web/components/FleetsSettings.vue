<script setup lang="ts">
import { ref } from 'vue'
import { useFleetsStore } from '~/stores/fleets'

const fleetsStore = useFleetsStore()
if (!fleetsStore.loaded) await fleetsStore.load()

const toast = useToast()
const newFleetName = ref('')
const error = ref('')

async function create() {
  error.value = ''
  try {
    const name = newFleetName.value.trim()
    await fleetsStore.create(name)
    toast.success(`Fleet "${name}" created`)
    newFleetName.value = ''
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    error.value = msg
    toast.error(`Create fleet failed: ${msg}`)
  }
}

async function remove(name: string) {
  error.value = ''
  try {
    await fleetsStore.remove(name)
    toast.success(`Fleet "${name}" deleted`)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    error.value = msg
    toast.error(`Delete fleet failed: ${msg}`)
  }
}
</script>

<template>
  <SettingsSection title="Fleets" description="User-defined tags for grouping hosts.">
    <div class="space-y-2 mb-4">
      <div
        v-for="f in fleetsStore.fleets"
        :key="f.name"
        class="flex items-center justify-between px-3 py-2 rounded border border-iso-border-subtle bg-iso-bg-elevated"
      >
        <div class="flex items-center gap-3">
          <span class="font-mono text-sm">{{ f.name }}</span>
          <span class="text-xs text-iso-text-muted">{{ f.host_count }} hosts</span>
        </div>
        <button
          v-if="f.name !== 'default'"
          class="text-xs text-iso-error hover:underline disabled:opacity-50"
          :disabled="f.host_count > 0"
          :title="f.host_count > 0 ? 'Cannot delete fleet with hosts' : ''"
          @click="remove(f.name)"
        >
          Delete
        </button>
      </div>
    </div>

    <form class="flex items-center gap-2" @submit.prevent="create">
      <input
        v-model="newFleetName"
        type="text"
        placeholder="new-fleet-name"
        class="bg-iso-bg-elevated border border-iso-border-subtle rounded px-3 py-1.5 text-sm font-mono w-64"
      />
      <button
        type="submit"
        class="px-3 py-1.5 text-sm rounded border border-iso-border-subtle hover:border-iso-success hover:text-iso-success disabled:opacity-50"
        :disabled="!newFleetName.trim()"
      >
        + New fleet
      </button>
    </form>

    <p v-if="error" class="text-xs text-iso-error mt-2">{{ error }}</p>
  </SettingsSection>
</template>
