<template>
  <div class="flex flex-col gap-4">
    <!-- Status panel -->
    <section
      v-if="!isConfigured"
      class="rounded-iso-lg border border-iso-warn bg-iso-bg-elevated p-5 flex items-center justify-between"
    >
      <div class="flex flex-col gap-1">
        <span class="text-sm font-semibold text-iso-text-primary">Backups not configured</span>
        <span class="text-xs text-iso-text-muted">
          Set up a destination to protect the controller state. Encrypted with age, shipped to S3-compatible storage or a local path.
        </span>
      </div>
      <button
        class="px-3 py-1.5 rounded-iso-md bg-iso-info border border-iso-info text-xs font-medium text-iso-bg-base"
        @click="modalOpen = true"
      >
        Get started
      </button>
    </section>

    <section
      v-else
      :class="[
        'rounded-iso-lg border bg-iso-bg-elevated p-5 flex items-center justify-between',
        statusColorBorder,
      ]"
    >
      <div class="flex items-center gap-4">
        <div :class="['w-2.5 h-2.5 rounded-full shrink-0', statusDotColor]"></div>
        <div class="flex flex-col gap-0.5">
          <span class="text-sm font-semibold text-iso-text-primary">{{ statusHeadline }}</span>
          <span class="text-xs text-iso-text-muted">{{ statusSummary }}</span>
        </div>
      </div>
      <div class="flex items-center gap-2">
        <button
          class="px-2.5 py-1.5 rounded-iso-md bg-iso-bg-base border border-iso-border-strong text-xs text-iso-text-secondary"
          :disabled="runningNow"
          @click="runNow"
        >
          {{ runningNow ? 'Running…' : 'Run now' }}
        </button>
        <button
          class="px-2.5 py-1.5 rounded-iso-md bg-iso-bg-base border border-iso-border-strong text-xs text-iso-text-secondary"
          @click="modalOpen = true"
        >
          Edit
        </button>
      </div>
    </section>

    <!-- Schedule + destination summary -->
    <section
      v-if="isConfigured"
      class="grid grid-cols-2 gap-4"
    >
      <div class="rounded-iso-lg border border-iso-border bg-iso-bg-elevated p-5 flex flex-col gap-3">
        <span class="text-[10px] font-semibold text-iso-text-muted tracking-widest">SCHEDULE & RETENTION</span>
        <div class="flex items-center justify-between text-xs">
          <span class="text-iso-text-muted">Interval</span>
          <span class="text-iso-text-primary">{{ formatInterval(config.interval_secs) }}</span>
        </div>
        <div class="flex items-center justify-between text-xs">
          <span class="text-iso-text-muted">Retention</span>
          <span class="font-mono text-iso-text-secondary">keep last {{ config.retention_keep }}</span>
        </div>
        <div class="flex items-center justify-between text-xs">
          <span class="text-iso-text-muted">Enabled</span>
          <span :class="config.enabled ? 'text-iso-success' : 'text-iso-warn'">
            {{ config.enabled ? 'yes' : 'no (paused)' }}
          </span>
        </div>
      </div>

      <div class="rounded-iso-lg border border-iso-border bg-iso-bg-elevated p-5 flex flex-col gap-3">
        <span class="text-[10px] font-semibold text-iso-text-muted tracking-widest">DESTINATION & ENCRYPTION</span>
        <div class="flex items-center justify-between text-xs">
          <span class="text-iso-text-muted">Provider</span>
          <span class="text-iso-text-primary">{{ destinationLabel }}</span>
        </div>
        <div class="flex items-center justify-between text-xs">
          <span class="text-iso-text-muted">Encryption</span>
          <span class="text-iso-success">age passphrase</span>
        </div>
        <div class="flex items-center justify-between text-xs">
          <span class="text-iso-text-muted">Key fingerprint</span>
          <span class="font-mono text-iso-text-secondary text-[11px]">
            {{ config.passphrase_fingerprint || 'not set' }}
          </span>
        </div>
      </div>
    </section>

    <!-- Recent runs -->
    <section
      v-if="isConfigured"
      class="rounded-iso-lg border border-iso-border bg-iso-bg-elevated overflow-hidden flex flex-col"
    >
      <div class="px-4 py-3 border-b border-iso-border flex items-center justify-between">
        <span class="text-xs font-semibold text-iso-text-primary">Recent snapshots</span>
        <span class="text-[11px] text-iso-text-muted">last {{ runs.length }} attempts</span>
      </div>
      <div v-if="runs.length === 0" class="px-4 py-6 text-xs text-iso-text-muted text-center">
        No snapshots yet. Click "Run now" to take the first one.
      </div>
      <table v-else class="w-full text-xs">
        <thead>
          <tr class="bg-iso-bg-base text-iso-text-muted text-[10px] tracking-wider">
            <th class="px-4 py-2 text-left font-semibold">Started</th>
            <th class="px-4 py-2 text-left font-semibold">Status</th>
            <th class="px-4 py-2 text-left font-semibold">Object</th>
            <th class="px-4 py-2 text-right font-semibold">Size</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="r in runs"
            :key="r.id"
            class="border-t border-iso-border"
          >
            <td class="px-4 py-2 text-iso-text-secondary font-mono">{{ formatStarted(r.started_at) }}</td>
            <td class="px-4 py-2">
              <span :class="runStatusClass(r.status)">{{ r.status }}</span>
              <span v-if="r.error" class="text-iso-text-faint ml-2">{{ r.error }}</span>
            </td>
            <td class="px-4 py-2 font-mono text-iso-text-faint truncate max-w-[260px]">
              {{ r.object_name || '-' }}
            </td>
            <td class="px-4 py-2 text-right text-iso-text-faint">
              {{ r.size_bytes ? formatBytes(r.size_bytes) : '-' }}
            </td>
          </tr>
        </tbody>
      </table>
    </section>

    <BackupSetupModal
      v-if="modalOpen"
      :initial="config"
      @close="modalOpen = false"
      @saved="onSaved"
    />
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'

