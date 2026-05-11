# Phase 0.17 Wisp cap-add + nsenter Race: Design Spec

> Status: spec only. Plan companion: [`docs/superpowers/plans/2026-05-11-phase-0-17-wisp-cap-and-nsenter-fix.md`](../plans/2026-05-11-phase-0-17-wisp-cap-and-nsenter-fix.md). Branch target: `phase/0-17-wisp-cap-and-nsenter` (stacks on `next`). No code in this PR.

## Goal

Operators can `isd deploy --file ./compose.toml --stack hello` with `image = "nginx:alpine"` and the container reaches `Running` state under wisp on iso-fresh-1. Same flow ships a `busybox sh -c 'exit 0'` (immediate-exit) and the lifecycle emits a `Die { exit_code: 0 }` event with no `nsenter` error in the agent log.

## Done bar

1. nginx:alpine deployed via wisp reaches `Running`. `curl http://localhost:<published>` returns the nginx welcome page.
2. Immediate-exit container (`busybox sh -c 'exit 0'`) emits a clean `Die { exit_code: 0 }`. The agent log contains zero `nsenter` errors for that container's lifecycle.
3. The default wisp cap allow-list does NOT change: existing busybox demo + every prior phase keeps its `CAP_KILL` + `CAP_NET_BIND_SERVICE` baseline.
4. Operator-supplied `cap_add: [CHOWN, SETUID, SETGID, DAC_OVERRIDE, FOWNER, SETPCAP]` on a compose service flows through to the OCI bundle's five capability sets.

## Out of scope

- Adding `cap_drop` semantics (separate phase).
- User namespace remapping (Phase 0.3+ deferral; cap-add unblocks nginx without it).
- Multi-network containers, AppArmor profiles, seccomp tightening.
- Changing the default cap allow-list to the runc/docker defaults (security-sensitive: tracked separately in the wisp polish backlog).

## Bug A: compose `cap_add:` is parsed and dropped before reaching WispBackend

### Current state

The capability plumbing exists end-to-end EXCEPT for one missing hop: the compose `cap_add:` field is parsed by the manifest layer (`crates/isengard-manifest/src/overlay.rs:76` lists it as a list-merge field), then DROPPED by the agent's compose reader. Specifically:

- `crates/isengard-agent/src/compose_reconciler.rs:40-60` (`DesiredService` struct) has no `cap_add` field. The struct lists name, image, container_name, command, entrypoint, environment, labels, ports, restart, secrets, networks. `cap_add` is absent.
- `crates/isengard-agent/src/compose_reconciler.rs:422` (`parse_service`) reads each compose key but skips `cap_add` (it has no handler arm).
- `crates/isengard-agent/src/compose_apply.rs:271-333` (`desired_service_to_create_spec`) builds a `ContainerCreateSpec` from `DesiredService`. With no `cap_add` field on the source, the resulting `ContainerCreateSpec.labels` never carries `isengard.cap.add`.
- `crates/isengard-agent/src/runtime/wisp_backend.rs:91-110` (`cap_add_from_labels`) reads the `isengard.cap.add` LABEL and turns it into a `wisp_image::CapabilityOverride`. The label is the only currently-supported wire format. Helper works. Caller helper works (`spec_to_config_overrides` at line 58 passes the override through to `BundleBuilder`).
- `crates/wisp-image/src/bundle.rs:179-202` (`synthesise_config`) consumes `ConfigOverrides::capabilities` and writes all five OCI cap sets into `config.json`. Works.
- `crates/wisp/src/lifecycle/mod.rs:283-313` reads the five cap sets out of the bundle and applies them in the child. Works.

The chain breaks at the compose parser. An operator who writes `cap_add: [CHOWN, SETUID, ...]` in `compose.yaml` has those bytes silently lost. There is no warning, no error: the agent persists the resulting spec sans the capability info and starts nginx with the default `CAP_KILL` + `CAP_NET_BIND_SERVICE`. nginx exits 1 because the master process can't `chown /var/cache/nginx/client_temp` to uid 101.

Phase 0.5's release notes (referenced in the wakeup briefing) promised `cap-add` at the wisp-cli surface, which IS wired (`crates/wisp-cli/src/main.rs:114-124, 534`, plus `cap_add_to_override` at line 1175). The agent inherited the helper but no caller bridges compose YAML to the `isengard.cap.add` label.

### Fix

Two-step bridge:

1. Add `pub cap_add: Vec<String>` to `DesiredService` (`crates/isengard-agent/src/compose_reconciler.rs:40`).
2. In `parse_service`, read the `cap_add:` field (compose accepts list form only: `cap_add: [CHOWN, SETUID]`). Strip a leading `CAP_` prefix or not; `bundle::parse_caps` accepts both forms. Store the raw strings as-given.
3. In `desired_service_to_create_spec`, if `svc.cap_add` is non-empty, set `labels.insert("isengard.cap.add", svc.cap_add.join(","))`. This re-uses the existing `cap_add_from_labels` reader, so no other code path changes.

