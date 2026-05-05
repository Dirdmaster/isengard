<script setup lang="ts">
import { computed, ref } from 'vue'
import { useEnrollment, type MintedToken } from '~/composables/useEnrollment'
import { useToast } from '~/composables/useToast'

const emit = defineEmits<{ close: []; minted: [] }>()

const { mint } = useEnrollment()
const toast = useToast()

const ttlMinutes = ref(15)
const loading = ref(false)
const error = ref('')
const minted = ref<MintedToken | null>(null)

const ttlValid = computed(() => ttlMinutes.value >= 1 && ttlMinutes.value <= 1440)
const canSubmit = computed(() => ttlValid.value && !loading.value)

/**
 * Best-effort controller URL for the docker run command. The dashboard
 * usually serves on the controller's HTTP port; the agent talks to the gRPC
 * endpoint on `:9417`. We swap whatever current port the dashboard is on for
 * `:9417` and force `https://`. Operators in fancier setups (reverse proxy,
 * non-default port) will need to edit the env var by hand: that's flagged
 * underneath the snippet.
 */
const controllerUrl = computed(() => {
  if (typeof window === 'undefined') return 'https://controller-host:9417'
  const origin = window.location.origin
  return origin.replace(/^http/, 'https').replace(/:\d+$/, ':9417')
})

const dockerRunCommand = computed(() => {
  const tokenValue = minted.value?.token ?? '<token-value>'
  return `docker run -d --name isengard-agent --restart=always \\
  --platform linux/amd64 \\
  -v /var/run/docker.sock:/var/run/docker.sock \\
  -v isengard-agent-data:/var/lib/isengard \\
  -e ISENGARD_CONTROLLER=${controllerUrl.value} \\
  -e ISENGARD_ENROLL_TOKEN=${tokenValue} \\
  ghcr.io/dirdmaster/isengard-agent:next`
})

async function submit() {
  if (!canSubmit.value) return
  loading.value = true
  error.value = ''
  try {
    minted.value = await mint('agent', ttlMinutes.value * 60)
    emit('minted')
    toast.info(`Token minted. Expires in ${ttlMinutes.value} min — copy it now.`)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    error.value = msg
    toast.error(`Mint failed: ${msg}`)
  } finally {
    loading.value = false
  }
}

async function copyCommand() {
  try {
    await navigator.clipboard.writeText(dockerRunCommand.value)
    toast.success('Docker run command copied to clipboard')
  } catch {
    toast.error('Copy failed: please copy manually')
  }
}

async function copyToken() {
  if (!minted.value) return
  try {
    await navigator.clipboard.writeText(minted.value.token)
    toast.success('Token copied to clipboard')
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
    <DialogContent class="bg-iso-bg-base border-iso-border-subtle sm:max-w-[680px]">
      <DialogHeader>
        <DialogTitle class="font-mono text-iso-text-primary">Mint enrollment token</DialogTitle>
        <DialogDescription v-if="!minted" class="text-iso-text-muted">
          Generates a single-use token an agent can redeem to enroll. Plaintext is shown only once.
        </DialogDescription>
        <DialogDescription v-else class="text-iso-text-muted">
          Copy the docker run snippet below and paste it on the host you want to enroll. The token
          is shown once: if you lose it, mint a new one.
        </DialogDescription>
      </DialogHeader>

      <!-- Form (pre-mint) -->
      <div v-if="!minted" class="space-y-4">
        <div class="space-y-1.5">
          <Label for="ttl" class="text-xs uppercase tracking-wider text-iso-text-faint">
            TTL (minutes)
          </Label>
          <Input
            id="ttl"
            v-model.number="ttlMinutes"
            type="number"
            min="1"
            max="1440"
            autofocus
            class="font-mono bg-iso-bg-elevated border-iso-border-subtle"
          />
          <p class="text-xs text-iso-text-faint">
            1–1440 minutes (max 24 h). Default 15 min.
          </p>
        </div>

        <p v-if="!ttlValid" class="text-xs text-iso-error">
          TTL must be between 1 and 1440 minutes.
        </p>
        <p v-if="error" class="text-xs text-iso-error">{{ error }}</p>
      </div>

      <!-- Result (post-mint) -->
      <div v-else class="space-y-4">
        <div class="space-y-1.5">
          <div class="flex items-center justify-between">
            <Label class="text-xs uppercase tracking-wider text-iso-text-faint">Token</Label>
            <button
              class="text-xs text-iso-text-muted hover:text-iso-text-primary underline"
              @click="copyToken"
            >
              Copy token only
            </button>
          </div>
          <pre class="text-xs font-mono bg-iso-bg-elevated border border-iso-border-subtle rounded-md p-3 overflow-x-auto whitespace-pre-wrap text-iso-text-primary">{{ minted.token }}</pre>
          <p class="text-xs text-iso-text-faint">
            Expires {{ new Date(minted.expires_at).toLocaleString() }}
          </p>
        </div>

        <div class="space-y-1.5">
          <Label class="text-xs uppercase tracking-wider text-iso-text-faint">
            Docker run command
          </Label>
          <pre class="text-xs font-mono bg-iso-bg-elevated border border-iso-border-subtle rounded-md p-3 overflow-x-auto whitespace-pre-wrap text-iso-text-primary">{{ dockerRunCommand }}</pre>
          <p class="text-xs text-iso-text-faint">
            Edit <span class="font-mono">ISENGARD_CONTROLLER</span> if your controller is reachable on a different host or port.
          </p>
        </div>
      </div>

      <DialogFooter v-if="!minted">
        <Button variant="ghost" @click="emit('close')">Cancel</Button>
        <Button
          variant="outline"
          :disabled="!canSubmit"
          class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success disabled:opacity-40"
          @click="submit"
        >
          {{ loading ? 'Minting…' : 'Mint token' }}
        </Button>
      </DialogFooter>

      <DialogFooter v-else>
        <Button variant="ghost" @click="emit('close')">Done</Button>
        <Button
          variant="outline"
          class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
          @click="copyCommand"
        >
          Copy docker command
        </Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
