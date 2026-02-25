export default defineNuxtConfig({
  compatibilityDate: '2025-01-01',

  devtools: { enabled: false },

  app: {
    head: {
      title: 'Isengard | Automatic Docker Container Updates',
      meta: [
        { charset: 'utf-8' },
        { name: 'viewport', content: 'width=device-width, initial-scale=1' },
        {
          name: 'description',
          content:
            'The tower that never sleeps. Lightweight, zero-config Docker container auto-updater with registry-first digest detection.',
        },
        { name: 'theme-color', content: '#1a1410' },
      ],
      link: [
        { rel: 'icon', type: 'image/svg+xml', href: '/favicon.svg' },
      ],
    },
  },

  css: ['~/assets/css/main.css'],

  vite: {
    plugins: [
      // @ts-expect-error - Tailwind v4 vite plugin
      (await import('@tailwindcss/vite')).default(),
    ],
    server: {
      allowedHosts: true,
    },
  },

  nitro: {
    preset: 'static',
  },
})
