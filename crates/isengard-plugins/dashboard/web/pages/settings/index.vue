<template>
  <AppShell>
    <PageHeader title="Settings" subtitle="Controller configuration" />
    <div class="flex-1 overflow-y-auto">
      <div class="max-w-3xl mx-auto w-full p-6">
        <SettingsTabs :tabs="tabs" default-tab="general" v-slot="{ activeTab }">
          <div v-if="activeTab === 'general'">
            <FleetsSettings />
            <EnrollmentSettings />
          </div>
          <div v-else-if="activeTab === 'networking'">
            <NetworkingSettings />
          </div>
          <div v-else-if="activeTab === 'notifier'">
            <NotifierSettings />
          </div>
        </SettingsTabs>
      </div>
    </div>
  </AppShell>
</template>

<script setup lang="ts">
const tabs = [
  { key: 'general', label: 'General' },
  { key: 'networking', label: 'Networking' },
  { key: 'notifier', label: 'Notifier' },
]
</script>
