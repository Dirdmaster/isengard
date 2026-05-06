<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useWebhooks, type WebhookDto, type WebhookCreatedDto } from '~/composables/useWebhooks'
import { useConfirm } from '~/composables/useConfirm'
import AddWebhookModal from '~/components/AddWebhookModal.vue'
import WebhookDeliveriesPanel from '~/components/WebhookDeliveriesPanel.vue'

/**
 * Body for the "Webhooks" settings tab. Mounted from
 * `pages/settings/index.vue`. Shipped in Phase 12a (#53).
 *
 * Shows the list of configured outbound webhooks plus a per-row deliveries
 * panel. The Add modal returns the plaintext secret exactly once and the
 * SecretFlash mode of the modal renders the copy step before the user
 * dismisses it.
 */

const { webhooks, loading, error, refresh, removeWebhook, updateWebhook, sendTest } =
  useWebhooks()
const { confirm } = useConfirm()
const toast = useToast()

onMounted(refresh)

const addOpen = ref(false)
const expandedId = ref<number | null>(null)

function toggleExpanded(id: number) {
  expandedId.value = expandedId.value === id ? null : id
}

async function handleCreated(_dto: WebhookCreatedDto) {
  await refresh()
}

async function handleRemove(w: WebhookDto) {
  const ok = await confirm({
    title: `Remove webhook?`,
    description: `Deletes ${w.url} and its delivery history. This cannot be undone.`,
    confirmText: 'Remove webhook',
    danger: true,
  })
  if (!ok) return
  try {
    await removeWebhook(w.id)
    toast.success('Webhook removed')
  } catch (e) {
    toast.error(`Remove failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function handleToggle(w: WebhookDto) {
  try {
    await updateWebhook(w.id, { enabled: !w.enabled })
    toast.success(w.enabled ? 'Webhook paused' : 'Webhook enabled')
  } catch (e) {
    toast.error(`Toggle failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function handleTest(w: WebhookDto) {
  try {
    await sendTest(w.id)
    toast.success('Test event queued')
  } catch (e) {
    toast.error(`Test failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

const isEmpty = computed(() => !loading.value && !error.value && webhooks.value.length === 0)

defineExpose({ openAdd: () => { addOpen.value = true } })
</script>

<template>
  <div class="flex flex-col gap-3">
    <div class="flex items-center justify-between">
      <p class="text-xs text-iso-text-muted">
        Outbound webhooks. Subscribed events POST to your URL with an
        <code class="text-iso-text-primary">X-Isengard-Signature</code> HMAC-SHA256 header.
      </p>
      <Button
        size="sm"
        variant="outline"
        class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
        @click="addOpen = true"
      >
        + Add webhook
      </Button>
    </div>

    <div
      v-if="loading && webhooks.length === 0"
      class="rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated px-4 py-6 text-center text-iso-text-muted text-xs"
    >
      Loading webhooks...
    </div>

    <div
      v-else-if="error"
      class="rounded-iso-md border border-iso-error/40 bg-iso-error-soft px-4 py-3 text-xs text-iso-error flex items-center justify-between gap-3"
    >
      <span>{{ error }}</span>
      <button
        class="px-2 py-1 rounded-iso-sm border border-iso-error/40 text-iso-error hover:bg-iso-error/10"
        @click="refresh"
      >Retry</button>
    </div>

    <template v-else>
      <div
        v-for="w in webhooks"
        :key="w.id"
        class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated"
      >
        <div class="px-4 py-3 flex items-center justify-between gap-4">
          <div class="flex items-center gap-3 min-w-0">
            <span
              :class="[
                'w-2 h-2 rounded-full shrink-0',
                w.enabled ? 'bg-iso-success' : 'bg-iso-text-muted',
              ]"
              :title="w.enabled ? 'enabled' : 'disabled'"
            />
            <div class="flex flex-col min-w-0">
              <span class="text-sm font-mono text-iso-text-primary truncate">{{ w.url }}</span>
              <span class="text-[11px] text-iso-text-muted">
                events: {{ w.eventKinds }} . secret {{ w.secretMasked }}
              </span>
            </div>
          </div>
          <div class="flex items-center gap-2 shrink-0">
            <Button size="sm" variant="ghost" @click="handleTest(w)">Test</Button>
            <Button size="sm" variant="ghost" @click="toggleExpanded(w.id)">
              {{ expandedId === w.id ? 'Hide' : 'Deliveries' }}
            </Button>
            <Button size="sm" variant="ghost" @click="handleToggle(w)">
              {{ w.enabled ? 'Disable' : 'Enable' }}
            </Button>
            <Button size="sm" variant="ghost" class="text-iso-error" @click="handleRemove(w)">
              Delete
            </Button>
          </div>
        </div>

        <WebhookDeliveriesPanel
          v-if="expandedId === w.id"
          :webhook-id="w.id"
          class="border-t border-iso-border-subtle"
        />
      </div>

      <div
        v-if="isEmpty"
        class="rounded-iso-lg border border-dashed border-iso-border-strong bg-iso-bg-elevated p-5 flex items-center justify-between gap-4"
      >
        <div class="flex flex-col gap-0.5">
          <span class="text-xs font-semibold text-iso-text-primary">No webhooks yet.</span>
          <span class="text-[11px] text-iso-text-muted">
            Subscribe an external endpoint to event kinds (e.g. update.success).
          </span>
        </div>
        <Button
          variant="outline"
          class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success shrink-0"
          @click="addOpen = true"
        >
          + Add your first webhook
        </Button>
      </div>
    </template>

    <AddWebhookModal v-model:open="addOpen" @created="handleCreated" />
  </div>
</template>
