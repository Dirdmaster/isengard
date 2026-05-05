<template>
  <AppShell>
    <main class="flex-1 grid grid-cols-[1fr_340px] min-h-0 overflow-hidden">
      <!-- Left column: fleet group cards + activity timeline -->
      <div class="flex flex-col min-h-0 overflow-y-auto">
        <!-- Fleet group cards (one per fleet that has at least one host) -->
        <div class="flex flex-col gap-2 px-3 pt-3 pb-1 shrink-0">
          <FleetGroupCard
            v-for="fleet in visibleFleets"
            :key="fleet.name"
            :fleet="fleet"
          />
          <!-- Empty: no fleets / hosts at all -->
          <div
            v-if="visibleFleets.length === 0"
            class="rounded-iso-md bg-iso-bg-elevated border border-iso-border-subtle px-4 py-6 text-center text-iso-text-faint text-iso-sm"
          >
            No fleets yet — enroll a host to start tracking.
          </div>
        </div>

        <!-- Activity timeline (day-grouped, reverse-chron) -->
        <div class="flex-1 min-h-0">
          <EventTimeline />
        </div>
      </div>

      <!-- Right column: Inspector rail -->
      <Inspector class="overflow-y-auto" />
    </main>
  </AppShell>
</template>

<script setup lang="ts">
import { computed, onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { shouldShowWizard } from '~/stores/wizard'

const eventsStore = useEventsStore()
const hostsStore = useHostsStore()
const fleetsStore = useFleetsStore()
const stacksStore = useStacksStore()
const router = useRouter()

onMounted(async () => {
  await Promise.all([
    eventsStore.load(100),
    hostsStore.load(),
    fleetsStore.load(),
    stacksStore.fetchAll(),
  ])
  if (hostsStore.hosts.length === 0 && shouldShowWizard()) {
    router.replace('/welcome')
  }
})

// Show fleets the user actually has hosts in. Falls back to the raw fleet list
// when host data hasn't loaded yet, so the cards don't briefly disappear.
const visibleFleets = computed(() => {
  const enrolledFleetNames = new Set(hostsStore.hosts.map(h => h.fleet))
  if (enrolledFleetNames.size === 0) return fleetsStore.fleets
  return fleetsStore.fleets.filter(f => enrolledFleetNames.has(f.name))
})
</script>
