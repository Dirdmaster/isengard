<template>
  <div class="h-14 border-b border-iso-border-subtle px-5 flex items-center gap-4 text-sm shrink-0">
    <!-- Brand cluster -->
    <div class="flex items-center gap-2.5">
      <div class="w-2.5 h-2.5 rounded-full bg-iso-success"></div>
      <span class="font-semibold text-iso-text-primary tracking-tight text-[15px]">isengard</span>
    </div>

    <FleetPicker />

    <!-- Tab bar -->
    <nav class="flex items-center ml-1">
      <NuxtLink
        v-for="tab in tabs"
        :key="tab.path"
        :to="tab.path"
        class="px-3 py-1.5 text-iso-xs transition-colors border-b-2 inline-flex items-center"
        :class="$route.path === tab.path ? 'text-iso-text-primary border-iso-text-primary font-medium' : 'text-iso-text-faint border-transparent hover:text-iso-text-secondary'"
      >
        {{ tab.label }}
        <ApprovalsBadge v-if="tab.path === '/approvals'" />
      </NuxtLink>
    </nav>

    <div class="flex-1"></div>

    <button
      class="h-8 px-2.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-subtle flex items-center gap-2 hover:border-iso-border-strong transition-colors"
      @click="ui.openCmdPane('navigator')"
    >
      <Icon name="lucide:search" class="w-3 h-3 text-iso-text-muted" />
      <span class="text-iso-xs text-iso-text-faint">Search or jump…</span>
      <kbd class="px-1.5 py-px rounded-iso-sm bg-iso-bg-base border border-iso-border-strong font-mono text-[10px] text-iso-text-secondary">⌘K</kbd>
    </button>

    <div class="relative" @click.stop>
      <button
        class="h-8 px-3 rounded-iso-md bg-iso-success text-iso-bg-base text-iso-xs font-medium flex items-center gap-1 hover:opacity-90 transition-opacity"
        @click="newOpen = !newOpen"
      >
        <Icon name="lucide:plus" class="w-3 h-3" />
        New
        <Icon name="lucide:chevron-down" class="w-3 h-3" />
      </button>

      <div
        v-if="newOpen"
        class="absolute top-full right-0 mt-1.5 w-[220px] bg-iso-bg-overlay border border-iso-border-strong rounded-iso-md shadow-2xl shadow-black/60 z-50 py-1 overflow-hidden"
      >
        <button
          class="w-full px-3 py-2 text-iso-sm flex items-center gap-2.5 text-iso-text-secondary hover:bg-iso-bg-row-hover transition-colors"
          @click="addHost"
        >
          <Icon name="lucide:server" class="w-3.5 h-3.5 text-iso-text-muted shrink-0" />
          <span class="flex-1 text-left">Add host</span>
        </button>
        <button
          class="w-full px-3 py-2 text-iso-sm flex items-center gap-2.5 text-iso-text-faint cursor-not-allowed"
          disabled
          title="Add stack flow coming soon"
        >
          <Icon name="lucide:layers" class="w-3.5 h-3.5 text-iso-text-faint shrink-0" />
          <span class="flex-1 text-left">Add stack</span>
          <span class="text-[10px] font-mono text-iso-text-faint">soon</span>
        </button>
        <div class="h-px bg-iso-border-subtle my-1"></div>
        <button
          class="w-full px-3 py-2 text-iso-sm flex items-center gap-2.5 text-iso-text-secondary hover:bg-iso-bg-row-hover transition-colors"
          @click="addRoutingRule"
        >
          <Icon name="lucide:route" class="w-3.5 h-3.5 text-iso-text-muted shrink-0" />
          <span class="flex-1 text-left">Add routing rule</span>
        </button>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { onMounted, onBeforeUnmount, ref } from 'vue'
import { useRouter } from 'vue-router'

const ui = useUiStore()
const router = useRouter()
const newOpen = ref(false)

// Approvals tab landed with Phase 9b (T5). The badge polls
// `usePendingApprovalsCount` every 30s and renders inline next to the label
// when count > 0.
const tabs = [
  { path: '/', label: 'Home' },
  { path: '/hosts', label: 'Hosts' },
  { path: '/stacks', label: 'Stacks' },
  { path: '/events', label: 'Events' },
  { path: '/approvals', label: 'Approvals' },
  { path: '/settings', label: 'Settings' },
]

onMounted(() => {
  document.addEventListener('click', closeOnOutside)
})

onBeforeUnmount(() => {
  document.removeEventListener('click', closeOnOutside)
})

function closeOnOutside() {
  newOpen.value = false
}

function addHost() {
  newOpen.value = false
  router.push('/welcome?step=2&fresh=1')
}

function addRoutingRule() {
  newOpen.value = false
  router.push('/settings?tab=networking&subtab=proxy')
}
</script>
