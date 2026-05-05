<script setup lang="ts">
import type { Stack } from '~/stores/stacks'

export interface StackRowData {
  stack: Stack
  hostHostname: string
  fleet: string
  serviceCount: number
  /** Primary image (longest reported); plus extra count for the badge. */
  primaryImage: string | null
  extraImageCount: number
  /** Derived health label and tone for the HEALTH cell. */
  health: 'healthy' | 'updating' | 'failed' | 'aborted' | 'unknown'
  /** ISO timestamp of last successful deploy or container event; '' when unknown. */
  lastDeployIso: string
}

interface Props {
  rows: StackRowData[]
}

defineProps<Props>()

const router = useRouter()

function relTime(iso: string): string {
  if (!iso) return '—'
  const ms = Date.now() - new Date(iso).getTime()
  if (!Number.isFinite(ms) || ms < 0) return '—'
  const s = Math.floor(ms / 1000)
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m} min ago`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ago`
  const d = Math.floor(h / 24)
  if (d < 30) return `${d} day${d === 1 ? '' : 's'} ago`
  return `${Math.floor(d / 30)}mo ago`
}

function dotColor(health: StackRowData['health']) {
  switch (health) {
    case 'healthy': return 'bg-iso-success'
    case 'updating': return 'bg-iso-warn'
    case 'failed': return 'bg-iso-error'
    case 'aborted': return 'bg-iso-warn'
    default: return 'bg-iso-text-muted'
  }
}

function healthClasses(health: StackRowData['health']) {
  switch (health) {
    case 'healthy': return 'text-iso-success'
    case 'updating': return 'text-iso-warn'
    case 'failed': return 'text-iso-error'
    case 'aborted': return 'text-iso-warn'
    default: return 'text-iso-text-muted'
  }
}

function sourceBadge(source: string) {
  // Backend yields compose | manual | inferred. Map to concept's icon vocabulary.
  switch (source) {
    case 'compose':  return { icon: 'lucide:package', label: 'compose' }
    case 'manual':   return { icon: 'lucide:wand-2', label: 'manual' }
    case 'inferred': return { icon: 'lucide:search', label: 'discovered' }
    default:         return { icon: 'lucide:circle', label: source }
  }
}
</script>

<template>
  <div class="flex flex-col min-h-0">
    <div
      v-if="rows.length === 0"
      class="flex-1 rounded-iso-lg border border-dashed border-iso-border-subtle bg-iso-bg-elevated/40 flex items-center justify-center p-6 m-4"
    >
      <div class="w-[680px] flex flex-col items-center gap-6 p-8 text-center">
        <div class="w-14 h-14 rounded-iso-lg border border-iso-border-strong bg-iso-bg-base flex items-center justify-center font-mono text-iso-text-secondary text-xl font-semibold">
          { }
        </div>
        <div class="flex flex-col gap-2">
          <h2 class="text-2xl font-semibold tracking-tight text-iso-text-primary">Deploy your first stack</h2>
          <p class="text-sm text-iso-text-muted leading-relaxed max-w-md">
            A <span class="text-iso-text-primary">stack</span> is a docker-compose project running on one or more of your hosts.
            They appear here automatically when an agent reports containers labelled
            <code class="font-mono text-xs text-iso-text-secondary">com.docker.compose.project</code>
            or
            <code class="font-mono text-xs text-iso-text-secondary">isengard.stack</code>.
          </p>
        </div>
        <div class="text-[11px] text-iso-text-muted">
          Stacks already running on your hosts? They appear automatically as
          <span class="text-iso-info">discovered</span> once the agent reports them.
        </div>
      </div>
    </div>

    <template v-else>
      <div
        class="grid items-center gap-3 px-4 py-2.5 text-[10px] uppercase tracking-wider text-iso-text-muted border-b border-iso-border-subtle shrink-0"
        style="grid-template-columns: 240px 110px 90px minmax(220px,1fr) 140px 110px"
      >
        <span>Stack</span>
        <span>Fleet</span>
        <span>Services</span>
        <span>Image</span>
        <span>Last deploy</span>
        <span>Health</span>
      </div>

      <div
        v-for="row in rows"
        :key="row.stack.id"
        class="grid items-center gap-3 px-4 py-3 text-xs border-b border-iso-border-subtle hover:bg-iso-bg-elevated cursor-pointer"
        :class="row.health === 'failed' ? 'border-l-2 border-l-iso-error/60' : ''"
        style="grid-template-columns: 240px 110px 90px minmax(220px,1fr) 140px 110px"
        @click="router.push(`/stacks/${row.stack.id}`)"
      >
        <!-- Stack: status dot + name + source badge -->
        <div class="flex items-center gap-2 min-w-0">
          <span class="w-2 h-2 rounded-full shrink-0" :class="dotColor(row.health)"></span>
          <span class="font-mono font-medium text-iso-text-primary truncate">{{ row.stack.name }}</span>
          <span
            class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded-iso-sm bg-iso-bg-elevated border border-iso-border-subtle text-[10px] text-iso-text-muted shrink-0"
            :title="`source: ${row.stack.source}`"
          >
            <Icon :name="sourceBadge(row.stack.source).icon" class="w-2.5 h-2.5" />
            <span>{{ sourceBadge(row.stack.source).label }}</span>
          </span>
        </div>

        <!-- Fleet -->
        <span class="text-iso-text-muted truncate">{{ row.fleet }}</span>

        <!-- Services -->
        <span class="text-iso-text-muted font-mono">
          {{ row.serviceCount }} {{ row.serviceCount === 1 ? 'service' : 'services' }}
        </span>

        <!-- Image -->
        <span class="font-mono text-iso-text-secondary truncate flex items-center gap-1.5 min-w-0">
          <span class="truncate">{{ row.primaryImage ?? '—' }}</span>
          <span
            v-if="row.extraImageCount > 0"
            class="px-1 py-px rounded-iso-sm bg-iso-bg-elevated border border-iso-border-subtle text-[10px] text-iso-text-muted shrink-0"
          >
            +{{ row.extraImageCount }}
          </span>
        </span>

        <!-- Last deploy -->
        <span class="text-iso-text-muted">{{ relTime(row.lastDeployIso) }}</span>

        <!-- Health -->
        <span class="font-mono" :class="healthClasses(row.health)">
          {{ row.health }}
        </span>
      </div>
    </template>
  </div>
</template>
