<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'

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

interface ServiceOp {
  kind: 'start' | 'recreate' | 'stop' | 'no_change'
  service: string
  image?: string
  reasons?: string[]
}

interface ReconcilePlan {
  stack: string
  ops: ServiceOp[]
}

interface ConflictBody {
  error: string
  current_sha256: string
  current_yaml: string
}

const yaml = ref<string | null>(null)
const draft = ref<string>('')
const sha256 = ref<string | null>(null)
const importedAt = ref<string | null>(null)
const stackName = ref<string | null>(null)
const loading = ref(false)
const error = ref<string | null>(null)
const editing = ref(false)
const saving = ref(false)
const plan = ref<ReconcilePlan | null>(null)
const conflict = ref<ConflictBody | null>(null)
const flash = ref<string | null>(null)

async function load() {
  loading.value = true
  error.value = null
  try {
    const res = await fetch(`/api/v1/stacks/${props.stackId}/compose`)
    if (res.status === 204) {
      yaml.value = null
      draft.value = ''
      sha256.value = null
      importedAt.value = null
      stackName.value = null
      return
    }
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
    const data = (await res.json()) as ComposeResponse
    yaml.value = data.compose_yaml
    draft.value = data.compose_yaml
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
  try { return new Date(importedAt.value).toLocaleString() }
  catch { return importedAt.value }
})

const dirty = computed(() => editing.value && draft.value !== yaml.value)

function copyToClipboard() {
  if (!yaml.value) return
  navigator.clipboard?.writeText(yaml.value).catch(() => {})
}

function startEdit() {
  draft.value = yaml.value ?? ''
  editing.value = true
  plan.value = null
  conflict.value = null
  flash.value = null
}

function cancelEdit() {
  editing.value = false
  draft.value = yaml.value ?? ''
  plan.value = null
  conflict.value = null
}

async function previewPlan() {
  plan.value = null
  conflict.value = null
  try {
    const res = await fetch(`/api/v1/stacks/${props.stackId}/diff`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/yaml' },
      body: draft.value,
    })
    if (!res.ok) {
      const txt = await res.text()
      throw new Error(`HTTP ${res.status}: ${txt}`)
    }
    plan.value = (await res.json()) as ReconcilePlan
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}

async function save(force = false) {
  saving.value = true
  conflict.value = null
  error.value = null
  try {
    const url = force
      ? `/api/v1/stacks/${props.stackId}/compose?force=true`
      : `/api/v1/stacks/${props.stackId}/compose`
    const headers: Record<string, string> = { 'Content-Type': 'application/yaml' }
    if (sha256.value) headers['If-Match'] = sha256.value
    const res = await fetch(url, { method: 'PUT', headers, body: draft.value })
    if (res.status === 409) {
      conflict.value = (await res.json()) as ConflictBody
      return
    }
    if (!res.ok) {
      const txt = await res.text()
      throw new Error(`HTTP ${res.status}: ${txt}`)
    }
    const ok = (await res.json()) as { written_sha256: string }
    flash.value = `Saved (sha256 ${ok.written_sha256.slice(0, 12)}). Reconcile in progress on the host.`
    editing.value = false
    plan.value = null
    await load()
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    saving.value = false
  }
}

function reloadFromConflict() {
  if (!conflict.value) return
  draft.value = conflict.value.current_yaml
  sha256.value = conflict.value.current_sha256
  yaml.value = conflict.value.current_yaml
  conflict.value = null
}

