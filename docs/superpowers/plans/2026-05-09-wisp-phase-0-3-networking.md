# Wisp Phase 0.3 Networking: Implementation Plan

> Spec: [`2026-05-09-wisp-phase-0-3-networking-design.md`](../specs/2026-05-09-wisp-phase-0-3-networking-design.md). Branch: `wisp/phase-0-1` (continuing the foundation arc).

## Scope

Land `crates/wisp-net/` (library) plus `wisp-runtime` integration so containers get a real network: bridge + veth + IPv4 IPAM + iptables NAT + resolv.conf + /etc/hosts. Add `wisp net` subcommand group + `--network` and `--port` flags on `wisp run`. Done bar: `wisp run --image nginx:alpine --network app --port 18080:80 web` then `curl http://127.0.0.1:18080/` returns the nginx welcome page from the OrbStack VM.

Out of scope per spec: IPv6, cross-host overlay, CNI, live reconfig, --network host, --network none (different from no-net default).

## Dev environment

OrbStack `wisp` VM. Networking tests need root + Linux + a real bridge/iptables/veth. Mac unit tests cover the side-effect-free planners (mirroring `mount::plan_mounts` from 0.1).

The OrbStack VM must have `iproute2` and `iptables-nft`. Verify in dispatch A first:

```
orb -m wisp bash -lc "ip --version && iptables --version"
```

If missing, install via `apt-get install -y iproute2 iptables`.

## Files touched

| File | Change |
| --- | --- |
| `Cargo.toml` (workspace) | add `crates/wisp-net` to `members` |
| `crates/wisp-net/Cargo.toml` | new, deps: `wisp` (path), `nix`, `ipnet`, `macaddr`, `serde`, `serde_json`, `thiserror`, `tracing`, `tempfile` (dev) |
| `crates/wisp-net/src/lib.rs` | new, public API |
| `crates/wisp-net/src/error.rs` | new, WispNetError |
| `crates/wisp-net/src/bridge.rs` | new, bridge create / delete / list (shells out to `ip`) |
| `crates/wisp-net/src/veth.rs` | new, veth pair create + move into ns |
| `crates/wisp-net/src/ipam.rs` | new, IPAM trait + StaticBitmapIpam |
| `crates/wisp-net/src/iptables.rs` | new, NAT + DNAT rule planning + apply |
| `crates/wisp-net/src/resolv.rs` | new, /etc/resolv.conf templating |
| `crates/wisp-net/src/hosts.rs` | new, /etc/hosts templating |
| `crates/wisp-net/src/ns.rs` | new, netns helpers |
| `crates/wisp-net/src/lifecycle.rs` | new, attach + detach orchestration |
| `crates/wisp-net/examples/attach-busybox.rs` | new, demo |
| `crates/wisp-net/tests/bridge_lifecycle.rs` | new, ignored, root-only |
| `crates/wisp-net/tests/veth_pair_into_ns.rs` | new, ignored, root-only |
| `crates/wisp-net/tests/iptables_nat_round_trip.rs` | new, ignored, root-only |
| `crates/wisp-net/tests/ports_publish.rs` | new, ignored, root-only |
| `crates/wisp/src/lifecycle/mod.rs` | extended: optional NetworkSpec param + second sync hop |
| `crates/wisp/src/runtime.rs` (or lib.rs) | extended: container_pid getter, NetworkSpec plumbing |
| `crates/wisp-cli/Cargo.toml` | add `wisp-net` |
| `crates/wisp-cli/src/main.rs` | new `Net` subcommand group + `--network` + `--port` on Run |
| `crates/wisp-net/README.md` | new |
| `docs/RELEASE_NOTES_WISP_PHASE_0_3.md` | new |

## Steps

Four sequenced dispatches. All commits land local. Per `feedback_implementer_opus`: Opus implementers; Sonnet code-reviewer at the iptables + lifecycle boundaries.

### Dispatch A: skeleton + IPAM + bridge planning

Pure logic + planners. No real `ip` invocations.

#### A1: workspace skeleton

- Add `crates/wisp-net` to workspace `Cargo.toml` `members`.
- `crates/wisp-net/Cargo.toml` with the deps listed in "Files touched".
- `crates/wisp-net/src/lib.rs`: module declarations + re-exports stub.
- `crates/wisp-net/src/error.rs`: `WispNetError` enum (Io, Parse, Cmd { command, exit_code, stderr }, NotFound, Conflict, Ipam(String), Iptables(String), Bridge(String), Veth(String), Resolv(String)).
- Validate: `cargo build -p wisp-net`, `cargo test -p wisp-net` (zero tests).
- Commit: `feat(wisp-net): workspace skeleton`

