<script setup lang="ts">
const features = [
  {
    icon: 'lucide:server',
    title: 'See every host',
    body: 'Group servers into fleets, watch agent state in one place.',
  },
  {
    icon: 'lucide:boxes',
    title: 'Manage stacks',
    body: 'Compose projects as first-class units across hosts.',
  },
  {
    icon: 'lucide:activity',
    title: 'Live event feed',
    body: 'Every check, every update, every issue. Filter, search, jump.',
  },
]

defineEmits<{ (e: 'getStarted'): void; (e: 'skip'): void }>()
</script>

<template>
  <WizardCard :width="740">
    <div class="w-22 h-22 rounded-full bg-iso-bg-base border border-iso-success flex items-center justify-center" style="width:88px;height:88px">
      <Icon name="lucide:sparkles" class="w-10 h-10 text-iso-success" />
    </div>

    <div class="flex flex-col items-center gap-2">
      <h1 class="font-mono text-[26px] font-semibold text-iso-text-primary">Welcome to Isengard</h1>
      <p class="text-sm text-iso-text-muted">A container and fleet manager for your servers.</p>
    </div>

    <div class="flex gap-3 pt-2">
      <div
        v-for="f in features"
        :key="f.title"
        class="w-52 p-4 rounded-lg bg-iso-bg-base border border-iso-border-subtle flex flex-col gap-2"
      >
        <Icon :name="f.icon" class="w-4 h-4 text-iso-text-secondary" />
        <h3 class="font-mono text-[12px] font-medium text-iso-text-primary">{{ f.title }}</h3>
        <p class="text-[11px] text-iso-text-muted leading-relaxed">{{ f.body }}</p>
      </div>
    </div>

    <template #actions>
      <button
        class="text-sm text-iso-text-muted hover:text-iso-text-primary px-2 py-1"
        @click="$emit('skip')"
      >
        Skip setup
      </button>
      <button
        class="flex items-center gap-1.5 px-4 py-2 rounded-md bg-iso-success/10 border border-iso-success text-sm font-medium text-iso-success hover:bg-iso-success/20"
        @click="$emit('getStarted')"
      >
        Get started
        <Icon name="lucide:arrow-right" class="w-3.5 h-3.5" />
      </button>
    </template>
  </WizardCard>
</template>
