import { defineStore } from 'pinia'
import { computed, ref, watch, type WatchStopHandle } from 'vue'
import type { Host } from '~/stores/hosts'

export type WizardStep = 1 | 2 | 3 | 4

const STORAGE_COMPLETE = 'isengard_setup_complete'
const STORAGE_SKIPPED = 'isengard_setup_skipped'

export const useWizardStore = defineStore('wizard', () => {
  const step = ref<WizardStep>(1)
  const hostname = ref('')
  const fleet = ref('default')
  const hostId = ref<string | null>(null)
  const enrollmentToken = ref<string | null>(null)
  const installCommand = ref<string | null>(null)
  const enrolledHost = ref<Host | null>(null)
  const discoveredStacks = ref(0)
  const discoveredServices = ref(0)
  const error = ref<string | null>(null)
  const startedAt = ref(0)

  // useMock can be flipped by the dev console (window.__wizardMock = true) or
  // a future ?mock=1 query param. Production default is FALSE — wizard uses
  // the real /api/v1/hosts endpoint and watches the live event stream for
  // agent.enroll events matching its issued host id.
  const useMock = ref(false)
  let mockTimerId: ReturnType<typeof setTimeout> | null = null
  let stopEnrollWatcher: WatchStopHandle | null = null

  function reset() {
    step.value = 1
    hostname.value = ''
    fleet.value = 'default'
    hostId.value = null
    enrollmentToken.value = null
    installCommand.value = null
    enrolledHost.value = null
    discoveredStacks.value = 0
    discoveredServices.value = 0
    error.value = null
    startedAt.value = 0
    if (mockTimerId) {
      clearTimeout(mockTimerId)
      mockTimerId = null
    }
    if (stopEnrollWatcher) {
      stopEnrollWatcher()
      stopEnrollWatcher = null
    }
  }

  async function issueToken(): Promise<void> {
    error.value = null
    if (useMock.value) {
      hostId.value = 'mock-host-' + Math.random().toString(36).slice(2, 10)
      enrollmentToken.value = 'tok_' + Math.random().toString(36).slice(2, 14)
      installCommand.value = renderDockerRun({
        controllerUrl: window.location.origin.replace(/^http/, 'http'),
        token: enrollmentToken.value,
      })
      return
    }
    try {
      const api = useApi()
      const dto = await api.post<{
        agent_id: string
        enrollment_token: string
        install_command: string
      }>('/hosts', {
        fleet: fleet.value,
        hostname: hostname.value || undefined,
      })
      hostId.value = dto.agent_id
      enrollmentToken.value = dto.enrollment_token
      installCommand.value = dto.install_command
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    }
  }

  function renderDockerRun(opts: { controllerUrl: string; token: string }) {
    const lines = [
      'docker run -d --name isengard-agent --restart=always \\',
      '  -v /var/run/docker.sock:/var/run/docker.sock \\',
      '  -v isengard-agent-data:/var/lib/isengard \\',
      `  -e CONTROLLER_URL=${opts.controllerUrl} \\`,
      `  -e ENROLLMENT_TOKEN=${opts.token} \\`,
      '  --group-add $(stat -c %g /var/run/docker.sock) \\',
      '  ghcr.io/dirdmaster/isengard-agent:latest',
    ]
    return lines.join('\n')
  }

  function listenForEnroll() {
    startedAt.value = Date.now()
    if (useMock.value) {
      mockTimerId = setTimeout(() => {
        enrolledHost.value = {
          id: hostId.value ?? 'mock-host',
          hostname: hostname.value || 'prod-04',
          fleet: fleet.value,
          fingerprint: 'mock-fingerprint-fffe',
          os: 'linux',
          arch: 'x86_64',
          agent_version: '0.1.0-alpha',
          docker_version: '26.1.0',
          enrolled_at: new Date().toISOString(),
          last_seen_at: new Date().toISOString(),
        } as Host
        discoveredStacks.value = 7
        discoveredServices.value = 14
        step.value = 4
      }, 5000)
      return
    }

    // Real path: watch the global event stream (already running from app.vue
    // via useEventStream + eventsStore) for an agent.enroll event matching
    // our issued host id. The id is the controller-assigned ULID returned by
    // POST /hosts. The agent dials enroll → controller persists + publishes
    // → eventsStore.events updates → this watcher fires.
    const eventsStore = useEventsStore()
    const hostsStore = useHostsStore()
    stopEnrollWatcher = watch(
      () => eventsStore.events,
      async (events) => {
        if (!hostId.value) return
        const match = events.find(
          (e) => e.kind === 'agent.enroll' && e.host_id === hostId.value,
        )
        if (!match) return

        // Found it. Refresh the hosts store and pull the row.
        await hostsStore.load()
        const enrolled = hostsStore.hosts.find((h) => h.id === hostId.value)
        if (enrolled) {
          enrolledHost.value = enrolled
        }

        // Stack/service discovery counts come from later heartbeats. Best
        // effort: peek at the stacks store; fall back to 0 (Step 4 renders
        // a graceful "Agent reporting" line in that case).
        try {
          const stacksStore = useStacksStore()
          if (!stacksStore.loaded) await stacksStore.fetchAll()
          const hostStacks = stacksStore.byHost(hostId.value)
          discoveredStacks.value = hostStacks.length
          // Stack DTO doesn't carry per-service counts yet; approximate as
          // 1 service per stack until Phase 5d's service rollup ships.
          discoveredServices.value = hostStacks.length
        } catch { /* discovery counts are best-effort */ }

        step.value = 4
        if (stopEnrollWatcher) {
          stopEnrollWatcher()
          stopEnrollWatcher = null
        }
      },
      { deep: false, immediate: true },
    )
  }

  function next() {
    if (step.value === 1) {
      step.value = 2
    } else if (step.value === 2) {
      step.value = 3
      void issueToken().then(() => listenForEnroll())
    } else if (step.value === 3) {
      step.value = 4
    }
  }

  function back() {
    if (step.value === 2) step.value = 1
    else if (step.value === 3) {
      if (mockTimerId) {
        clearTimeout(mockTimerId)
        mockTimerId = null
      }
      step.value = 2
    }
  }

  function skip() {
    localStorage.setItem(STORAGE_SKIPPED, 'true')
    reset()
  }

  function complete() {
    localStorage.setItem(STORAGE_COMPLETE, 'true')
    reset()
  }

  const elapsedSeconds = computed(() =>
    startedAt.value === 0 ? 0 : Math.floor((Date.now() - startedAt.value) / 1000)
  )

  return {
    step,
    hostname,
    fleet,
    hostId,
    enrollmentToken,
    installCommand,
    enrolledHost,
    discoveredStacks,
    discoveredServices,
    error,
    elapsedSeconds,
    useMock,
    next,
    back,
    skip,
    complete,
    reset,
    issueToken,
    listenForEnroll,
  }
})

export function shouldShowWizard(): boolean {
  if (typeof window === 'undefined') return false
  if (localStorage.getItem(STORAGE_COMPLETE)) return false
  if (localStorage.getItem(STORAGE_SKIPPED)) return false
  return true
}
