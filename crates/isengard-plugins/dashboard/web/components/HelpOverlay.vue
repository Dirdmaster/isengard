<template>
  <div v-if="open" class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" @click.self="$emit('close')">
    <div class="bg-iso-bg-base border border-iso-border-subtle rounded-iso-lg w-[680px] max-w-full p-8">
      <header class="flex items-center justify-between mb-6">
        <h2 class="font-mono text-lg text-iso-text-primary">Keyboard shortcuts</h2>
        <button class="text-iso-text-muted hover:text-iso-text-primary" @click="$emit('close')">
          <Icon name="lucide:x" class="w-4 h-4" />
        </button>
      </header>
      <div class="grid grid-cols-2 gap-x-10 gap-y-6">
        <section v-for="g in groups" :key="g.title">
          <h3 class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-3">{{ g.title }}</h3>
          <dl class="space-y-2">
            <div v-for="i in g.items" :key="i.desc" class="flex items-center justify-between gap-4">
              <dt class="text-iso-sm text-iso-text-secondary">{{ i.desc }}</dt>
              <dd class="flex items-center gap-1">
                <kbd v-for="k in i.keys" :key="k" class="px-1.5 py-0.5 bg-iso-bg-elevated rounded font-mono text-iso-xs text-iso-text-secondary">{{ k }}</kbd>
              </dd>
            </div>
          </dl>
        </section>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
defineProps<{ open: boolean }>()
defineEmits<{ close: [] }>()

const groups = [
  {
    title: 'Global',
    items: [
      { keys: ['⌘K'], desc: 'open cmd pane (navigator)' },
      { keys: ['⌘.'], desc: 'toggle cmd pane position (center ↔ docked)' },
      { keys: ['/'],  desc: 'focus filter on current page' },
      { keys: ['?'],  desc: 'show this overlay' },
      { keys: ['j', 'k'], desc: 'move down/up in lists' },
      { keys: ['Esc'], desc: 'close overlay or cmd pane' },
    ],
  },
  {
    title: 'In cmd pane navigator',
    items: [
      { keys: ['↑', '↓'], desc: 'move selection' },
      { keys: ['Enter'], desc: 'select result' },
      { keys: [':'], desc: 'actions only' },
      { keys: ['$'], desc: 'shell command (after picking a container)' },
    ],
  },
  {
    title: 'In cmd pane terminal',
    items: [
      { keys: ['⌘P'], desc: 'back to navigator' },
      { keys: ['⌘N'], desc: 'new shell session' },
      { keys: ['⌘W'], desc: 'close pane' },
      { keys: ['⌘↑'], desc: 'scrollback' },
    ],
  },
]
</script>
