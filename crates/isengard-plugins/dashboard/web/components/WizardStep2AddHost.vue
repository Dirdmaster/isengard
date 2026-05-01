<script setup lang="ts">
import { useWizardStore } from '~/stores/wizard'
import { useFleetsStore } from '~/stores/fleets'
import { useToast } from '~/composables/useToast'

const wizard = useWizardStore()
const fleetsStore = useFleetsStore()
if (!fleetsStore.loaded) await fleetsStore.load()

const toast = useToast()

if (!wizard.installCommand) {
  await wizard.issueToken().catch(() => {})
}

async function copy() {
  if (!wizard.installCommand) return
  try {
    await navigator.clipboard.writeText(wizard.installCommand)
    toast.success('Install command copied')
  } catch {
    toast.error('Copy failed: please copy manually')
  }
}

async function regenerate() {
  await wizard.issueToken().catch(() => {})
}
</script>

<template>
  <div class="flex-1 flex flex-col items-center gap-6 py-10 overflow-y-auto">
    <div class="flex flex-col items-center gap-2 text-center">
      <h1 class="font-mono text-[26px] font-semibold text-iso-text-primary">Add your first host</h1>
      <p class="text-sm text-iso-text-muted">Run one Docker command on the server you want to manage.</p>
    </div>

    <div class="w-[560px] flex gap-4">
      <div class="flex-1 flex flex-col gap-1.5">
        <label class="text-[10px] uppercase tracking-wider font-medium text-iso-text-faint">Hostname</label>
        <input
          v-model="wizard.hostname"
          type="text"
          placeholder="prod-04"
          class="h-[38px] px-3 rounded-md bg-iso-bg-elevated border border-iso-border-subtle font-mono text-[13px] text-iso-text-primary outline-none focus:border-iso-success/50"
          @blur="regenerate"
        />
        <p class="text-[11px] text-iso-text-faint">Optional. Defaults to the server's hostname.</p>
      </div>

      <div class="flex-1 flex flex-col gap-1.5">
        <label class="text-[10px] uppercase tracking-wider font-medium text-iso-text-faint">Fleet</label>
        <select
          v-model="wizard.fleet"
          class="h-[38px] px-3 rounded-md bg-iso-bg-elevated border border-iso-border-subtle font-mono text-[13px] text-iso-text-primary outline-none focus:border-iso-success/50"
          @change="regenerate"
        >
          <option value="default">default</option>
          <option v-for="f in fleetsStore.fleets.filter(f => f.name !== 'default')" :key="f.name" :value="f.name">
            {{ f.name }}
          </option>
        </select>
        <p class="text-[11px] text-iso-text-faint">Group hosts to apply policies in batch.</p>
      </div>
    </div>

    <div class="w-[680px] flex flex-col gap-2">
      <div class="flex items-center justify-between">
        <label class="text-[10px] uppercase tracking-wider font-medium text-iso-text-faint">Install command</label>
        <button
          class="flex items-center gap-1.5 px-2.5 py-1 rounded-md bg-iso-bg-elevated border border-iso-border-subtle text-[11px] text-iso-text-secondary hover:border-iso-text-secondary"
          @click="copy"
        >
          <Icon name="lucide:copy" class="w-3 h-3" />
          Copy
        </button>
      </div>
      <pre class="rounded-md p-4 border border-iso-border-subtle font-mono text-[12px] text-iso-text-secondary leading-[1.7] overflow-x-auto" style="background:#050505">{{ wizard.installCommand ?? 'Generating install command…' }}</pre>
      <div class="flex items-center gap-2 text-[12px] text-iso-text-faint">
        <Icon name="lucide:info" class="w-3 h-3" />
        The agent is itself a container. No host install, no sudo, no systemd.
      </div>
    </div>

    <p v-if="wizard.error" class="text-xs text-iso-error">{{ wizard.error }}</p>
  </div>
</template>
