<template>
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" @click.self="$emit('close')">
    <div class="bg-iso-bg-elevated border border-iso-border rounded-iso-lg w-[640px] max-h-[90vh] flex flex-col overflow-hidden">
      <header class="px-5 py-4 border-b border-iso-border flex items-center justify-between">
        <div class="flex flex-col gap-0.5">
          <span class="text-sm font-semibold text-iso-text-primary">Set up backups</span>
          <span class="text-xs text-iso-text-muted">Step {{ step }} of 3, {{ stepLabel }}</span>
        </div>
        <button
          class="text-iso-text-muted hover:text-iso-text-primary text-sm"
          @click="$emit('close')"
        >
          Cancel
        </button>
      </header>

      <!-- Step 1: destination -->
      <section v-if="step === 1" class="flex-1 overflow-y-auto p-5 flex flex-col gap-4">
        <p class="text-xs text-iso-text-muted">
          Pick where snapshots get shipped. Cloudflare R2 is recommended for the zero-egress pricing; local writes are good for a NAS or external disk.
        </p>

        <div class="flex gap-2">
          <button
            v-for="opt in providerOptions"
            :key="opt.kind"
            :class="[
              'flex-1 px-3 py-2 rounded-iso-md border text-xs',
              kind === opt.kind
                ? 'border-iso-info bg-iso-info-soft text-iso-text-primary'
                : 'border-iso-border text-iso-text-muted hover:text-iso-text-primary',
            ]"
            @click="setKind(opt.kind)"
          >
            {{ opt.label }}
          </button>
        </div>

        <div v-if="kind === 'local'" class="flex flex-col gap-3">
          <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Root path</span>
            <input
              v-model="local.root"
              type="text"
              placeholder="/var/lib/isengard/backups"
              class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
            />
            <span class="text-[10px] text-iso-text-faint">Absolute path on the controller host. Created if missing.</span>
          </div>
          <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Prefix</span>
            <input
              v-model="local.prefix"
              type="text"
              placeholder="controllers/prod"
              class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
            />
            <span class="text-[10px] text-iso-text-faint">Optional sub-directory under root.</span>
          </div>
        </div>

        <div v-else-if="kind === 's3'" class="flex flex-col gap-3">
          <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Endpoint</span>
            <input
              v-model="s3.endpoint"
              type="text"
              placeholder="https://abc123.r2.cloudflarestorage.com"
              class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
            />
            <span class="text-[10px] text-iso-text-faint">R2 example: https://&lt;account&gt;.r2.cloudflarestorage.com</span>
          </div>
          <div class="grid grid-cols-2 gap-3">
            <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Bucket</span>
            <input
                v-model="s3.bucket"
                type="text"
                class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
              />
            
          </div>
            <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Region</span>
            <input
                v-model="s3.region"
                type="text"
                class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
              />
            <span class="text-[10px] text-iso-text-faint">Use 'auto' for R2.</span>
          </div>
          </div>
          <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Prefix</span>
            <input
              v-model="s3.prefix"
              type="text"
              placeholder="controllers/prod"
              class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
            />
            <span class="text-[10px] text-iso-text-faint">Sub-path inside the bucket.</span>
          </div>
          <div class="grid grid-cols-2 gap-3">
            <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Access key id</span>
            <input
                v-model="s3.access_key_id"
                type="text"
                class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
              />
            
          </div>
            <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Secret access key</span>
            <input
                v-model="s3.secret_access_key"
                type="password"
                class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
              />
            <span class="text-[10px] text-iso-text-faint">Stored masked. Never re-displayed.</span>
          </div>
          </div>
        </div>
      </section>

      <!-- Step 2: encryption -->
      <section v-else-if="step === 2" class="flex-1 overflow-y-auto p-5 flex flex-col gap-4">
        <p class="text-xs text-iso-text-muted">
          Snapshots are encrypted with age before upload. Pick a passphrase the controller process can read via the env var
          <code class="font-mono text-iso-text-secondary">ISENGARD_BACKUP_PASSPHRASE</code>.
          The dashboard never persists the passphrase, only a 12-char fingerprint so you can confirm the running controller has the same value.
        </p>

        <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Passphrase</span>
            <input
            v-model="passphrase"
            type="password"
            class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs font-mono text-iso-text-primary"
          />
            <span class="text-[10px] text-iso-text-faint">Paste a long random string. We hash it for the fingerprint and immediately discard the value.</span>
          </div>

        <div v-if="passphrase" class="text-xs text-iso-text-muted">
          Fingerprint preview:
          <span class="font-mono text-iso-text-secondary">{{ previewFingerprint }}</span>
        </div>

        <div class="rounded-iso-md border border-iso-warn bg-iso-warn-soft p-3 text-xs text-iso-text-secondary">
          <strong>Lost passphrase = lost backups.</strong>
          The controller is the only thing that holds the secret (in your env). Store it in a password manager. There is no recovery service.
        </div>
      </section>

      <!-- Step 3: schedule -->
      <section v-else class="flex-1 overflow-y-auto p-5 flex flex-col gap-4">
        <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Interval</span>
            <select
            v-model.number="intervalSecs"
            class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs text-iso-text-primary"
          >
            <option :value="3600">Hourly</option>
            <option :value="21600">Every 6 hours</option>
            <option :value="86400">Daily</option>
            <option :value="604800">Weekly</option>
          </select>
            <span class="text-[10px] text-iso-text-faint">How often the scheduler fires. Default: daily.</span>
          </div>

        <div class="flex flex-col gap-1">
            <span class="text-[11px] font-semibold text-iso-text-muted tracking-wide">Retention</span>
            <input
            v-model.number="retentionKeep"
            type="number"
            min="1"
            max="1000"
            class="w-full px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs text-iso-text-primary"
          />
            <span class="text-[10px] text-iso-text-faint">Number of most-recent snapshots to keep at the destination. Older snapshots are pruned after each successful run.</span>
          </div>

        <label class="flex items-center gap-2 text-xs text-iso-text-secondary">
          <input v-model="enabled" type="checkbox" />
          Enable the scheduler now
        </label>
      </section>

      <footer class="px-5 py-4 border-t border-iso-border flex items-center justify-between">
        <button
          v-if="step > 1"
          class="px-3 py-1.5 rounded-iso-md bg-iso-bg-base border border-iso-border text-xs text-iso-text-secondary"
          @click="step -= 1"
        >
          Back
        </button>
        <span v-else class="text-[11px] text-iso-text-faint">{{ stepHelp }}</span>
        <div class="flex items-center gap-2">
          <button
            v-if="step < 3"
            class="px-3 py-1.5 rounded-iso-md bg-iso-info border border-iso-info text-xs font-medium text-iso-bg-base"
            :disabled="!canAdvance"
            @click="step += 1"
          >
            Continue
          </button>
          <button
            v-else
            class="px-3 py-1.5 rounded-iso-md bg-iso-info border border-iso-info text-xs font-medium text-iso-bg-base"
            :disabled="saving"
            @click="save"
          >
            {{ saving ? 'Saving…' : 'Save backup config' }}
          </button>
        </div>
      </footer>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'

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

