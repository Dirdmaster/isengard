<script setup lang="ts">
import { useRouter } from 'vue-router'

/**
 * Hosts page CTA. Per `design/pages/hosts.md` this enters the wizard at step 2
 * with `fresh=1` so the form is empty. Settings → Enrollment still uses
 * `<AddHostModal>` directly for the in-place flow; only the entry-point button
 * routes to the wizard.
 */
const router = useRouter()

function open() {
  router.push('/welcome?step=2&fresh=1')
}
</script>

<template>
  <Button
    variant="outline"
    size="sm"
    class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success font-medium"
    @click="open"
  >
    <Icon name="lucide:plus" class="w-3.5 h-3.5 mr-1.5" />
    Add host
  </Button>
</template>
