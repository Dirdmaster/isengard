<template>
  <div class="min-h-screen bg-bg font-sans text-text overflow-x-hidden pt-16">
    <!-- Subtle atmospheric background -->
    <div
      class="pointer-events-none fixed inset-0"
      aria-hidden="true"
      style="box-shadow: inset 0 -200px 400px -200px oklch(0.78 0.08 75 / 0.04), inset 0 200px 400px -100px oklch(0.10 0.005 60 / 0.8)"
    />

    <TheNav />
    <HeroSection />
    <SectionDivider />
    <FeaturesGrid />
    <SectionDivider />
    <InstallSection />
    <TheFooter />
  </div>
</template>
