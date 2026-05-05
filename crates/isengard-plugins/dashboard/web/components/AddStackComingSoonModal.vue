<script setup lang="ts">
/**
 * "Coming soon" modal for the stack-creation wizard. The real flow
 * (paste compose / form builder / git sync) ships in a later phase;
 * for now we surface a per-mode preview so the empty-state cards on
 * /stacks have somewhere to land.
 */

export type StackMode = 'paste' | 'form' | 'git'

const props = withDefaults(defineProps<{ mode?: StackMode | null }>(), {
  mode: null,
})

defineEmits<{ close: [] }>()

interface ModeCopy {
  title: string
  blurb: string
}

const modeCopy: Record<StackMode, ModeCopy> = {
  paste: {
    title: 'Paste compose · coming soon',
    blurb: 'Drop in a compose.yml, pick which hosts run it, deploy. The fastest path: it lands first.',
  },
  form: {
    title: 'Form builder · coming soon',
    blurb: 'Click through services, ports, volumes, env. Isengard generates the YAML and ships it.',
  },
  git: {
    title: 'Git sync · coming soon',
    blurb: 'Point at a repo + path. Every commit triggers a redeploy. GitOps without the YAML pipeline.',
  },
}

const heading = computed(() => (props.mode ? modeCopy[props.mode].title : 'Add stack'))
const blurb = computed(() => (props.mode ? modeCopy[props.mode].blurb : "Stack creation isn't wired up yet."))
</script>

<template>
  <Dialog :open="true" @update:open="(v) => !v && $emit('close')">
    <DialogContent class="bg-iso-bg-base border-iso-border-subtle sm:max-w-[560px]">
      <DialogHeader>
        <DialogTitle class="font-mono text-iso-text-primary">{{ heading }}</DialogTitle>
        <DialogDescription class="text-iso-text-muted">
          {{ blurb }}
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
          (<span :class="mode === 'paste' ? 'text-iso-info font-semibold' : 'text-iso-text-primary'">paste compose</span>,
          <span :class="mode === 'form'  ? 'text-iso-info font-semibold' : 'text-iso-text-primary'">form builder</span>,
          <span :class="mode === 'git'   ? 'text-iso-info font-semibold' : 'text-iso-text-primary'">git sync</span>)
          is on the roadmap. For now, deploy via your usual compose flow on the host and Isengard will pick it up.
        </p>
      </div>

      <DialogFooter>
        <Button variant="ghost" @click="$emit('close')">Got it</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
