<template>
  <!--
    Baseline "no adapter" card. Concept v1 shows adapters as cards with a
    status pill and per-adapter explainer; this is the no-adapter case.
  -->
  <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-5">
    <div class="flex items-center justify-between mb-3">
      <h3 class="text-sm font-semibold text-iso-text-primary">none</h3>
      <span class="px-2 py-0.5 rounded-iso-sm border border-iso-border-subtle font-mono text-[10px] text-iso-text-muted uppercase tracking-wider">
        baseline
      </span>
    </div>
    <p class="text-xs text-iso-text-muted leading-relaxed">
      No external adapter. Routing rules with adapter=none rely on the host having a public IP +
      DNS pointed at it. Pingora handles TLS via Let's Encrypt (HTTP-01).
    </p>
  </div>
</template>
