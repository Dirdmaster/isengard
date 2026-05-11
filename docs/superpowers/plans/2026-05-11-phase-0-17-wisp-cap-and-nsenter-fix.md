# Phase 0.17 Wisp cap-add + nsenter Race: Implementation Plan

> Spec: [`2026-05-11-phase-0-17-wisp-cap-and-nsenter-fix-design.md`](../specs/2026-05-11-phase-0-17-wisp-cap-and-nsenter-fix-design.md). Branch target: `phase/0-17-wisp-cap-and-nsenter` (stacks on `next`).

## Scope

Five sequential commits. Each ends in a green workspace + tests; each is shippable in isolation. Steps 1 and 2 are independent and can be reviewed in parallel; steps 3, 4, 5 stack.

Subagent dispatch model for every implementer step: **Opus** (per `feedback_implementer_opus`). Reviewer subagents may stay on Sonnet.

## Prerequisite gates (BEFORE step 1)

- Operator nod on spec Option A (pre-exec hold) vs Option B (pre-create netns). Default is A; flipping to B doubles the scope and rewrites step 2 + step 3.
- Operator nod on the label-routing decision for compose `cap_add` (vs a new `ContainerCreateSpec::cap_add` field). Default is label; flipping rewrites step 1.

If either default is flipped, this plan needs a v2 before impl dispatches.

## Steps

### Step 1: compose `cap_add:` flows through to the `isengard.cap.add` label (Bug A)

Standalone fix. Ships before step 2.

- Add `pub cap_add: Vec<String>` to `DesiredService` in `crates/isengard-agent/src/compose_reconciler.rs` (field 12 in the struct; update the `Default` derive coverage).
- Extend `parse_service` in the same file: read `cap_add:` as a YAML/TOML list of strings, store unchanged. Accept both `CAP_CHOWN` and `CHOWN` forms (the downstream `bundle::parse_caps` handles both).
- In `desired_service_to_create_spec` (`crates/isengard-agent/src/compose_apply.rs:271`), if `svc.cap_add` is non-empty, set `labels.insert("isengard.cap.add", svc.cap_add.join(","))`. Leave the label absent when `cap_add` is empty (preserves existing tests).
- Add unit tests (Mac, in-tree):
  - `parse_service_reads_cap_add_list` covers `cap_add: [CHOWN, SETUID]` + `[CAP_CHOWN, CAP_SETUID]`.
  - `desired_service_to_create_spec_lifts_cap_add_into_label` asserts label content matches.
  - `parse_service_omits_cap_add_when_absent` confirms the label stays out when compose omits the field.

Files touched:
- `crates/isengard-agent/src/compose_reconciler.rs` (struct + parser + 2 tests)
- `crates/isengard-agent/src/compose_apply.rs` (label hop + 1 test)

Deliverable: commit `feat(agent): compose cap_add wires through to wisp cap label`.

Verify:
- `cargo test -p isengard-agent`
- `cargo clippy -p isengard-agent --all-targets -- -D warnings`
- `cargo fmt --check`

### Step 2: lifecycle pre-exec hold pipe pair (refactor sync pipe; no behavior change yet)

Builds the second sync pipe but does NOT yet move the network attach into it. Keeps the runtime behaving identically: child writes `ready`, parent reads, child gets immediate `go`, child execs. Step 3 then moves the attach call to between read and write.

- Extend `crates/wisp/src/lifecycle/pipe.rs`: add `GoPipe` struct mirroring `ReadyPipe`, plus `signal_go` + `wait_go` functions writing/reading the literal `go` (2 bytes). Same `O_CLOEXEC` discipline.
- Extend the pair constructor to optionally return a `GoPipe` alongside `ReadyPipe`; OR add a second `pair_go()` constructor. Pick the simpler one.
- In `crates/wisp/src/lifecycle/mod.rs::start_container`:
  - Open a GoPipe alongside the existing ReadyPipe before `clone3`.
  - Child path: after `signal_ready`, call `wait_go(reader)`. Close the reader fd before raise_ambient + execvpe (CLOEXEC handles it but explicit is clearer).
  - Parent path: after `wait_ready`, call `signal_go(writer)`. Close the writer fd.
  - For now the parent's signal_go happens IMMEDIATELY after wait_ready (no functional change). Step 3 inserts the network attach between wait_ready and signal_go.
- Add unit tests in pipe.rs covering signal_go / wait_go (same shape as ReadyPipe tests).
- Add a comment in mod.rs marking the inserted-between-here spot for step 3.

Files touched:
- `crates/wisp/src/lifecycle/pipe.rs` (GoPipe + 3 tests)
- `crates/wisp/src/lifecycle/mod.rs` (sequence change)

Deliverable: commit `refactor(wisp): add pre-exec hold pipe for lifecycle child`.

Verify:
- `cargo test -p wisp`
- Existing busybox e2e in the wisp VM still passes (run if you have a VM handy; gated `WISP_E2E=1`).
- `cargo clippy -p wisp --all-targets -- -D warnings`

### Step 3: move veth attach into the pre-exec window (Bug B fix)

Depends on step 2. Now the architectural fix lands.

