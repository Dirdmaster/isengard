<script setup lang="ts">
import { ref } from 'vue'

const showModal = ref(false)
</script>

<template>
  <SettingsSection
    title="Agent enrollment"
    description="Add a new host by running a generated install command on it."
  >
    <Button
      variant="outline"
      class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
      @click="showModal = true"
    >
      + Generate install command
    </Button>

    <AddHostModal v-if="showModal" @close="showModal = false" />
  </SettingsSection>
</template>
