<template>
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/70"
    @click.self="$emit('close')"
  >
    <div
      class="bg-iso-bg-elevated border border-iso-danger rounded-iso-lg w-[680px] max-h-[90vh] flex flex-col overflow-hidden"
    >
      <header class="px-5 py-4 border-b border-iso-danger flex items-center justify-between bg-iso-danger-soft">
        <div class="flex flex-col gap-0.5">
          <span class="text-sm font-semibold text-iso-text-primary">Restore from backup</span>
          <span class="text-xs text-iso-text-secondary">Step {{ step }} of 4, {{ stepLabel }}</span>
        </div>
        <button
          class="text-iso-text-muted hover:text-iso-text-primary text-sm"
          @click="$emit('close')"
        >
          Cancel
        </button>
      </header>

      <!-- Step 1: pick a snapshot -->
      <section v-if="step === 1" class="flex-1 overflow-y-auto p-5 flex flex-col gap-3">
        <p class="text-xs text-iso-text-muted">
          Pick the snapshot to restore. The list shows successful backups recorded by this controller, newest first.
        </p>

        <div v-if="loadingRuns" class="text-xs text-iso-text-faint">Loading snapshots...</div>
        <div v-else-if="successfulRuns.length === 0" class="text-xs text-iso-text-faint">
          No successful backups found. Run a backup before attempting a restore.
        </div>

        <div v-else class="flex flex-col gap-2 max-h-[360px] overflow-y-auto">
          <button
            v-for="r in successfulRuns"
            :key="r.id"
            :class="[
              'flex items-center justify-between px-3 py-2 rounded-iso-md border text-left',
              picked?.id === r.id
                ? 'border-iso-danger bg-iso-danger-soft'
                : 'border-iso-border hover:border-iso-border-strong',
            ]"
            @click="picked = r"
          >
            <div class="flex flex-col gap-0.5">
              <span class="text-xs font-mono text-iso-text-primary">{{ r.object_name }}</span>
              <span class="text-[10px] text-iso-text-faint">
                {{ formatStarted(r.started_at) }} : {{ formatBytes(r.size_bytes ?? 0) }}
              </span>
            </div>
            <span class="text-[10px] text-iso-success">success</span>
          </button>
        </div>
      </section>

      <!-- Step 2: passphrase -->
      <section v-else-if="step === 2" class="flex-1 overflow-y-auto p-5 flex flex-col gap-3">
        <p class="text-xs text-iso-text-muted">
          Paste the passphrase used to encrypt this snapshot. The dashboard hashes it locally and compares to the controller's stored fingerprint before allowing the restore to proceed.
        </p>

        <div class="flex flex-col gap-1">
          <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Passphrase</span>
          <input
            v-model="passphrase"
            type="password"
            class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
            placeholder="paste here"
          />
        </div>

        <div v-if="manifest" class="grid grid-cols-2 gap-3 text-xs">
          <div class="flex flex-col gap-1">
            <span class="text-[10px] uppercase tracking-wide text-iso-text-muted">Stored fingerprint</span>
            <span class="font-mono text-iso-text-secondary">{{ manifest.passphrase_fingerprint || 'not set' }}</span>
          </div>
          <div class="flex flex-col gap-1">
            <span class="text-[10px] uppercase tracking-wide text-iso-text-muted">Pasted fingerprint</span>
            <span class="font-mono text-iso-text-secondary">{{ pastedFingerprint || '...' }}</span>
          </div>
        </div>

        <div
          v-if="passphrase && pastedFingerprint && manifest && fingerprintMatches"
          class="rounded-iso-md border border-iso-success bg-iso-success-soft p-3 text-xs text-iso-text-secondary"
        >
          Fingerprint matches. The passphrase will decrypt the snapshot.
        </div>
        <div
          v-else-if="passphrase && pastedFingerprint && manifest && !fingerprintMatches"
          class="rounded-iso-md border border-iso-warn bg-iso-warn-soft p-3 text-xs text-iso-text-secondary"
        >
          Fingerprint does not match the controller's stored value. The passphrase is probably wrong, or this snapshot was encrypted with a different key.
        </div>
      </section>

      <!-- Step 3: what will happen -->
      <section v-else-if="step === 3" class="flex-1 overflow-y-auto p-5 flex flex-col gap-3">
        <div class="rounded-iso-md border border-iso-danger bg-iso-danger-soft p-4 flex flex-col gap-2">
          <span class="text-xs font-semibold text-iso-text-primary">This restore will:</span>
          <ul class="text-xs text-iso-text-secondary list-disc pl-5 flex flex-col gap-1">
            <li>Replace the current controller database with the snapshot from <strong>{{ picked ? formatStarted(picked.started_at) : '?' }}</strong>.</li>
            <li>Save the current database as a sibling at <code class="font-mono text-iso-text-primary">isengard.db.bak.&lt;utc&gt;</code> next to the live file. The previous database is never deleted automatically.</li>
            <li>Re-run forward migrations on the restored file so any newer schema applies.</li>
            <li>Drop active connections briefly while the swap completes. Agents reconnect automatically.</li>
          </ul>
        </div>

        <div class="rounded-iso-md border border-iso-warn bg-iso-warn-soft p-3 text-xs text-iso-text-secondary">
          Recommended: pause the backup scheduler before restoring (so a snapshot does not fire mid-swap), and restart the controller afterwards for a clean state.
        </div>

        <label class="flex items-center gap-2 text-xs text-iso-text-secondary mt-2">
          <input v-model="dryRun" type="checkbox" />
          Dry-run only (download + decrypt + verify the snapshot is a valid SQLite database, do not swap)
        </label>
      </section>

      <!-- Step 4: type RESTORE -->
      <section v-else class="flex-1 overflow-y-auto p-5 flex flex-col gap-3">
        <p class="text-xs text-iso-text-muted">
          Type the literal phrase
          <span class="font-mono text-iso-danger">RESTORE</span>
          to confirm. This action cannot be undone, but the previous database will remain on disk as a sibling backup.
        </p>

        <input
          v-model="confirmText"
          type="text"
          class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
          placeholder="type RESTORE"
        />

        <div v-if="restoreError" class="rounded-iso-md border border-iso-danger bg-iso-danger-soft p-3 text-xs text-iso-text-secondary">
          {{ restoreError }}
        </div>

        <div v-if="restoreOutcome" class="rounded-iso-md border border-iso-success bg-iso-success-soft p-3 text-xs text-iso-text-secondary flex flex-col gap-1">
          <span class="font-semibold text-iso-text-primary">
            {{ restoreOutcome.dry_run ? 'Dry-run verified the snapshot.' : 'Restore complete.' }}
          </span>
          <span v-if="!restoreOutcome.dry_run" class="font-mono text-[11px]">
            Previous DB: {{ restoreOutcome.previous_db_backup_path }}
          </span>
          <span v-if="!restoreOutcome.dry_run" class="font-mono text-[11px]">
            Bytes restored: {{ formatBytes(restoreOutcome.bytes_restored) }}
          </span>
        </div>
      </section>

      <footer class="px-5 py-4 border-t border-iso-border flex items-center justify-between">
        <button
          v-if="step > 1 && !restoreOutcome"
          class="px-3 py-1.5 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs text-iso-text-secondary"
          @click="step -= 1"
        >
          Back
        </button>
        <span v-else class="text-[11px] text-iso-text-faint">{{ stepHelp }}</span>

        <div class="flex items-center gap-2">
          <button
            v-if="step < 4"
            class="px-3 py-1.5 rounded-iso-md bg-iso-info border border-iso-info text-xs font-medium text-iso-bg-base"
            :disabled="!canAdvance"
            @click="step += 1"
          >
            Continue
          </button>
          <button
            v-else-if="!restoreOutcome"
            class="px-3 py-1.5 rounded-iso-md bg-iso-danger border border-iso-danger text-xs font-medium text-iso-bg-base"
            :disabled="!canRestore || running"
            @click="run"
          >
            {{ running ? 'Restoring...' : (dryRun ? 'Verify snapshot' : 'Restore') }}
          </button>
          <button
            v-else
            class="px-3 py-1.5 rounded-iso-md bg-iso-info border border-iso-info text-xs font-medium text-iso-bg-base"
            @click="$emit('done')"
          >
            Close
          </button>
        </div>
      </footer>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'

