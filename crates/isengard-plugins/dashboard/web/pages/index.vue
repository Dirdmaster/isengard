<template>
  <div class="h-screen flex flex-col">
    <TopBar />

    <!-- Zero hosts: never enrolled -->
    <div v-if="!hostsStore.loading && hostsStore.loaded && hostsStore.hosts.length === 0" class="flex-1 flex items-center justify-center">
      <div class="text-center max-w-md">
        <Icon name="lucide:server" class="w-12 h-12 text-iso-text-faint mx-auto mb-4" />
        <h2 class="font-mono text-lg text-iso-text-primary mb-2">No hosts yet</h2>
        <p class="text-iso-sm text-iso-text-muted mb-6">
          Add your first host and watch its containers appear in real time.
        </p>
        <Button
          variant="outline"
          as-child
          class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
        >
          <NuxtLink to="/hosts">
            <Icon name="lucide:plus" class="w-3.5 h-3.5 mr-1.5" />
            Add a host
          </NuxtLink>
        </Button>
      </div>
    </div>

    <!-- Zero events but hosts exist -->
    <div v-else-if="!eventsStore.loading && eventsStore.loaded && eventsStore.events.length === 0" class="flex-1 px-4 py-12 text-center">
      <p class="text-iso-sm text-iso-text-faint">
        No events yet. Events appear as your hosts check for image updates and apply them.
      </p>
    </div>

    <!-- Normal layout -->
    <main v-else class="flex-1 grid grid-cols-[1fr_340px] overflow-hidden">
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

    <CmdPane />
    <HelpOverlay :open="ui.helpOpen" @close="ui.helpOpen = false" />
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted } from 'vue'

const eventsStore = useEventsStore()
const hostsStore = useHostsStore()
const fleetsStore = useFleetsStore()
const ui = useUiStore()

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
