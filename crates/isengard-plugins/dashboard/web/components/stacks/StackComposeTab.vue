<template>
  <div class="p-6 flex flex-col gap-4">
    <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-6 flex flex-col items-center gap-3 text-center">
      <div class="w-12 h-12 rounded-iso-lg border border-iso-border-strong bg-iso-bg-base flex items-center justify-center font-mono text-iso-text-secondary text-base font-semibold">
        { }
      </div>
      <h2 class="font-mono text-base text-iso-text-primary">compose.yml</h2>
      <p class="text-sm text-iso-text-muted max-w-md leading-relaxed">
        Compose source isn't synced back from hosts yet. When this stack was created
        from a Git repo or paste, the YAML and a sync log will live here — read-only,
        with a link to edit at the source.
      </p>
      <div class="text-[11px] text-iso-text-faint">
        Today, <code class="font-mono text-xs text-iso-text-secondary">compose.yml</code>
        lives on the host filesystem and isn't streamed back to the controller.
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
</script>
