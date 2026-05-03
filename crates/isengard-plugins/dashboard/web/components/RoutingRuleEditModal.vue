<template>
  <Dialog :open="open" @update:open="emit('update:open', $event)">
    <DialogContent class="max-w-md">
      <DialogHeader>
        <DialogTitle>{{ rule ? 'Edit routing rule' : 'Add routing rule' }}</DialogTitle>
      </DialogHeader>

      <div class="space-y-4 py-4">
        <div>
          <label class="text-xs text-iso-text-muted">Hostname</label>
          <input v-model="form.public_hostname" type="text" placeholder="blog.example.com"
                 class="w-full font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-elevated" />
        </div>

        <div class="grid grid-cols-2 gap-3">
          <div>
            <label class="text-xs text-iso-text-muted">Service</label>
            <input v-model="form.service_name" type="text" placeholder="web"
                   class="w-full font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-elevated" />
          </div>
          <div>
            <label class="text-xs text-iso-text-muted">Container port</label>
            <input v-model.number="form.container_port" type="number"
                   class="w-full font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-elevated" />
          </div>
        </div>

        <div>
          <label class="text-xs text-iso-text-muted">Adapter</label>
          <div class="flex gap-2 mt-1">
            <label v-for="a in ['none', 'tailscale', 'cf-tunnel']" :key="a"
                   class="flex items-center gap-1 text-xs cursor-pointer">
              <input v-model="form.adapter" type="radio" :value="a" />
              {{ a }}
            </label>
          </div>
        </div>

        <div>
          <label class="text-xs text-iso-text-muted">TLS mode</label>
          <div class="flex gap-2 mt-1">
            <label v-for="m in ['edge', 'acme', 'manual']" :key="m"
                   class="flex items-center gap-1 text-xs cursor-pointer">
              <input v-model="form.tls_mode" type="radio" :value="m" />
              {{ m }}
            </label>
          </div>
        </div>

        <div>
          <label class="text-xs text-iso-text-muted">Healthcheck path (optional)</label>
          <input v-model="form.healthcheck_path" type="text" placeholder="/healthz"
                 class="w-full font-mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-elevated" />
        </div>
      </div>

      <DialogFooter>
        <button class="px-3 py-1.5 text-sm text-iso-text-muted" @click="emit('update:open', false)">
          Cancel
        </button>
        <button class="px-3 py-1.5 text-sm bg-iso-info text-iso-bg-base rounded font-medium"
                :disabled="!canSave" @click="onSave">
          {{ rule ? 'Save' : 'Create' }}
        </button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>

<script setup lang="ts">
import { reactive, computed, watch } from 'vue'
import { useRoutingRules, type RoutingRule } from '~/composables/useRoutingRules'

const props = defineProps<{ open: boolean; rule?: RoutingRule | null; defaultHostId?: string }>()
const emit = defineEmits<{ (e: 'update:open', v: boolean): void }>()

const { createRule, updateRule } = useRoutingRules()

const form = reactive<Partial<RoutingRule>>({
  public_hostname: '',
  service_name: '',
  container_port: 8080,
  adapter: 'none',
  tls_mode: 'acme',
  healthcheck_path: null,
  fleet: 'default',
  source: 'ui',
})

watch(() => props.rule, (r) => {
  if (r) {
    Object.assign(form, r)
  } else {
    Object.assign(form, {
      public_hostname: '', service_name: '', container_port: 8080,
      adapter: 'none', tls_mode: 'acme', healthcheck_path: null,
      fleet: 'default', source: 'ui',
    })
  }
}, { immediate: true })

const canSave = computed(() => !!form.public_hostname && !!form.service_name && !!form.container_port)

async function onSave() {
  const body: any = { ...form }
  if (props.defaultHostId && !body.host_id) body.host_id = props.defaultHostId
  if (props.rule) {
    await updateRule(props.rule.id, body)
  } else {
    await createRule(body)
  }
  emit('update:open', false)
}
</script>
