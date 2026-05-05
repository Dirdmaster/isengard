<script setup lang="ts">
import { ref } from 'vue'
import { useFleetsStore } from '~/stores/fleets'

/**
 * Fleets card — aligned to `design/concepts/settings-general/v1.html` card
 * chrome (elevated card with small-caps section header). Fleets aren't in
 * the concept directly, but they're the only General-tab content we have
 * backend support for; rendering them as a section card keeps the page
 * visually consistent with the concept.
 */

const fleetsStore = useFleetsStore()
if (!fleetsStore.loaded) await fleetsStore.load()

const newFleetName = ref('')
const error = ref('')
const toast = useToast()

async function create() {
  error.value = ''
  try {
    const name = newFleetName.value.trim()
    if (!name) return
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
  const { confirm } = useConfirm()
  const ok = await confirm({
    title: `Delete fleet "${name}"?`,
    description: 'The fleet tag is removed. Hosts assigned to this fleet must be reassigned first.',
    confirmText: 'Delete',
    danger: true,
  })
  if (!ok) return

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
  <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-5 flex flex-col gap-4">
    <div class="flex items-center justify-between">
      <span class="text-[10px] font-semibold text-iso-text-muted tracking-widest">FLEETS</span>
      <span class="text-[11px] text-iso-text-faint">User-defined tags for grouping hosts</span>
    </div>

    <div class="space-y-2">
      <div
        v-for="f in fleetsStore.fleets"
        :key="f.name"
        class="flex items-center justify-between px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border-subtle"
      >
        <div class="flex items-center gap-3">
          <span class="font-mono text-xs text-iso-text-primary">{{ f.name }}</span>
          <span class="text-[11px] text-iso-text-muted">{{ f.host_count }} {{ f.host_count === 1 ? 'host' : 'hosts' }}</span>
        </div>
        <span
          v-if="f.name === 'default'"
          class="px-1.5 py-0.5 rounded-iso-sm border border-iso-border-subtle font-mono text-[10px] text-iso-text-faint uppercase tracking-wider"
        >
          system
        </span>
        <button
          v-else
          class="px-2 py-1 rounded-iso-sm text-[11px] text-iso-error hover:bg-iso-error/10 disabled:opacity-40 disabled:cursor-not-allowed"
          :disabled="f.host_count > 0"
          :title="f.host_count > 0 ? 'Cannot delete fleet with hosts' : ''"
          @click="remove(f.name)"
        >
          Delete
        </button>
      </div>
    </div>

    <form class="flex items-center gap-2" @submit.prevent="create">
      <Input
        v-model="newFleetName"
        type="text"
        placeholder="new-fleet-name"
        class="font-mono w-64 bg-iso-bg-base border-iso-border-subtle text-xs"
      />
      <Button
        type="submit"
        variant="outline"
        size="sm"
        class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success text-xs"
        :disabled="!newFleetName.trim()"
      >
        + New fleet
      </Button>
    </form>

    <p v-if="error" class="text-xs text-iso-error">{{ error }}</p>
  </section>
</template>
