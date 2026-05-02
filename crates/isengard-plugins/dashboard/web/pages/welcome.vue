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

function getStarted() { wizard.next() }
function back() { wizard.back() }
function iveRunIt() { wizard.next() }

function skip() {
  wizard.skip()
  router.replace('/')
}

function done() {
  wizard.complete()
  router.replace('/')
}
</script>

<template>
  <WizardShell :step="wizard.step">
    <WizardStep1Welcome
      v-if="wizard.step === 1"
      @get-started="getStarted"
      @skip="skip"
    />
    <WizardStep2AddHost
      v-else-if="wizard.step === 2"
      @back="back"
      @next="iveRunIt"
    />
    <WizardStep3Listening
      v-else-if="wizard.step === 3"
      @back="back"
      @cancel="skip"
    />
    <WizardStep4Connected
      v-else-if="wizard.step === 4"
      @done="done"
    />
  </WizardShell>
</template>
