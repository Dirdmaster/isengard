<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'

const router = useRouter()
const showModal = ref(false)

function reRunWizard() {
  router.push('/welcome?step=1&fresh=1')
}
</script>

<template>
  <SettingsSection
    title="Agent enrollment"
    description="Add a new host by running a generated install command on it."
  >
    <div class="flex items-center gap-3 flex-wrap">
      <Button
        variant="outline"
        class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
        @click="reRunWizard"
      >
        + Add host with wizard
      </Button>
      <Button
        variant="ghost"
        class="text-iso-text-muted hover:text-iso-text-primary"
        @click="showModal = true"
      >
        Generate install command (advanced)
      </Button>
    </div>

    <AddHostModal v-if="showModal" @close="showModal = false" />
  </SettingsSection>
</template>
