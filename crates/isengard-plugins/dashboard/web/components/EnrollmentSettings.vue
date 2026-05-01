<script setup lang="ts">
import { ref } from 'vue'

const showModal = ref(false)
</script>

<template>
  <SettingsSection
    title="Agent enrollment"
    description="Add a new host by running a generated install command on it."
  >
    <button
      class="px-4 py-2 rounded border border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
      @click="showModal = true"
    >
      + Generate install command
    </button>

    <div v-if="showModal" class="text-xs text-iso-text-muted mt-3">(modal landing in next commit)</div>
  </SettingsSection>
</template>