#### A2: IPAM (StaticBitmapIpam)

- `crates/wisp-net/src/ipam.rs` per the spec.
- Trait + tempdir-backed JSON-state impl.
- Tests:
  - `alloc_returns_lowest_free`
  - `alloc_skips_gateway_and_broadcast`
  - `alloc_persists_across_reload`
  - `release_returns_addr_to_pool`
  - `alloc_errors_when_subnet_full`
  - `list_returns_currently_allocated`
- Commit: `feat(wisp-net): static-bitmap IPAM with persistence`

#### A3: bridge planner

- `crates/wisp-net/src/bridge.rs` with `bridge::plan_create(net) -> Vec<IpCommand>` and similar for delete + listing. The actual command-execution helper (`exec_ip`) lives in `ns.rs` or a new `cmd.rs`; it just shells out to `ip` and returns Result with stderr captured on non-zero exit.
- Tests (Mac OK, just plan-comparison):
  - `plan_create_emits_link_add_addr_set_up_in_order`
  - `plan_delete_emits_link_delete`
  - bridge name derivation: `wbr-<net>` (truncate to 15 chars max for kernel constraint)
- Commit: `feat(wisp-net): bridge command planner`

#### A4: iptables planner

- `crates/wisp-net/src/iptables.rs` with `plan_for_network(net) -> RuleSet` and `plan_for_attachment(att) -> RuleSet`.
- `RuleSet` is a struct with `creates: Vec<Rule>` and `deletes: Vec<Rule>` so we can dry-run / verify.
- `Rule` is `{ table, chain, args }` plus a `comment` for grep-by-comment cleanup.
- Tests:
  - `network_rules_include_masquerade_and_forward`
  - `attachment_rules_include_dnat_for_each_port`
  - `loopback_dnat_present_for_localhost_clients`
  - `comment_includes_wisp_marker_with_id`
- Commit: `feat(wisp-net): iptables rule planner`

### Dispatch B: real syscalls + integration tests

Linux-only writers + the integration test harness.

#### B1: cmd.rs + bridge::ensure / delete / list (real)

- `crates/wisp-net/src/cmd.rs`: `pub fn run_ip(args: &[&str]) -> Result<String, WispNetError>` + `run_iptables`. Capture stdout + stderr; preserve stderr in the error on non-zero.
- `bridge::ensure(net) -> Result<()>`: walk plan_create's ops; idempotent (existing bridge with same subnet is OK; existing bridge with different subnet is an error).
- `bridge::delete(net) -> Result<()>`: walk plan_delete; tolerate missing.
- `bridge::list_wisp_bridges() -> Vec<String>`: scan `/sys/class/net/` for `wbr-*`.
- Integration test `tests/bridge_lifecycle.rs`, `#[ignore]`, root-only:
  ```
  let net = Network { name: "wisp-test", bridge: "wbr-test", subnet: "10.99.0.0/24".parse().unwrap(), gateway: "10.99.0.1".parse().unwrap() };
  bridge::ensure(&net)?;
  assert!(bridge::list_wisp_bridges()?.contains("wbr-test"));
  bridge::delete(&net)?;
  assert!(!bridge::list_wisp_bridges()?.contains("wbr-test"));
  ```
- Commit: `feat(wisp-net): real bridge create / delete / list`

#### B2: veth.rs + ns.rs + clone3-driven test

- `crates/wisp-net/src/veth.rs`: `attach(net, ipam, ns_pid, ports) -> NetworkAttachment`. Wires the veth pair, moves the container side into the ns via `ip link set <name> netns <pid>`, runs `nsenter -t <pid> -n ip ...` to rename + addr-add + route-add.
- `crates/wisp-net/src/ns.rs`: helpers wrapping `nsenter`.
- Integration test `tests/veth_pair_into_ns.rs`, `#[ignore]`, root-only:
  - clone3 a child that just sleeps for a minute (mimicking a wisp container).
  - Set up a bridge.
  - Call `attach(...)` against the child's PID.
  - From the host, assert `ip link show wveth-h-<id>` exists.
  - From inside the child's ns (via `nsenter`), assert `ip addr show eth0` shows the allocated IP.
  - Reap.
- Commit: `feat(wisp-net): veth pair create + move into ns + nsenter wiring`

