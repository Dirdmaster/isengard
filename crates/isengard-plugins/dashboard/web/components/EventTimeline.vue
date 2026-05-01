<template>
  <div class="overflow-y-auto h-full">
    <template v-for="(group, label) in grouped" :key="label">
      <DayLabel :label="label" />
      <EventRow
        v-for="e in group"
        :key="e.id"
        :event="e"
        :selected="ui.selectedEventId === e.id"
        @select="ui.selectEvent(e.id)"
      />
    </template>
    <div v-if="eventsStore.events.length === 0" class="p-8 text-center text-iso-text-faint text-iso-sm">
      No events yet.
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const eventsStore = useEventsStore()
const ui = useUiStore()

const grouped = computed(() => {
  const groups: Record<string, typeof eventsStore.events> = {}
  for (const e of eventsStore.events) {
    const date = new Date(e.occurred_at)
    const today = new Date()
    let label: string
    if (sameDay(date, today)) label = 'TODAY · ' + dateLabel(date)
    else if (sameDay(date, dayBefore(today))) label = 'YESTERDAY · ' + dateLabel(date)
    else label = dateLabel(date).toUpperCase()
    if (!groups[label]) groups[label] = []
    groups[label].push(e)
  }
  return groups
})

function sameDay(a: Date, b: Date) {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate()
}

function dayBefore(d: Date) {
  const x = new Date(d)
  x.setDate(x.getDate() - 1)
  return x
}

function dateLabel(d: Date) {
  return d.toLocaleDateString(undefined, { month: 'long', day: 'numeric' })
}
</script>
