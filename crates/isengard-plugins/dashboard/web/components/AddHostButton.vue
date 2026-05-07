<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import { useHostsStore } from '~/stores/hosts'
import AddHostModal from './AddHostModal.vue'

/**
 * Hosts page CTA. First host: route to /welcome (full onboarding wizard
 * with reachability checks + listening step). Subsequent hosts: open the
 * AddHostModal directly. The wizard's framing ("Add your first host")
 * makes no sense after the first one.
 */
const router = useRouter()
const hostsStore = useHostsStore()
const showModal = ref(false)

function open() {
  if (hostsStore.hosts.length === 0) {
    router.push('/welcome?step=2&fresh=1')
  } else {
    showModal.value = true
  }
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
  <AddHostModal v-if="showModal" @close="showModal = false" />
</template>
