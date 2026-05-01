<template>
  <div class="h-13 border-b border-iso-border-subtle px-5 flex items-center gap-4 text-iso-sm">
    <!-- Brand cluster -->
    <div class="flex items-center gap-2">
      <div class="w-2.5 h-2.5 rounded-full bg-iso-success"></div>
      <span class="font-semibold text-iso-text-primary tracking-tight">isengard</span>
      <span class="text-iso-text-faint">·</span>
      <FleetPicker />
    </div>

    <!-- Tab bar -->
    <nav class="flex items-center gap-0.5 ml-4">
      <NuxtLink
        v-for="tab in tabs"
        :key="tab.path"
        :to="tab.path"
        class="px-3 py-1.5 rounded-iso-sm text-iso-sm transition-colors"
        :class="$route.path === tab.path ? 'bg-iso-bg-overlay text-iso-text-primary font-medium' : 'text-iso-text-muted hover:text-iso-text-secondary'"
      >
        {{ tab.label }}
      </NuxtLink>
    </nav>

    <div class="flex-1"></div>

    <!-- Right cluster -->
    <button
      class="px-2.5 py-1 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle text-iso-text-secondary text-iso-sm flex items-center gap-1.5 hover:border-iso-border-strong"
      title="Atlas mode (coming in v1.x)"
    >
      <Icon name="lucide:compass" class="w-3.5 h-3.5 text-iso-text-muted" />
      Atlas
    </button>

    <button
      class="px-2.5 py-1 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm flex items-center gap-2 hover:border-iso-border-strong"
      @click="ui.openCmdPane('navigator')"
    >
      <Icon name="lucide:search" class="w-3.5 h-3.5 text-iso-text-muted" />
      <span class="text-iso-text-muted">Search or jump…</span>
      <kbd class="px-1.5 py-0.5 rounded text-iso-xs font-mono border border-iso-border-strong text-iso-text-secondary bg-iso-bg-base">⌘K</kbd>
    </button>

    <button class="w-8 h-7 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle flex items-center justify-center hover:border-iso-border-strong" title="Settings">
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
]
</script>
