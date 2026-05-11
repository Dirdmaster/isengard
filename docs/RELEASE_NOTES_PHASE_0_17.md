# Phase 0.17: wisp cap_add + nsenter race

> Phase 0.17 of the v0.3 arc. Two surgical bug fixes that unblock real workloads (nginx, short-lived containers) under wisp on iso-fresh-1. No new public surface; no config migration; the default cap allow-list does not change.

## Headline

nginx + short-lived containers now deploy cleanly via `stack.toml` under wisp. Two race / plumbing bugs that surfaced once iso-fresh-1 left the busybox happy path are closed.

## Bug A: compose `cap_add:` is parsed and dropped before reaching WispBackend

### What broke

An operator writing `cap_add: [CHOWN, SETUID, SETGID, DAC_OVERRIDE, FOWNER, SETPCAP]` in compose had those bytes silently lost. The capability chain existed end-to-end except for one missing hop: the agent's compose reader (`compose_reconciler::parse_service`) had no `cap_add` field on `DesiredService`, so the manifest-overlay-merged list went nowhere. nginx:alpine then exec'd against the wisp default allow-list (`CAP_KILL` + `CAP_NET_BIND_SERVICE`), the master tried to chown `/var/cache/nginx/client_temp` to uid 101, hit EPERM, and exited 1.

### Fix

Two-step bridge in the agent (no wisp-image / wisp changes):

1. `DesiredService` gains `pub cap_add: Vec<String>`. `parse_service` reads the compose `cap_add:` list and stores each entry as-given (both `CHOWN` and `CAP_CHOWN` forms are accepted; the downstream parse_caps handles the prefix).
2. `desired_service_to_create_spec` joins the list with commas and sets the `isengard.cap.add` label on the resulting `ContainerCreateSpec` when non-empty. The label is the wire format the WispBackend's `cap_add_from_labels` reader already honored: it lands in all five OCI capability sets (bounding, effective, permitted, inheritable, ambient).

We routed through a label rather than adding a typed field on `ContainerCreateSpec` because the spec is persisted to disk (`<state_dir>/wisp/specs/<name>.json`) and read back during inspect / list. A new typed field would have broken the persisted-spec shape. Phase 0.18 can promote with a versioned migration.

### Example

```toml
# stack.toml
[web]
image = "docker.io/library/nginx:alpine"
ports = ["127.0.0.1:8080:80"]
cap_add = [
    "CHOWN",
    "SETUID",
    "SETGID",
    "DAC_OVERRIDE",
    "FOWNER",
    "SETPCAP",
    "NET_BIND_SERVICE",
    "KILL",
]
```

Note: the wisp `cap_add` implementation REPLACES the default allow-list rather than unioning it (matching `wisp-cli --cap-add` and `wisp_image::CapabilityOverride` semantics). If you need port 80 (`NET_BIND_SERVICE`) or signal delivery on stop (`KILL`), include them explicitly when you supply any `cap_add` list.

## Bug B: nsenter races a short-lived child

### What broke

The pre-0.17 lifecycle ran network attach CONCURRENTLY with the child's post-clone setup:

```
clone3 -> cgroup.add_pid -> attach (six nsenter calls) -> wait_ready
                            |
                            child: drop bounding, mount, pivot,
                                   set caps, signal_ready, exec
```

For long-lived workloads (the busybox demo: `sleep 30`) the entrypoint outlived nsenter by minutes. For short-lived ones (`sh -c 'exit 0'`) or workloads that hit an early EPERM (nginx without cap_add): the child exec'd, ran, exited, and the kernel reaped its netns BEFORE one of the parent's `nsenter -t <child_pid> -n ip ...` calls finished. The attach surfaced as `veth::attach_to_ns: No such file or directory`, masking the actual exit_code.

### Fix: pre-exec hold via a second sync pipe (Option A in the spec)

Adds a parent-to-child `GoPipe` (mirror of the existing child-to-parent `ReadyPipe`). The child now blocks in pre-exec hold until the parent finishes attach:

```
Parent: clone3
        cgroup.add_pid
        wait_ready          (child is post-pivot, pinned in wait_go)
        attach (if network) (child cannot exec or exit while we run)
        signal_go           (release child)
        spawn_reaper

Child:  pre-clone setup
        drop bounding, mount, pivot, chdir, sethostname, set caps
        signal_ready
        wait_go             (NEW: blocks until parent finishes attach)
        raise_ambient
        execvpe
```

The child's `wait_go` uses the same `read_exact` pattern as `wait_ready`: if the parent crashes between `wait_ready` and `signal_go`, the child observes EOF and exits cleanly with a "parent died before signalling go" diagnostic. The reaper picks up the dead child as a normal `Die`.

Every failure path in the parent now drops the GoPipe writer before kill + reap so the child's `wait_go` cannot hang.

### What this does NOT change

- Default cap allow-list: still `CAP_KILL` + `CAP_NET_BIND_SERVICE`. Containers without `cap_add` behave identically to v0.3.6.
- AppArmor / seccomp profiles: unchanged. Tracked in the wisp polish backlog.
- User namespace remapping: still deferred (Phase 0.3+).
- Multi-network containers: unchanged.

## Operator decisions locked in this phase

1. **nsenter race fix: Option A** (pre-exec hold via second sync pipe). NOT Option B (pre-create netns refactor) or Option C (retry-with-grace). Option B is still the recommended long-term direction for true network-setup / runtime decoupling, tracked in the wisp polish backlog.
2. **cap_add plumbing: label bridge via `isengard.cap.add`** rather than a new `ContainerCreateSpec::cap_add` field. Keeps the persisted spec.json shape unchanged. Phase 0.18 can promote to a typed field with a versioned migration.

## Migration

Zero. The default cap set is unchanged. Containers that don't use `cap_add` behave identically to v0.3.6. The lifecycle change adds one pipe round-trip per container start (sub-millisecond on iso-fresh-1).

## Verification

Inside the wisp VM as root:

```sh
cargo test -p wisp --tests -- --ignored
cargo test -p isengard-agent --test wisp_nginx_cap_add_e2e -- --ignored --nocapture
```

Both green on the iso-fresh-1 wisp VM. The wakeup-briefing reproduction case (nginx:alpine via compose) is the load-bearing assertion in `wisp_nginx_cap_add_e2e`.

## Files touched

- `crates/isengard-agent/src/compose_reconciler.rs` (cap_add field + parser + tests)
- `crates/isengard-agent/src/compose_apply.rs` (label hop + tests)
- `crates/wisp/src/lifecycle/pipe.rs` (GoPipe + signal_go + wait_go + tests)
- `crates/wisp/src/lifecycle/mod.rs` (sequence reorder)
- `crates/wisp/tests/lifecycle_immediate_exit.rs` (new)
- `crates/isengard-agent/tests/wisp_nginx_cap_add_e2e.rs` (new)
- `docs/RELEASE_NOTES_PHASE_0_17.md` (this file)
