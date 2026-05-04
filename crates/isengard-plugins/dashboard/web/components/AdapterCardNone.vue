<template>
  <div class="border border-iso-border rounded-lg p-4 bg-iso-bg-elevated">
    <div class="flex items-center justify-between mb-3">
      <h3 class="text-sm font-semibold">none</h3>
      <span class="text-[10px] uppercase px-2 py-0.5 rounded bg-iso-bg-base text-iso-text-muted">baseline</span>
    </div>
    <p class="text-xs text-iso-text-muted">
      No external adapter. Routing rules with adapter=none rely on the host having a public IP +
      DNS pointed at it. Pingora handles TLS via Let's Encrypt (HTTP-01).
    </p>
  </div>
</template>
