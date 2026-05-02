<script setup lang="ts">
import { useWizardStore } from '~/stores/wizard'

const wizard = useWizardStore()

const nextSteps = [
  {
    icon: 'lucide:server',
    title: 'Add another host',
    body: 'Build a fleet of two or more.',
    action: () => wizard.$patch({ step: 2, installCommand: null, hostId: null, enrollmentToken: null, fleet: '' }),
  },
  {
    icon: 'lucide:bell',
    title: 'Set up notifications',
    body: 'Telegram, Discord, or webhooks.',
    action: () => navigateTo('/settings'),
  },
]

function discoverySummary(): string {
  if (wizard.discoveredStacks === 0 && wizard.discoveredServices === 0) {
    return 'Agent reporting. First container scan in progress.'
  }
  const stacksLabel = `${wizard.discoveredStacks} ${wizard.discoveredStacks === 1 ? 'stack' : 'stacks'}`
  const servicesLabel = `${wizard.discoveredServices} ${wizard.discoveredServices === 1 ? 'service' : 'services'}`
  return `Isengard discovered ${stacksLabel} · ${servicesLabel} running on this host.`
}

defineEmits<{ (e: 'done'): void }>()
</script>

<template>
  <WizardCard :width="580" :content-gap="18">
    <div class="rounded-full bg-iso-success/10 border border-iso-success flex items-center justify-center" style="width:88px;height:88px">
      <Icon name="lucide:check" class="w-12 h-12 text-iso-success" />
    </div>

    <div class="flex flex-col items-center gap-1.5 text-center">
      <h1 class="font-mono text-[24px] font-semibold text-iso-text-primary">
        {{ wizard.enrolledHost?.hostname ?? wizard.hostname ?? 'Host' }} is connected
      </h1>
      <div v-if="wizard.fleet" class="flex items-center gap-1.5">
        <span class="text-[9px] uppercase tracking-wider font-medium text-iso-text-faint">added to fleet</span>
        <span class="font-mono text-[11px] text-iso-text-secondary">{{ wizard.fleet }}</span>
      </div>
      <p class="text-[13px] text-iso-text-muted">{{ discoverySummary() }}</p>
    </div>

    <div class="w-full flex flex-col gap-2.5 pt-2">
      <p class="text-[10px] uppercase tracking-wider font-medium text-iso-text-faint">What's next</p>
      <button
        v-for="item in nextSteps"
        :key="item.title"
        class="flex items-center justify-between gap-3 px-4 py-3 rounded-lg bg-iso-bg-base border border-iso-border-subtle hover:border-iso-text-secondary text-left"
        @click="item.action"
      >
        <div class="flex items-center gap-2.5">
          <Icon :name="item.icon" class="w-4 h-4 text-iso-text-secondary" />
          <div class="flex flex-col gap-0.5">
            <span class="font-mono text-[12px] font-medium text-iso-text-primary">{{ item.title }}</span>
            <span class="text-[11px] text-iso-text-faint">{{ item.body }}</span>
          </div>
        </div>
        <Icon name="lucide:chevron-right" class="w-3.5 h-3.5 text-iso-text-faint" />
      </button>
    </div>

    <template #actions>
      <button
        class="flex items-center gap-1.5 px-4 py-1.5 rounded-md bg-iso-success/10 border border-iso-success text-sm font-medium text-iso-success hover:bg-iso-success/20"
        @click="$emit('done')"
      >
        Take me to the dashboard
        <Icon name="lucide:arrow-right" class="w-3.5 h-3.5" />
      </button>
    </template>
  </WizardCard>
</template>
