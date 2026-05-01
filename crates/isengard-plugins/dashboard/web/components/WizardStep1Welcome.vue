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
</script>

<template>
  <div class="flex-1 flex flex-col items-center justify-center gap-6 py-10">
    <div class="w-24 h-24 rounded-full bg-iso-bg-elevated border border-iso-success flex items-center justify-center">
      <Icon name="lucide:sparkles" class="w-10 h-10 text-iso-success" />
    </div>

    <div class="flex flex-col items-center gap-2.5">
      <h1 class="font-mono text-[30px] font-semibold text-iso-text-primary">Welcome to Isengard</h1>
      <p class="text-base text-iso-text-muted">A container and fleet manager for your servers.</p>
    </div>

    <div class="flex gap-3 mt-3">
      <div
        v-for="f in features"
        :key="f.title"
        class="w-60 p-[18px] rounded-lg bg-iso-bg-elevated border border-iso-border-subtle flex flex-col gap-2"
      >
        <Icon :name="f.icon" class="w-[18px] h-[18px] text-iso-text-secondary" />
        <h3 class="font-mono text-[13px] font-medium text-iso-text-primary">{{ f.title }}</h3>
        <p class="text-xs text-iso-text-muted leading-relaxed">{{ f.body }}</p>
      </div>
    </div>
  </div>
</template>
