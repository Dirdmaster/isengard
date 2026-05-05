<template>
  <div>
    <!--
      Sub-tab strip styled to match `design/concepts/settings-networking/v1.html`
      sub-nav: simple underline-on-active, sitting above content with a divider.
      Concept v1 uses an info-blue accent for the active item.
    -->
    <div class="flex items-center border-b border-iso-border-subtle mb-6">
      <button
        v-for="tab in tabs"
        :key="tab.key"
        :class="[
          'px-4 py-2.5 text-xs font-medium transition-colors -mb-px border-b-2',
          activeSubTab === tab.key
            ? 'text-iso-text-primary border-iso-info'
            : 'text-iso-text-muted border-transparent hover:text-iso-text-primary',
        ]"
        @click="setActive(tab.key)"
      >
        {{ tab.label }}
      </button>
    </div>

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