#### B3: iptables apply (real) + NAT round-trip

- `crates/wisp-net/src/iptables.rs::apply(rules) -> Result<()>` and `revoke(rules) -> Result<()>`. Use `iptables-restore --noflush` for atomicity where possible; fall back to per-rule `iptables -A` / `-D` if iptables-restore complains about the syntax.
- Use a `WISP-<net-id>` chain in `nat` POSTROUTING + PREROUTING + OUTPUT, and in `filter` FORWARD. All wisp rules live there; clean up by flushing + deleting the chain.
- Integration test `tests/iptables_nat_round_trip.rs`, `#[ignore]`, root-only:
  - Set up a bridge + a single container with `--port 12080:80`.
  - Inside the container's ns, run a tiny HTTP server (`busybox httpd -f -p 80` or netcat with a fixed response).
  - From the host: `curl http://127.0.0.1:12080/`. Assert response received.
  - Tear down.
- Commit: `feat(wisp-net): iptables NAT + DNAT apply / revoke`

### Dispatch C: lifecycle integration into wisp-runtime + resolv/hosts

Wires the whole thing into `wisp-runtime`'s create/start/delete sequence. Highest-risk dispatch in 0.3 because it modifies existing 0.1 code.

#### C1: NetworkSpec + Runtime API extensions

