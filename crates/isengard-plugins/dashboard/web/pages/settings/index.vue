<template>
  <AppShell>
    <PageHeader title="Settings" subtitle="Controller configuration" />
    <div class="flex-1 overflow-y-auto">
      <!--
        Concept v1 pages render at 1440px wide; clamp to 5xl (~1280px) so the
        routing rules table + per-channel notifier cards have room to breathe.
        Previously max-w-3xl (~768px) cramped wide content into a single column.
      -->
      <div class="max-w-5xl mx-auto w-full p-6">
        <SettingsTabs :tabs="tabs" default-tab="general" v-slot="{ activeTab }">
          <div v-if="activeTab === 'general'">
            <SettingsControllerIdentityCard />
            <FleetsSettings />
          </div>
          <div v-else-if="activeTab === 'enrollment'">
            <EnrollmentSettings />
          </div>
          <div v-else-if="activeTab === 'policies'">
            <PoliciesSettings />
          </div>
          <div v-else-if="activeTab === 'networking'">
            <NetworkingSettings />
          </div>
          <div v-else-if="activeTab === 'deployments'">
            <DeploymentsSettings />
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
  { key: 'enrollment', label: 'Enrollment' },
  { key: 'policies', label: 'Update policies' },
  { key: 'networking', label: 'Networking' },
  { key: 'deployments', label: 'Deployments' },
  { key: 'notifier', label: 'Notifier' },
]
</script>
