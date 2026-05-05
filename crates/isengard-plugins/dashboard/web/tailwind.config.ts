import type { Config } from 'tailwindcss'
import animate from 'tailwindcss-animate'

// All iso-* values come from /design/tokens.css (the canonical source),
// imported by assets/css/main.css. Mirror tw-config.js — read CSS vars,
// don't redeclare hex literals.
export default {
  content: [
    './components/**/*.{vue,js,ts}',
    './composables/**/*.{js,ts}',
    './utils/**/*.{js,ts}',
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
          'bg-base':         'var(--iso-bg-base)',
          'bg-elevated':     'var(--iso-bg-elevated)',
          'bg-overlay':      'var(--iso-bg-overlay)',
          'bg-row-hover':    'var(--iso-bg-row-hover)',
          'bg-selected':     'var(--iso-bg-selected)',
          'border-subtle':   'var(--iso-border-subtle)',
          'border-strong':   'var(--iso-border-strong)',
          'text-primary':    'var(--iso-text-primary)',
          'text-secondary':  'var(--iso-text-secondary)',
          'text-muted':      'var(--iso-text-muted)',
          'text-faint':      'var(--iso-text-faint)',
          success:           'var(--iso-accent-success)',
          'success-soft':    'var(--iso-accent-success-soft)',
          warn:              'var(--iso-accent-warn)',
          'warn-soft':       'var(--iso-accent-warn-soft)',
          error:             'var(--iso-accent-error)',
          'error-soft':      'var(--iso-accent-error-soft)',
          info:              'var(--iso-accent-info)',
          'info-soft':       'var(--iso-accent-info-soft)',
          neutral:           'var(--iso-accent-neutral)',
        },
        terminal: {
          bg: 'var(--iso-terminal-bg)',
        },
        // shadcn tokens, wired to iso-* via CSS vars in main.css
        border: 'hsl(var(--border))',
        input: 'hsl(var(--input))',
        ring: 'hsl(var(--ring))',
        background: 'hsl(var(--background))',
        foreground: 'hsl(var(--foreground))',
        primary: {
          DEFAULT: 'hsl(var(--primary))',
          foreground: 'hsl(var(--primary-foreground))',
        },
        secondary: {
          DEFAULT: 'hsl(var(--secondary))',
          foreground: 'hsl(var(--secondary-foreground))',
        },
        destructive: {
          DEFAULT: 'hsl(var(--destructive))',
          foreground: 'hsl(var(--destructive-foreground))',
        },
        muted: {
          DEFAULT: 'hsl(var(--muted))',
          foreground: 'hsl(var(--muted-foreground))',
        },
        accent: {
          DEFAULT: 'hsl(var(--accent))',
          foreground: 'hsl(var(--accent-foreground))',
        },
        popover: {
          DEFAULT: 'hsl(var(--popover))',
          foreground: 'hsl(var(--popover-foreground))',
        },
        card: {
          DEFAULT: 'hsl(var(--card))',
          foreground: 'hsl(var(--card-foreground))',
        },
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', 'sans-serif'],
        mono: ['JetBrains Mono', 'ui-monospace', 'monospace'],
      },
      fontSize: {
        'iso-xs':   ['var(--iso-font-size-xs)',   { lineHeight: 'var(--iso-line-height-xs)' }],
        'iso-sm':   ['var(--iso-font-size-sm)',   { lineHeight: 'var(--iso-line-height-sm)' }],
        'iso-base': ['var(--iso-font-size-base)', { lineHeight: 'var(--iso-line-height-base)' }],
        'iso-md':   ['var(--iso-font-size-md)',   { lineHeight: 'var(--iso-line-height-md)' }],
        'iso-lg':   ['var(--iso-font-size-lg)',   { lineHeight: 'var(--iso-line-height-lg)' }],
      },
      spacing: {
        'iso-1': 'var(--iso-space-1)',
        'iso-2': 'var(--iso-space-2)',
        'iso-3': 'var(--iso-space-3)',
        'iso-4': 'var(--iso-space-4)',
        'iso-5': 'var(--iso-space-5)',
        'iso-6': 'var(--iso-space-6)',
      },
      borderRadius: {
        'iso-sm': 'var(--iso-radius-sm)',
        'iso-md': 'var(--iso-radius-md)',
        'iso-lg': 'var(--iso-radius-lg)',
      },
    },
  },
  plugins: [animate],
} satisfies Config
