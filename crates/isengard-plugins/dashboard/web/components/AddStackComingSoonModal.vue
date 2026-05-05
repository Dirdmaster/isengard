<script setup lang="ts">
defineEmits<{ close: [] }>()

function handleOpenChange(v: boolean, emit: (e: 'close') => void) {
  if (!v) emit('close')
}
</script>

<template>
  <Dialog :open="true" @update:open="(v) => !v && $emit('close')">
    <DialogContent class="bg-iso-bg-base border-iso-border-subtle sm:max-w-[560px]">
      <DialogHeader>
        <DialogTitle class="font-mono text-iso-text-primary">Add stack</DialogTitle>
        <DialogDescription class="text-iso-text-muted">
          Stack creation isn't wired up yet.
        </DialogDescription>
      </DialogHeader>

      <div class="space-y-4 text-sm text-iso-text-secondary leading-relaxed">
        <p>
          Today, stacks appear in this list automatically once your hosts report containers
          carrying either of these labels:
        </p>
        <ul class="space-y-1.5 pl-1">
          <li class="flex items-start gap-2">
            <span class="text-iso-text-faint mt-0.5">•</span>
            <code class="font-mono text-xs text-iso-text-secondary">com.docker.compose.project</code>
          </li>
          <li class="flex items-start gap-2">
            <span class="text-iso-text-faint mt-0.5">•</span>
            <code class="font-mono text-xs text-iso-text-secondary">isengard.stack</code>
          </li>
        </ul>
        <p>
          A first-class
          <span class="text-iso-text-primary">+ Add stack</span>
          wizard
          (<span class="text-iso-text-primary">paste compose</span>,
          <span class="text-iso-text-primary">git sync</span>,
          <span class="text-iso-text-primary">form builder</span>) is on the roadmap.
          For now, deploy via your usual compose flow on the host and Isengard will pick it up.
        </p>
      </div>

      <DialogFooter>
        <Button variant="ghost" @click="$emit('close')">Got it</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
