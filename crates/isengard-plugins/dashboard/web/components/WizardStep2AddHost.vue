<script setup lang="ts">
import { computed } from 'vue'
import { useWizardStore } from '~/stores/wizard'
import { useFleetsStore } from '~/stores/fleets'
import { useToast } from '~/composables/useToast'
import { tokenizeDockerCommand, shellTokenClass } from '~/composables/useShellHighlight'

const wizard = useWizardStore()
const fleetsStore = useFleetsStore()
if (!fleetsStore.loaded) await fleetsStore.load()

const toast = useToast()

const tokens = computed(() => wizard.installCommand ? tokenizeDockerCommand(wizard.installCommand) : [])

defineEmits<{ (e: 'back'): void; (e: 'next'): void }>()

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
  <WizardCard :width="760" :content-gap="20">
    <div class="flex flex-col items-center gap-2 text-center">
      <h1 class="font-mono text-[22px] font-semibold text-iso-text-primary">Add your first host</h1>
      <p class="text-[13px] text-iso-text-muted">Name a fleet, then run one Docker command on the server you want to manage.</p>
    </div>

    <div class="w-full flex gap-3.5">
      <div class="flex-1 flex flex-col gap-1.5">
        <label class="text-[10px] uppercase tracking-wider font-medium text-iso-text-faint">
          Fleet <span class="text-iso-error">*</span>
        </label>
        <input
          v-model="wizard.fleet"
          type="text"
          placeholder="prod, homelab, edge…"
          autofocus
          class="h-9 px-3 rounded-md bg-iso-bg-base border border-iso-border-subtle font-mono text-[13px] text-iso-text-primary outline-none focus:border-iso-success/60"
          @blur="regenerate"
          @keydown.enter="regenerate"
        />
        <p class="text-[11px] text-iso-text-faint">Group hosts to apply policies in batch.</p>
      </div>

      <div class="flex-1 flex flex-col gap-1.5">
        <label class="text-[10px] uppercase tracking-wider font-medium text-iso-text-faint">Hostname</label>
        <input
          v-model="wizard.hostname"
          type="text"
          placeholder="prod-04"
          class="h-9 px-3 rounded-md bg-iso-bg-base border border-iso-border-subtle font-mono text-[13px] text-iso-text-primary outline-none focus:border-iso-success/60"
          @blur="regenerate"
        />
        <p class="text-[11px] text-iso-text-faint">Optional. Defaults to the server's hostname.</p>
      </div>
    </div>

    <div class="w-full flex flex-col gap-2">
      <div class="flex items-center justify-between">
        <label class="text-[10px] uppercase tracking-wider font-medium text-iso-text-faint">Install command</label>
        <button
          v-if="wizard.installCommand"
          class="flex items-center gap-1.5 px-2.5 py-1 rounded-md bg-iso-bg-base border border-iso-border-subtle text-[11px] text-iso-text-secondary hover:border-iso-text-secondary"
          @click="copy"
        >
          <Icon name="lucide:copy" class="w-3 h-3" />
          Copy
        </button>
      </div>

      <pre
        v-if="wizard.installCommand"
        class="rounded-md p-4 border border-iso-border-subtle font-mono text-[12px] leading-[1.65] overflow-x-auto whitespace-pre"
        style="background:#050505"
      ><span
        v-for="(t, i) in tokens"
        :key="i"
        :class="shellTokenClass(t.type)"
      >{{ t.text }}</span></pre>
      <div
        v-else
        class="rounded-md p-4 border border-dashed border-iso-border-subtle font-mono text-[12px] text-iso-text-faint italic text-center"
        style="background:#050505"
      >
        Name a fleet to generate the install command.
      </div>

      <div class="flex items-center gap-2 text-[11px] text-iso-text-faint">
        <Icon name="lucide:info" class="w-3 h-3" />
        The agent is itself a container. No host install, no sudo, no systemd.
      </div>
    </div>

    <p v-if="wizard.error" class="text-xs text-iso-error">{{ wizard.error }}</p>

    <template #actions>
      <button
        class="flex items-center gap-1.5 px-3.5 py-1.5 rounded-md bg-iso-bg-base border border-iso-border-subtle text-sm text-iso-text-secondary hover:border-iso-text-secondary"
        @click="$emit('back')"
      >
        <Icon name="lucide:arrow-left" class="w-3.5 h-3.5" />
        Back
      </button>
      <button
        class="flex items-center gap-1.5 px-4 py-1.5 rounded-md bg-iso-success/10 border border-iso-success text-sm font-medium text-iso-success hover:bg-iso-success/20 disabled:opacity-40 disabled:cursor-not-allowed"
        :disabled="!wizard.installCommand"
        @click="$emit('next')"
      >
        I've run it
        <Icon name="lucide:arrow-right" class="w-3.5 h-3.5" />
      </button>
    </template>
  </WizardCard>
</template>
