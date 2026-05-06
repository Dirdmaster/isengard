<script setup lang="ts">
import { ref, watch } from 'vue'
import { useWebhooks, type WebhookCreatedDto } from '~/composables/useWebhooks'

/**
 * Add webhook modal.
 *
 * Two-stage flow:
 *   1. Form: URL + secret (with auto-generate fallback) + event kinds + enabled
 *   2. Secret flash: shows the plaintext secret with a copy button and a
 *      warning that it will not be shown again
 *
 * The secret stage is the only chance the operator gets to see the plaintext.
 * On dismiss, the modal returns to the form stage so a follow-up create starts
 * fresh.
 */

const props = defineProps<{ open: boolean }>()
const emit = defineEmits<{
  (e: 'update:open', v: boolean): void
  (e: 'created', dto: WebhookCreatedDto): void
}>()

const { createWebhook } = useWebhooks()
const toast = useToast()

const url = ref('')
const secret = ref('')
const eventKinds = ref('*')
const enabled = ref(true)
const submitting = ref(false)

const flashed = ref<WebhookCreatedDto | null>(null)

function reset() {
  url.value = ''
  secret.value = ''
  eventKinds.value = '*'
  enabled.value = true
  submitting.value = false
  flashed.value = null
}

watch(
  () => props.open,
  (v) => {
    if (!v) {
      // Defer reset so the closing transition doesn't flash empty fields.
      setTimeout(reset, 200)
    }
  },
)

async function onSubmit() {
  if (!url.value.trim()) {
    toast.error('URL is required')
    return
  }
  submitting.value = true
  try {
    const dto = await createWebhook({
      url: url.value.trim(),
      secret: secret.value.trim() || undefined,
      eventKinds: eventKinds.value.trim() || '*',
      enabled: enabled.value,
    })
    flashed.value = dto
    emit('created', dto)
  } catch (e) {
    toast.error(`Create failed: ${e instanceof Error ? e.message : String(e)}`)
  } finally {
    submitting.value = false
  }
}

async function copySecret() {
  if (!flashed.value) return
  try {
    await navigator.clipboard.writeText(flashed.value.secret)
    toast.success('Secret copied to clipboard')
  } catch {
    toast.error('Copy failed; select the text and copy manually')
  }
}

function close() {
  emit('update:open', false)
}
</script>

<template>
  <Dialog :open="open" @update:open="(v: boolean) => emit('update:open', v)">
    <DialogContent class="bg-iso-bg-base border-iso-border-subtle max-w-xl">
      <DialogHeader>
        <DialogTitle class="font-mono text-iso-text-primary">
          {{ flashed ? 'Webhook created' : 'Add webhook' }}
        </DialogTitle>
        <DialogDescription class="text-iso-text-muted">
          <span v-if="!flashed">
            Subscribe an external endpoint to controller events. The secret is
            shown once: copy it now.
          </span>
          <span v-else class="text-iso-warning">
            Copy this secret now: it will not be shown again.
          </span>
        </DialogDescription>
      </DialogHeader>

      <!-- Stage 1: form -->
      <form
        v-if="!flashed"
        class="flex flex-col gap-3 mt-2"
        @submit.prevent="onSubmit"
      >
        <label class="flex flex-col gap-1">
          <span class="text-[11px] font-semibold text-iso-text-muted">URL</span>
          <input
            v-model="url"
            type="url"
            required
            placeholder="https://example.com/hook"
            class="px-3 py-2 rounded-iso-sm bg-iso-bg-elevated border border-iso-border-subtle text-sm font-mono text-iso-text-primary"
          />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-[11px] font-semibold text-iso-text-muted">
            Secret (optional: auto-generated if empty)
          </span>
          <input
            v-model="secret"
            type="text"
            placeholder="leave blank to auto-generate"
            class="px-3 py-2 rounded-iso-sm bg-iso-bg-elevated border border-iso-border-subtle text-sm font-mono text-iso-text-primary"
          />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-[11px] font-semibold text-iso-text-muted">
            Event kinds (comma-separated, * for all)
          </span>
          <input
            v-model="eventKinds"
            type="text"
            class="px-3 py-2 rounded-iso-sm bg-iso-bg-elevated border border-iso-border-subtle text-sm font-mono text-iso-text-primary"
          />
        </label>
        <label class="flex items-center gap-2 mt-1">
          <input v-model="enabled" type="checkbox" class="accent-iso-success" />
          <span class="text-sm text-iso-text-primary">Enabled</span>
        </label>
        <DialogFooter class="mt-3">
          <Button type="button" variant="ghost" @click="close">Cancel</Button>
          <Button type="submit" :disabled="submitting">
            {{ submitting ? 'Creating...' : 'Create webhook' }}
          </Button>
        </DialogFooter>
      </form>

      <!-- Stage 2: secret flash -->
      <div v-else class="flex flex-col gap-3 mt-2">
        <div
          class="rounded-iso-md border border-iso-warning/40 bg-iso-warning-soft px-3 py-2 text-xs text-iso-warning"
        >
          This is the only time the secret is shown. Copy it now and store it
          alongside your receiver code.
        </div>
        <div class="flex flex-col gap-1">
          <span class="text-[11px] font-semibold text-iso-text-muted">Secret</span>
          <div
            class="flex items-stretch rounded-iso-sm bg-iso-bg-elevated border border-iso-border-subtle"
          >
            <code class="flex-1 px-3 py-2 text-sm font-mono text-iso-text-primary break-all">
              {{ flashed.secret }}
            </code>
            <button
              type="button"
              class="px-3 border-l border-iso-border-subtle text-xs text-iso-text-primary hover:bg-iso-bg-base"
              @click="copySecret"
            >
              Copy
            </button>
          </div>
        </div>
        <div class="flex flex-col gap-1 text-xs text-iso-text-muted">
          <span>URL: <code class="text-iso-text-primary">{{ flashed.url }}</code></span>
          <span>Events: <code class="text-iso-text-primary">{{ flashed.eventKinds }}</code></span>
        </div>
        <DialogFooter class="mt-3">
          <Button type="button" @click="close">Done</Button>
        </DialogFooter>
      </div>
    </DialogContent>
  </Dialog>
</template>
