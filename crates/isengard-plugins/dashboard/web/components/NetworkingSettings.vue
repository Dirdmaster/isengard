<template>
  <NetworkingSubTabs :tabs="subTabs" default-tab="adapter" v-slot="{ activeSubTab }">
    <section v-if="activeSubTab === 'adapter'" class="flex flex-col gap-4">
      <div class="flex flex-col gap-0.5">
        <h2 class="text-sm font-semibold text-iso-text-primary">Adapters</h2>
        <p class="text-xs text-iso-text-muted">
          Choose how this controller is reachable. Adapter config is per-host today; multi-adapter
          per controller arrives with the global adapter model.
        </p>
      </div>
      <div v-if="!firstHost" class="rounded-iso-lg border border-dashed border-iso-border-subtle bg-iso-bg-elevated/40 px-5 py-8 text-center text-xs text-iso-text-muted">
        No hosts enrolled yet. Add a host first, then configure adapters per-host.
      </div>
      <div v-else class="grid gap-4">
        <AdapterCardNone />
        <AdapterCardTailscale :host-id="firstHost.id" />
        <AdapterCardCfTunnel :host-id="firstHost.id" />
      </div>
    </section>

    <section v-else-if="activeSubTab === 'proxy'" class="flex flex-col gap-4">
      <RoutingRulesTable @add="onAdd" @edit="onEdit" />
      <RoutingRuleEditModal v-model:open="modalOpen" :rule="editingRule" />
    </section>
  </NetworkingSubTabs>
</template>

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useHostsStore } from '~/stores/hosts'
import RoutingRulesTable from '~/components/RoutingRulesTable.vue'
import RoutingRuleEditModal from '~/components/RoutingRuleEditModal.vue'
import AdapterCardNone from '~/components/AdapterCardNone.vue'
import AdapterCardTailscale from '~/components/AdapterCardTailscale.vue'
import AdapterCardCfTunnel from '~/components/AdapterCardCfTunnel.vue'
import NetworkingSubTabs from '~/components/settings/NetworkingSubTabs.vue'
import type { RoutingRule } from '~/composables/useRoutingRules'

const subTabs = [
  { key: 'adapter', label: 'Adapter' },
  { key: 'proxy', label: 'Proxy' },
]

const hostsStore = useHostsStore()
const firstHost = computed(() => hostsStore.hosts[0])
onMounted(() => {
  if (!hostsStore.loaded) hostsStore.load()
})

const modalOpen = ref(false)
const editingRule = ref<RoutingRule | null>(null)
function onAdd() {
  editingRule.value = null
  modalOpen.value = true
}
function onEdit(r: RoutingRule) {
  editingRule.value = r
  modalOpen.value = true
}
</script>