interface BackupRunDto {
  id: number
  started_at: string
  finished_at: string | null
  status: 'running' | 'success' | 'failed'
  object_name: string | null
  size_bytes: number | null
  error: string | null
}

interface BackupRunManifestDto {
  id: number
  object_name: string
  size_bytes: number
  started_at: string
  finished_at: string | null
  passphrase_fingerprint: string
}

interface RestoreOutcomeDto {
  run_id: number
  source_object: string
  restored_at: string
  previous_db_backup_path: string
  bytes_restored: number
  dry_run: boolean
}

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'done'): void
}>()

const step = ref(1)
const runs = ref<BackupRunDto[]>([])
const loadingRuns = ref(true)
const picked = ref<BackupRunDto | null>(null)
const passphrase = ref('')
const pastedFingerprint = ref('')
const manifest = ref<BackupRunManifestDto | null>(null)
const dryRun = ref(false)
const confirmText = ref('')
const running = ref(false)
const restoreError = ref<string | null>(null)
const restoreOutcome = ref<RestoreOutcomeDto | null>(null)
const toast = useToast()

const successfulRuns = computed(() =>
  runs.value.filter(r => r.status === 'success' && r.object_name),
)

const fingerprintMatches = computed(() => {
  if (!manifest.value) return false
  if (!manifest.value.passphrase_fingerprint) return false
  return manifest.value.passphrase_fingerprint === pastedFingerprint.value
})

