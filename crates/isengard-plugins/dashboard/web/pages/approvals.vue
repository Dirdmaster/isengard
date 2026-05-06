<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import {
  useApprovals,
  type ApprovalDto,
  type ApprovalFilterState,
  type DecisionKind,
} from '~/composables/useApprovals'

/**
 * Approvals queue page (Phase 9 Plan B, T5).
 *
 * Displays pending update.pending_approval rows from
 * `GET /api/v1/approvals?state=`. Filter chips toggle between Open / Decided
 * / All. Decisions POST back to the same endpoint via the composable, which
 * also dispatches optimistic UI + refresh.
 *
 * Sister design: design/pages/approvals.md
 *               design/concepts/approvals/v1.html
 */

const {
  sorted,
  filter,
  loading,
  error,
  refresh,
  setFilter,
  decide,
  isInFlight,
} = useApprovals('open')

const toast = useToast()
const { refresh: refreshBadge } = usePendingApprovalsCount()

const FILTERS: Array<{ key: ApprovalFilterState; label: string }> = [
  { key: 'open', label: 'Open' },
  { key: 'decided', label: 'Decided' },
  { key: 'all', label: 'All' },
]

const initialised = ref(false)

onMounted(async () => {
  try {
    await refresh()
  } catch {
    // Error is already on the `error` ref; the body renders the retry state.
  } finally {
    initialised.value = true
  }
})

async function onFilterClick(key: ApprovalFilterState) {
  try {
    await setFilter(key)
  } catch (e) {
    toast.error(`Filter failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function onRefreshClick() {
  try {
    await refresh()
  } catch (e) {
    toast.error(`Refresh failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function onDecide(payload: {
  id: string
  decision: DecisionKind
  snoozeHours?: number
}) {
  const verb = payload.decision === 'snooze'
    ? `Snoozed ${payload.snoozeHours ?? '?'}h`
    : payload.decision === 'approve'
      ? 'Approved'
      : 'Rejected'
  try {
    await decide(payload.id, payload.decision, payload.snoozeHours)
    toast.success(verb)
    // Bump the global badge so the TopBar count drops without waiting for
    // its next 30s tick.
    refreshBadge().catch(() => {})
  } catch (e) {
    toast.error(`${verb} failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

const subtitle = computed(() => {
  if (loading.value && !initialised.value) return 'Loading...'
  if (error.value) return 'Could not load approvals'
  const n = sorted.value.length
  if (filter.value === 'open') {
    if (n === 0) return 'No updates waiting on you.'
    return `${n} pending ${n === 1 ? 'update' : 'updates'} waiting on you.`
  }
  if (filter.value === 'decided') {
    return `${n} recently decided.`
  }
  return `${n} approval ${n === 1 ? 'row' : 'rows'} (open + decided).`
})

function trackBy(a: ApprovalDto): string {
  return a.actionId
}
</script>

<template>
  <AppShell>
    <PageHeader title="Approvals" :subtitle="subtitle">
      <template #actions>
        <button
          type="button"
          :disabled="loading"
          class="h-8 px-3 rounded-iso-md bg-iso-bg-elevated border border-iso-border-strong text-xs text-iso-text-secondary hover:text-iso-text-primary disabled:opacity-50 disabled:cursor-not-allowed transition-colors inline-flex items-center gap-2"
          @click="onRefreshClick"
        >
          <Icon
            name="lucide:refresh-ccw"
            class="w-3 h-3"
            :class="loading ? 'animate-spin' : ''"
          />
          Refresh
        </button>
      </template>
    </PageHeader>

    <div class="flex-1 overflow-y-auto">
      <div class="max-w-4xl mx-auto w-full p-6 flex flex-col gap-4">
        <!-- Filter chips -->
        <div
          class="flex items-center gap-2 px-3 py-2 rounded-iso-md bg-iso-bg-elevated border border-iso-border-subtle shrink-0 flex-wrap"
        >
          <span class="text-[10px] font-semibold text-iso-text-muted tracking-wider mr-1">
            FILTER
          </span>
          <button
            v-for="f in FILTERS"
            :key="f.key"
            type="button"
            :class="[
              'px-2.5 py-1 rounded-iso-sm border font-mono text-[11px] transition-colors',
              filter === f.key
                ? 'bg-iso-bg-overlay border-iso-border-strong text-iso-text-primary'
                : 'bg-iso-bg-base border-iso-border-subtle text-iso-text-secondary hover:text-iso-text-primary',
            ]"
            @click="onFilterClick(f.key)"
          >
            {{ f.label }}
          </button>
        </div>

        <!-- Loading state (initial only; subsequent refreshes show spinner on the button). -->
        <div
          v-if="loading && !initialised"
          class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated/40 px-6 py-12 flex flex-col items-center justify-center gap-2"
        >
          <Icon name="lucide:loader" class="w-5 h-5 text-iso-text-muted animate-spin" />
          <p class="text-sm text-iso-text-muted">Loading approvals...</p>
        </div>

        <!-- Error state with retry. -->
        <div
          v-else-if="error"
          class="rounded-iso-lg border border-iso-error/40 bg-iso-error-soft px-6 py-8 flex flex-col items-center justify-center gap-3"
        >
          <Icon name="lucide:alert-triangle" class="w-5 h-5 text-iso-error" />
          <p class="text-sm text-iso-error">Could not load approvals.</p>
          <p class="text-xs text-iso-text-muted text-center max-w-md">{{ error }}</p>
          <button
            type="button"
            class="mt-1 px-3 py-1.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-strong text-xs text-iso-text-secondary hover:text-iso-text-primary transition-colors"
            @click="onRefreshClick"
          >
            Retry
          </button>
        </div>

        <!-- Populated state: card stack newest-first. -->
        <div v-else-if="sorted.length > 0" class="flex flex-col gap-3">
          <ApprovalCard
            v-for="row in sorted"
            :key="trackBy(row)"
            :approval="row"
            :busy="isInFlight(row.actionId)"
            @decide="onDecide"
          />
        </div>

        <!-- Empty state with in-container CTA per feedback rule. -->
        <div
          v-else
          class="rounded-iso-xl border border-dashed border-iso-border-subtle bg-iso-bg-elevated/40 px-6 py-12 flex flex-col items-center justify-center gap-3 text-center"
        >
          <div
            class="w-12 h-12 rounded-full bg-iso-bg-elevated border border-iso-border-subtle flex items-center justify-center"
          >
            <Icon name="lucide:check-circle-2" class="w-5 h-5 text-iso-text-muted" />
          </div>
          <p class="text-sm text-iso-text-primary font-medium">
            <template v-if="filter === 'open'">No updates waiting on you.</template>
            <template v-else-if="filter === 'decided'">No decisions yet.</template>
            <template v-else>No approval rows yet.</template>
          </p>
          <p class="text-xs text-iso-text-muted max-w-md">
            Approvals appear here when a service has
            <span class="font-mono text-iso-text-secondary">gate=approval</span>
            and the updater detects a new image digest. Set a service to require
            approval from the policies page.
          </p>
          <NuxtLink
            to="/settings/policies"
            class="mt-1 px-3 py-1.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-strong text-xs text-iso-text-secondary hover:text-iso-text-primary transition-colors"
          >
            Configure policies
          </NuxtLink>
        </div>
      </div>
    </div>
  </AppShell>
</template>
