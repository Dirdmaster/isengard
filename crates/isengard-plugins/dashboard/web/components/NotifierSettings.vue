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
    description="Send events to external chat platforms. Configure secrets via env vars on the controller."
  >
    <div class="space-y-2">
      <div class="flex items-center justify-between px-4 py-3 rounded-md border border-iso-border-subtle bg-iso-bg-elevated">
        <div>
          <div class="font-mono text-sm text-iso-text-primary">Telegram</div>
          <div class="text-xs text-iso-text-muted mt-0.5">Set TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID env vars on the controller.</div>
        </div>
        <Switch v-model="telegramEnabled" class="data-[state=checked]:bg-iso-success" />
      </div>

      <div class="flex items-center justify-between px-4 py-3 rounded-md border border-iso-border-subtle bg-iso-bg-elevated">
        <div>
          <div class="font-mono text-sm text-iso-text-primary">Discord</div>
          <div class="text-xs text-iso-text-muted mt-0.5">Set DISCORD_WEBHOOK_URL on the controller.</div>
        </div>
        <Switch v-model="discordEnabled" class="data-[state=checked]:bg-iso-success" />
      </div>
    </div>
  </SettingsSection>
</template>
