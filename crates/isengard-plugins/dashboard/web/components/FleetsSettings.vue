<script setup lang="ts">
import { ref } from 'vue'
import { useFleetsStore } from '~/stores/fleets'

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
  <SettingsSection title="Fleets" description="User-defined tags for grouping hosts.">
    <div class="space-y-2 mb-4">
      <div
        v-for="f in fleetsStore.fleets"
        :key="f.name"
        class="flex items-center justify-between px-4 py-3 rounded-md border border-iso-border-subtle bg-iso-bg-elevated"
      >
        <div class="flex items-center gap-3">
          <span class="font-mono text-sm text-iso-text-primary">{{ f.name }}</span>
          <span class="text-xs text-iso-text-muted">{{ f.host_count }} {{ f.host_count === 1 ? 'host' : 'hosts' }}</span>
        </div>
        <Badge
          v-if="f.name === 'default'"
          variant="outline"
          class="text-iso-text-faint border-iso-border-subtle uppercase tracking-wider text-[10px]"
        >
          system
        </Badge>
        <Button
          v-else
          variant="ghost"
          size="sm"
          class="text-iso-error hover:text-iso-error hover:bg-iso-error/10"
          :disabled="f.host_count > 0"
          :title="f.host_count > 0 ? 'Cannot delete fleet with hosts' : ''"
          @click="remove(f.name)"
        >
          Delete
        </Button>
      </div>
    </div>

    <form class="flex items-center gap-2" @submit.prevent="create">
      <Input
        v-model="newFleetName"
        type="text"
        placeholder="new-fleet-name"
        class="font-mono w-64 bg-iso-bg-elevated border-iso-border-subtle"
      />
      <Button
        type="submit"
        variant="outline"
        class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
        :disabled="!newFleetName.trim()"
      >
        + New fleet
      </Button>
    </form>

    <p v-if="error" class="text-xs text-iso-error mt-2">{{ error }}</p>
  </SettingsSection>
</template>
