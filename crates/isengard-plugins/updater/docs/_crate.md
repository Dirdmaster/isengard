Image-version policy driver for the Isengard agent.

The plugin runs agent-side. Each cycle: list local Docker containers,
filter to `isengard.enable=true`, compare each container's local
digest against the remote registry digest, classify as
`up_to_date | needs_update | unknown`, then take action per policy.

# What the cycle does

1. **Filter candidates.** Containers carrying `isengard.enable=true`.
2. **Pull the policy snapshot.** Once per cycle via the configured
   [`PolicyLoader`]. Loader errors fail-safe to an empty snapshot
   (don't block updates on a transient DB problem).
3. **Resolve per-candidate policy.** Build an
   [`policy::OwnedPolicyContext`] from the container's compose
   labels, the cached fleet, and the host id. Resolve via
   `isengard_core::policy::resolve_policy`.
4. **Early skips.** `Pinned` and active `paused_until` skip the
   candidate with an `update.policy_skipped` event. Outside a
   maintenance window emits `update.deferred` and skips.
5. **Probe digests.** `inspect_image` (local) and `head_digest`
   (remote). The local probe targets the original running tag even
   when the [`tag_cache::pick_highest_minor`] path bumps to a new tag.
6. **Minor strategy.** When `strategy = Minor`, optionally bump to
   the highest patch+minor on the current major via
   [`maybe_minor_bump`] (degrades to tag-only on registry errors).
7. **Apply.** On `needs_update`, re-resolve the policy with both
   digests so `gate = Approval` can trigger
   `handle_pending_approval` (idempotent insert + dedupe). When
   the gate doesn't apply, hand off to the [`UpdateDispatcher`]
   (blue-green driver) if wired, else recreate in place via
   [`recreate::update_container`]. Self-update (the updater's own
   container) takes the rename-then-replace path in
   [`self_update::update_self`].

# Strategy ladder

`UpdateStrategy::Recreate` is the default in-place recreate. The
[`UpdateDispatcher`] hand-off (when wired) chooses between in-place
and blue-green by inspecting container labels, mounts, and ports
via [`dispatch_helpers`]. The dispatcher's `Handled` outcome means a
driver has taken ownership of the deploy; the cycle never recreates
behind it.

# Approval gate

When the resolved policy has `gate = Approval`, the cycle calls
`handle_pending_approval` before any recreate. The helper:

- Looks up `(host, stack, service, proposed_digest)` for an existing
  open row. A match returns `Deduplicated` (no event, no insert).
- Inserts a new row with `expires_at = now + 24h` and emits
  `update.pending_approval` with `action_id`. The dashboard /
  notifier plugin renders an interactive message; operator
  decision queues an `apply_update` `HostAction`.

# Self-update

[`self_update`] handles the updater updating itself. Different
ordering from a normal recreate: rename the running container to a
sibling, start the new container under the original name, then
schedule process exit. [`self_id::current_container_id`] resolves
the agent's own container id from `/proc/self/cgroup`.
