export default defineNuxtConfig({
  compatibilityDate: '2026-05-01',
  ssr: false,  // SPA mode — Rust serves the static bundle
  modules: [
    '@nuxtjs/tailwindcss',
    '@nuxt/icon',
    '@pinia/nuxt',
  ],
  css: [
    '@fontsource/inter/400.css',
    '@fontsource/inter/500.css',
    '@fontsource/inter/600.css',
    '@fontsource/jetbrains-mono/400.css',
    '@fontsource/jetbrains-mono/500.css',
    '~/assets/css/main.css',
  ],
  app: {
    head: {
      title: 'Isengard Dashboard',
      meta: [
        { name: 'viewport', content: 'width=device-width, initial-scale=1' },
        { name: 'description', content: 'Container fleet management' },
      ],
    },
  },
  nitro: {
    preset: 'static',
  },
  // Dev server proxies /api and /ws to the Rust backend on 9418
  vite: {
    server: {
      proxy: {
        '/api': 'http://localhost:9418',
        '/ws': { target: 'ws://localhost:9418', ws: true },
      },
    },
  },
})
