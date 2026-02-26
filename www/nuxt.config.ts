import tailwindcss from '@tailwindcss/vite'

export default defineNuxtConfig({
  compatibilityDate: '2025-01-01',

  app: {
    head: {
      htmlAttrs: { lang: 'en' },
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
        { property: 'og:title', content: 'Isengard | Automatic Docker Container Updates' },
        { property: 'og:description', content: 'The tower that never sleeps. Lightweight, zero-config Docker container auto-updater with registry-first digest detection.' },
        { property: 'og:type', content: 'website' },
        { property: 'og:image', content: 'https://isengard.app/og.png' },
        { property: 'og:image:width', content: '1200' },
        { property: 'og:image:height', content: '630' },
        { property: 'og:image:type', content: 'image/png' },
        { property: 'og:image:alt', content: 'Isengard: The tower that never sleeps' },
        { name: 'twitter:card', content: 'summary_large_image' },
        { name: 'twitter:title', content: 'Isengard | Automatic Docker Container Updates' },
        { name: 'twitter:description', content: 'Lightweight, zero-config Docker container auto-updater with registry-first digest detection.' },
        { name: 'twitter:image', content: 'https://isengard.app/og.png' },
      ],
      link: [
        { rel: 'icon', type: 'image/svg+xml', href: '/favicon.svg' },
      ],
    },
  },

  css: ['~/assets/css/main.css'],

  vite: {
    plugins: [tailwindcss()],
  },

  $development: {
    vite: {
      server: { allowedHosts: true },
    },
  },

  nitro: {
    preset: 'cloudflare-pages',
  },
})
