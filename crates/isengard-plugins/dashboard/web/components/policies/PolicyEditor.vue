<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import {
  scopeKeyForUrl,
  type ExternalGate,
  type FailureHandling,
  type MaintenanceWindow,
  type PolicyBody,
  type PolicyDto,
  type PolicyScopeType,
  type UpdateGate,
  type UpdateStrategy,
} from '~/composables/usePolicies'
import {
  useEffectivePolicy,
  type EffectivePolicyContext,
  type ResolvedPolicy,
} from '~/composables/useEffectivePolicy'
import { useToast } from '~/composables/useToast'
import { formatFiring, nextFirings } from '~/lib/cron-preview'

/** Shipped tz options for the window picker. `custom` reveals a free-form input. */
const WINDOW_TIMEZONES = [
  'UTC',
  'Europe/Zurich',
  'America/New_York',
  'Asia/Tokyo',
  'custom',
] as const
type WindowTzChoice = typeof WINDOW_TIMEZONES[number]

/**
 * PolicyEditor: fields-with-inheritance form. T6 Phase 9a-9d.
 *
 * Lifecycle is owned by `PoliciesSettings.vue`, which mounts this component
 * inside a `<Dialog>` from `~/components/ui/dialog`. ESC + click-outside are
 * handled by that wrapper via `update:open`, which calls back into our parent
 * (it emits `close` on our behalf). We only emit `close` from explicit Cancel
 * presses + after a successful Save (alongside `saved`).
 *
 * Design choices worth flagging:
 *
 *   - Each field has an "Override at this level" checkbox. The control beneath
 *     stays mounted (so tab order is stable) but is `disabled` when the
 *     checkbox is off. Disabled fields render the inherited value as their
 *     placeholder + a "(inherited from <provenance>)" subtitle. Toggling the
 *     checkbox off does NOT clear the user's typed value: it just stops the
 *     value from being included in the saved body, which lets people flip
 *     back without losing work.
 *
 *   - The Approval gate radio is rendered but `disabled`. The backend returns
 *     422 if anyone manages to submit it (T4 enforces this); we surface a
 *     "(Phase 9e)" tag inline so users see it exists but is not enforceable.
 *
 *   - paused_until uses HTML <input type="datetime-local"> which gives the
 *     browser its native picker. The value is in the user's local tz; we
 *     attach the browser's tz offset on save so the backend stores RFC3339.
 *
 *   - Save is disabled until at least one field is overridden. An all-None row
 *     is technically valid storage (placeholder for a future override) but
 *     pollutes the policy list, so we force at least one override for sanity.
 */

interface Props {
  mode: 'create' | 'edit'
  existing?: PolicyDto
  /**
   * Pre-fetched ResolvedPolicy from the parent so placeholders can show
   * inherited values immediately. If absent we lazy-fetch it ourselves once
   * the chosen scope is fully specified.
   */
  effective?: ResolvedPolicy
  /**
   * When set in `create` mode, pre-fills the scope picker with the given
   * type+key and locks it (renders the read-only chip instead of the radio
   * group). Used by per-resource pages (e.g. the stack-detail Settings tab)
   * that always want a single-scope override and don't need users to choose.
   * Ignored in edit mode, where the existing row's scope is already locked.
   */
  lockedScope?: { type: PolicyScopeType; key: string }
}

const props = defineProps<Props>()
const emit = defineEmits<{
  (e: 'close'): void
  (e: 'saved'): void
}>()

const toast = useToast()

// ---- Scope state -----------------------------------------------------------

const scopeType = ref<PolicyScopeType>(
  props.existing?.scopeType ?? props.lockedScope?.type ?? 'fleet',
)
const scopeKey = ref<string>(
  props.existing?.scopeKey ?? props.lockedScope?.key ?? '',
)

// In edit mode the scope is locked (we PUT the same scope key, not a rename).
// In create mode `lockedScope` also locks the picker so callers like the
// stack-detail tab can pre-fill without exposing a needless radio group.
const scopeLocked = computed(
  () => props.mode === 'edit' || props.lockedScope != null,
)

// Container scope rows are discovered automatically from compose labels
// (Phase 9b.1: `isengard.policy.*` keys on a running container). The UI
// stays read-only for them: the radio is disabled here, and any existing
// container row is rendered with a "discovered from labels" pill in the
// list view rather than an Edit button.
function isScopeRadioDisabled(t: PolicyScopeType): boolean {
  return t === 'container'
}

