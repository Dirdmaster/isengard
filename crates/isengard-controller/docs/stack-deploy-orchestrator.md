Stack-level deployment orchestrator that sits above the per-host
deployment supervisor.

When the same stack runs on multiple hosts and an image change is
detected (or a force-update is dispatched stack-wide), the orchestrator
decides how many hosts deploy in lockstep: 1 (rolling, the default), 2,
3, or all of them. Single-host deploys bypass the orchestrator
entirely; the existing per-host supervisors keep their behaviour.
Agents only ever receive the `apply_update` host action the
orchestrator (or the bypass path) queued for them.

# State

Three pieces:

1. The persistent `deployment_groups` row (one per active group).
2. An in-memory map keyed by `group_id` holding the wave plan plus the
   deployment ids the orchestrator is waiting on for the current wave.
3. A subscription to the controller's event bus for
   `deployment.completed`, `deployment.aborted`, and
   `deployment.failed`.

The state machine runs under `tokio::select!` over (event bus,
periodic reconcile, abort signal). The reconcile tick exists to break
ties when an event is missed (network partition, lagged subscriber):
every N seconds the orchestrator re-queries storage for the current
wave's deployments and treats the result as authoritative.

# Dispatch trait

[`WaveDispatcher`] is the seam between the orchestrator and the rest
of the controller. Production wires it to [`Inventory::queue_action`];
tests inject a recording mock so they don't need a real agent or gRPC
stack.

# Failure handling

[`OnFailure::Rollback`] (default) stops on the first failure in a wave
and marks the group `aborted`. [`OnFailure::Continue`] keeps rolling
forward; if any wave failed by the end the group lands in `failed`
instead of `done`.
