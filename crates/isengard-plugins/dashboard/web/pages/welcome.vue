<script setup lang="ts">
import { onMounted, onUnmounted, watch } from 'vue'
import { useRouter, useRoute } from 'vue-router'
import { useWizardStore } from '~/stores/wizard'

definePageMeta({
  layout: false,
})

const wizard = useWizardStore()
const router = useRouter()
const route = useRoute()

onMounted(() => {
  const stepParam = parseInt(String(route.query.step ?? ''), 10)
  if (stepParam >= 1 && stepParam <= 4) {
    wizard.$patch({ step: stepParam as 1 | 2 | 3 | 4 })
  }
  if (route.query.fresh === '1') {
    wizard.reset()
  }
})

onUnmounted(() => {
  wizard.reset()
})

watch(
  () => wizard.step,
  (s) => {
    router.replace({ path: '/welcome', query: { step: String(s) } })
  }
)

function getStarted() {
  wizard.next()
}

function skipSetup() {
  wizard.skip()
  router.replace('/')
}

function back() {
  wizard.back()
}

function iveRunIt() {
  wizard.next()
}

function cancelSetup() {
  wizard.skip()
  router.replace('/')
}

function takeMeToDashboard() {
  wizard.complete()
  router.replace('/')
}
</script>

<template>
  <WizardShell :step="wizard.step">
    <WizardStep1Welcome v-if="wizard.step === 1" />
    <WizardStep2AddHost v-else-if="wizard.step === 2" />
    <WizardStep3Listening v-else-if="wizard.step === 3" />
    <WizardStep4Connected v-else-if="wizard.step === 4" />

    <template #footer-left>
      <button
        v-if="wizard.step === 1"
        class="text-sm text-iso-text-muted hover:text-iso-text-primary"
        @click="skipSetup"
      >
        Skip setup
      </button>
      <button
        v-else-if="wizard.step === 2 || wizard.step === 3"
        class="flex items-center gap-1.5 px-4 py-2 rounded-md bg-iso-bg-elevated border border-iso-border-subtle text-sm text-iso-text-secondary hover:border-iso-text-secondary"
        @click="back"
      >
        <Icon name="lucide:arrow-left" class="w-3.5 h-3.5" />
        Back
      </button>
    </template>

    <template #footer-right>
      <button
        v-if="wizard.step === 1"
        class="flex items-center gap-1.5 px-4 py-2 rounded-md bg-iso-success/10 border border-iso-success text-sm font-medium text-iso-success hover:bg-iso-success/20"
        @click="getStarted"
      >
        Get started
        <Icon name="lucide:arrow-right" class="w-3.5 h-3.5" />
      </button>
      <button
        v-else-if="wizard.step === 2"
        class="flex items-center gap-1.5 px-4 py-2 rounded-md bg-iso-success/10 border border-iso-success text-sm font-medium text-iso-success hover:bg-iso-success/20 disabled:opacity-40 disabled:cursor-not-allowed"
        :disabled="!wizard.installCommand"
        @click="iveRunIt"
      >
        I've run it
        <Icon name="lucide:arrow-right" class="w-3.5 h-3.5" />
      </button>
      <button
        v-else-if="wizard.step === 3"
        class="text-sm text-iso-text-muted hover:text-iso-text-primary"
        @click="cancelSetup"
      >
        Cancel setup
      </button>
      <button
        v-else-if="wizard.step === 4"
        class="flex items-center gap-1.5 px-4 py-2 rounded-md bg-iso-success/10 border border-iso-success text-sm font-medium text-iso-success hover:bg-iso-success/20"
        @click="takeMeToDashboard"
      >
        Take me to the dashboard
        <Icon name="lucide:arrow-right" class="w-3.5 h-3.5" />
      </button>
    </template>
  </WizardShell>
</template>
