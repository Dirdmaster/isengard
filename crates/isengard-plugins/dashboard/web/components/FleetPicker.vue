<template>
  <div class="relative" @click.stop>
    <button
      class="h-8 px-3 rounded-md bg-iso-bg-overlay border border-iso-border-subtle text-sm font-medium text-iso-text-secondary flex items-center gap-2 hover:border-iso-border-strong"
      @click="open = !open"
    >
      {{ activeLabel }}
      <Icon name="lucide:chevron-down" class="w-3.5 h-3.5 text-iso-text-faint" />
    </button>

    <div
      v-if="open"
      class="absolute top-full left-0 mt-1.5 w-[220px] bg-iso-bg-overlay border border-iso-border-strong rounded-md shadow-2xl shadow-black/60 z-50 py-1.5 overflow-hidden"
    >
      <div class="flex items-center justify-between px-3 pt-1.5 pb-1">
        <span class="text-[9px] uppercase tracking-[0.6px] font-medium text-iso-text-faint">Fleets</span>
        <span class="text-[9px] font-mono text-iso-text-faint">
          {{ totalHosts }} {{ totalHosts === 1 ? 'host' : 'hosts' }}
        </span>
      </div>

      <button
        class="w-full px-3 py-2 text-sm flex items-center gap-2"
        :class="ui.activeFleet === 'all' ? 'bg-iso-bg-selected text-iso-text-primary font-medium' : 'text-iso-text-muted hover:bg-iso-bg-row-hover'"
        @click="select('all')"
      >
        <Icon
          v-if="ui.activeFleet === 'all'"
          name="lucide:check"
          class="w-3.5 h-3.5 text-iso-success shrink-0"
        />
        <span v-else class="w-3.5 shrink-0"></span>
        <span class="flex-1 text-left">All fleets</span>
        <span class="text-[11px] font-mono text-iso-text-muted">{{ totalHosts }}</span>
      </button>

      <template v-if="populatedFleets.length">
        <div class="h-px bg-iso-border-subtle"></div>
        <button
          v-for="f in populatedFleets"
          :key="f.name"
          class="w-full pl-[34px] pr-3 py-2 text-sm flex items-center gap-2"
          :class="ui.activeFleet === f.name ? 'bg-iso-bg-selected text-iso-text-primary' : 'text-iso-text-secondary hover:bg-iso-bg-row-hover'"
          @click="select(f.name)"
        >
          <span class="flex-1 text-left font-mono">{{ f.name }}</span>
          <span class="text-[11px] font-mono text-iso-text-faint">{{ f.host_count }}</span>
        </button>
      </template>

      <div class="h-px bg-iso-border-subtle mt-1"></div>

      <button
        class="w-full px-3 py-2 text-sm flex items-center gap-2 text-iso-text-secondary hover:bg-iso-bg-row-hover"
        @click="goToFleets"
      >
        <Icon name="lucide:plus" class="w-3 h-3 text-iso-text-muted shrink-0" />
        <span class="flex-1 text-left">New fleet…</span>
      </button>

      <button
        class="w-full px-3 py-2 text-sm flex items-center gap-2 text-iso-text-secondary hover:bg-iso-bg-row-hover"
        @click="goToFleets"
      >
        <Icon name="lucide:settings" class="w-3 h-3 text-iso-text-muted shrink-0" />
        <span class="flex-1 text-left">Manage in Settings</span>
        <Icon name="lucide:arrow-up-right" class="w-2.5 h-2.5 text-iso-text-faint" />
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onBeforeUnmount, ref } from 'vue'

const ui = useUiStore()
const fleetsStore = useFleetsStore()
const router = useRouter()
const open = ref(false)

onMounted(() => {
  fleetsStore.load()
  document.addEventListener('click', closeOnOutside)
})

onBeforeUnmount(() => {
  document.removeEventListener('click', closeOnOutside)
})

function closeOnOutside() {
  open.value = false
}

const populatedFleets = computed(() => fleetsStore.fleets.filter(f => f.host_count > 0))

const totalHosts = computed(() =>
  fleetsStore.fleets.reduce((sum, f) => sum + f.host_count, 0)
)

const activeLabel = computed(() => {
  if (ui.activeFleet === 'all') return 'All fleets'
  return ui.activeFleet
})

function select(name: string) {
  ui.setActiveFleet(name)
  open.value = false
}

function goToFleets() {
  open.value = false
  router.push('/settings')
}
</script>
