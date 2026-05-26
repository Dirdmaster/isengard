<script setup lang="ts">
/**
 * Inline callout for Isengard-flavoured asides.
 *
 * Used in MDC as `::iso-callout` (kebab-cased from the component name).
 *
 * Props:
 * - `kind`: one of `note`, `warn`, `danger`. Selects the left-border accent.
 *   Defaults to `note`.
 *
 * Docus ships its own `::callout` shortcode (Nuxt UI's `UAlert`), so this
 * one stays scoped to Isengard-specific moments: the trust-on-first-use
 * fingerprint warning and the "this command nukes containers" red-flag block.
 */
const props = withDefaults(
  defineProps<{
    kind?: 'note' | 'warn' | 'danger'
  }>(),
  { kind: 'note' },
)

const accent = computed(() => {
  switch (props.kind) {
    case 'danger':
      return 'border-rose-500'
    case 'warn':
      return 'border-amber-500'
    default:
      return 'border-emerald-500'
  }
})
</script>

<template>
  <div
    :class="[
      'iso-callout my-4 rounded-md border-l-4 bg-muted/40 px-4 py-3 text-sm',
      accent,
    ]"
  >
    <slot />
  </div>
</template>
