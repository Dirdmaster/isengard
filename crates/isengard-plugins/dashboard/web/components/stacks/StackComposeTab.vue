<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useApi } from '~/composables/useApi'

interface Props {
  stackId: string
}
const props = defineProps<Props>()

interface ComposeResponse {
  stack_id: number
  stack_name: string
  compose_yaml: string
  sha256: string
  imported_at: string
}

const api = useApi()
const yaml = ref<string | null>(null)
const sha256 = ref<string | null>(null)
const importedAt = ref<string | null>(null)
const stackName = ref<string | null>(null)
const loading = ref(false)
const error = ref<string | null>(null)

async function load() {
  loading.value = true
  error.value = null
  try {
    const res = await fetch(`/api/v1/stacks/${props.stackId}/compose`)
    if (res.status === 204) {
      yaml.value = null
      sha256.value = null
      importedAt.value = null
      stackName.value = null
      return
    }
    if (!res.ok) {
      throw new Error(`HTTP ${res.status}`)
    }
    const data = (await res.json()) as ComposeResponse
    yaml.value = data.compose_yaml
    sha256.value = data.sha256
    importedAt.value = data.imported_at
    stackName.value = data.stack_name
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

onMounted(load)
watch(() => props.stackId, load)

const importedAtLabel = computed(() => {
  if (!importedAt.value) return null
  try {
    return new Date(importedAt.value).toLocaleString()
  } catch {
    return importedAt.value
  }
})

function copyToClipboard() {
  if (!yaml.value) return
  navigator.clipboard?.writeText(yaml.value).catch(() => {
    // Best-effort: clipboard API can be denied. The user can still
    // select-and-copy from the visible <pre> block.
  })
}
</script>

<template>
  <div class="p-6 flex flex-col gap-4">
    <div
      class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-4 flex items-start gap-3"
    >
      <div
        class="font-mono text-iso-text-secondary text-xs px-2 py-1 rounded bg-iso-bg-base border border-iso-border-strong shrink-0"
      >
        v0.3c
      </div>
      <div class="flex flex-col gap-1 text-sm leading-relaxed">
        <p class="text-iso-text-primary">
          Imported from running containers.
        </p>
        <p class="text-iso-text-muted text-xs">
          Some compose-only metadata is not represented (build context,
          env interpolation, comments, depends_on). Re-imports overwrite
          this view; manual edits to the on-disk file are refused without
          force.
        </p>
      </div>
    </div>

    <div v-if="loading" class="text-sm text-iso-text-muted">
      Loading compose.yaml...
    </div>

    <div
      v-else-if="error"
      class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-6 text-sm text-iso-error"
    >
      Failed to load compose.yaml: {{ error }}
    </div>

    <div
      v-else-if="!yaml"
      class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-6 flex flex-col items-center gap-3 text-center"
    >
      <div
        class="w-12 h-12 rounded-iso-lg border border-iso-border-strong bg-iso-bg-base flex items-center justify-center font-mono text-iso-text-secondary text-base font-semibold"
      >
        { }
      </div>
      <h2 class="font-mono text-base text-iso-text-primary">compose.yaml</h2>
      <p class="text-sm text-iso-text-muted max-w-md leading-relaxed">
        Compose import not yet run for this stack.
      </p>
      <p class="text-[11px] text-iso-text-faint max-w-md">
        The agent imports any
        <code class="font-mono text-xs text-iso-text-secondary">isengard.enable=true</code>
        stack on its next discovery sweep. Trigger manually via
        <code class="font-mono text-xs text-iso-text-secondary">isd compose import &lt;stack&gt;</code>
        (coming in v0.3d).
      </p>
    </div>

    <template v-else>
      <div class="flex items-center justify-between text-xs text-iso-text-faint">
        <div class="flex items-center gap-3">
          <span v-if="stackName" class="font-mono text-iso-text-secondary">
            {{ stackName }}/compose.yaml
          </span>
          <span v-if="importedAtLabel">imported {{ importedAtLabel }}</span>
        </div>
        <div class="flex items-center gap-2">
          <span v-if="sha256" class="font-mono">
            sha256: {{ sha256.slice(0, 12) }}
          </span>
          <button
            class="px-2 py-1 border border-iso-border-subtle rounded text-iso-text-muted hover:text-iso-text-primary hover:border-iso-border"
            @click="copyToClipboard"
          >
            Copy
          </button>
          <button
            class="px-2 py-1 border border-iso-border-subtle rounded text-iso-text-muted hover:text-iso-text-primary hover:border-iso-border"
            @click="load"
          >
            Refresh
          </button>
        </div>
      </div>

      <pre
        class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-4 font-mono text-xs text-iso-text-primary whitespace-pre overflow-auto"
        >{{ yaml }}</pre>
    </template>
  </div>
</template>
