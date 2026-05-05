<template>
  <!--
    Routing rules — laid out per `design/concepts/settings-networking/v1.html`.
    Concept uses a CSS grid (not <table>) so column widths line up across header
    + rows. Status pill = colored dot + colored label text. Source uses semantic
    colors (info=ui, warn=label, muted=imported).
  -->
  <div class="flex flex-col gap-4">
    <div class="flex items-center justify-between">
      <div class="flex flex-col gap-0.5">
        <h3 class="text-sm font-semibold text-iso-text-primary">Routing rules</h3>
        <span class="text-[11px] text-iso-text-muted">
          UI rules + label-discovered routes + imported NPM/Traefik configs.
        </span>
      </div>
      <button
        class="px-3 py-1.5 rounded-iso-md bg-iso-success text-iso-bg-base text-xs font-medium hover:opacity-90"
        @click="emit('add')"
      >
        + Add routing rule
      </button>
    </div>

    <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden">
      <div
        class="grid grid-cols-[minmax(220px,1.6fr)_minmax(200px,1.5fr)_110px_70px_140px_120px_110px] px-4 py-2.5 text-[10px] font-semibold tracking-wider text-iso-text-muted border-b border-iso-border-subtle"
      >
        <div>HOSTNAME</div>
        <div>TARGET</div>
        <div>ADAPTER</div>
        <div>TLS</div>
        <div>HEALTH</div>
        <div>SOURCE</div>
        <div class="text-right">ACTIONS</div>
      </div>

      <div v-if="loading" class="px-4 py-6 text-xs text-iso-text-muted">Loading…</div>
      <div v-else-if="error" class="px-4 py-6 text-xs text-iso-error">Error: {{ error }}</div>
      <div v-else-if="rules.length === 0" class="px-4 py-6 text-xs text-iso-text-muted">
        No routing rules. Click + Add routing rule, or apply
        <code class="font-mono text-iso-text-secondary">isengard.expose</code>
        labels to your containers.
      </div>

      <div
        v-for="(r, i) in rules"
        v-else
        :key="r.id"
        class="grid grid-cols-[minmax(220px,1.6fr)_minmax(200px,1.5fr)_110px_70px_140px_120px_110px] px-4 py-3 text-xs items-center"
        :class="i < rules.length - 1 ? 'border-b border-iso-border-subtle' : ''"
      >
        <div class="font-medium text-iso-text-primary truncate">{{ r.public_hostname }}</div>
        <div class="font-mono text-iso-text-muted truncate">{{ r.service_name }}:{{ r.container_port }}</div>
        <div class="text-iso-text-muted">{{ r.adapter }}</div>
        <div :class="tlsClass(r.tls_mode)">{{ r.tls_mode }}</div>
        <div class="flex items-center gap-2">
          <div class="w-2 h-2 rounded-full" :class="stateDot(r.state)"></div>
          <span :class="stateText(r.state)">{{ r.state }}</span>
        </div>
        <div :class="sourceClass(r.source)">
          {{ sourceLabel(r) }}
        </div>
        <div class="flex items-center justify-end gap-2 text-iso-text-muted">
          <button class="hover:text-iso-text-primary" @click="emit('edit', r)">edit</button>
          <span>·</span>
          <button class="hover:text-iso-error" @click="onDelete(r)">delete</button>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { useRoutingRules, type RoutingRule } from '~/composables/useRoutingRules'

const { rules, loading, error, deleteRule } = useRoutingRules()
const emit = defineEmits<{ (e: 'add'): void; (e: 'edit', rule: RoutingRule): void }>()

async function onDelete(r: RoutingRule) {
  if (!confirm(`Delete rule for ${r.public_hostname}?`)) return
  await deleteRule(r.id)
}

function stateDot(s: string): string {
  if (s === 'active') return 'bg-iso-success'
  if (s === 'pending' || s === 'draining') return 'bg-iso-warn'
  if (s === 'failed') return 'bg-iso-error'
  return 'bg-iso-text-muted'
}

function stateText(s: string): string {
  if (s === 'active') return 'text-iso-success'
  if (s === 'pending' || s === 'draining') return 'text-iso-warn'
  if (s === 'failed') return 'text-iso-error'
  return 'text-iso-text-muted'
}

function tlsClass(mode: string): string {
  if (!mode || mode === 'manual') return 'text-iso-text-muted'
  // edge / acme = automated TLS
  return 'text-iso-success'
}

function sourceClass(s: string): string {
  if (s === 'ui') return 'text-iso-info'
  if (s === 'label') return 'text-iso-warn'
  return 'text-iso-text-muted'
}

function sourceLabel(r: RoutingRule): string {
  if (r.source === 'label') return 'label'
  if (r.source === 'imported') return 'imported'
  return 'ui'
}
</script>