Why route through a label rather than adding a first-class `ContainerCreateSpec::cap_add` field: `ContainerCreateSpec` is also persisted to disk (`<state_dir>/wisp/specs/<name>.json`) and read back by inspect / list. Adding a new field would break-change the persisted shape. Labels are already a free-form map and round-trip through inspect cleanly. Phase 0.18 can promote the label to a typed field once the persisted shape gets a versioned migration.

### Risk

- `cap_add` lets a container author grant itself any subset of Linux capabilities. Threat model: the operator's compose is the trust boundary (same trust model as docker / kubernetes). The author of a stack.toml chooses what their container needs; the agent is not deciding for them. Operator-controlled compose => caps approved by definition.
- The default cap set stays at `CAP_KILL` + `CAP_NET_BIND_SERVICE`. Any image that relies on the default (i.e. the busybox demo, every test fixture, every prior phase) is unchanged.
- A stack.toml that omits `cap_add` for nginx will still fail to start exactly as it does today. Surfacing a hint in the error path is NOT in this phase's scope.

### Tests

- Unit (Mac): `parse_service` round-trips `cap_add: [CHOWN, SETUID, SETGID, DAC_OVERRIDE, FOWNER, SETPCAP]` to `DesiredService.cap_add`.
- Unit (Mac): `desired_service_to_create_spec` with a non-empty `cap_add` produces a `ContainerCreateSpec` whose labels carry `isengard.cap.add=CHOWN,SETUID,...`.
- Unit (Mac): `spec_to_config_overrides` with that label produces a `ConfigOverrides::capabilities` populated in all five sets.
- Ignored Linux integration (root + wisp VM): `wisp_compose_e2e::nginx_alpine_with_cap_add_starts` deploys `image: nginx:alpine, cap_add: [CHOWN, SETUID, SETGID, DAC_OVERRIDE, FOWNER, SETPCAP]` and asserts `state == Running` and `curl 127.0.0.1:<port>` returns the nginx welcome page.

## Bug B: nsenter race when the child exits before veth attach completes

### Current state

The lifecycle sequence today (`crates/wisp/src/lifecycle/mod.rs:386-563`):

1. Parent calls `clone3` with five `CLONE_NEW*` flags. Returns parent + child.
2. Parent path: `cgroup.add_pid(child_pid)`, then `attacher.attach(spec, id, child_pid, rootfs)` if a network spec is set, then `pipe::wait_ready(&reader)`.
3. Child path (`run_child` at line 720): drop bounding caps, mount rootfs, pivot, chdir, sethostname, set permitted/effective/inheritable caps, `signal_ready` (writes `ready` to pipe), raise ambient caps, `execvpe`.

`attacher.attach` ends up calling `wisp_net::veth::attach_to_ns` (`crates/wisp-net/src/veth.rs:88-126`), which does six `nsenter -t <pid> -n ip ...` invocations (`crates/wisp-net/src/ns.rs:18-49`). Each one shells out to `nsenter` which opens `/proc/<pid>/ns/net` to enter the target netns.

Concurrency hazard: the parent's `attacher.attach` runs CONCURRENTLY with the child's `run_child` sequence. The child does NOT wait for attach to complete; it does its own setup and signals "ready" once pivot + caps are done. The child's signal_ready happens BEFORE execvpe but after pivot_root, so by the time the parent reads "ready" the child has already torn down its view of the host rootfs and is one syscall from exec.

For long-running workloads (the busybox demo) this is fine: the entrypoint outlives nsenter's lifetime by minutes. For short-lived workloads:

- `sh -c 'exit 0'`: child execs, runs, exits, kernel reaps, `/proc/<pid>/ns/net` disappears, ALL while the parent's nsenter calls may still be in flight. The parent sees `No such file or directory` from nsenter on any of steps 4..8 in `attach_to_ns`. The whole attach errors, the lifecycle kills + reaps a child that's already dead, and the operator gets a "veth::attach_to_ns" error masking what was actually a clean exit.
- nginx hits a variant: nginx's master process tries to `chown` early (before `execvpe`'s child has done anything net-related), exits with EPERM, gets reaped, and the parent's nsenter races against the same vanished netns.

The wakeup-briefing reproduction confirms both shapes: nginx hits EPERM AND then nsenter races; alpine + `sleep 3600` works (no EPERM, long-lived) but the briefing notes "same nsenter race afterwards" suggesting a separate occurrence path that still wants tightening.

### Fix options

