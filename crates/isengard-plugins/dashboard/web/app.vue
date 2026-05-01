<template>
  <div class="min-h-screen bg-iso-bg-base text-iso-text-primary font-sans antialiased">
    <NuxtPage />
    <Toaster
      theme="dark"
      :toast-options="{
        classes: {
          toast: 'bg-iso-bg-overlay border-iso-border-subtle text-iso-text-primary',
          title: 'font-mono',
          description: 'text-iso-text-muted',
          actionButton: 'bg-iso-success text-iso-bg-base',
          cancelButton: 'bg-iso-bg-elevated text-iso-text-muted',
          success: 'border-iso-success',
          error: 'border-iso-error',
          info: 'border-iso-info',
        },
      }"
      position="bottom-right"
      :duration="4000"
    />
    <ConfirmDialogShell />
  </div>
</template>

<script setup lang="ts">
useShortcuts()
</script>
