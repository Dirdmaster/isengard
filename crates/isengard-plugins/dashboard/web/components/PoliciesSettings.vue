<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { usePolicies, type PolicyDto, type PolicyScopeType } from '~/composables/usePolicies'
import { useConfirm } from '~/composables/useConfirm'
import { useToast } from '~/composables/useToast'
import PolicyRow from '~/components/policies/PolicyRow.vue'
import PolicyEditor from '~/components/policies/PolicyEditor.vue'

/**
 * Body for the "Update policies" settings tab. Mounted from
 * `pages/settings/index.vue` (tab strip) and `pages/settings/policies.vue`
 * (direct route) so both URLs render the same UI.
 *
 * Owns:
 *   - Composable lifecycle (refresh on mount)
 *   - Implicit Global Default synthesis
 *   - Editor modal + remove confirm + resume action
 *
 * The PageHeader CTA lives on the parent so the empty-state in-container CTA
 * stays the only "add policy" affordance inside this component.
 */

const { policies, sorted, loading, error, refresh, removePolicy, clearPaused } = usePolicies()
const { confirm } = useConfirm()
const toast = useToast()

onMounted(refresh)

// ---- Editor modal state ----------------------------------------------------

const editorOpen = ref(false)
const editorMode = ref<'create' | 'edit'>('create')
const editorTarget = ref<PolicyDto | undefined>(undefined)

function openCreateEditor() {
  editorMode.value = 'create'
  editorTarget.value = undefined
  editorOpen.value = true
}

function openEditEditor(policy: PolicyDto) {
  editorMode.value = 'edit'
  editorTarget.value = policy
  editorOpen.value = true
}

function closeEditor() {
  editorOpen.value = false
  editorTarget.value = undefined
}

async function onEditorSaved() {
  closeEditor()
  await refresh()
}

// Expose to parent so the page-header "+ Add policy" button can drive the
// same modal without duplicating the editor mount.
defineExpose({ openCreateEditor })

// ---- Implicit Global Default ----------------------------------------------

const hasGlobalRow = computed(() =>
  sorted.value.some(p => p.scopeType === 'global'),
)

const syntheticGlobal: PolicyDto = {
  id: 0,
  scopeType: 'global' as PolicyScopeType,
  scopeKey: '',
  body: {},
  createdAt: '',
  updatedAt: '',
}

const rows = computed<PolicyDto[]>(() => {
  if (hasGlobalRow.value) return sorted.value
  return [syntheticGlobal, ...sorted.value]
})

const isEmpty = computed(() =>
  !hasGlobalRow.value && sorted.value.length === 0,
)

// ---- Row actions -----------------------------------------------------------

function handleEdit(policy: PolicyDto) {
  if (policy.id === 0) {
    editorMode.value = 'create'
    editorTarget.value = policy
    editorOpen.value = true
    return
  }
  openEditEditor(policy)
}

async function handleRemove(policy: PolicyDto) {
  const label =
    policy.scopeType === 'global'
      ? 'Global default'
      : `${policy.scopeType.toUpperCase()} . ${policy.scopeKey}`
  const ok = await confirm({
    title: `Remove policy ${label}?`,
    description:
      'The override is deleted; affected services fall back to the next less-specific scope. This does not undo updates already applied.',
    confirmText: 'Remove policy',
    danger: true,
  })
  if (!ok) return
  try {
    await removePolicy(policy.scopeType, policy.scopeKey)
    toast.success(`Removed policy ${label}`)
  } catch (e) {
    toast.error(`Remove failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function handleResume(policy: PolicyDto) {
  const label = `${policy.scopeType.toUpperCase()} . ${policy.scopeKey}`
  try {
    await clearPaused(policy.scopeType, policy.scopeKey)
    toast.success(`Resumed ${label}`)
  } catch (e) {
    toast.error(`Resume failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}
</script>

<template>
  <div class="flex flex-col gap-3">
    <!-- Loading -->
    <div
      v-if="loading && policies.length === 0"
      class="rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated px-4 py-6 text-center text-iso-text-muted text-xs"
    >
      Loading policies...
    </div>

    <!-- Error -->
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

    <!-- Stable list -->
    <template v-else>
      <PolicyRow
        v-for="row in rows"
        :key="`${row.scopeType}:${row.scopeKey || 'global'}`"
        :policy="row"
        :implicit-default="row.id === 0"
        @edit="handleEdit"
        @remove="handleRemove"
        @resume="handleResume"
      />

      <!-- Empty state: in-container CTA per empty-state convention. -->
      <div
        v-if="isEmpty"
        class="rounded-iso-lg border border-dashed border-iso-border-strong bg-iso-bg-elevated p-5 flex items-center justify-between gap-4"
      >
        <div class="flex flex-col gap-0.5">
          <span class="text-xs font-semibold text-iso-text-primary">No overrides yet.</span>
          <span class="text-[11px] text-iso-text-muted">
            Add a fleet, stack, or service policy to start customizing.
          </span>
        </div>
        <Button
          variant="outline"
          class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success shrink-0"
          @click="openCreateEditor"
        >
          + Add policy
        </Button>
      </div>
    </template>

    <!-- Editor modal: T6 owns the body, T5 owns the lifecycle. -->
    <Dialog :open="editorOpen" @update:open="(v: boolean) => { if (!v) closeEditor() }">
      <DialogContent class="bg-iso-bg-base border-iso-border-subtle max-w-xl">
        <DialogHeader>
          <DialogTitle class="font-mono text-iso-text-primary">
            {{ editorMode === 'create' ? 'Add policy' : 'Edit policy' }}
          </DialogTitle>
          <DialogDescription class="text-iso-text-muted">
            <span v-if="editorMode === 'edit' && editorTarget">
              {{ editorTarget.scopeType.toUpperCase() }} . {{ editorTarget.scopeKey || 'global default' }}
            </span>
            <span v-else>
              Pick a scope and override only the fields that should differ from the inherited policy.
            </span>
          </DialogDescription>
        </DialogHeader>
        <PolicyEditor
          :mode="editorMode"
          :existing="editorTarget"
          @close="closeEditor"
          @saved="onEditorSaved"
        />
      </DialogContent>
    </Dialog>
  </div>
</template>
