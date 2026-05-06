<script setup lang="ts">
/**
 * Phase 13B logs panel. Mounts on `pages/stacks/[id]/services/[name].vue`,
 * opens a WebSocket via `useLogStream`, and renders backfill + live lines
 * with pause/resume, host tabs, regex filter, and level dropdown.
 *
 * Hard cap 5000 lines client-side (enforced inside `useLogStream`).
 */
import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue'
import { useLogStream, type LogLine } from '~/composables/useLogStream'

const props = defineProps<{
  stackId: string
  serviceName: string
}>()

const tail = ref(200)
const level = ref<'all' | 'info' | 'warn' | 'error'>('all')
const filter = ref('')
const activeHost = ref<string>('all')

const stream = useLogStream(
  computed(() => props.stackId),
  computed(() => props.serviceName),
)

const scrollEl = ref<HTMLDivElement | null>(null)
const stickToBottom = ref(true)

stream.connect()

onBeforeUnmount(() => {
  stream.disconnect()
})

const visibleLines = computed<LogLine[]>(() => {
  let out = stream.lines.value
  if (activeHost.value !== 'all') {
    out = out.filter((l) => l.host === activeHost.value)
  }
  if (level.value !== 'all') {
    const re = levelRegex(level.value)
    out = out.filter((l) => re.test(l.msg))
  }
  if (filter.value) {
    const text = filter.value
    if (text.startsWith('/') && text.endsWith('/') && text.length >= 2) {
      try {
        const re = new RegExp(text.slice(1, -1), 'i')
        out = out.filter((l) => re.test(l.msg))
      } catch {
        const lc = text.toLowerCase()
        out = out.filter((l) => l.msg.toLowerCase().includes(lc))
      }
    } else {
      const lc = text.toLowerCase()
      out = out.filter((l) => l.msg.toLowerCase().includes(lc))
    }
  }
  return out
})

function levelRegex(lv: 'info' | 'warn' | 'error'): RegExp {
  switch (lv) {
    case 'info':
      return /\binfo\b/i
    case 'warn':
      return /\b(warn|warning)\b/i
    case 'error':
      return /\b(err|error|fatal)\b/i
  }
}

function onScroll() {
  const el = scrollEl.value
  if (!el) return
  const distance = el.scrollHeight - el.scrollTop - el.clientHeight
  stickToBottom.value = distance < 32
}

function jumpToLive() {
  const el = scrollEl.value
  if (!el) return
  el.scrollTop = el.scrollHeight
  stickToBottom.value = true
}

watch(
  () => stream.lines.value.length,
  () => {
    if (stickToBottom.value) {
      void nextTick(() => {
        const el = scrollEl.value
        if (el) el.scrollTop = el.scrollHeight
      })
    }
  },
)

function togglePause() {
  if (stream.state.value === 'paused') {
    stream.resume()
  } else {
    stream.pause()
  }
}

function onTailChange() {
  stream.setTail(tail.value)
  stream.clear()
}
</script>

