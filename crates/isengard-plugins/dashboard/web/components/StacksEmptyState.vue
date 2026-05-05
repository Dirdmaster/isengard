<script setup lang="ts">
/**
 * Stacks page empty state — mirrors `design/concepts/stacks/empty-v1.html`.
 *
 * Big dashed-border tile, `{ }` mono icon, headline, and a 3-card grid
 * of mode pickers (Paste compose / Form builder / Git sync). Each card
 * is a button that emits a `pick` event with the chosen mode; the page
 * routes that into AddStackComingSoonModal until the wizard ships.
 */

export type StackMode = 'paste' | 'form' | 'git'

defineEmits<{ pick: [mode: StackMode] }>()
</script>

<template>
  <div class="flex-1 rounded-iso-xl border border-dashed border-iso-border-subtle bg-iso-bg-elevated/40 flex items-center justify-center min-h-0 m-4">
    <div class="w-[820px] flex flex-col items-center gap-8 p-10 text-center">

      <div class="w-14 h-14 rounded-iso-lg border border-iso-border-subtle bg-iso-bg-base flex items-center justify-center font-mono text-iso-text-secondary text-xl font-semibold">
        { }
      </div>

      <div class="flex flex-col gap-2">
        <h2 class="text-2xl font-semibold tracking-tight text-iso-text-primary">Deploy your first stack</h2>
        <p class="text-sm text-iso-text-muted leading-relaxed">
          A <span class="text-iso-text-primary">stack</span> is a docker-compose project running on one or more of your hosts. Pick how you want to define it.
        </p>
      </div>

      <div class="grid grid-cols-3 gap-3 w-full">
        <button
          type="button"
          class="text-left p-4 rounded-iso-lg border border-iso-border-subtle bg-iso-bg-base hover:border-iso-info flex flex-col gap-2 transition-colors"
          @click="$emit('pick', 'paste')"
        >
          <div class="flex items-center gap-2">
            <div class="w-7 h-7 rounded-iso-sm bg-iso-bg-elevated flex items-center justify-center font-mono text-xs text-iso-text-secondary">
              { }
            </div>
            <span class="text-xs font-semibold text-iso-text-primary">Paste compose</span>
          </div>
          <div class="text-[10px] text-iso-text-muted leading-tight text-left">
            Already have a compose.yml? Paste, target hosts, deploy.
          </div>
          <span class="text-[10px] text-iso-info text-left">→ start here (fastest)</span>
        </button>

        <button
          type="button"
          class="text-left p-4 rounded-iso-lg border border-iso-border-subtle bg-iso-bg-base hover:border-iso-info flex flex-col gap-2 transition-colors"
          @click="$emit('pick', 'form')"
        >
          <div class="flex items-center gap-2">
            <div class="w-7 h-7 rounded-iso-sm bg-iso-bg-elevated flex items-center justify-center">
              <Icon name="lucide:sliders-horizontal" class="w-4 h-4 text-iso-text-secondary" />
            </div>
            <span class="text-xs font-semibold text-iso-text-primary">Form builder</span>
          </div>
          <div class="text-[10px] text-iso-text-muted leading-tight text-left">
            Click through services, ports, env. We generate YAML.
          </div>
          <span class="text-[10px] text-iso-text-faint text-left">→ no YAML required</span>
        </button>

        <button
          type="button"
          class="text-left p-4 rounded-iso-lg border border-iso-border-subtle bg-iso-bg-base hover:border-iso-info flex flex-col gap-2 transition-colors"
          @click="$emit('pick', 'git')"
        >
          <div class="flex items-center gap-2">
            <div class="w-7 h-7 rounded-iso-sm bg-iso-bg-elevated flex items-center justify-center">
              <Icon name="lucide:git-branch" class="w-4 h-4 text-iso-text-secondary" />
            </div>
            <span class="text-xs font-semibold text-iso-text-primary">Git sync</span>
          </div>
          <div class="text-[10px] text-iso-text-muted leading-tight text-left">
            Point at a repo. Re-deploy on every commit.
          </div>
          <span class="text-[10px] text-iso-text-faint text-left">→ GitOps</span>
        </button>
      </div>

      <div class="text-[11px] text-iso-text-muted">
        Stacks already running on your hosts? They appear automatically as
        <span class="text-iso-info">discovered</span>.
      </div>
    </div>
  </div>
</template>
