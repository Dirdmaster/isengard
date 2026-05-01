<script setup lang="ts">
import { ref } from 'vue'

const open = ref(false)
</script>

<template>
  <Button
    variant="outline"
    size="sm"
    class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success font-medium"
    @click="open = true"
  >
    <Icon name="lucide:plus" class="w-3.5 h-3.5 mr-1.5" />
    Add host
  </Button>
  <AddHostModal v-if="open" @close="open = false" />
</template>