const scopeKeyPlaceholder = computed<string>(() => {
  switch (scopeType.value) {
    case 'global': return ''
    case 'fleet': return 'prod'
    case 'stack': return 'prod/blog'
    case 'service': return 'prod/blog/web'
    case 'container': return ''
  }
})

const scopeKeyHelper = computed<string>(() => {
  switch (scopeType.value) {
    case 'global': return 'Global default applies when no more specific override exists.'
    case 'fleet': return 'Fleet name (matches the fleet rendered in inventory).'
    case 'stack': return 'Format: fleet/stack'
    case 'service': return 'Format: fleet/stack/service'
    case 'container': return 'Discovered automatically from compose labels (read-only here).'
  }
})

// Validate the slash count + non-empty rules. Mirrors `validate_policy` on
// the backend so users see the error before we round-trip.
const scopeKeyValidationError = computed<string | null>(() => {
  if (scopeType.value === 'global') return null
  const v = scopeKey.value.trim()
  if (v === '') return 'Required for non-global scopes.'
  const parts = v.split('/')
  switch (scopeType.value) {
    case 'fleet':
      return parts.length === 1 && parts[0] !== ''
        ? null
        : 'Fleet name must not contain slashes.'
    case 'stack':
      return parts.length === 2 && parts.every(p => p !== '')
        ? null
        : 'Format must be fleet/stack.'
    case 'service':
      return parts.length === 3 && parts.every(p => p !== '')
        ? null
        : 'Format must be fleet/stack/service.'
    case 'container':
      return 'Container scope is discovered from compose labels and read-only here.'
  }
  return null
})

// Force scope_key empty when global is selected, mirroring backend invariant.
watch(scopeType, t => {
  if (t === 'global') scopeKey.value = ''
})

// ---- Inheritance preview ---------------------------------------------------

// If the parent passed `effective`, use it directly. Otherwise we maintain
// our own fetch keyed on the chosen scope.
const ownCtx = ref<EffectivePolicyContext>({})
const { effective: ownEffective, load: loadOwnEffective } = useEffectivePolicy(ownCtx)

// Single computed merging the two sources: parent prop wins.
const effective = computed<ResolvedPolicy | null>(() =>
  props.effective ?? ownEffective.value,
)

function deriveCtx(t: PolicyScopeType, key: string): EffectivePolicyContext {
  if (t === 'global' || key === '') return {}
  const parts = key.split('/')
  switch (t) {
    case 'fleet':
      return { fleet: parts[0] }
    case 'stack':
      return parts.length === 2
        ? { fleet: parts[0], stack: parts[1] }
        : {}
    case 'service':
      return parts.length === 3
        ? { fleet: parts[0], stack: parts[1], service: parts[2] }
        : {}
    default:
      return {}
  }
}

watch(
  () => [scopeType.value, scopeKey.value] as const,
  ([t, k]) => {
    if (props.effective) return
    ownCtx.value = deriveCtx(t, k)
  },
  { immediate: true },
)

onMounted(async () => {
  if (props.effective) return
  if (Object.keys(ownCtx.value).length > 0) {
    await loadOwnEffective()
  }
})

// ---- Field state -----------------------------------------------------------

// Each field has an `override` flag + a working value. The working value is
// kept stable across override toggles so the user can flip back without
// losing what they typed.

const overrideStrategy = ref<boolean>(props.existing?.body.strategy != null)
const valueStrategy = ref<UpdateStrategy>(
  props.existing?.body.strategy ?? 'tag-only',
)

const overrideGate = ref<boolean>(props.existing?.body.gate != null)
const valueGate = ref<UpdateGate>(
  // Never seed from the disabled approval option.
  (props.existing?.body.gate === 'approval' ? 'auto' : props.existing?.body.gate)
    ?? 'auto',
)

const overridePausedUntil = ref<boolean>(props.existing?.body.paused_until != null)
const valuePausedUntilLocal = ref<string>(
  props.existing?.body.paused_until
    ? toDatetimeLocal(props.existing.body.paused_until)
    : '',
)

const overrideOnFailure = ref<boolean>(props.existing?.body.on_failure != null)
const valueOnFailure = ref<FailureHandling>(
  props.existing?.body.on_failure ?? 'notify',
)