const props = defineProps<{ initial: BackupConfigDto }>()
const emit = defineEmits<{
  (e: 'close'): void
  (e: 'saved'): void
}>()

const step = ref(1)
const kind = ref<'local' | 's3'>(
  props.initial.destination.kind === 's3' ? 's3' : 'local',
)
const local = ref({
  root: props.initial.destination.kind === 'local' ? (props.initial.destination as DestinationLocal).root : '/var/lib/isengard/backups',
  prefix: props.initial.destination.kind === 'local' ? (props.initial.destination as DestinationLocal).prefix : 'controllers',
})
const s3 = ref({
  endpoint: props.initial.destination.kind === 's3' ? (props.initial.destination as DestinationS3).endpoint : 'https://<account>.r2.cloudflarestorage.com',
  region: props.initial.destination.kind === 's3' ? (props.initial.destination as DestinationS3).region : 'auto',
  bucket: props.initial.destination.kind === 's3' ? (props.initial.destination as DestinationS3).bucket : 'isengard-backups',
  prefix: props.initial.destination.kind === 's3' ? (props.initial.destination as DestinationS3).prefix : 'controllers/prod',
  access_key_id: props.initial.destination.kind === 's3' ? (props.initial.destination as DestinationS3).access_key_id : '',
  secret_access_key: props.initial.destination.kind === 's3' ? (props.initial.destination as DestinationS3).secret_access_key : '',
})
const passphrase = ref('')
const intervalSecs = ref(props.initial.interval_secs || 86400)
const retentionKeep = ref(props.initial.retention_keep || 14)
const enabled = ref(props.initial.enabled || false)
const saving = ref(false)
const toast = useToast()

const providerOptions = [
  { kind: 'local' as const, label: 'Local path' },
  { kind: 's3' as const, label: 'S3 / R2' },
]

const stepLabel = computed(() => {
  switch (step.value) {
    case 1: return 'destination'
    case 2: return 'encryption key'
    case 3: return 'schedule'
    default: return ''
  }
})

const stepHelp = computed(() => {
  switch (step.value) {
    case 1: return 'Where snapshots get shipped.'
    case 2: return 'Encryption protects the snapshot bytes.'
    case 3: return 'When and how many.'
    default: return ''
  }
})

const canAdvance = computed(() => {
  if (step.value === 1) {
    if (kind.value === 'local') return Boolean(local.value.root)
    return Boolean(s3.value.endpoint && s3.value.bucket && s3.value.access_key_id && s3.value.secret_access_key)
  }
  if (step.value === 2) {
    return passphrase.value.length > 0 || props.initial.passphrase_fingerprint.length > 0
  }
  return true
})

const previewFingerprint = ref('')

function setKind(k: 'local' | 's3') {
  kind.value = k
}

function destination(): Destination {
  if (kind.value === 'local') {
    return { kind: 'local', root: local.value.root, prefix: local.value.prefix }
  }
  return {
    kind: 's3',
    endpoint: s3.value.endpoint,
    region: s3.value.region,
    bucket: s3.value.bucket,
    prefix: s3.value.prefix,
    access_key_id: s3.value.access_key_id,
    secret_access_key: s3.value.secret_access_key,
  }
}

async function save() {
  saving.value = true
  try {
    const body: BackupConfigDto = {
      enabled: enabled.value,
      destination: destination(),
      interval_secs: intervalSecs.value,
      retention_keep: retentionKeep.value,
      passphrase_fingerprint: props.initial.passphrase_fingerprint,
      passphrase: passphrase.value || undefined,
    }
    await $fetch('/api/v1/backup/config', { method: 'PUT', body })
    toast.success('Backup config saved')
    emit('saved')
  } catch (e) {
    toast.error(`Save failed: ${e instanceof Error ? e.message : String(e)}`)
  } finally {
    saving.value = false
  }
}

watch(
  passphrase,
  async (v) => {
    if (!v || typeof crypto === 'undefined' || !crypto.subtle) {
      previewFingerprint.value = ''
      return
    }
    const buf = new TextEncoder().encode(v)
    const digest = await crypto.subtle.digest('SHA-256', buf)
    previewFingerprint.value = Array.from(new Uint8Array(digest).slice(0, 6))
      .map(b => b.toString(16).padStart(2, '0'))
      .join('')
  },
)
</script>
