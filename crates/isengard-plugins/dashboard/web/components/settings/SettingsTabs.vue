<template>
  <div>
    <nav class="flex gap-1 border-b border-iso-border mb-6">
      <button
        v-for="tab in tabs"
        :key="tab.key"
        :class="[
          'px-4 py-2 text-sm font-medium border-b-2 -mb-px transition-colors',
          activeTab === tab.key
            ? 'border-iso-info text-iso-text-primary'
            : 'border-transparent text-iso-text-muted hover:text-iso-text-primary hover:border-iso-border',
        ]"
        @click="setActive(tab.key)"
      >
        {{ tab.label }}
      </button>
    </nav>

    <div>
      <slot :active-tab="activeTab" />
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'

interface Tab {
  key: string
  label: string
}

const props = defineProps<{ tabs: Tab[]; defaultTab?: string }>()
const route = useRoute()
const router = useRouter()

const activeTab = computed(() => {
  const t = route.query.tab as string | undefined
  if (t && props.tabs.some(tab => tab.key === t)) return t
  return props.defaultTab ?? props.tabs[0]?.key ?? ''
})

function setActive(key: string) {
  router.push({ query: { ...route.query, tab: key } })
}
</script>
