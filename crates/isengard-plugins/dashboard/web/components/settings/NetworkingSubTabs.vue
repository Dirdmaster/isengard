<template>
  <div>
    <nav class="inline-flex rounded-md border border-iso-border-subtle bg-iso-bg-elevated p-0.5 mb-6">
      <button
        v-for="tab in tabs"
        :key="tab.key"
        :class="[
          'px-3 py-1 text-xs font-medium rounded transition-colors',
          activeSubTab === tab.key
            ? 'bg-iso-bg-base text-iso-text-primary shadow-sm'
            : 'text-iso-text-muted hover:text-iso-text-primary',
        ]"
        @click="setActive(tab.key)"
      >
        {{ tab.label }}
      </button>
    </nav>

    <div>
      <slot :active-sub-tab="activeSubTab" />
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

const activeSubTab = computed(() => {
  const t = route.query.subtab as string | undefined
  if (t && props.tabs.some(tab => tab.key === t)) return t
  return props.defaultTab ?? props.tabs[0]?.key ?? ''
})

function setActive(key: string) {
  router.push({ query: { ...route.query, subtab: key } })
}
</script>
