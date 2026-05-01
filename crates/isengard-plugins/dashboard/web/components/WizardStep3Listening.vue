<script setup lang="ts">
import { useWizardStore } from '~/stores/wizard'

const wizard = useWizardStore()
</script>

<template>
  <div class="flex-1 flex flex-col items-center justify-center gap-5 py-10">
    <div class="w-24 h-24 rounded-full bg-iso-bg-elevated border border-iso-warn flex items-center justify-center">
      <Icon name="lucide:loader" class="w-10 h-10 text-iso-warn animate-spin" />
    </div>

    <div class="flex flex-col items-center gap-2.5">
      <h1 class="font-mono text-[26px] font-semibold text-iso-text-primary">Listening for first check-in</h1>
      <p class="text-sm text-iso-text-muted text-center max-w-[520px] leading-relaxed">
        Once the agent registers it will appear here. This usually takes under 30 seconds.
      </p>
    </div>

    <div class="flex items-center gap-2.5 px-4 py-2 rounded-full bg-iso-bg-elevated border border-iso-border-subtle">
      <span class="w-2 h-2 rounded-full bg-iso-warn"></span>
      <span class="font-mono text-xs text-iso-text-muted">controller :9417</span>
      <span class="text-xs text-iso-text-faint">·</span>
      <span class="font-mono text-xs text-iso-text-muted">agent.enroll</span>
      <span v-if="wizard.elapsedSeconds > 0" class="text-xs text-iso-text-faint">·</span>
      <span v-if="wizard.elapsedSeconds > 0" class="font-mono text-xs text-iso-text-faint">{{ wizard.elapsedSeconds }}s</span>
    </div>
  </div>
</template>
