<script setup lang="ts">
import type { Toast } from '~/stores/toasts'
import { useToastsStore } from '~/stores/toasts'

defineProps<{ toast: Toast }>()
const store = useToastsStore()

const kindClasses: Record<string, string> = {
  success: 'border-iso-success bg-iso-success/10 text-iso-success',
  error:   'border-iso-error bg-iso-error/10 text-iso-error',
  info:    'border-iso-border-subtle bg-iso-bg-overlay text-iso-text-secondary',
}

const kindIcons: Record<string, string> = {
  success: 'lucide:check-circle',
  error:   'lucide:x-circle',
  info:    'lucide:info',
}
</script>

<template>
  <div
    class="flex items-center gap-3 px-4 py-3 rounded-lg border min-w-[280px] max-w-[420px] shadow-lg"
    :class="kindClasses[toast.kind]"
  >
    <Icon :name="kindIcons[toast.kind]" class="w-4 h-4 shrink-0" />
    <span class="text-sm flex-1">{{ toast.text }}</span>
    <button class="opacity-60 hover:opacity-100" @click="store.dismiss(toast.id)">
      <Icon name="lucide:x" class="w-3.5 h-3.5" />
    </button>
  </div>
</template>
