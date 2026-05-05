<template>
  <div class="min-h-screen flex flex-col bg-iso-bg-base text-iso-text-primary font-sans antialiased">
    <div class="flex-1 flex flex-col min-h-0">
      <NuxtPage />
    </div>
    <BottomStatusBar :connection-state="connectionState" :event-count="eventsStore.events.length" />
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
    <CmdPane />
    <HelpOverlay :open="ui.helpOpen" @close="ui.helpOpen = false" />
  </div>
</template>

<script setup lang="ts">
const eventsStore = useEventsStore()
const ui = useUiStore()
const { connectionState } = useEventStream()

useShortcuts()
</script>
