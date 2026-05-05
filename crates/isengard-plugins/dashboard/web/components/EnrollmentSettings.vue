<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useEnrollment, type ActiveToken } from '~/composables/useEnrollment'
import { useConfirm } from '~/composables/useConfirm'
import { useToast } from '~/composables/useToast'

const router = useRouter()
const { tokens, loading, error, refresh, revokeToken } = useEnrollment()
const { confirm } = useConfirm()
const toast = useToast()

const showAddHostModal = ref(false)
const showMintModal = ref(false)

onMounted(refresh)

function reRunWizard() {
  router.push('/welcome?step=1&fresh=1')
}

async function onMinted() {
  // Refresh in the background so the freshly-minted token shows up under
  // "active tokens" as soon as the user closes the modal.
  await refresh()
}

async function onRevoke(token: ActiveToken) {
  const ok = await confirm({
    title: `Revoke token ${token.hash_prefix}?`,
    description:
      'The token will be marked consumed and any agent that still has the plaintext will be unable to enroll. Hosts already enrolled with this token are unaffected.',
    confirmText: 'Revoke token',
    danger: true,
  })
  if (!ok) return
  try {
    await revokeToken(token.hash_prefix)
    toast.success(`Token ${token.hash_prefix} revoked`)
  } catch (e) {
    toast.error(`Revoke failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

function formatTs(ts: string): string {
  return new Date(ts).toLocaleString()
}

const sortedTokens = computed(() =>
  [...tokens.value].sort((a, b) => b.created_at.localeCompare(a.created_at)),
)
</script>

<template>
  <SettingsSection
    title="Agent enrollment"
    description="Add a new host by running a generated install command on it."
  >
    <div class="flex items-center gap-3 flex-wrap">
      <Button
        variant="outline"
        class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
        @click="reRunWizard"
      >
        + Add host with wizard
      </Button>
      <Button
        variant="ghost"
        class="text-iso-text-muted hover:text-iso-text-primary"
        @click="showAddHostModal = true"
      >
        Generate install command (advanced)
      </Button>
    </div>

    <AddHostModal v-if="showAddHostModal" @close="showAddHostModal = false" />
  </SettingsSection>

  <SettingsSection
    title="Active enrollment tokens"
    description="Mint short-lived tokens for manually enrolling agents. Each token can be redeemed exactly once and is shown in plaintext only at mint time."
  >
    <div class="flex items-center justify-between mb-4">
      <div class="text-xs text-iso-text-muted">
        <span v-if="loading && tokens.length === 0">Loading…</span>
        <span v-else-if="error" class="text-iso-error">{{ error }}</span>
        <span v-else>{{ tokens.length }} active</span>
      </div>
      <Button
        variant="outline"
        class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
        @click="showMintModal = true"
      >
        Mint token
      </Button>
    </div>

    <div
      v-if="!loading && tokens.length === 0 && !error"
      class="text-iso-text-muted text-sm py-6 text-center border border-dashed border-iso-border-subtle rounded-md"
    >
      No active enrollment tokens.
      <button
        class="text-iso-text-primary underline hover:text-iso-success ml-1"
        @click="showMintModal = true"
      >
        Mint one
      </button>
      to enroll a new agent.
    </div>

    <table v-else-if="tokens.length > 0" class="w-full text-xs">
      <thead class="text-iso-text-muted">
        <tr>
          <th class="text-left pb-2 font-medium">Hash prefix</th>
          <th class="text-left pb-2 font-medium">Role</th>
          <th class="text-left pb-2 font-medium">Created</th>
          <th class="text-left pb-2 font-medium">Expires</th>
          <th class="text-right pb-2 font-medium">Actions</th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="token in sortedTokens"
          :key="token.hash_prefix"
          class="border-t border-iso-border"
        >
          <td class="py-2 font-mono text-iso-text-primary">{{ token.hash_prefix }}</td>
          <td class="py-2 font-mono text-iso-text-secondary">{{ token.role }}</td>
          <td class="py-2 text-iso-text-secondary">{{ formatTs(token.created_at) }}</td>
          <td class="py-2 text-iso-text-secondary">{{ formatTs(token.expires_at) }}</td>
          <td class="py-2 text-right">
            <button
              class="text-iso-error hover:underline"
              @click="onRevoke(token)"
            >
              Revoke
            </button>
          </td>
        </tr>
      </tbody>
    </table>

    <MintTokenModal
      v-if="showMintModal"
      @close="showMintModal = false"
      @minted="onMinted"
    />
  </SettingsSection>
</template>