const overrideApproverChannel = ref<boolean>(props.existing?.body.approver_channel != null)
const valueApproverChannel = ref<string>(
  props.existing?.body.approver_channel ?? '',
)

// ---- External gate state (Phase 12c, #55) ---------------------------------

const overrideExternalGate = ref<boolean>(props.existing?.body.external_gate != null)
const valueGateUrl = ref<string>(props.existing?.body.external_gate?.url ?? '')
const valueGateSecret = ref<string>(props.existing?.body.external_gate?.secret ?? '')
const valueGateTimeout = ref<number>(props.existing?.body.external_gate?.timeout_secs ?? 10)

// ---- Window state ----------------------------------------------------------

const overrideWindow = ref<boolean>(props.existing?.body.window != null)
const valueWindowCron = ref<string>(props.existing?.body.window?.cron_expr ?? '0 2 * * 0')
const initialTz = props.existing?.body.window?.timezone ?? ''
const valueWindowTzChoice = ref<WindowTzChoice>(
  initialTz === ''
    ? 'UTC'
    : (WINDOW_TIMEZONES.includes(initialTz as WindowTzChoice)
      ? (initialTz as WindowTzChoice)
      : 'custom'),
)
const valueWindowTzCustom = ref<string>(
  initialTz && !WINDOW_TIMEZONES.includes(initialTz as WindowTzChoice) ? initialTz : '',
)

const resolvedWindowTz = computed<string>(() => {
  if (valueWindowTzChoice.value === 'custom') {
    return valueWindowTzCustom.value.trim() || 'UTC'
  }
  return valueWindowTzChoice.value
})

const windowPreview = computed<string[]>(() => {
  if (!overrideWindow.value) return []
  const expr = valueWindowCron.value.trim()
  if (!expr) return []
  const firings = nextFirings(expr, resolvedWindowTz.value, new Date(), 3)
  if (firings.length === 0) return []
  return firings.map(d => formatFiring(d, resolvedWindowTz.value))
})

const windowPreviewIsInvalid = computed<boolean>(() =>
  overrideWindow.value
    && valueWindowCron.value.trim() !== ''
    && windowPreview.value.length === 0,
)

// ---- Inheritance helpers ---------------------------------------------------

function inheritedStrategy(): string {
  return effective.value?.strategy ?? 'tag-only'
}
function inheritedGate(): string {
  return effective.value?.gate ?? 'auto'
}
function inheritedPausedUntil(): string {
  return effective.value?.paused_until ?? '-'
}
function inheritedOnFailure(): string {
  return effective.value?.on_failure ?? 'notify'
}
function inheritedApproverChannel(): string {
  return effective.value?.approver_channel ?? '-'
}
function inheritedWindow(): string {
  const w = effective.value?.window
  if (!w) return '-'
  const tz = w.timezone ?? 'UTC'
  return `${w.cron_expr} (${tz})`
}

function provenanceFor(field: keyof NonNullable<ResolvedPolicy['provenance']>): string {
  const p = effective.value?.provenance
  if (!p) return 'inherited from DEFAULTS'
  switch (p[field]) {
    case 'default': return 'inherited from DEFAULTS'
    case 'global': return 'inherited from GLOBAL DEFAULT'
    case 'fleet': return 'inherited from FLEET'
    case 'stack': return 'inherited from STACK'
    case 'service': return 'inherited from SERVICE'
    case 'container': return 'inherited from CONTAINER LABEL'
  }
}

// ---- datetime-local conversion --------------------------------------------

/**
 * Convert an RFC3339 timestamp into the value shape required by
 * `<input type="datetime-local">`: `YYYY-MM-DDTHH:MM`. Browser displays in
 * the user's local tz so we slice from a Date in local fields.
 */
function toDatetimeLocal(rfc3339: string): string {
  const d = new Date(rfc3339)
  if (Number.isNaN(d.getTime())) return ''
  const pad = (n: number) => String(n).padStart(2, '0')
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
    `T${pad(d.getHours())}:${pad(d.getMinutes())}`
  )
}

/**
 * Inverse of `toDatetimeLocal`: parse the local-tz string the input emits
 * and serialise as RFC3339 with the browser's tz offset. The backend stores
 * UTC; this preserves the wall-clock the user picked.
 */
function fromDatetimeLocal(local: string): string | null {
  if (!local) return null
  const d = new Date(local)
  if (Number.isNaN(d.getTime())) return null
  return d.toISOString()
}

// ---- Save ------------------------------------------------------------------

