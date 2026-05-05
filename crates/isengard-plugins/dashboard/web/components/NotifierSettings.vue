<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useSettingsStore } from '~/stores/settings'

/**
 * Notifier settings — rebuilt to match `design/concepts/settings-notifier/v1.html`.
 *
 * Concept v1 shows per-channel cards: status dot + channel name + provider tag,
 * health pill, Edit / Send test / Disable buttons, last-message timestamp,
 * subscribed event-kind chips, fleet filter.
 *
 * Implementation today only has env-var-gated on/off toggles for Telegram +
 * Discord (Phase 4 backend; the spec'd UI is a Phase 4.5 polish pass that
 * hasn't been planned). To stay design-honest:
 *   - Render per-channel cards (concept fidelity for layout)
 *   - The card *body* surfaces what's actually wired up — env-var name and
 *     enable toggle. Timestamps / test / rate-limits / kind filters are
 *     marked "soon" (no backend support yet)
 *   - HTTP channel is pure "coming soon" (no backend at all)
 *   - "+ Add channel" header button is disabled with a "soon" tooltip
 *
 * Existing toggle behavior is preserved verbatim: operators who set the env
 * vars still flip the same `notifier.{provider}.enabled` setting key.
 */

const settings = useSettingsStore()
if (!settings.loaded) await settings.load()

const toast = useToast()

interface ChannelMeta {
  key: 'telegram' | 'discord'
  label: string
  provider: string
  envHint: string
  description: string
}

const channels: ChannelMeta[] = [
  {
    key: 'telegram',
    label: 'Telegram',
    provider: 'Telegram',
    envHint: 'TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID',
    description: 'Bot-driven channel. Set env vars on the controller, then toggle below.',
  },
  {
    key: 'discord',
    label: 'Discord',
    provider: 'Discord',
    envHint: 'DISCORD_WEBHOOK_URL',
    description: 'Webhook-driven channel. Set DISCORD_WEBHOOK_URL on the controller, then toggle below.',
  },
]

const settingKey = (k: ChannelMeta['key']) => `notifier.${k}.enabled` as const

const enabledMap = computed<Record<ChannelMeta['key'], boolean>>(() => ({
  telegram: Boolean(settings.values[settingKey('telegram')]),
  discord: Boolean(settings.values[settingKey('discord')]),
}))

const toggling = ref<Record<string, boolean>>({})

async function setEnabled(ch: ChannelMeta, v: boolean) {
  toggling.value[ch.key] = true
  try {
    await settings.patch({ [settingKey(ch.key)]: v })
    toast.success(`${ch.label} notifications ${v ? 'enabled' : 'disabled'}`)
  } catch (e) {
    toast.error(`Save failed: ${e instanceof Error ? e.message : String(e)}`)
  } finally {
    toggling.value[ch.key] = false
  }
}

const callbackUrl = ref<string>('https://<controller>/api/v1/notifier/callback/<provider>')
onMounted(() => {
  if (typeof window !== 'undefined' && window.location?.origin) {
    callbackUrl.value = `${window.location.origin}/api/v1/notifier/callback/<provider>`
  }
})
</script>

