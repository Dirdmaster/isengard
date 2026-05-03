<template>
  <div class="space-y-8">
    <section>
      <h2 class="text-sm font-semibold text-iso-text-primary mb-4">Adapters</h2>
      <div class="text-iso-text-muted text-sm">Adapter cards land in PC-T9.</div>
    </section>
    <section>
      <RoutingRulesTable @add="onAdd" @edit="onEdit" />
    </section>
  </div>
</template>

<script setup lang="ts">
import type { RoutingRule } from '~/composables/useRoutingRules'

function onAdd() {
  // Modal hook lands in PC-T8.
  alert('Add rule modal: PC-T8')
}
function onEdit(r: RoutingRule) {
  alert(`Edit rule modal: PC-T8 (id=${r.id})`)
}
</script>