const saving = ref(false)
const saveError = ref<string | null>(null)

const hasAnyOverride = computed<boolean>(() =>
  overrideStrategy.value
    || overrideGate.value
    || overridePausedUntil.value
    || overrideOnFailure.value
    || overrideApproverChannel.value
    || overrideWindow.value
    || overrideExternalGate.value,
)

const canSave = computed<boolean>(() => {
  if (saving.value) return false
  if (!hasAnyOverride.value) return false
  if (scopeKeyValidationError.value) return false
  // Block save when the operator has activated the window override but the
  // cron is unparseable. The server's croner-backed validator may still
  // accept patterns our tiny client parser rejects, so we only block when
  // both a value is set AND the parser cannot make sense of it; an empty
  // value is already covered by the empty-cron guard in buildBody.
  if (overrideWindow.value && valueWindowCron.value.trim() !== '' && windowPreviewIsInvalid.value) {
    // We allow save anyway: server has the authoritative parser. UI only
    // hints at the issue. Keep this branch for future tightening.
  }
  return true
})

function buildBody(): PolicyBody {
  const body: PolicyBody = {}
  if (overrideStrategy.value) body.strategy = valueStrategy.value
  if (overrideGate.value) body.gate = valueGate.value
  if (overridePausedUntil.value) {
    const rfc = fromDatetimeLocal(valuePausedUntilLocal.value)
    if (rfc) body.paused_until = rfc
  }
  if (overrideOnFailure.value) body.on_failure = valueOnFailure.value
  if (overrideApproverChannel.value) {
    const trimmed = valueApproverChannel.value.trim()
    if (trimmed !== '') body.approver_channel = trimmed
  }
  if (overrideWindow.value) {
    const cron = valueWindowCron.value.trim()
    if (cron !== '') {
      const w: MaintenanceWindow = { cron_expr: cron }
      const tz = resolvedWindowTz.value
      // Skip the timezone field for the implicit-UTC default to keep the
      // payload tidy; the server treats missing tz as UTC.
      if (tz !== 'UTC') w.timezone = tz
      body.window = w
    }
  }
  if (overrideExternalGate.value) {
    const url = valueGateUrl.value.trim()
    if (url !== '') {
      const g: ExternalGate = { url, timeout_secs: valueGateTimeout.value }
      const sec = valueGateSecret.value.trim()
      if (sec !== '') g.secret = sec
      body.external_gate = g
    }
  }
  return body
}

interface FetchErrorLike {
  status?: number
  data?: { error?: string; message?: string }
  message?: string
}

function decodeError(e: unknown): { status: number | null; message: string } {
  const fe = e as FetchErrorLike
  return {
    status: fe?.status ?? null,
    message: fe?.data?.error ?? fe?.data?.message ?? fe?.message ?? String(e),
  }
}

async function onSave() {
  if (!canSave.value) return
  saving.value = true
  saveError.value = null

  const api = useApi()
  const body = buildBody()
  // Defensive: never POST `gate=approval`. UI prevents it but a stale state
  // could still allow it; we strip rather than 422.
  if (body.gate === 'approval') delete body.gate

  try {
    if (props.mode === 'create') {
      await api.post('/policies', {
        scope_type: scopeType.value,
        scope_key: scopeKey.value.trim(),
        body,
      })
    } else {
      const t = props.existing?.scopeType ?? scopeType.value
      const k = props.existing?.scopeKey ?? scopeKey.value
      const urlKey = scopeKeyForUrl(t, k)
      await api.put(`/policies/${t}/${urlKey}`, { body })
    }
    toast.success(props.mode === 'create' ? 'Policy added' : 'Policy updated')
    emit('saved')
  } catch (e) {
    const { status, message } = decodeError(e)
    if (status === 409) {
      saveError.value = 'A policy already exists at this scope.'
    } else if (status === 422) {
      saveError.value = 'Approval gate is not enforceable until Phase 9e.'
    } else if (status === 400) {
      saveError.value = message || 'Validation failed.'
    } else {
      saveError.value = 'Save failed; try again.'
    }
  } finally {
    saving.value = false
  }
}

/** "Switch to Edit" affordance when create runs into a 409. We re-fetch the
 *  existing row from the server and tell the parent to re-mount us in edit
 *  mode by emitting `saved` (which triggers refresh) + `close`; the user
 *  re-opens via the row's Edit button. Cheaper than a parent contract change.
 */
