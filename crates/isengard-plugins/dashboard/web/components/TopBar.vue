<template>
  <div class="h-14 border-b border-iso-border-subtle px-5 flex items-center gap-5 text-sm shrink-0">
    <!-- Brand cluster -->
    <div class="flex items-center gap-2.5">
      <div class="w-2.5 h-2.5 rounded-full bg-iso-success"></div>
      <span class="font-semibold text-iso-text-primary tracking-tight text-[15px]">isengard</span>
      <span class="text-iso-text-faint">·</span>
      <FleetPicker />
    </div>

    <!-- Tab bar -->
    <nav class="flex items-center gap-1 ml-2">
      <NuxtLink
        v-for="tab in tabs"
        :key="tab.path"
        :to="tab.path"
        class="px-3.5 py-1.5 rounded-md text-sm transition-colors"
        :class="$route.path === tab.path ? 'bg-iso-bg-overlay text-iso-text-primary font-medium' : 'text-iso-text-muted hover:text-iso-text-secondary'"
      >
        {{ tab.label }}
      </NuxtLink>
    </nav>

    <div class="flex-1"></div>

    <button
      class="h-8 px-3 rounded-md bg-iso-bg-overlay border border-iso-border-subtle text-sm flex items-center gap-2 hover:border-iso-border-strong"
      @click="ui.openCmdPane('navigator')"
    >
      <Icon name="lucide:search" class="w-4 h-4 text-iso-text-muted" />
      <span class="text-iso-text-muted">Search or jump…</span>
      <kbd class="px-1.5 py-0.5 rounded text-[11px] font-mono border border-iso-border-strong text-iso-text-secondary bg-iso-bg-base">⌘K</kbd>
    </button>

    <button class="w-9 h-8 rounded-md bg-iso-bg-overlay border border-iso-border-subtle flex items-center justify-center hover:border-iso-border-strong" title="Settings">
      <Icon name="lucide:settings" class="w-4 h-4 text-iso-text-muted" />
    </button>
  </div>
</template>

<script setup lang="ts">
const ui = useUiStore()

const tabs = [
  { path: '/', label: 'Home' },
  { path: '/hosts', label: 'Hosts' },
  { path: '/stacks', label: 'Stacks' },
  { path: '/events', label: 'Events' },
  { path: '/settings', label: 'Settings' },
]
</script>
