<script setup lang="ts">
import { useConfirm } from '~/composables/useConfirm'

const { open, opts, resolve } = useConfirm()
</script>

<template>
  <AlertDialog :open="open" @update:open="(v: boolean) => { if (!v) resolve(false) }">
    <AlertDialogContent class="bg-iso-bg-base border-iso-border-subtle">
      <AlertDialogHeader>
        <AlertDialogTitle class="font-mono text-iso-text-primary">{{ opts.title }}</AlertDialogTitle>
        <AlertDialogDescription class="text-iso-text-muted">{{ opts.description }}</AlertDialogDescription>
      </AlertDialogHeader>
      <AlertDialogFooter>
        <AlertDialogCancel @click="resolve(false)">Cancel</AlertDialogCancel>
        <AlertDialogAction
          :class="opts.danger
            ? 'bg-iso-error text-iso-bg-base hover:bg-iso-error/90'
            : 'bg-iso-success text-iso-bg-base hover:bg-iso-success/90'"
          @click="resolve(true)"
        >
          {{ opts.confirmText ?? 'Confirm' }}
        </AlertDialogAction>
      </AlertDialogFooter>
    </AlertDialogContent>
  </AlertDialog>
</template>
