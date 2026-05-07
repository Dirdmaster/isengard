<template>
  <AppShell>
    <main class="flex-1 flex flex-col gap-5 p-6 min-h-0 overflow-auto">
      <div class="flex items-center justify-between shrink-0">
        <div class="flex flex-col gap-1">
          <h1 class="text-[22px] font-semibold text-iso-text-primary">Home</h1>
          <span class="text-iso-xs text-iso-text-muted">{{ subtitle }}</span>
        </div>
      </div>

      <StatRow />

      <div class="grid grid-cols-1 lg:grid-cols-[3fr_2fr] gap-4 flex-1 min-h-0">
        <ActivityCard />
        <div class="flex flex-col gap-4 min-h-0">
          <ActiveDeploysCard />
          <HealthSnapshotCard />
        </div>
      </div>
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

// Subtitle: relative time since the most-recent event, mirroring the concept
// pattern "Your fleet at a glance · last updated just now".
const subtitle = computed(() => {
  const e = eventsStore.events[0]
  const base = 'Your fleet at a glance'
  if (!e) return `${base} · no activity yet`
  const ms = Date.now() - new Date(e.occurred_at).getTime()
  const s = Math.floor(ms / 1000)
  let rel: string
  if (s < 30) rel = 'just now'
  else if (s < 60) rel = `${s}s ago`
  else if (s < 3600) rel = `${Math.floor(s / 60)}m ago`
  else if (s < 86400) rel = `${Math.floor(s / 3600)}h ago`
  else rel = `${Math.floor(s / 86400)}d ago`
  return `${base} · last updated ${rel}`
})
</script>
