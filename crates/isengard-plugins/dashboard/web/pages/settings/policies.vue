<script setup lang="ts">
import { ref } from 'vue'
import PoliciesSettings from '~/components/PoliciesSettings.vue'

/**
 * Settings to Policies page (Phase 9 T5).
 *
 * Direct route for `/settings/policies`. Mirrors the layout of
 * `pages/settings/index.vue` but pre-selects the Update policies tab. The
 * actual tab body lives in `<PoliciesSettings />` so this route and the
 * `?tab=policies` query route render the same component.
 */

const tabs = [
  { key: 'general', label: 'General' },
  { key: 'enrollment', label: 'Enrollment' },
  { key: 'policies', label: 'Update policies' },
  { key: 'networking', label: 'Networking' },
  { key: 'deployments', label: 'Deployments' },
  { key: 'notifier', label: 'Notifier' },
]

const policiesRef = ref<InstanceType<typeof PoliciesSettings> | null>(null)

function onAddPolicy() {
  policiesRef.value?.openCreateEditor()
}
</script>

<template>
  <AppShell>
    <PageHeader title="Settings" subtitle="Update policies">
      <template #actions>
        <Button
          variant="outline"
          class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
          @click="onAddPolicy"
        >
          + Add policy
        </Button>
      </template>
    </PageHeader>

    <div class="flex-1 overflow-y-auto">
      <div class="max-w-5xl mx-auto w-full p-6">
        <SettingsTabs :tabs="tabs" default-tab="policies">
          <PoliciesSettings ref="policiesRef" />
        </SettingsTabs>
      </div>
    </div>
  </AppShell>
</template>
