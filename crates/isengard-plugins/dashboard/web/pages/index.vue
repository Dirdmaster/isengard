<template>
  <AppShell>
    <main class="flex-1 grid grid-cols-[1fr_340px] overflow-hidden min-h-0">
      <div class="flex flex-col overflow-hidden">
        <PageHeader title="Activity" :subtitle="activitySubtitle" />

        <div class="flex-1 flex flex-col min-h-0 overflow-y-auto">
          <StateStrip
            v-for="f in fleetsToShow"
            :key="f.name"
            :fleet="f"
          />

          <EmptyState
            v-if="eventsStore.events.length === 0 && eventsStore.loaded"
            icon="activity"
            title="All quiet"
            description="Events appear as Isengard checks for image updates and applies them. Quiet is the default state. Nothing here means nothing has changed."
          >
            <template v-if="hostsStore.hosts.length === 0" #cta>
              <Button
                variant="outline"
                size="sm"
                class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
                @click="addHostOpen = true"
              >
                <Icon name="lucide:plus" class="w-3.5 h-3.5 mr-1.5" />
                Add a host
              </Button>
            </template>
          </EmptyState>
          <EventTimeline v-else />
        </div>
      </div>
      <Inspector />
    </main>

    <CmdPane />
    <HelpOverlay :open="ui.helpOpen" @close="ui.helpOpen = false" />
    <AddHostModal v-if="addHostOpen" @close="addHostOpen = false" />
  </AppShell>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { shouldShowWizard } from '~/stores/wizard'

const eventsStore = useEventsStore()
const hostsStore = useHostsStore()
const fleetsStore = useFleetsStore()
const ui = useUiStore()
const router = useRouter()

const addHostOpen = ref(false)

onMounted(async () => {
  await Promise.all([
    eventsStore.load(100),
    hostsStore.load(),
    fleetsStore.load(),
  ])
  if (hostsStore.hosts.length === 0 && shouldShowWizard()) {
    router.replace('/welcome')
  }
})

const fleetsToShow = computed(() => {
  const list = ui.activeFleet === 'all'
    ? fleetsStore.fleets
    : fleetsStore.fleets.filter(f => f.name === ui.activeFleet)
  return list.filter(f => f.host_count > 0)
})

const activitySubtitle = computed(() => {
  const events = eventsStore.events.length
  const hosts = hostsStore.hosts.length
  const eventLabel = `${events} ${events === 1 ? 'event' : 'events'}`
  if (hosts === 0) return eventLabel
  return `${eventLabel} · ${hosts} ${hosts === 1 ? 'host' : 'hosts'}`
})
</script>
