<template>
  <div class="relative">
    <button
      class="px-2.5 py-1 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm font-medium text-iso-text-secondary flex items-center gap-1.5 hover:border-iso-border-strong"
      @click="open = !open"
    >
      {{ activeLabel }}
      <Icon name="lucide:chevron-down" class="w-3 h-3 text-iso-text-faint" />
    </button>

    <div
      v-if="open"
      class="absolute top-full left-0 mt-1 min-w-48 bg-iso-bg-overlay border border-iso-border-strong rounded-iso-md shadow-lg z-50 py-1"
    >
      <button
        class="w-full text-left px-3 py-1.5 text-iso-sm hover:bg-iso-bg-row-hover"
        :class="ui.activeFleet === 'all' ? 'text-iso-text-primary' : 'text-iso-text-muted'"
        @click="select('all')"
      >
        All fleets
      </button>
      <div class="h-px bg-iso-border-subtle my-1"></div>
      <button
        v-for="f in fleetsStore.fleets"
        :key="f.name"
        class="w-full text-left px-3 py-1.5 text-iso-sm flex items-center justify-between hover:bg-iso-bg-row-hover"
        :class="ui.activeFleet === f.name ? 'text-iso-text-primary' : 'text-iso-text-muted'"
        @click="select(f.name)"
      >
        <span>{{ f.name }}</span>
        <span class="text-iso-xs text-iso-text-faint">{{ f.host_count }} hosts</span>
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'

const ui = useUiStore()
const fleetsStore = useFleetsStore()
const open = ref(false)

onMounted(() => fleetsStore.load())

const activeLabel = computed(() => {
  if (ui.activeFleet === 'all') return 'All fleets'
  return ui.activeFleet
})

function select(name: string) {
  ui.setActiveFleet(name)
  open.value = false
}
</script>