async function switchToEditAfterConflict() {
  emit('saved')
  emit('close')
}

function onCancel() {
  emit('close')
}

// ---- Header label ----------------------------------------------------------

const scopeLabel = computed<string>(() => {
  if (props.mode === 'create' && !props.lockedScope) return ''
  const t = (props.existing?.scopeType
    ?? props.lockedScope?.type
    ?? scopeType.value
  ).toUpperCase()
  const k = props.existing?.scopeKey
    ?? props.lockedScope?.key
    ?? 'global default'
  return `${t} . ${k || 'global default'}`
})
</script>

<template>
  <div class="flex flex-col gap-4">
    <!-- Scope picker (create-mode only). In edit mode the scope is fixed and
         shown as a chip in the dialog header (rendered by the parent) so we
         don't repeat the label here. -->
    <fieldset
      v-if="!scopeLocked"
      class="flex flex-col gap-2 border border-iso-border-subtle rounded-iso-md p-3 bg-iso-bg-elevated"
    >
      <legend class="text-[10px] uppercase tracking-wider text-iso-text-faint px-1">
        Scope
      </legend>

      <div class="flex flex-wrap gap-x-4 gap-y-1.5">
        <label
          v-for="t in (['global', 'fleet', 'stack', 'service', 'container'] as PolicyScopeType[])"
          :key="t"
          class="flex items-center gap-1.5 text-xs cursor-pointer"
          :class="isScopeRadioDisabled(t) ? 'opacity-50 cursor-not-allowed' : ''"
        >
          <input
            type="radio"
            name="scope_type"
            :value="t"
            :checked="scopeType === t"
            :disabled="isScopeRadioDisabled(t)"
            class="accent-iso-info"
            @change="scopeType = t"
          />
          <span class="font-mono text-iso-text-secondary">{{ t }}</span>
          <span
            v-if="t === 'container'"
            class="text-[10px] text-iso-text-faint"
            title="Discovered automatically from compose labels: read-only here."
          >(from labels)</span>
        </label>
      </div>

      <div v-if="scopeType !== 'global'" class="flex flex-col gap-1 mt-1">
        <Label
          for="scope_key"
          class="text-[10px] uppercase tracking-wider text-iso-text-faint"
        >
          Scope key
        </Label>
        <Input
          id="scope_key"
          v-model="scopeKey"
          :placeholder="scopeKeyPlaceholder"
          :disabled="saving"
          class="font-mono bg-iso-bg-base border-iso-border-subtle text-sm"
        />
        <p class="text-[10px] text-iso-text-muted">{{ scopeKeyHelper }}</p>
        <p
          v-if="scopeKeyValidationError && scopeKey.length > 0"
          class="text-[10px] text-iso-error"
        >
          {{ scopeKeyValidationError }}
        </p>
      </div>
    </fieldset>

    <!-- Edit-mode scope summary chip. Visible read-only label, no editing. -->
    <div
      v-else
      class="flex items-center gap-2 px-3 py-2 rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated"
    >
      <span class="text-[10px] uppercase tracking-wider text-iso-text-faint">
        Scope
      </span>
      <span class="font-mono text-xs text-iso-text-primary">{{ scopeLabel }}</span>
    </div>

    <!-- Field: strategy ---------------------------------------------------- -->
    <div class="flex flex-col gap-1.5">
      <div class="flex items-center justify-between">
        <Label class="text-[11px] uppercase tracking-wider text-iso-text-secondary">
          Strategy
        </Label>
        <label class="flex items-center gap-1.5 text-[11px] text-iso-text-muted cursor-pointer">
          <input
            v-model="overrideStrategy"
            type="checkbox"
            class="accent-iso-info"
          />
          Override at this level
        </label>
      </div>
      <div
        class="flex flex-wrap gap-x-4 gap-y-1.5 px-2 py-1.5 rounded-iso-sm border border-iso-border-subtle bg-iso-bg-elevated"
        :class="overrideStrategy ? '' : 'opacity-60'"
      >
        <label
          v-for="s in (['pinned', 'tag-only', 'minor', 'any'] as UpdateStrategy[])"
          :key="s"
          class="flex items-center gap-1.5 text-xs cursor-pointer"
        >
          <input
            type="radio"
            name="strategy"
            :value="s"
            :checked="valueStrategy === s"
            :disabled="!overrideStrategy"
            class="accent-iso-info"
            @change="valueStrategy = s"
          />
          <span class="font-mono">{{ s }}</span>
        </label>
      </div>
      <p
        v-if="!overrideStrategy"
        class="text-[10px] text-iso-text-muted"
      >
        Currently: <span class="font-mono">{{ inheritedStrategy() }}</span>
        ({{ provenanceFor('strategy') }})
      </p>
    </div>

    <!-- Field: gate -------------------------------------------------------- -->
    <div class="flex flex-col gap-1.5">
      <div class="flex items-center justify-between">
        <Label class="text-[11px] uppercase tracking-wider text-iso-text-secondary">
          Gate
        </Label>
        <label class="flex items-center gap-1.5 text-[11px] text-iso-text-muted cursor-pointer">
          <input
            v-model="overrideGate"
            type="checkbox"
            class="accent-iso-info"
          />
          Override at this level
        </label>
      </div>
      <div
        class="flex flex-wrap gap-x-4 gap-y-1.5 px-2 py-1.5 rounded-iso-sm border border-iso-border-subtle bg-iso-bg-elevated"
        :class="overrideGate ? '' : 'opacity-60'"
      >
        <label class="flex items-center gap-1.5 text-xs cursor-pointer">
          <input
            type="radio"
            name="gate"
            value="auto"
            :checked="valueGate === 'auto'"
            :disabled="!overrideGate"
            class="accent-iso-info"
            @change="valueGate = 'auto'"
          />
          <span class="font-mono">auto</span>
        </label>
        <label class="flex items-center gap-1.5 text-xs cursor-not-allowed opacity-50">
          <input
            type="radio"
            name="gate"
            value="approval"
            :disabled="true"
            class="accent-iso-info"
          />
          <span class="font-mono">approval</span>
          <span class="text-[10px] text-iso-text-faint">(Phase 9e)</span>
        </label>
        <label class="flex items-center gap-1.5 text-xs cursor-pointer">
          <input
            type="radio"
            name="gate"
            value="never"
            :checked="valueGate === 'never'"
            :disabled="!overrideGate"
            class="accent-iso-info"
            @change="valueGate = 'never'"
          />
          <span class="font-mono">never</span>
        </label>
      </div>
      <p
        v-if="!overrideGate"
        class="text-[10px] text-iso-text-muted"
      >
        Currently: <span class="font-mono">{{ inheritedGate() }}</span>
        ({{ provenanceFor('gate') }})
      </p>
    </div>

    <!-- Field: paused_until ----------------------------------------------- -->
    <div class="flex flex-col gap-1.5">
      <div class="flex items-center justify-between">
        <Label
          for="paused_until"
          class="text-[11px] uppercase tracking-wider text-iso-text-secondary"
        >
          Paused until
        </Label>
        <label class="flex items-center gap-1.5 text-[11px] text-iso-text-muted cursor-pointer">
          <input
            v-model="overridePausedUntil"
            type="checkbox"
            class="accent-iso-info"
          />
          Override at this level
        </label>
      </div>
      <input
        id="paused_until"
        v-model="valuePausedUntilLocal"
        type="datetime-local"
        :disabled="!overridePausedUntil"
        class="font-mono text-sm bg-iso-bg-elevated border border-iso-border-subtle rounded-iso-sm px-2 py-1.5 text-iso-text-primary disabled:opacity-50"
      />
      <p
        v-if="!overridePausedUntil"
        class="text-[10px] text-iso-text-muted"
      >
        Currently: <span class="font-mono">{{ inheritedPausedUntil() }}</span>
        ({{ provenanceFor('paused_until') }})
      </p>
      <p
        v-else
        class="text-[10px] text-iso-text-muted"
      >
        Wall-clock time in your browser's timezone; stored as UTC.
      </p>
    </div>

    <!-- Field: window (Phase 9d) ------------------------------------------ -->
    <div class="flex flex-col gap-1.5">
      <div class="flex items-center justify-between">
        <Label
          for="window_cron"
          class="text-[11px] uppercase tracking-wider text-iso-text-secondary"
        >
          Maintenance window
        </Label>
        <label class="flex items-center gap-1.5 text-[11px] text-iso-text-muted cursor-pointer">
          <input
            v-model="overrideWindow"
            type="checkbox"
            class="accent-iso-info"
          />
          Override at this level
        </label>
      </div>
      <div
        class="flex flex-col gap-2 px-2 py-2 rounded-iso-sm border border-iso-border-subtle bg-iso-bg-elevated"
        :class="overrideWindow ? '' : 'opacity-60'"
      >
        <Input
          id="window_cron"
          v-model="valueWindowCron"
          placeholder="0 2 * * 0"
          :disabled="!overrideWindow"
          class="font-mono bg-iso-bg-base border-iso-border-subtle text-sm"
        />
        <p class="text-[10px] text-iso-text-muted">
          Use cron syntax: minute hour day-of-month month day-of-week
        </p>
        <div class="flex items-center gap-2">
          <select
            v-model="valueWindowTzChoice"
            :disabled="!overrideWindow"
            class="font-mono text-xs bg-iso-bg-base border border-iso-border-subtle rounded-iso-sm px-2 py-1 text-iso-text-primary disabled:opacity-50"
          >
            <option v-for="tz in WINDOW_TIMEZONES" :key="tz" :value="tz">{{ tz }}</option>
          </select>
          <Input
            v-if="valueWindowTzChoice === 'custom'"
            v-model="valueWindowTzCustom"
            placeholder="e.g. America/Los_Angeles"
            :disabled="!overrideWindow"
            class="font-mono bg-iso-bg-base border-iso-border-subtle text-xs"
          />
        </div>
        <div v-if="overrideWindow" class="flex flex-col gap-0.5">
          <p
            v-if="windowPreview.length > 0"
            class="text-[10px] text-iso-text-muted"
          >
            Next 3 firings: {{ windowPreview.join(' / ') }}
          </p>
          <p
            v-else-if="windowPreviewIsInvalid"
            class="text-[10px] text-iso-error"
          >
            (invalid expression)
          </p>
        </div>
      </div>
      <p
        v-if="!overrideWindow"
        class="text-[10px] text-iso-text-muted"
      >
        Currently: <span class="font-mono">{{ inheritedWindow() }}</span>
        ({{ provenanceFor('window') }})
      </p>
      <p
        v-else
        class="text-[10px] text-iso-text-muted"
      >
        Outside this window updates emit update.deferred and skip recreation.
      </p>
    </div>

    <!-- Field: on_failure ------------------------------------------------- -->
    <div class="flex flex-col gap-1.5">
      <div class="flex items-center justify-between">
        <Label class="text-[11px] uppercase tracking-wider text-iso-text-secondary">
          On failure
        </Label>
        <label class="flex items-center gap-1.5 text-[11px] text-iso-text-muted cursor-pointer">
          <input
            v-model="overrideOnFailure"
            type="checkbox"
            class="accent-iso-info"
          />
          Override at this level
        </label>
      </div>
      <div
        class="flex flex-wrap gap-x-4 gap-y-1.5 px-2 py-1.5 rounded-iso-sm border border-iso-border-subtle bg-iso-bg-elevated"
        :class="overrideOnFailure ? '' : 'opacity-60'"
      >
        <label
          v-for="f in (['rollback', 'keep', 'notify'] as FailureHandling[])"
          :key="f"
          class="flex items-center gap-1.5 text-xs cursor-pointer"
        >
          <input
            type="radio"
            name="on_failure"
            :value="f"
            :checked="valueOnFailure === f"
            :disabled="!overrideOnFailure"
            class="accent-iso-info"
            @change="valueOnFailure = f"
          />
          <span class="font-mono">{{ f }}</span>
        </label>
      </div>
      <p
        v-if="!overrideOnFailure"
        class="text-[10px] text-iso-text-muted"
      >
        Currently: <span class="font-mono">{{ inheritedOnFailure() }}</span>
        ({{ provenanceFor('on_failure') }})
      </p>
    </div>

    <!-- Field: approver_channel ------------------------------------------ -->
    <div class="flex flex-col gap-1.5">
      <div class="flex items-center justify-between">
        <Label
          for="approver_channel"
          class="text-[11px] uppercase tracking-wider text-iso-text-secondary"
        >
          Approver channel
        </Label>
        <label class="flex items-center gap-1.5 text-[11px] text-iso-text-muted cursor-pointer">
          <input
            v-model="overrideApproverChannel"
            type="checkbox"
            class="accent-iso-info"
          />
          Override at this level
        </label>
      </div>
      <Input
        id="approver_channel"
        v-model="valueApproverChannel"
        :placeholder="overrideApproverChannel ? 'e.g. ops-team-chat' : inheritedApproverChannel()"
        :disabled="!overrideApproverChannel"
        class="font-mono bg-iso-bg-elevated border-iso-border-subtle text-sm"
      />
      <p
        v-if="!overrideApproverChannel"
        class="text-[10px] text-iso-text-muted"
      >
        Currently: <span class="font-mono">{{ inheritedApproverChannel() }}</span>
        ({{ provenanceFor('approver_channel') }})
      </p>
      <p
        v-else
        class="text-[10px] text-iso-text-muted"
      >
        Notifier channel id (informational; wired in Phase 9f).
      </p>
    </div>

    <!-- Field: external_gate (Phase 12c, #55) ----------------------------- -->
    <div class="flex flex-col gap-1.5">
      <div class="flex items-center justify-between">
        <Label
          for="ext_gate_url"
          class="text-[11px] uppercase tracking-wider text-iso-text-secondary"
        >
          External gate
        </Label>
        <label class="flex items-center gap-1.5 text-[11px] text-iso-text-muted cursor-pointer">
          <input
            v-model="overrideExternalGate"
            type="checkbox"
            class="accent-iso-info"
          />
          Override at this level
        </label>
      </div>
      <div
        class="flex flex-col gap-2 px-2 py-2 rounded-iso-sm border border-iso-border-subtle bg-iso-bg-elevated"
        :class="overrideExternalGate ? '' : 'opacity-60'"
      >
        <Input
          id="ext_gate_url"
          v-model="valueGateUrl"
          placeholder="https://gate.example.com/decide"
          :disabled="!overrideExternalGate"
          class="font-mono bg-iso-bg-base border-iso-border-subtle text-sm"
        />
        <p class="text-[10px] text-iso-text-muted">
          POSTed before any update; response decides approve / reject / defer / manual.
        </p>
        <Input
          id="ext_gate_secret"
          v-model="valueGateSecret"
          placeholder="Optional HMAC secret"
          :disabled="!overrideExternalGate"
          class="font-mono bg-iso-bg-base border-iso-border-subtle text-sm"
        />
        <div class="flex items-center gap-2">
          <Label
            for="ext_gate_timeout"
            class="text-[10px] uppercase tracking-wider text-iso-text-faint"
          >
            Timeout (s)
          </Label>
          <input
            id="ext_gate_timeout"
            v-model.number="valueGateTimeout"
            type="number"
            min="1"
            max="300"
            :disabled="!overrideExternalGate"
            class="font-mono w-20 text-sm bg-iso-bg-base border border-iso-border-subtle rounded-iso-sm px-2 py-1 text-iso-text-primary disabled:opacity-50"
          />
        </div>
      </div>
      <p
        v-if="!overrideExternalGate"
        class="text-[10px] text-iso-text-muted"
      >
        No gate configured. Updates apply per the resolved policy without consulting an external endpoint.
      </p>
      <p
        v-else
        class="text-[10px] text-iso-text-muted"
      >
        Receiver must respond with JSON: { decision: "approve" | "reject" | "defer" | "manual" }.
      </p>
    </div>

    <!-- Save error banner -------------------------------------------------- -->
    <div
      v-if="saveError"
      class="rounded-iso-md border border-iso-error/40 bg-iso-error-soft px-3 py-2 text-xs text-iso-error flex items-center justify-between gap-3"
    >
      <span>{{ saveError }}</span>
      <button
        v-if="saveError === 'A policy already exists at this scope.'"
        type="button"
        class="px-2 py-1 rounded-iso-sm border border-iso-error/40 text-iso-error hover:bg-iso-error/10"
        @click="switchToEditAfterConflict"
      >Switch to Edit</button>
    </div>

    <!-- Helper banner: empty override row would be a no-op. -->
    <p
      v-if="!hasAnyOverride && !saveError"
      class="text-[11px] text-iso-text-muted"
    >
      Toggle at least one "Override at this level" checkbox to save.
    </p>

    <DialogFooter>
      <Button
        variant="ghost"
        :disabled="saving"
        @click="onCancel"
      >Cancel</Button>
      <Button
        variant="outline"
        :disabled="!canSave"
        class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success disabled:opacity-40"
        @click="onSave"
      >
        {{ saving ? 'Saving...' : (props.mode === 'create' ? 'Create policy' : 'Save changes') }}
      </Button>
    </DialogFooter>
  </div>
</template>