const stepLabel = computed(() => {
  switch (step.value) {
    case 1: return 'pick a snapshot'
    case 2: return 'verify passphrase'
    case 3: return 'review what happens'
    case 4: return 'confirm'
    default: return ''
  }
})

const stepHelp = computed(() => {
  switch (step.value) {
    case 1: return 'Pick the snapshot you want to restore.'
    case 2: return 'The fingerprint comparison stays local.'
    case 3: return 'Review the destructive change.'
    case 4: return 'Type RESTORE to confirm.'
    default: return ''
  }
})

const canAdvance = computed(() => {
  if (step.value === 1) return picked.value !== null
  if (step.value === 2) return Boolean(passphrase.value) && fingerprintMatches.value
  if (step.value === 3) return true
  return false
})

const canRestore = computed(() => confirmText.value === 'RESTORE')

async function loadRuns() {
  loadingRuns.value = true
  try {
    const r = await $fetch<BackupRunDto[]>('/api/v1/backup/runs', { query: { limit: 60 } })
    runs.value = r
  } catch (e) {
    toast.error(`Failed to load backup runs: ${e instanceof Error ? e.message : String(e)}`)
  } finally {
    loadingRuns.value = false
  }
}

async function loadManifest(runId: number) {
  try {
    manifest.value = await $fetch<BackupRunManifestDto>(`/api/v1/backup/runs/${runId}/manifest`)
  } catch (e) {
    manifest.value = null
    toast.error(`Failed to load manifest: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function run() {
  if (!picked.value || !picked.value.object_name) return
  running.value = true
  restoreError.value = null
  try {
    restoreOutcome.value = await $fetch<RestoreOutcomeDto>('/api/v1/backup/restore', {
      method: 'POST',
      body: {
        object_name: picked.value.object_name,
        passphrase: passphrase.value,
        dry_run: dryRun.value,
      },
    })
    if (dryRun.value) {
      toast.success('Dry-run verified the snapshot.')
    } else {
      toast.success('Restore complete.')
    }
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    restoreError.value = msg
  } finally {
    running.value = false
  }
}

watch(picked, async (v) => {
  if (v) await loadManifest(v.id)
})

watch(passphrase, async (v) => {
  if (!v || typeof crypto === 'undefined' || !crypto.subtle) {
    pastedFingerprint.value = ''
    return
  }
  const buf = new TextEncoder().encode(v)
  const digest = await crypto.subtle.digest('SHA-256', buf)
  pastedFingerprint.value = Array.from(new Uint8Array(digest).slice(0, 6))
    .map(b => b.toString(16).padStart(2, '0'))
    .join('')
})

function formatStarted(s: string): string {
  try {
    const d = new Date(s)
    return d.toISOString().replace('T', ' ').slice(0, 19)
  } catch {
    return s
  }
}

function formatBytes(b: number): string {
  if (b < 1024) return `${b} B`
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`
  if (b < 1024 * 1024 * 1024) return `${(b / 1024 / 1024).toFixed(1)} MB`
  return `${(b / 1024 / 1024 / 1024).toFixed(2)} GB`
}

onMounted(loadRuns)
</script>
