<template>
  <div class="h-screen flex flex-col">
    <TopBar />
    <main class="flex-1 grid grid-cols-[1fr_340px] overflow-hidden">
      <div class="overflow-y-auto">
        <StateStrip
          v-for="f in fleetsToShow"
          :key="f.name"
          :fleet="f"
        />
        <EventTimeline />
      </div>
      <Inspector />
    </main>
    <BottomStatusBar :connected="connected" :event-count="eventsStore.events.length" />
    <CmdPane />
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted } from 'vue'

const eventsStore = useEventsStore()
const hostsStore = useHostsStore()
const fleetsStore = useFleetsStore()
const ui = useUiStore()
const { connected } = useEventStream()

onMounted(async () => {
  await Promise.all([
    eventsStore.load(100),
    hostsStore.load(),
    fleetsStore.load(),
  ])
})

const fleetsToShow = computed(() => {
  if (ui.activeFleet === 'all') return fleetsStore.fleets
  return fleetsStore.fleets.filter(f => f.name === ui.activeFleet)
})
</script>
