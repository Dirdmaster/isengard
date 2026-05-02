/*
 * Tailwind CDN config for use in concept HTML.
 * Mirrors the production Tailwind config.
 *
 * Usage in concept HTML:
 *   <script src="https://cdn.tailwindcss.com"></script>
 *   <script src="../tw-config.js"></script>
 */

tailwind.config = {
  theme: {
    extend: {
      colors: {
        iso: {
          'bg-base':         'var(--iso-bg-base)',
          'bg-elevated':     'var(--iso-bg-elevated)',
          'bg-overlay':      'var(--iso-bg-overlay)',
          'text-primary':    'var(--iso-text-primary)',
          'text-secondary':  'var(--iso-text-secondary)',
          'text-muted':      'var(--iso-text-muted)',
          'text-faint':      'var(--iso-text-faint)',
          'success':         'var(--iso-accent-success)',
          'success-soft':    'var(--iso-accent-success-soft)',
          'warn':            'var(--iso-accent-warn)',
          'warn-soft':       'var(--iso-accent-warn-soft)',
          'error':           'var(--iso-accent-error)',
          'error-soft':      'var(--iso-accent-error-soft)',
          'info':            'var(--iso-accent-info)',
          'info-soft':       'var(--iso-accent-info-soft)',
          'border':          'var(--iso-border-subtle)',
          'border-strong':   'var(--iso-border-strong)',
        },
      },
      borderRadius: {
        'iso-sm':   'var(--iso-radius-sm)',
        'iso-md':   'var(--iso-radius-md)',
        'iso-lg':   'var(--iso-radius-lg)',
        'iso-xl':   'var(--iso-radius-xl)',
        'iso-full': 'var(--iso-radius-full)',
      },
      fontFamily: {
        'iso-sans': ['ui-sans-serif', 'system-ui', '-apple-system', 'sans-serif'],
        'iso-mono': ['ui-monospace', 'SF Mono', 'Cascadia Code', 'monospace'],
      },
    },
  },
};