- In `crates/wisp/src/lifecycle/mod.rs::start_container`, the network attach block at lines ~480-514 currently runs BEFORE `wait_ready`. Move the sequence to:
  1. clone3
  2. cgroup.add_pid
  3. wait_ready (child has pivoted, set caps, written `ready`)
  4. attach(spec, id, child_pid, rootfs) IF network_spec is set
  5. signal_go
  6. spawn_reaper
- Verify the rationale comment block above the attach call gets rewritten: the old comment claims pivot + attach can run concurrently. Under the new sequence, pivot is DONE by the time attach starts, so the comment needs to explain why the child waits for go even after signaling ready.
- The error path (attach fails) must still kill + reap the child; same shape as today but the cleanup now also has to drop the GoPipe writer so the child's wait_go observes EOF rather than hanging.
- Update the integration tests in `crates/wisp/tests/runtime_with_network.rs` to assert the new ordering. They likely already pass as-is because the ordering is invisible to a long-lived workload.

Files touched:
- `crates/wisp/src/lifecycle/mod.rs` (sequence move + error-path cleanup + comment rewrite)
- `crates/wisp/tests/runtime_with_network.rs` (assertions for ordering, if needed)

Deliverable: commit `fix(wisp): attach network in pre-exec window to close nsenter race`.

Verify:
- `cargo test -p wisp` (full suite)
- `cargo clippy -p wisp --all-targets -- -D warnings`
- In wisp VM: `WISP_E2E=1 cargo test -p wisp --test runtime_with_network -- --ignored` (busybox happy path still green).

### Step 4: integration tests for both bugs (nginx happy path + busybox immediate-exit)

Validates the fixes against the real lifecycle. Both tests gated by `WISP_E2E=1` and root.

- Add `crates/isengard-agent/tests/wisp_nginx_cap_add_e2e.rs`:
  - Deploy `image: nginx:alpine, cap_add: [CHOWN, SETUID, SETGID, DAC_OVERRIDE, FOWNER, SETPCAP]` via the WispBackend (mirror `wisp_compose_e2e`'s setup pattern).
  - Assert state == Running after a 5s settle.
  - Assert `curl http://127.0.0.1:<published>` returns 200 + the `nginx` server header.
  - Tear down + assert state.json is gone.
- Add `crates/wisp/tests/lifecycle_immediate_exit.rs`:
  - Use the busybox demo bundle with `args = ["sh", "-c", "exit 0"]` and a default network spec.
  - Drive Runtime::create_with_network + start_with_attacher.
  - Poll state.json for Stopped (up to 3s).
  - Assert `exit_status` file contains `0`.
  - Assert nothing logged via tracing contains `nsenter`, `attach_to_ns`, or `No such file or directory` for that container ID. (Use `tracing-subscriber` test capture or scrape the stderr.)
- Both tests `#[ignore = "needs root + linux + wisp VM"]`.

Files touched:
- `crates/isengard-agent/tests/wisp_nginx_cap_add_e2e.rs` (new)
- `crates/wisp/tests/lifecycle_immediate_exit.rs` (new)

Deliverable: commit `test(wisp): nginx cap_add e2e + immediate-exit nsenter regression`.

Verify:
- Run both tests in wisp VM with `WISP_E2E=1` and root. Both green.
- On Mac, the tests compile but stay ignored.

### Step 5: release notes

- New file: `docs/RELEASE_NOTES_WISP_PHASE_0_17.md`. Sections:
  - Headline: "nginx + short-lived containers now deploy cleanly via stack.toml under wisp."
  - Bug A summary + the new `cap_add:` compose field (with the example compose snippet operators will copy-paste).
  - Bug B summary + the lifecycle change (pre-exec hold; one-paragraph explanation).
  - Migration: zero. The default cap set is unchanged. Containers that don't use cap_add behave identically.
  - Acknowledge the wakeup-briefing reproduction case (iso-fresh-1, nginx:alpine).
- Update `docs/superpowers/specs/...` and `docs/superpowers/plans/...` cross-links so the release notes resolve.

Files touched:
- `docs/RELEASE_NOTES_WISP_PHASE_0_17.md` (new)

Deliverable: commit `docs(wisp): release notes for phase 0.17 cap-add + nsenter race`.

Verify:
- Visual inspection. No code touched.

## Optional follow-ups (NOT in this phase)

- Promote `isengard.cap.add` label to a typed `ContainerCreateSpec::cap_add` field (requires a persisted-spec migration). Phase 0.18 candidate.
- Option B refactor (pre-create netns via temp helper) for true network-setup / runtime decoupling. Tracked in wisp polish backlog.
- Tighten default cap allow-list to the runc/docker defaults so common images (nginx, postgres, redis) work without explicit cap_add. Security-sensitive; needs its own threat-model review.
- AppArmor profile + seccomp default profile (wisp polish backlog).

## Charter notes

- Per `feedback_isengard_v03_charter`: this phase touches `crates/` (steps 1-4) and main is OFF-limits without explicit nod. Land on `phase/0-17-wisp-cap-and-nsenter` stacked on `next`; PR opens once all five commits are green.
- No `Co-Authored-By` trailers.
- No em or en dashes anywhere.
- Hard stop: if step 3's pre-exec hold breaks the busybox happy path in the wisp VM, revert step 3 + step 2 and revisit the spec with Option C as a temporary patch.
