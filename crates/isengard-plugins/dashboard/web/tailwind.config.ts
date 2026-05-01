import type { Config } from 'tailwindcss'

export default {
  content: [
    './components/**/*.{vue,js,ts}',
    './layouts/**/*.vue',
    './pages/**/*.vue',
    './plugins/**/*.{js,ts}',
    './app.vue',
    './error.vue',
  ],
  theme: {
    extend: {
      colors: {
        iso: {
          'bg-base': '#0b0d0f',
          'bg-elevated': '#0e1114',
          'bg-overlay': '#15181b',
          'bg-row-hover': '#11151a',
          'bg-selected': '#0f1a12',
          'border-subtle': '#1c2024',
          'border-strong': '#2a2f35',
          'text-primary': '#e6e8eb',
          'text-secondary': '#d8dde2',
          'text-muted': '#8a9099',
          'text-faint': '#6f7680',
          success: '#4ade80',
          'success-soft': '#1e3826',
          warn: '#fbbf24',
          error: '#f87171',
          info: '#c084fc',
          neutral: '#94a3b8',
        },
        terminal: {
          bg: '#050505',
        },
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', 'sans-serif'],
        mono: ['JetBrains Mono', 'ui-monospace', 'monospace'],
      },
      fontSize: {
        'iso-xs': ['11px', { lineHeight: '14px' }],
        'iso-sm': ['12px', { lineHeight: '16px' }],
        'iso-base': ['13px', { lineHeight: '18px' }],
        'iso-md': ['14px', { lineHeight: '20px' }],
        'iso-lg': ['16px', { lineHeight: '22px' }],
      },
      spacing: {
        'iso-1': '4px',
        'iso-2': '8px',
        'iso-3': '12px',
        'iso-4': '16px',
        'iso-5': '20px',
        'iso-6': '24px',
      },
      borderRadius: {
        'iso-sm': '4px',
        'iso-md': '6px',
        'iso-lg': '8px',
      },
    },
  },
  plugins: [],
} satisfies Config
