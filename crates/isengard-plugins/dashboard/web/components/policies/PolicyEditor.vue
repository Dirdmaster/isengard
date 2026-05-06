<script setup lang="ts">
// stub for T6; will be replaced
//
// Mount point reserved by T5. The full editor lands in Phase 9 T6 with
// fields-with-inheritance form, scope picker, and disabled Approval gate
// option. Until then this placeholder lets the page wire through the
// modal lifecycle without breaking the build.
import type { PolicyDto } from '~/composables/usePolicies'

interface Props {
  mode: 'create' | 'edit'
  existing?: PolicyDto
}
defineProps<Props>()
defineEmits<{ (e: 'close'): void; (e: 'saved'): void }>()
</script>

<template>
  <div class="p-4 text-iso-text-muted">
    Policy editor (T6).
  </div>
</template>
