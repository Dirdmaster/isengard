<template>
  <div class="flex items-center gap-3.5 h-[60px] px-5 border-b border-iso-border-subtle">
    <Icon name="lucide:search" class="w-[18px] h-[18px] text-iso-text-muted shrink-0" />
    <input
      ref="inputRef"
      v-model="query"
      type="text"
      placeholder="Type to navigate, run, or shell…"
      class="flex-1 bg-transparent text-iso-text-primary text-iso-lg outline-none placeholder:text-iso-text-faint cmd-input-caret"
      @keydown="$emit('keydown', $event)"
    />
    <kbd class="px-1.5 py-0.5 rounded text-iso-xs font-mono border border-iso-border-subtle text-iso-text-muted bg-iso-bg-base">esc</kbd>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'

const props = defineProps<{ modelValue: string }>()
const emit = defineEmits<{ 'update:modelValue': [v: string], keydown: [e: KeyboardEvent] }>()

const inputRef = ref<HTMLInputElement>()
const query = ref(props.modelValue)

watch(query, v => emit('update:modelValue', v))
watch(() => props.modelValue, v => { query.value = v })

onMounted(() => inputRef.value?.focus())
</script>

<style scoped>
.cmd-input-caret {
  caret-color: var(--iso-accent-success);
}
</style>
