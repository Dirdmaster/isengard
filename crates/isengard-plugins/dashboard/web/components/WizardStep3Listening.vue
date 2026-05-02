<script setup lang="ts">
import { computed } from 'vue'
import { useWizardStore } from '~/stores/wizard'

const wizard = useWizardStore()

const stage = computed<'normal' | 'slow' | 'stuck'>(() => {
  if (wizard.elapsedSeconds > 600) return 'stuck'
  if (wizard.elapsedSeconds > 60) return 'slow'
  return 'normal'
})

defineEmits<{ (e: 'back'): void; (e: 'cancel'): void }>()
</script>

<template>
  <WizardCard :width="540" :content-gap="20">
    <div class="rounded-full bg-iso-bg-base border border-iso-warn flex items-center justify-center" style="width:88px;height:88px">
      <Icon name="lucide:loader" class="w-10 h-10 text-iso-warn animate-spin" />
    </div>

    <div class="flex flex-col items-center gap-2">
      <h1 class="font-mono text-[22px] font-semibold text-iso-text-primary">Listening for first check-in</h1>
      <p v-if="stage === 'normal'" class="text-[13px] text-iso-text-muted text-center leading-relaxed">
        Once the agent registers it will appear here. This usually takes under 30 seconds.
      </p>
      <p v-else-if="stage === 'slow'" class="text-[13px] text-iso-text-muted text-center leading-relaxed">
        Still waiting. Check that the docker run command finished cleanly on your server, and that it can reach this controller's address.
      </p>
      <div v-else class="flex flex-col items-center gap-2 text-center">
        <p class="text-[13px] text-iso-error">No check-in after 10 minutes.</p>
        <p class="text-[13px] text-iso-text-muted leading-relaxed">
          Common causes: token expired (30 min TTL), network blocking outbound to the controller, or the agent container exited. Check the host's container logs:
        </p>
        <pre class="font-mono text-xs text-iso-text-secondary px-3 py-2 rounded bg-iso-bg-base border border-iso-border-subtle">docker logs isengard-agent</pre>
      </div>
    </div>

    <div class="flex items-center gap-2.5 px-3 py-1.5 rounded-full bg-iso-bg-base border border-iso-border-subtle">
      <span class="w-1.5 h-1.5 rounded-full bg-iso-warn"></span>
      <span class="font-mono text-[11px] text-iso-text-muted">controller :9417</span>
      <span class="text-[11px] text-iso-text-faint">·</span>
      <span class="font-mono text-[11px] text-iso-text-muted">agent.enroll</span>
      <template v-if="wizard.elapsedSeconds > 0">
        <span class="text-[11px] text-iso-text-faint">·</span>
        <span class="font-mono text-[11px] text-iso-text-faint">{{ wizard.elapsedSeconds }}s</span>
      </template>
    </div>

    <template #actions>
      <button
        class="flex items-center gap-1.5 px-3.5 py-1.5 rounded-md bg-iso-bg-base border border-iso-border-subtle text-sm text-iso-text-secondary hover:border-iso-text-secondary"
        @click="$emit('back')"
      >
        <Icon name="lucide:arrow-left" class="w-3.5 h-3.5" />
        Back
      </button>
      <button
        class="text-sm text-iso-text-muted hover:text-iso-text-primary px-2 py-1"
        @click="$emit('cancel')"
      >
        Cancel setup
      </button>
    </template>
  </WizardCard>
</template>