<template>
  <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden flex flex-col min-h-[420px]">
    <!-- Header / toolbar -->
    <div class="px-4 py-2.5 border-b border-iso-border-subtle flex flex-wrap items-center gap-2">
      <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
        LOGS
      </span>
      <span
        v-if="stream.state.value === 'connecting'"
        class="text-[11px] text-iso-text-muted"
      >connecting</span>
      <span
        v-else-if="stream.state.value === 'paused'"
        class="text-[11px] text-iso-warn"
      >paused</span>
      <span
        v-else-if="stream.state.value === 'connected'"
        class="text-[11px] text-iso-success"
      >live</span>
      <span
        v-else-if="stream.state.value === 'closed'"
        class="text-[11px] text-iso-text-faint"
      >closed</span>

      <div class="flex-1" />

      <div class="flex items-center gap-1.5">
        <button
          v-for="h in ['all', ...stream.hosts.value]"
          :key="h"
          class="px-2 py-0.5 rounded-iso-md border text-[11px] font-mono"
          :class="
            activeHost === h
              ? 'border-iso-info text-iso-info'
              : 'border-iso-border-subtle text-iso-text-muted hover:text-iso-text-primary'
          "
          @click="activeHost = h"
        >
          {{ h }}
        </button>
      </div>

      <select
        v-model="tail"
        class="bg-iso-bg-base border border-iso-border-subtle rounded-iso-md text-[11px] px-1.5 py-0.5 text-iso-text-secondary"
        @change="onTailChange"
      >
        <option :value="50">last 50</option>
        <option :value="200">last 200</option>
        <option :value="1000">last 1000</option>
      </select>

      <select
        v-model="level"
        class="bg-iso-bg-base border border-iso-border-subtle rounded-iso-md text-[11px] px-1.5 py-0.5 text-iso-text-secondary"
      >
        <option value="all">all</option>
        <option value="info">info</option>
        <option value="warn">warn</option>
        <option value="error">error</option>
      </select>

      <input
        v-model="filter"
        type="text"
        placeholder="filter (text or /regex/)"
        class="bg-iso-bg-base border border-iso-border-subtle rounded-iso-md text-[11px] px-2 py-0.5 text-iso-text-secondary w-44"
      >

      <button
        class="px-2 py-0.5 rounded-iso-md border border-iso-border-subtle text-[11px] text-iso-text-secondary hover:border-iso-info hover:text-iso-info"
        @click="togglePause"
      >
        {{ stream.state.value === 'paused' ? 'Resume' : 'Pause' }}
      </button>
    </div>

    <!-- Body -->
    <div
      ref="scrollEl"
      class="flex-1 overflow-y-auto bg-iso-bg-base font-mono text-[11.5px] leading-[1.45] p-3 relative"
      @scroll="onScroll"
    >
      <div
        v-if="stream.error.value"
        class="text-iso-error text-xs"
      >
        Stream unavailable: {{ stream.error.value.reason }}
        <button
          class="ml-3 underline"
          @click="stream.connect()"
        >Retry</button>
      </div>

      <div
        v-else-if="visibleLines.length === 0 && stream.state.value !== 'connecting'"
        class="text-iso-text-muted text-xs"
      >
        No logs yet.
      </div>

      <div
        v-else
        class="flex flex-col gap-0.5"
      >
        <div
          v-for="(l, idx) in visibleLines"
          :key="`${l.ts}-${idx}`"
          class="whitespace-pre-wrap break-all"
          :class="{
            'text-iso-text-faint': l.backfill,
            'text-iso-warn': l.stream === 'stderr' && !l.backfill,
            'text-iso-text-primary': l.stream === 'stdout' && !l.backfill,
          }"
        >
          <span class="text-iso-text-muted">{{ l.ts.slice(11, 19) }}</span>
          <span
            v-if="activeHost === 'all' && stream.hosts.value.length > 1"
            class="text-iso-info ml-1"
          >[{{ l.host }}]</span>
          <span class="ml-1">{{ l.msg }}</span>
        </div>
      </div>

      <button
        v-if="!stickToBottom"
        class="sticky bottom-2 left-1/2 -translate-x-1/2 px-2.5 py-1 rounded-iso-md bg-iso-bg-elevated border border-iso-border-strong text-[11px] text-iso-info hover:bg-iso-info hover:text-iso-bg-base"
        @click="jumpToLive"
      >
        Jump to live
      </button>
    </div>

    <!-- Footer counters -->
    <div
      v-if="Object.keys(stream.dropped.value).length > 0"
      class="px-4 py-1.5 border-t border-iso-border-subtle text-[10px] text-iso-warn font-mono"
    >
      Dropped:
      <span
        v-for="(n, h) in stream.dropped.value"
        :key="String(h)"
        class="ml-2"
      >{{ h }}={{ n }}</span>
    </div>
  </section>
</template>