function forceOverwrite() {
  if (!confirm('Force overwrite the on-disk file? Concurrent operator edits will be lost.')) return
  save(true)
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
        v0.3d
      </div>
      <div class="flex flex-col gap-1 text-sm leading-relaxed">
        <p class="text-iso-text-primary">
          Compose-as-truth: edits here, in <code class="font-mono">isd edit</code>, and
          <code class="font-mono">vim</code> on the host all converge on the same
          <code class="font-mono">compose.yaml</code>.
        </p>
        <p class="text-iso-text-muted text-xs">
          Saving rewrites the file in canonical form: comments and key ordering may
          change. The agent reconciles running containers against the new file
          immediately after save.
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
      {{ error }}
      <button class="ml-2 underline" @click="error = null">Dismiss</button>
    </div>

    <div
      v-if="flash"
      class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-3 text-sm text-iso-text-primary"
    >
      {{ flash }}
      <button class="ml-2 text-iso-text-muted underline" @click="flash = null">Dismiss</button>
    </div>

    <div
      v-if="conflict"
      class="rounded-iso-lg border border-iso-border-strong bg-iso-bg-elevated p-4 text-sm flex flex-col gap-3"
    >
      <div class="font-semibold text-iso-text-primary">
        Conflict: the on-disk file changed under you.
      </div>
      <p class="text-xs text-iso-text-muted">
        {{ conflict.error }} (current sha256: {{ conflict.current_sha256.slice(0, 12) }}).
      </p>
      <div class="flex gap-2">
        <button
          class="px-3 py-1 border border-iso-border-subtle rounded hover:border-iso-border"
          @click="reloadFromConflict"
        >
          Reload (discard mine)
        </button>
        <button
          class="px-3 py-1 border border-iso-border-subtle rounded text-iso-error hover:border-iso-error"
          @click="forceOverwrite"
        >
          Force overwrite
        </button>
        <button
          class="px-3 py-1 border border-iso-border-subtle rounded hover:border-iso-border"
          @click="conflict = null"
        >
          Keep editing
        </button>
      </div>
    </div>

    <div
      v-if="!loading && !yaml && !editing"
      class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-6 flex flex-col items-center gap-3 text-center"
    >
      <div
        class="w-12 h-12 rounded-iso-lg border border-iso-border-strong bg-iso-bg-base flex items-center justify-center font-mono text-iso-text-secondary text-base font-semibold"
      >
        { }
      </div>
      <h2 class="font-mono text-base text-iso-text-primary">compose.yaml</h2>
      <p class="text-sm text-iso-text-muted max-w-md leading-relaxed">
        No compose imported yet. Edit one in to bootstrap the stack from this view.
      </p>
      <button
        class="px-3 py-1 border border-iso-border-subtle rounded hover:border-iso-border"
        @click="startEdit"
      >
        Start editing
      </button>
    </div>

    <template v-else-if="!loading && yaml">
      <div class="flex items-center justify-between text-xs text-iso-text-faint">
        <div class="flex items-center gap-3">
          <span v-if="stackName" class="font-mono text-iso-text-secondary">
            {{ stackName }}/compose.yaml
          </span>
          <span v-if="importedAtLabel">imported {{ importedAtLabel }}</span>
          <span v-if="dirty" class="text-iso-text-primary">(unsaved)</span>
        </div>
        <div class="flex items-center gap-2">
          <span v-if="sha256" class="font-mono">sha256: {{ sha256.slice(0, 12) }}</span>
          <template v-if="!editing">
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
            <button
              class="px-2 py-1 border border-iso-border-subtle rounded text-iso-text-primary hover:border-iso-border-strong"
              @click="startEdit"
            >
              Edit
            </button>
          </template>
          <template v-else>
            <button
              class="px-2 py-1 border border-iso-border-subtle rounded text-iso-text-muted hover:text-iso-text-primary hover:border-iso-border"
              :disabled="saving"
              @click="cancelEdit"
            >
              Cancel
            </button>
            <button
              class="px-2 py-1 border border-iso-border-subtle rounded text-iso-text-muted hover:text-iso-text-primary hover:border-iso-border"
              :disabled="saving"
              @click="previewPlan"
            >
              Apply preview
            </button>
            <button
              class="px-2 py-1 border border-iso-border-subtle rounded text-iso-text-primary hover:border-iso-border-strong"
              :disabled="saving || !dirty"
              @click="save(false)"
            >
              {{ saving ? 'Saving…' : 'Save' }}
            </button>
          </template>
        </div>
      </div>

      <textarea
        v-if="editing"
        v-model="draft"
        class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-4 font-mono text-xs text-iso-text-primary min-h-[400px] focus:outline-none focus:border-iso-border-strong"
        spellcheck="false"
      />
      <pre
        v-else
        class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-4 font-mono text-xs text-iso-text-primary whitespace-pre overflow-auto"
        >{{ yaml }}</pre>

      <div
        v-if="plan"
        class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-4 text-xs flex flex-col gap-2"
      >
        <div class="font-semibold text-iso-text-primary">
          Reconcile plan ({{ plan.ops.length }} ops)
        </div>
        <ul class="font-mono flex flex-col gap-1">
          <li v-for="op in plan.ops" :key="op.service">
            <template v-if="op.kind === 'no_change'">
              <span class="text-iso-text-muted">~ {{ op.service }}: no change</span>
            </template>
            <template v-else-if="op.kind === 'start'">
              <span class="text-iso-text-primary">+ {{ op.service }}: start ({{ op.image }})</span>
            </template>
            <template v-else-if="op.kind === 'recreate'">
              <span class="text-iso-text-primary">! {{ op.service }}: recreate ({{ op.image }})</span>
              <ul v-if="op.reasons?.length" class="ml-4 text-iso-text-muted">
                <li v-for="r in op.reasons" :key="r">{{ r }}</li>
              </ul>
            </template>
            <template v-else-if="op.kind === 'stop'">
              <span class="text-iso-text-primary">- {{ op.service }}: stop</span>
            </template>
          </li>
        </ul>
      </div>
    </template>
  </div>
</template>