| Option | Approach | Pro | Con |
|---|---|---|---|
| A | Pre-exec hold: child waits for a parent-side "go" signal AFTER pivot but BEFORE execvpe. Parent's `attach` runs to completion, then writes "go" to a SECOND pipe; only then does the child exec. | Surgical, deterministic. Race window eliminated by design. Reuses existing sync-pipe pattern. | Adds one syscall + one pipe pair. Child blocks longer pre-exec. State machine extends to two transitions instead of one. |
| B | Pre-create veth + move into pre-existing netns: wisp creates the netns explicitly (via `unshare(CLONE_NEWNET)` in a temp helper), attaches veth there, then `clone3 | CLONE_NEWNET = 0` and uses `setns` to enter the pre-built netns. | Clean separation of network setup from runtime. Future-proof. | Significant refactor across wisp + wisp-net. The temp helper process needs its own pid-1 lifecycle. setns + CLONE_NEW* interactions are subtle. |
| C | Retry-with-grace nsenter: if `/proc/<pid>/ns/net` disappears mid-attach, emit `Die` and treat attach as a no-op (the container's exit_status is fine). | One-file patch. | Patches symptom, not cause. Long-running workloads that hit a transient nsenter blip (e.g. fork-storm cgroup churn) still error out. Doesn't fix nginx-EPERM case. |

### Recommendation: Option A (pre-exec hold)

One-line summary: hold the child in a pre-exec wait until the parent finishes attach, then let the child exec.

The race is intrinsic to "child exec races parent nsenter". Plugging it via a second sync point is the smallest change that makes the failure mode disappear, and it leans on a pattern (ready-pipe) wisp already operates correctly. Option B is the right end state but a phase-sized refactor; Option C is a band-aid that papers over the immediate failure while leaving the architecture brittle.

State-machine extension:

- Existing pipe (child -> parent): "ready" token, written after pivot.
- NEW pipe (parent -> child): "go" token, written after attach + cgroup attach + persisted state.
- Child sequence becomes: drop bounding, mount, pivot, chdir, sethostname, set caps, signal_ready, **wait_go**, raise ambient, execvpe.
- Parent sequence becomes: clone3, cgroup.add_pid, attach (if network), wait_ready, **signal_go**, spawn reaper.

The order of "ready" and "go" matters: the parent must observe "ready" before signalling "go" because the child needs pivot to complete before the parent's rootfs-side writes (resolv.conf, hosts) are visible inside the netns. The current code already writes resolv.conf via the rootfs path (`crates/isengard-agent/src/runtime/wisp_backend_attacher.rs:87-107`), which sits on the host filesystem at attach-time (the child hasn't pivoted away from it yet); the new wait_go pattern preserves that invariant by delaying the child's execvpe, not its pivot.

### Risk

- If signal_go is not sent (parent crashes between wait_ready and signal_go), the child blocks indefinitely in wait_go. Mitigation: the child's wait_go uses the same `read_exact` pattern as wait_ready; if the parent's writer end is dropped without writing, the child observes EOF and exits with a clear "parent died before signalling go" diagnostic. Reaper picks up the dead child as a normal `Die`.
- The added wait_go window keeps the child pinned in pre-pivot caps + permitted set. We deliberately raise ambient AFTER wait_go so the operator-controlled caps land in ambient post-attach. This is the existing ordering invariant (`mod.rs:851`); no change.
- The new pipe pair is opened in the parent BEFORE clone3 (same as ReadyPipe today). Both pairs survive into the child via inheritance. Both writer ends get closed in the parent after clone3, both reader ends get closed in the child after the relevant token reads. CLOEXEC keeps them out of the execvpe'd entrypoint.

### Tests

- Unit (Mac): `pipe::pair` + `signal_go` + `wait_go` round-trip (mirrors existing ReadyPipe tests).
- Unit (Mac): wait_go observes EOF when the parent's writer is dropped without writing.
- Ignored Linux integration (root + wisp VM): `wisp/tests/lifecycle_immediate_exit.rs` runs a busybox bundle with `args = ["sh", "-c", "exit 0"]` and asserts:
  - Lifecycle returns `Ok(())`.
  - State transitions: Created -> Running -> Stopped.
  - `exit_status` file contains `0`.
  - Agent log (or wisp-cli stderr) contains zero `nsenter` errors AND zero `veth::attach_to_ns` errors.
  - If a network spec is set (use the default agent bridge), `nginx_alpine_with_cap_add_starts` from Bug A covers the happy path.

## Threat model carry-over

Adding cap_add does not change wisp's broader security stance:

- `noNewPrivileges: true` stays on. An suid binary inside the rootfs cannot escalate beyond `permitted`.
- Bounding is dropped pre-pivot so ambient + permitted can never re-acquire dropped caps.
- The cap set the operator specifies replaces the default in ALL five OCI sets. There is no partial form (matching docker `--cap-add` behavior); a future `cap_drop` would intersect rather than union.
- AppArmor / seccomp are unchanged. The wisp polish backlog tracks a tighter profile.

## Operator-review asks (default-and-document)

None requested as default-and-document. Two clarifying notes for the reviewer:

1. We route compose `cap_add` through the `isengard.cap.add` label rather than a new `ContainerCreateSpec` field. This avoids breaking the persisted-spec shape. Phase 0.18 can promote.
2. Option A vs Option B was decided in this spec. Option B (pre-create netns) is the recommended long-term direction but is a phase-sized refactor; Option A is the smallest patch that closes the race today. If you prefer the bigger refactor, the impl phase pivots to Option B (~2x the effort) and the spec gets updated before code starts.
