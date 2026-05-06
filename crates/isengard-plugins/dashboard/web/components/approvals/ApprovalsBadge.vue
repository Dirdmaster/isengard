<script setup lang="ts">
/**
 * Pill rendering the open-approval count for the TopBar nav. Hidden when
 * the count is zero. The actual polling is owned by
 * `usePendingApprovalsCount` which dedupes across mount points so the badge
 * does not refetch on every route change.
 */
const { count } = usePendingApprovalsCount()
</script>

<template>
  <span
    v-if="count > 0"
    class="ml-1 inline-flex items-center justify-center min-w-[18px] h-[18px] px-1.5 rounded-full border border-iso-warn/40 bg-iso-warn-soft font-mono text-[10px] font-semibold text-iso-warn"
    :aria-label="`${count} pending approvals`"
  >
    {{ count }}
  </span>
</template>
