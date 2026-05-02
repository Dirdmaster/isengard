<script setup lang="ts">
import { computed, ref } from 'vue'
import { useFleetsStore } from '~/stores/fleets'
import { useToast } from '~/composables/useToast'

const emit = defineEmits<{ close: [] }>()

const fleetsStore = useFleetsStore()
if (!fleetsStore.loaded) await fleetsStore.load()

const fleet = ref('')
const hostname = ref('')
const installCommand = ref('')
const loading = ref(false)
const error = ref('')
const toast = useToast()

const canGenerate = computed(() => fleet.value.trim().length > 0 && !loading.value)

async function generate() {
  if (!canGenerate.value) return
  loading.value = true
  error.value = ''
  try {
    const api = useApi()
    const dto = await api.post<{ install_command: string }>('/hosts', {
      fleet: fleet.value.trim(),
      hostname: hostname.value || undefined,
    })
    installCommand.value = dto.install_command
    await fleetsStore.load()
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
    toast.error('Copy failed: please copy manually')
  }
}

function handleOpenChange(v: boolean) {
  if (!v) emit('close')
}
</script>

<template>
  <Dialog :open="true" @update:open="handleOpenChange">
    <DialogContent class="bg-iso-bg-base border-iso-border-subtle sm:max-w-[640px]">
      <DialogHeader>
        <DialogTitle class="font-mono text-iso-text-primary">Add host</DialogTitle>
        <DialogDescription v-if="!installCommand" class="text-iso-text-muted">
          Generate a one-time install command. Run it on the host to enroll.
        </DialogDescription>
        <DialogDescription v-else class="text-iso-text-muted">
          Run this on the host you want to enroll. It will install the agent and contact this controller.
        </DialogDescription>
      </DialogHeader>

      <!-- Form (pre-generation) -->
      <div v-if="!installCommand" class="space-y-4">
        <div class="space-y-1.5">
          <Label for="fleet" class="text-xs uppercase tracking-wider text-iso-text-faint">
            Fleet <span class="text-iso-error">*</span>
          </Label>
          <Input
            id="fleet"
            v-model="fleet"
            placeholder="prod, homelab, edge…"
            list="existing-fleets"
            autofocus
            class="font-mono bg-iso-bg-elevated border-iso-border-subtle"
          />
          <datalist id="existing-fleets">
            <option v-for="f in fleetsStore.fleets" :key="f.name" :value="f.name" />
          </datalist>
        </div>

        <div class="space-y-1.5">
          <Label for="hostname" class="text-xs uppercase tracking-wider text-iso-text-faint">Hostname (optional)</Label>
          <Input
            id="hostname"
            v-model="hostname"
            placeholder="e.g. prod-04"
            class="font-mono bg-iso-bg-elevated border-iso-border-subtle"
          />
        </div>

        <p v-if="error" class="text-xs text-iso-error">{{ error }}</p>
      </div>

      <!-- Result (post-generation) -->
      <div v-else class="space-y-3">
        <pre class="text-xs font-mono bg-iso-bg-elevated border border-iso-border-subtle rounded-md p-3 overflow-x-auto whitespace-pre-wrap text-iso-text-primary">{{ installCommand }}</pre>
      </div>

      <DialogFooter v-if="!installCommand">
        <Button variant="ghost" @click="emit('close')">Cancel</Button>
        <Button
          variant="outline"
          :disabled="!canGenerate"
          class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success disabled:opacity-40"
          @click="generate"
        >
          {{ loading ? 'Generating…' : 'Generate install command' }}
        </Button>
      </DialogFooter>

      <DialogFooter v-else>
        <Button variant="ghost" @click="emit('close')">Done</Button>
        <Button
          variant="outline"
          class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
          @click="copy"
        >
          Copy command
        </Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