- `crates/wisp/src/lib.rs`: add `NetworkSpec { network_name, ports, resolv_source }` (probably re-exported from wisp-net via a feature, OR define a small parallel struct here so wisp doesn't pull in wisp-net unconditionally; pick whichever has cleaner imports).
  - Recommend: put `NetworkSpec` and `PortPublish` in `wisp` (the runtime) since the runtime owns the lifecycle. wisp-net consumes them.
- New method: `Runtime::create_with_network(id, bundle, network_spec) -> Result<ContainerHandle>`. Existing `create` keeps current shape; document it as "create without networking".
- New getter: `Runtime::container_pid(&id) -> Option<u32>`. Reads from state.json.
- Tests via mock: `create_with_network_persists_network_spec`.
- Commit: `feat(wisp): NetworkSpec + Runtime::create_with_network`

#### C2: lifecycle integration (the key step)

- `crates/wisp/src/lifecycle/mod.rs`: extend `start_container` to accept an optional NetworkSpec. The new sync sequence:
  ```
  parent: clone3
  parent: cgroup add_pid
  parent: if network_spec: wisp_net::attach(...)  -> writes resolv.conf + /etc/hosts pre-pivot
  parent: signal_ready_to_exec
  child: ... existing setup ...
  child: wait_ready_to_exec
  child: exec
  ```
- The two-hop pipe pattern: parent has TWO writers + child has TWO readers; OR a single bidirectional pair where the child waits for "go" twice (rootfs-ready, then network-ready). Pick whichever is cleaner; the existing `pipe.rs` may need a second pair.
- resolv.conf and hosts files are written by the PARENT into `<rootfs>/etc/resolv.conf` etc. (since the rootfs is bind-mounted on the host before pivot). This means resolv writes happen BEFORE the child enters the rootfs, which is fine because `mount::setup_rootfs` already bind-mounted the bundle's rootfs.
  - Wait: the rootfs path the parent sees is `<bundle>/rootfs`, but the CHILD is what does setup_rootfs and pivot_root. So actually the parent should write resolv.conf into `<bundle>/rootfs/etc/resolv.conf` BEFORE `clone3`. That's even simpler. Do that.
- `Runtime::delete` calls `wisp_net::detach` if a NetworkAttachment is recorded.
- Integration test (Linux as root): `tests/runtime_with_network.rs` end-to-end:
  - create_with_network on a busybox bundle + alpine bundle.
  - inside the container, ping the gateway address (10.83.0.1).
  - ping out to a public IP if connectivity works.
  - Reap.
- Commit: `feat(wisp): integrate wisp-net into Runtime lifecycle`

#### C3: resolv.rs + hosts.rs (templating)

- `crates/wisp-net/src/resolv.rs` + `hosts.rs` per spec.
- `populate_resolv_and_hosts(rootfs, network_spec, attachment)` -> writes the files.
- Tests (Mac OK):
  - `resolv_host_copy_filters_localhost_nameservers`
  - `resolv_static_emits_nameserver_per_addr`
  - `hosts_minimal_template_includes_localhost_v4_and_v6`
  - `hosts_includes_attachment_id_and_ip`
- Commit: `feat(wisp-net): resolv.conf and /etc/hosts templating`

### Dispatch D: wisp-cli + demo + release notes

#### D1: wisp-cli net subcommands + --network / --port flags

- `crates/wisp-cli/Cargo.toml`: add `wisp-net = { path = "../wisp-net" }`.
- `crates/wisp-cli/src/main.rs`: new `Net` subcommand group (`Create { name, subnet }`, `List`, `Rm { name }`, `Inspect { name }`).
- Extend `RunArgs` with `--network <name>` and `--port <host:container>` (repeatable). Auto-create `wisp-default` (10.83.0.0/24) if `--port` is set without `--network`.
- Default ID derivation unchanged.
- Tests: clap shape via `command().debug_assert()`.
- Commit: `feat(wisp-cli): net subcommand + --network / --port flags`

#### D2: demo + README + release notes

- `crates/wisp-net/examples/attach-busybox.rs`: standalone demo (set up bridge, clone3 a sleep busybox, attach, sleep, detach, reap).
- `crates/wisp-net/README.md`: status, run-the-demo (the nginx + curl flow), ABI choices (MAC prefix, bridge naming).
- `docs/RELEASE_NOTES_WISP_PHASE_0_3.md`: standard pattern.
- Run the end-to-end demo on the VM:
  ```
  wisp net create app
  wisp run --image nginx:alpine --network app --port 18080:80 web
  curl http://127.0.0.1:18080/
  # expected: nginx welcome HTML
  ```
- Capture verbatim output. If demo passes, include in release notes. If failure: report honestly.
- Commit: `docs(wisp-net): example, README, and phase 0.3 release notes`

## Validation per dispatch

- `cargo build -p wisp-net -p wisp -p wisp-cli` (Mac + VM)
- `cargo test -p wisp-net --lib` (Mac, planner tests)
- `cargo test -p wisp-net --tests -- --ignored` (VM as root, integration tests)
- `cargo clippy -p wisp-net -p wisp -p wisp-cli --all-targets -- -D warnings`
- `cargo fmt --check`

After dispatch D: run the demo on the VM as root. The done-bar is a successful curl response from a running nginx via the published port.

## Risks

Spec calls them out; reproduce here for the implementer:

- iptables coexistence: use isolated `WISP-<net-id>` chains so cleanup is `iptables -t <table> -F WISP-<net-id> && -X WISP-<net-id>`.
- OrbStack network model: tests use unique bridge names + subnets to avoid collisions across runs.
- Loopback DNAT: don't forget the `OUTPUT` chain rule for `127.0.0.1:<port>` clients.
- Veth GC on agent restart: documented as a startup `Runtime::reconcile` step but punted to 0.4 if too involved.
- ip / iptables-nft availability: validate at `Network::ensure` time with a clear error.

## Dispatch sequence

| Dispatch | Steps | Notes |
|---|---|---|
| A | A1 / A2 / A3 / A4 | Pure logic + planners. Mac-friendly. Lowest risk. |
| B | B1 / B2 / B3 | Real Linux syscalls. Sonnet review at the end. Tests on the VM. |
| C | C1 / C2 / C3 | Modifies wisp-runtime lifecycle. Highest risk. Sonnet review. |
| D | D1 / D2 | Tests + demo + docs. Demo verification gates the dispatch. |

After dispatch D, the done-bar is `curl http://127.0.0.1:18080/` returning nginx HTML from the OrbStack VM.

## Open questions during implementation

- **NetworkSpec home: `wisp` or `wisp-net`?** Spec recommends `wisp` so the runtime doesn't pull in wisp-net unconditionally. Decision in dispatch C1.
- **rtnetlink vs `ip` CLI?** 0.3 ships with `ip` shell-out for simplicity. Dispatch B1 may revisit if subprocess cost shows up.
- **MAC prefix.** `02:42:` (matches docker) or `02:50:43:` (wisp-specific)? Default to `02:42:` for compatibility with anyone debugging from a docker mental model. Dispatch B2.
- **Subprocess concurrency.** Each `ip` / `iptables` invocation is a fork/exec. If multiple containers attach simultaneously, are there serialization concerns? Default: yes, hold an attach-lock per network during attach (mutex on `<state-dir>/networks/<net>/.lock`). Dispatch C2.
- **Does the runtime own the IPAM or wisp-net?** Spec implies wisp-net owns its own state. So the IPAM file lives at `<state-dir>/networks/<net-name>/allocs.json`. Confirmed in dispatch A2.