interface DestinationLocal { kind: 'local'; root: string; prefix: string }
interface DestinationS3 {
  kind: 's3'
  endpoint: string
  region: string
  bucket: string
  prefix: string
  access_key_id: string
  secret_access_key: string
}
interface DestinationNone { kind: 'none' }
type Destination = DestinationLocal | DestinationS3 | DestinationNone

interface BackupConfigDto {
  enabled: boolean
  destination: Destination
  interval_secs: number
  retention_keep: number
  passphrase_fingerprint: string
  passphrase?: string
}

interface BackupRunDto {
  id: number
  started_at: string
  finished_at: string | null
  status: 'running' | 'success' | 'failed'
  object_name: string | null
  size_bytes: number | null
  error: string | null
}

const config = ref<BackupConfigDto>({
  enabled: false,
  destination: { kind: 'none' },
  interval_secs: 86400,
  retention_keep: 14,
  passphrase_fingerprint: '',
})
const runs = ref<BackupRunDto[]>([])
const modalOpen = ref(false)
const runningNow = ref(false)
const toast = useToast()

const isConfigured = computed(() => config.value.destination.kind !== 'none')

const destinationLabel = computed(() => {
  switch (config.value.destination.kind) {
    case 's3': {
      const d = config.value.destination as DestinationS3
      return `S3 ${d.bucket}${d.prefix ? '/' + d.prefix : ''}`
    }
    case 'local': {
      const d = config.value.destination as DestinationLocal
      return `Local ${d.root}${d.prefix ? '/' + d.prefix : ''}`
    }
    default:
      return 'not configured'
  }
})

const lastSuccessfulRun = computed(() =>
  runs.value.find(r => r.status === 'success'),
)

const statusHeadline = computed(() => {
  if (!config.value.enabled) return 'Backups paused'
  const last = runs.value[0]
  if (!last) return 'Backups configured (no runs yet)'
  if (last.status === 'failed') return 'Last backup failed'
  if (last.status === 'success') return 'Backups healthy'
  return 'Backup in progress'
})

const statusSummary = computed(() => {
  const last = lastSuccessfulRun.value
  if (!last) return 'Click "Run now" to take the first snapshot.'
  return `Last successful: ${formatStarted(last.started_at)} · ${formatBytes(last.size_bytes ?? 0)} · keep last ${config.value.retention_keep}`
})

const statusDotColor = computed(() => {
  if (!config.value.enabled) return 'bg-iso-warn'
  const last = runs.value[0]
  if (!last) return 'bg-iso-warn'
  if (last.status === 'success') return 'bg-iso-success'
  if (last.status === 'failed') return 'bg-iso-danger'
  return 'bg-iso-info'
})

const statusColorBorder = computed(() => {
  if (!config.value.enabled) return 'border-iso-warn'
  const last = runs.value[0]
  if (!last) return 'border-iso-warn'
  if (last.status === 'success') return 'border-iso-success'
  if (last.status === 'failed') return 'border-iso-danger'
  return 'border-iso-border'
})

async function load() {
  try {
    const [cfgResp, runsResp] = await Promise.all([
      $fetch<BackupConfigDto>('/api/v1/backup/config'),
      $fetch<BackupRunDto[]>('/api/v1/backup/runs', { query: { limit: 30 } }),
    ])
    config.value = cfgResp
    runs.value = runsResp
  } catch (e) {
    toast.error(`Failed to load backup config: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function runNow() {
  runningNow.value = true
  try {
    await $fetch('/api/v1/backup/run-now', { method: 'POST' })
    toast.success('Backup triggered')
    await load()
  } catch (e) {
    toast.error(`Run failed: ${e instanceof Error ? e.message : String(e)}`)
  } finally {
    runningNow.value = false
  }
}

function onSaved() {
  modalOpen.value = false
  load()
}

function formatInterval(secs: number): string {
  if (secs >= 86400 && secs % 86400 === 0) return `every ${secs / 86400} day${secs === 86400 ? '' : 's'}`
  if (secs >= 3600 && secs % 3600 === 0) return `every ${secs / 3600} hour${secs === 3600 ? '' : 's'}`
  return `every ${secs}s`
}

function formatBytes(b: number): string {
  if (b < 1024) return `${b} B`
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`
  if (b < 1024 * 1024 * 1024) return `${(b / 1024 / 1024).toFixed(1)} MB`
  return `${(b / 1024 / 1024 / 1024).toFixed(2)} GB`
}

function formatStarted(s: string): string {
  try {
    const d = new Date(s)
    return d.toISOString().replace('T', ' ').slice(0, 19)
  } catch {
    return s
  }
}

function runStatusClass(status: string): string {
  switch (status) {
    case 'success': return 'text-iso-success'
    case 'failed': return 'text-iso-danger'
    case 'running': return 'text-iso-info'
    default: return 'text-iso-text-muted'
  }
}

onMounted(load)
</script>
