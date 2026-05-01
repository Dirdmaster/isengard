<template>
  <div class="flex-1 flex flex-col min-h-0">
    <TopBar />

    <main class="flex-1 grid grid-cols-[1fr_340px] overflow-hidden">
      <div class="flex flex-col overflow-hidden">
        <header class="flex items-center justify-between px-4 py-3 border-b border-iso-border-subtle">
          <div class="flex items-center gap-3">
            <h1 class="font-mono text-base text-iso-text-primary">Activity</h1>
            <span class="text-xs text-iso-text-muted">
              {{ eventsStore.events.length }} {{ eventsStore.events.length === 1 ? 'event' : 'events' }}
              <template v-if="hostsStore.hosts.length"> · {{ hostsStore.hosts.length }} {{ hostsStore.hosts.length === 1 ? 'host' : 'hosts' }}</template>
            </span>
          </div>
          <Button
            variant="outline"
            size="sm"
            class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
            @click="addHostOpen = true"
          >
            <Icon name="lucide:plus" class="w-3.5 h-3.5 mr-1.5" />
            Add host
          </Button>
        </header>

        <div class="flex-1 overflow-y-auto">
          <StateStrip
            v-for="f in fleetsToShow"
            :key="f.name"
            :fleet="f"
          />

          <div v-if="eventsStore.events.length === 0 && eventsStore.loaded" class="px-6 py-8 text-center text-sm text-iso-text-faint">
            No events yet.
          </div>
          <EventTimeline v-else />
        </div>
      </div>
      <Inspector />
    </main>

    <CmdPane />
    <HelpOverlay :open="ui.helpOpen" @close="ui.helpOpen = false" />
    <AddHostModal v-if="addHostOpen" @close="addHostOpen = false" />
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'

const eventsStore = useEventsStore()
const hostsStore = useHostsStore()
const fleetsStore = useFleetsStore()
const ui = useUiStore()

const addHostOpen = ref(false)

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
