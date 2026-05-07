<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import EffectivePolicyPreview from '~/components/policies/EffectivePolicyPreview.vue'
import PolicyRow from '~/components/policies/PolicyRow.vue'
import PolicyEditor from '~/components/policies/PolicyEditor.vue'
import { usePolicies, type PolicyDto } from '~/composables/usePolicies'
import { useConfirm } from '~/composables/useConfirm'
import { useToast } from '~/composables/useToast'

/**
 * Stack-detail Settings tab: shows the resolved policy that currently applies
 * to this stack and lets operators add / edit / remove a stack-scope override.
 *
 * Phase 9 shipped the data model + REST endpoints + the global Settings to
 * Policies page, but the per-stack tab was left as a placeholder. This wires
 * the same composables + components used by `pages/settings/policies.vue` to
 * the stack scope so operators can override without leaving the stack view.
 *
 * Filtering is local: `usePolicies()` returns every row in the database, and
 * we narrow to `scope=stack` with `scope_key = "<fleet>/<stack-name>"`. That
 * matches the storage convention enforced by `PolicyEditor`'s validator and
 * the resolver (`resolve_policy` walks scopes by exact key match).
 */

interface Props {
  stackId: string
  hostId: string
  stackName: string
  fleet: string
}
const props = defineProps<Props>()

const { policies, refresh, removePolicy, clearPaused, loading, error } = usePolicies()
const { confirm } = useConfirm()
const toast = useToast()

onMounted(refresh)

/** Stack-scope storage key: `<fleet>/<stack-name>`. */
const stackScopeKey = computed(() => `${props.fleet}/${props.stackName}`)

const overrides = computed<PolicyDto[]>(() =>
  policies.value.filter(
    p => p.scopeType === 'stack' && p.scopeKey === stackScopeKey.value,
  ),
)

// ---- Editor lifecycle ------------------------------------------------------

const editorOpen = ref(false)
const editorMode = ref<'create' | 'edit'>('create')
const editorTarget = ref<PolicyDto | undefined>(undefined)

function openCreate() {
  editorMode.value = 'create'
  editorTarget.value = undefined
  editorOpen.value = true
}

function openEdit(policy: PolicyDto) {
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

// ---- Row actions -----------------------------------------------------------

async function handleRemove(policy: PolicyDto) {
  const ok = await confirm({
    title: `Remove stack override for ${props.stackName}?`,
    description:
      'The override is deleted; this stack falls back to the next less-specific scope (fleet, then global default). Already-applied updates are not undone.',
    confirmText: 'Remove override',
    danger: true,
  })
  if (!ok) return
  try {
    await removePolicy(policy.scopeType, policy.scopeKey)
    toast.success(`Removed stack override`)
  } catch (e) {
    toast.error(`Remove failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function handleResume(policy: PolicyDto) {
  try {
    await clearPaused(policy.scopeType, policy.scopeKey)
    toast.success(`Resumed updates for ${props.stackName}`)
  } catch (e) {
    toast.error(`Resume failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}
</script>

<template>
  <div class="p-6 max-w-5xl mx-auto w-full flex flex-col gap-6">
    <!-- Effective policy section ------------------------------------------- -->
    <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden">
      <header class="px-4 py-3 border-b border-iso-border-subtle flex items-center justify-between">
        <div class="flex flex-col gap-0.5">
          <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
            APPLIED TO THIS STACK
          </span>
          <span class="text-[11px] text-iso-text-muted">
            Resolved by walking scopes: container, then service, then stack, then fleet, then global default.
          </span>
        </div>
        <span class="font-mono text-[11px] text-iso-text-faint">
          {{ fleet }} / {{ stackName }}
        </span>
      </header>
      <EffectivePolicyPreview
        :fleet="fleet"
        :stack="stackName"
        :host_id="hostId"
      />
    </section>

    <!-- Stack-scope override section --------------------------------------- -->
    <section class="flex flex-col gap-3">
      <header class="flex flex-col gap-0.5">
        <h2 class="text-sm font-semibold text-iso-text-primary font-mono">
          Stack override
        </h2>
        <p class="text-[11px] text-iso-text-muted">
          Override the policy fields that should differ from the inherited fleet or global default for this stack.
        </p>
      </header>

      <div
        v-if="loading && policies.length === 0"
        class="rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated px-4 py-6 text-center text-iso-text-muted text-xs"
      >
        Loading policies...
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
        <PolicyRow
          v-for="row in overrides"
          :key="`${row.scopeType}:${row.scopeKey}`"
          :policy="row"
          @edit="openEdit"
          @remove="handleRemove"
          @resume="handleResume"
        />

        <div
          v-if="overrides.length === 0"
          class="rounded-iso-lg border border-dashed border-iso-border-strong bg-iso-bg-elevated p-5 flex items-center justify-between gap-4"
        >
          <div class="flex flex-col gap-0.5">
            <span class="text-xs font-semibold text-iso-text-primary">
              No stack override yet.
            </span>
            <span class="text-[11px] text-iso-text-muted">
              This stack inherits its policy from the fleet (or global default if no fleet rule exists).
            </span>
          </div>
          <Button
            variant="outline"
            class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success shrink-0"
            @click="openCreate"
          >
            + Add stack override
          </Button>
        </div>
      </template>
    </section>

    <!-- Editor modal: re-uses the global PolicyEditor with scope locked to
         this stack so the radio group stays out of the way. -->
    <Dialog :open="editorOpen" @update:open="(v: boolean) => { if (!v) closeEditor() }">
      <DialogContent class="bg-iso-bg-base border-iso-border-subtle max-w-xl">
        <DialogHeader>
          <DialogTitle class="font-mono text-iso-text-primary">
            {{ editorMode === 'create' ? 'Add stack override' : 'Edit stack override' }}
          </DialogTitle>
          <DialogDescription class="text-iso-text-muted">
            STACK . {{ stackScopeKey }}
          </DialogDescription>
        </DialogHeader>
        <PolicyEditor
          :mode="editorMode"
          :existing="editorTarget"
          :locked-scope="editorMode === 'create'
            ? { type: 'stack', key: stackScopeKey }
            : undefined"
          @close="closeEditor"
          @saved="onEditorSaved"
        />
      </DialogContent>
    </Dialog>
  </div>
</template>
