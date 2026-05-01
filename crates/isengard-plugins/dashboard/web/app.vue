<template>
  <div class="min-h-screen bg-iso-bg-base text-iso-text-primary font-sans antialiased">
    <NuxtPage />
    <ToastContainer />
    <ConfirmDialog
      :open="confirm.open.value"
      :title="confirm.opts.value.title"
      :description="confirm.opts.value.description"
      :confirm-text="confirm.opts.value.confirmText"
      :danger="confirm.opts.value.danger"
      @resolve="confirm.resolve"
    />
  </div>
</template>

<script setup lang="ts">
useShortcuts()
const confirm = useConfirm()
</script>
