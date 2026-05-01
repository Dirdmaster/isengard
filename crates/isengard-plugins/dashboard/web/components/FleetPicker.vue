<template>
  <div class="relative">
    <button
      class="h-8 px-3 rounded-md bg-iso-bg-overlay border border-iso-border-subtle text-sm font-medium text-iso-text-secondary flex items-center gap-2 hover:border-iso-border-strong"
      @click="open = !open"
    >
      {{ activeLabel }}
      <Icon name="lucide:chevron-down" class="w-3.5 h-3.5 text-iso-text-faint" />
    </button>

    <div
      v-if="open"
      class="absolute top-full left-0 mt-1.5 min-w-52 bg-iso-bg-overlay border border-iso-border-strong rounded-md shadow-xl z-50 py-1.5"
    >
      <button
        class="w-full text-left px-3 py-2 text-sm hover:bg-iso-bg-row-hover"
        :class="ui.activeFleet === 'all' ? 'text-iso-text-primary' : 'text-iso-text-muted'"
        @click="select('all')"
      >
        All fleets
      </button>
      <template v-if="populatedFleets.length">
        <div class="h-px bg-iso-border-subtle my-1"></div>
        <button
          v-for="f in populatedFleets"
          :key="f.name"
          class="w-full text-left px-3 py-2 text-sm flex items-center justify-between hover:bg-iso-bg-row-hover"
          :class="ui.activeFleet === f.name ? 'text-iso-text-primary' : 'text-iso-text-muted'"
          @click="select(f.name)"
        >
          <span class="font-mono">{{ f.name }}</span>
          <span class="text-xs text-iso-text-faint">{{ f.host_count }} {{ f.host_count === 1 ? 'host' : 'hosts' }}</span>
        </button>
      </template>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'

const ui = useUiStore()
const fleetsStore = useFleetsStore()
const open = ref(false)

onMounted(() => fleetsStore.load())

const populatedFleets = computed(() => fleetsStore.fleets.filter(f => f.host_count > 0))

const activeLabel = computed(() => {
  if (ui.activeFleet === 'all') return 'All fleets'
  return ui.activeFleet
})

function select(name: string) {
  ui.setActiveFleet(name)
  open.value = false
}
</script>