<template>
  <div class="flex flex-col gap-4">
    <div class="flex items-center justify-between">
      <div class="flex flex-col gap-0.5">
        <h2 class="text-sm font-semibold text-iso-text-primary">Notifier channels</h2>
        <span class="text-[11px] text-iso-text-muted">
          Outbound notifications for events + approval requests. Per-channel kind filters,
          test button, and rate limits arrive in Phase 4.5.
        </span>
      </div>
      <button
        class="px-3 py-1.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-subtle text-xs text-iso-text-faint cursor-not-allowed"
        disabled
        title="Adding new channels requires backend support not yet shipped"
      >
        + Add channel
        <span class="ml-1 text-[10px] text-iso-text-faint">(soon)</span>
      </button>
    </div>

    <!-- Approval callback URL: surfaced per concept, but the reachability check
         needs an endpoint we haven't shipped. Render as info-only banner. -->
    <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-4 flex items-start gap-3">
      <div class="w-1.5 h-1.5 rounded-full bg-iso-info mt-1.5"></div>
      <div class="flex flex-col gap-0.5 flex-1 min-w-0">
        <span class="text-xs font-semibold text-iso-text-primary">Approval callback URL</span>
        <span class="text-[11px] text-iso-text-muted">
          Telegram + Discord can hit
          <span class="font-mono text-iso-text-secondary">{{ callbackUrl }}</span>
          for inline-button approvals once Phase 9 lands. Reachability check pending.
        </span>
      </div>
    </section>

    <!-- Configured channels -->
    <section
      v-for="ch in channels"
      :key="ch.key"
      class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-5 flex flex-col gap-3"
    >
      <div class="flex items-center justify-between">
        <div class="flex items-center gap-3">
          <div
            class="w-2 h-2 rounded-full"
            :class="enabledMap[ch.key] ? 'bg-iso-success' : 'bg-iso-text-faint'"
          ></div>
          <span class="text-sm font-semibold text-iso-text-primary">
            {{ ch.label }}
            <span class="text-iso-text-muted font-normal">· {{ ch.provider }}</span>
          </span>
          <span
            v-if="enabledMap[ch.key]"
            class="px-2 py-0.5 rounded-iso-sm bg-iso-success-soft border border-iso-success font-mono text-[11px] text-iso-success"
          >
            enabled
          </span>
          <span
            v-else
            class="px-2 py-0.5 rounded-iso-sm border border-iso-border-subtle font-mono text-[11px] text-iso-text-muted"
          >
            disabled
          </span>
        </div>
        <div class="flex items-center gap-2">
          <button
            class="px-2 py-1 rounded-iso-sm text-[11px] text-iso-text-faint cursor-not-allowed"
            disabled
            title="Edit form arrives in Phase 4.5"
          >
            Edit
          </button>
          <button
            class="px-2 py-1 rounded-iso-sm text-[11px] text-iso-text-faint cursor-not-allowed"
            disabled
            title="Test message arrives in Phase 4.5"
          >
            Send test
          </button>
          <Switch
            :model-value="enabledMap[ch.key]"
            :disabled="toggling[ch.key]"
            class="data-[state=checked]:bg-iso-success"
            @update:model-value="(v: boolean) => setEnabled(ch, v)"
          />
        </div>
      </div>

      <div class="grid grid-cols-3 gap-3 text-[11px]">
        <div class="flex flex-col gap-0.5">
          <span class="text-iso-text-muted">Configured via</span>
          <span class="font-mono text-iso-text-secondary truncate" :title="ch.envHint">{{ ch.envHint }}</span>
        </div>
        <div class="flex flex-col gap-0.5">
          <span class="text-iso-text-muted">Last message</span>
          <span class="text-iso-text-faint">— (soon)</span>
        </div>
        <div class="flex flex-col gap-0.5">
          <span class="text-iso-text-muted">Rate limit</span>
          <span class="text-iso-text-faint">— (soon)</span>
        </div>
      </div>

      <div class="flex items-center gap-2 flex-wrap">
        <span class="text-[11px] text-iso-text-muted">subscribed:</span>
        <span class="px-1.5 py-px rounded-iso-sm bg-iso-bg-base border border-iso-border-subtle font-mono text-[10px] text-iso-text-faint">
          all events (kind filters: soon)
        </span>
        <span class="text-[11px] text-iso-text-faint">· fleet:</span>
        <span class="px-1.5 py-px rounded-iso-sm bg-iso-bg-base border border-iso-border-subtle font-mono text-[10px] text-iso-text-faint">all</span>
      </div>

      <p class="text-[11px] text-iso-text-muted leading-relaxed">{{ ch.description }}</p>
    </section>

    <!-- HTTP channel: not wired up at all yet -->
    <section class="rounded-iso-lg border border-dashed border-iso-border-subtle bg-iso-bg-elevated/40 p-5 flex flex-col gap-2">
      <div class="flex items-center justify-between">
        <div class="flex items-center gap-3">
          <div class="w-2 h-2 rounded-full bg-iso-text-faint"></div>
          <span class="text-sm font-semibold text-iso-text-faint">
            Generic HTTP
            <span class="font-normal">· webhook</span>
          </span>
          <span class="px-2 py-0.5 rounded-iso-sm border border-iso-border-subtle font-mono text-[11px] text-iso-text-faint">
            not configured
          </span>
        </div>
        <button
          class="px-2 py-1 rounded-iso-sm text-[11px] text-iso-text-faint cursor-not-allowed"
          disabled
          title="HTTP channel arrives in Phase 4.5"
        >
          Configure (soon)
        </button>
      </div>
      <p class="text-[11px] text-iso-text-muted leading-relaxed">
        Send events to an arbitrary HTTP endpoint with a custom auth header. Useful for piping into
        Slack-incoming-webhook, PagerDuty, or your own dispatcher.
      </p>
    </section>
  </div>
</template>
