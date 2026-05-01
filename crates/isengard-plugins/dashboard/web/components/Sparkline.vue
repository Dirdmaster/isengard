<script setup lang="ts">
interface Props {
  data: number[]
  color?: 'success' | 'warn' | 'error' | 'info'
  width?: number
  height?: number
}

const props = withDefaults(defineProps<Props>(), {
  color: 'info',
  width: 130,
  height: 24,
})

const colorClass = computed(() => ({
  success: 'fill-iso-success',
  warn:    'fill-iso-warn',
  error:   'fill-iso-error',
  info:    'fill-iso-info',
})[props.color])

const max = computed(() => Math.max(1, ...props.data))
const barWidth = computed(() => props.data.length > 0 ? (props.width / props.data.length) - 1 : 0)
</script>

<template>
  <svg :width="width" :height="height" :viewBox="`0 0 ${width} ${height}`" class="overflow-visible">
    <g :class="colorClass">
      <rect
        v-for="(v, i) in data"
        :key="i"
        :x="i * (barWidth + 1)"
        :y="height - (v / max) * height"
        :width="barWidth"
        :height="(v / max) * height"
        rx="1"
      />
    </g>
  </svg>
</template>
