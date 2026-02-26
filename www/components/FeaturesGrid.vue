<script setup lang="ts">
import { PhEye, PhLightning, PhShieldCheck, PhFeather } from '@phosphor-icons/vue'

const features = [
  {
    icon: PhEye,
    title: 'Registry-first detection',
    description:
      'Checks remote digests via registry HEAD requests in ~50ms. Images are only pulled when an update is actually found.',
  },
  {
    icon: PhLightning,
    title: 'Zero configuration',
    description:
      'Mount the Docker socket and set an interval. Every running container is watched by default. Opt out with a single label.',
  },
  {
    icon: PhShieldCheck,
    title: 'Faithful recreation',
    description:
      'Ports, volumes, networks, env vars, labels, restart policies, resource limits. Every detail preserved across updates.',
  },
  {
    icon: PhFeather,
    title: 'Vanishingly small',
    description:
      '~3MB scratch-based image. Static Go binary, no runtime dependencies. Uses fewer resources than the containers it guards.',
  },
]
</script>

<template>
  <section id="features" class="relative z-10 mx-auto max-w-5xl scroll-mt-20 px-6 py-16 sm:py-32">
    <div class="grid gap-4 sm:grid-cols-2">
      <div
        v-for="(feature, i) in features"
        :key="feature.title"
        class="group cursor-pointer rounded-xl border border-border-subtle bg-bg-raised/50 p-6 transition-all duration-300 hover:border-border hover:bg-bg-raised animate-fade-up"
        :style="{ animationDelay: `${0.7 + i * 0.1}s` }"
      >
        <div
          class="mb-4 flex h-10 w-10 items-center justify-center rounded-lg bg-amber-glow text-amber transition-colors duration-300 group-hover:bg-amber-glow-strong"
        >
          <component :is="feature.icon" :size="20" weight="regular" />
        </div>
        <h3 class="mb-2 text-sm font-semibold tracking-wide">
          {{ feature.title }}
        </h3>
        <p class="text-sm leading-relaxed text-text-muted">
          {{ feature.description }}
        </p>
      </div>
    </div>
  </section>
</template>
