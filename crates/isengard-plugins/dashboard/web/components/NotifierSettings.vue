<script setup lang="ts">
import { computed } from 'vue'
import { useSettingsStore } from '~/stores/settings'

const settings = useSettingsStore()
if (!settings.loaded) await settings.load()

const toast = useToast()

const telegramEnabled = computed({
  get: () => Boolean(settings.values['notifier.telegram.enabled']),
  set: async (v: boolean) => {
    try {
      await settings.patch({ 'notifier.telegram.enabled': v })
      toast.success(`Telegram notifications ${v ? 'enabled' : 'disabled'}`)
    } catch (e) {
      toast.error(`Save failed: ${e instanceof Error ? e.message : String(e)}`)
    }
  },
})

const discordEnabled = computed({
  get: () => Boolean(settings.values['notifier.discord.enabled']),
  set: async (v: boolean) => {
    try {
      await settings.patch({ 'notifier.discord.enabled': v })
      toast.success(`Discord notifications ${v ? 'enabled' : 'disabled'}`)
    } catch (e) {
      toast.error(`Save failed: ${e instanceof Error ? e.message : String(e)}`)
    }
  },
})
</script>

<template>
  <SettingsSection
    title="Notifiers"
    description="Send events to external chat platforms. Configure secrets in the controller config file."
  >
    <div class="space-y-3">
      <label class="flex items-center justify-between px-3 py-2 rounded border border-iso-border-subtle bg-iso-bg-elevated cursor-pointer">
        <div>
          <div class="font-mono text-sm">Telegram</div>
          <div class="text-xs text-iso-text-muted">Set TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID env vars on the controller.</div>
        </div>
        <input v-model="telegramEnabled" type="checkbox" class="w-4 h-4" />
      </label>

      <label class="flex items-center justify-between px-3 py-2 rounded border border-iso-border-subtle bg-iso-bg-elevated cursor-pointer">
        <div>
          <div class="font-mono text-sm">Discord</div>
          <div class="text-xs text-iso-text-muted">Set DISCORD_WEBHOOK_URL on the controller.</div>
        </div>
        <input v-model="discordEnabled" type="checkbox" class="w-4 h-4" />
      </label>
    </div>
  </SettingsSection>
</template>
