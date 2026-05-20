Deployment row and its supervisor state machine.

Migration `0011` lands `deployments`. A row is one in-flight or
completed deployment. Later migrations extend the state machine:
`0012` adds the per-service `deploy_strategy` column; `0013` adds
`Recovering`; `0022` adds rollback (`RollingBack`, `RolledBack`,
`RollbackFailed`, plus the `previous_digest` and
`rollback_attempted_at` columns); `0018` adds the `group_id` link to
multi-host orchestrations.

# State machine

Single-service deploys walk:

```text
Pending -> SpinningUp -> Switching -> Draining -> DestroyingBlue -> Done
                                  \-> Recovering -> Failed (post-switch collapse)
       -> RollingBack -> RolledBack
                     \-> RollbackFailed
```

Terminals are `Done`, `Aborted`, `Failed`, `RolledBack`,
`RollbackFailed`. The supervisor's `is_in_flight` predicate is
`!is_terminal()`.

[`DeploymentState::Recovering`] is the post-switch collapse handler:
green went unhealthy after the swap, the driver is rolling back to
the snapshotted blue upstream. Non-terminal (transitions to `Failed`
once the swap-back completes).

[`DeploymentState::RollingBack`] is the supervisor's `Rollback`
failure-handler branch. The driver is re-pulling `previous_digest`
and recreating the container. Non-terminal (transitions to
`RolledBack` on success or `RollbackFailed` on error).

# Strategies

[`DeployStrategy::BlueGreen`] brings a parallel color up, swaps
traffic, then tears the old color down. [`DeployStrategy::InPlace`]
recreates the container at the new image without a parallel color.

# Multi-host fan-out

When a stack rolls across more than one host, the orchestrator wraps
each per-host deployment in a [`crate::DeploymentGroup`] and stamps
`group_id` on every member via `Inventory::set_deployment_group`.
Single-host deploys leave `group_id = NULL`.
