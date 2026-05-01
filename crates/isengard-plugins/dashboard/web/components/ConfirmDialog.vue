<script setup lang="ts">
interface Props {
  open: boolean
  title: string
  description: string
  confirmText?: string
  danger?: boolean
}

withDefaults(defineProps<Props>(), {
  confirmText: 'Confirm',
  danger: false,
})

defineEmits<{ resolve: [confirmed: boolean] }>()
</script>

<template>
  <div v-if="open" class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" @click.self="$emit('resolve', false)">
    <div class="bg-iso-bg-base border border-iso-border-subtle rounded-lg w-[460px] max-w-full p-6 space-y-4">
      <h2 class="font-mono text-base">{{ title }}</h2>
      <p class="text-sm text-iso-text-muted">{{ description }}</p>
      <div class="flex justify-end gap-2 pt-2">
        <button
          class="px-3 py-1.5 text-sm rounded border border-iso-border-subtle hover:border-iso-text-muted"
          @click="$emit('resolve', false)"
        >
          Cancel
        </button>
        <button
          class="px-3 py-1.5 text-sm rounded border"
          :class="danger
            ? 'border-iso-error text-iso-error hover:bg-iso-error/10'
            : 'border-iso-success text-iso-success hover:bg-iso-success/10'"
          @click="$emit('resolve', true)"
        >
          {{ confirmText }}
        </button>
      </div>
    </div>
  </div>
</template>
