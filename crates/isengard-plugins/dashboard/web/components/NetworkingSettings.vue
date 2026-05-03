<template>
  <div class="space-y-8">
    <section>
      <h2 class="text-sm font-semibold text-iso-text-primary mb-4">Adapters</h2>
      <div class="text-iso-text-muted text-sm">Adapter cards land in PC-T9.</div>
    </section>
    <section>
      <RoutingRulesTable @add="onAdd" @edit="onEdit" />
    </section>

    <RoutingRuleEditModal v-model:open="modalOpen" :rule="editingRule" />
  </div>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import RoutingRulesTable from '~/components/RoutingRulesTable.vue'
import RoutingRuleEditModal from '~/components/RoutingRuleEditModal.vue'
import type { RoutingRule } from '~/composables/useRoutingRules'

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
