# wisp-net

## What

`wisp-net` is the local-fabric networking layer for the wisp container
runtime: bridge management, veth-pair lifecycle, IPv4 IPAM, iptables
NAT/DNAT planning + apply, and `/etc/resolv.conf` + `/etc/hosts`
templating. Runs as a sibling crate to `wisp` and is consumed by
`wisp-cli` through the [`wisp::NetworkAttacher`] trait so the runtime
crate stays free of `wisp-net` deps.

## Why

Phase 0.1 of wisp ran with a per-container netns and no link to the
host's network. Containers had no IP, no route, no DNS. Phase 0.3
gives the operator a real network model: a wisp-managed bridge per
named network, veth pairs into each container's netns, IPAM with
persistence, NAT for outbound traffic, and DNAT for published ports.
Plus the resolv / hosts files most images expect on first boot.

## Status

Phase 0.3, alpha. Demo runs end-to-end on the OrbStack `wisp` VM as
root. IPv4-only; cross-host overlay, CNI, and live reconfig are
deferred. cargo build + clippy clean on Mac and arm64 Linux.

## Run the demo

The done bar: nginx in a wisp container, port published, `curl` from
the host returns the nginx welcome page.

```sh
orb -m wisp -u root bash
PATH=/home/dirdmaster/.cargo/bin:$PATH
cd /Users/dirdmaster/Projects/isengard/.worktrees/next

# step 1: create the bridge network
cargo run -p wisp-cli --release -- net create app

# step 2: pull + run nginx with port published
WISP_STATE_DIR=/var/lib/wisp-demo cargo run -p wisp-cli --release -- \
    run --image docker.io/library/nginx:alpine \
    --network app --port 18080:80 --detach --id web

# step 3: curl the published port from the host (the VM)
curl -s -i http://127.0.0.1:18080/ | head -20
# expected: HTTP/1.1 200 OK + nginx welcome HTML

# step 4: cleanup
WISP_STATE_DIR=/var/lib/wisp-demo cargo run -p wisp-cli --release -- \
    kill web --signal SIGKILL
WISP_STATE_DIR=/var/lib/wisp-demo cargo run -p wisp-cli --release -- \
    delete web --force
WISP_STATE_DIR=/var/lib/wisp-demo cargo run -p wisp-cli --release -- \
    net rm app
```

The library-only demo (no `wisp-cli`, no image pull) lives at
`examples/attach-busybox.rs`:

```sh
PATH=/home/dirdmaster/.cargo/bin:$PATH \
    cargo run -p wisp-net --example attach-busybox
# demo network: name=wisp-demo bridge=wbr-wisp-demo subnet=10.111.0.0/24 ...
# bridge ensured: wbr-wisp-demo
# iptables network rules applied: 5 rule(s)
# unshare child pid=...
# allocated ip=10.111.0.2 for demo-busybox
# veth pair: host=wveth-h-... ctr=wveth-c-...
# veth attached + addr/route configured inside ns
# eth0 visible inside ns
# ping 10.111.0.1 succeeded from inside ns
# demo OK: bridge + veth + iptables + IPAM round-trip clean
```

## Public API

```rust
use wisp_net::{Network, StaticBitmapIpam, Ipam};

let net = Network::new("app", "10.83.0.0/24".parse()?)?;
wisp_net::bridge::ensure(&net)?;

let net_rs = wisp_net::iptables::plan_for_network(&net);
wisp_net::iptables::apply(&net_rs)?;

let mut ipam = StaticBitmapIpam::new(&state_dir);
let ip = ipam.alloc(&net.name, net.subnet, net.gateway, "ctr-1")?;

let pair = wisp_net::veth::create_pair()?;
wisp_net::veth::attach_to_ns(&pair, container_pid, &net.bridge,
    ip, net.subnet.prefix_len(), net.gateway)?;

let attach_rs = wisp_net::iptables::plan_for_attachment(
    &net, ip, "ctr-1", &ports);
wisp_net::iptables::apply(&attach_rs)?;
```

| Module | Description |
|--------|-------------|
| `bridge::Network` | Wisp-managed bridge: name + subnet + derived gateway + bridge interface name (`wbr-<name>`, IFNAMSIZ-truncated). |
| `bridge::ensure / delete / list_wisp_bridges` | Idempotent bridge lifecycle. `ensure` toggles `route_localnet=1` for loopback DNAT. |
| `veth::create_pair` | New veth pair with random `wveth-h-<hex> / wveth-c-<hex>` names. |
| `veth::attach_to_ns` | Move container side into PID's netns, rename to `eth0`, configure addressing + default route, enslave host side to the bridge. |
| `veth::delete` | Tolerant host-side veth removal. Container side rides with the namespace. |
| `StaticBitmapIpam` | Lowest-free IPv4 allocator persisted under `<state>/<network>/allocs.json`. Idempotent for same `bundle_id`. |
| `iptables::plan_for_network / plan_for_attachment` | Pure-logic [`RuleSet`] builders with grep-friendly `wisp:<scope>:<purpose>` comments. |
| `iptables::apply / revoke` | Walk the [`RuleSet`]'s creates / deletes; revoke is tolerant of missing rules so detach is idempotent. |
| `host_nameservers` | Read host `/etc/resolv.conf`, filter loopback nameservers (useless to a containerised app on a different bridge subnet). |
| `write_resolv_conf / write_hosts` | Render `/etc/resolv.conf` + `/etc/hosts` into a rootfs at attach time, before the child pivot_roots. |

## Disk layout

`StaticBitmapIpam` is the only stateful component; everything else
lives in kernel tables and is enumerable via `ip link` / `iptables-save`.

```text
<state-dir>/networks/<network-name>/
  allocs.json    # { "version": 1, "allocs": { "<container-id>": "<ipv4>" } }
```

The IPAM file is rewritten atomically via
`tempfile::NamedTempFile::persist`. Single-process writer assumption.

## ABI choices

- **Bridge name `wbr-<network>`** truncated to 15 bytes (Linux
  `IFNAMSIZ - 1`). The `wbr-` prefix is the discriminator
  `bridge::list_wisp_bridges()` keys off so wisp can find its own
  bridges without an external registry.
- **Veth name `wveth-h-<hex>` / `wveth-c-<hex>`** with a 6-hex random
  suffix. Both fit inside IFNAMSIZ. Collisions retry up to 5 times.
- **IPv4 only.** `Network` carries an `ipnet::Ipv4Net` and IPAM
  emits `Ipv4Addr`. IPv6 is a deliberate phase 0.4+ item.
- **iptables rules tagged with `wisp:<scope>:<purpose>` comments.**
  Cleanup falls back to `iptables-save | grep wisp:<scope>` if a
  revoke ever drifts. The scope is the network name for
  `plan_for_network` and the container id for `plan_for_attachment`.
- **route_localnet=1 + loopback-snat MASQUERADE.** Without these,
  `curl 127.0.0.1:<host-port>` (host-local traffic going through
  OUTPUT-chain DNAT) gets ARP'd from `127.0.0.1` onto the bridge and
  the container can't reply. The `bridge::ensure` path sets the
  per-bridge sysctl; the `plan_for_network` path emits the
  matching MASQUERADE rule.
- **`NetworkAttacher` trait.** Lives in the `wisp` crate so the
  runtime can call into a network provider without depending on
  `wisp-net`. The production impl is `WispNetAttacher` in
  `crates/wisp-cli/src/net_attacher.rs`; the tests in
  `crates/wisp/tests/runtime_with_network.rs` ship a parallel impl
  that exercises the same trait shape.

## Known limitations

- **No IPv6.** Single-stack IPv4. The protocol surfaces (`Network`,
  `PortPublish`, `NetworkSpec`) leave room for an additive IPv6
  upgrade without a schema break, but the planners + IPAM do not.
- **No cross-host networking.** wisp-net is a single-host bridge
  layer. Cross-host overlays go through the existing Isengard
  agent's networking adapter, not through wisp.
- **No CNI.** Plumbing is direct via `ip` + `iptables` shell-out.
  CNI is a v1.x line item if a real ecosystem need shows up.
- **No live reconfig.** Adding a port to a running container is
  delete + recreate.
- **No `--network host`, no `--network none` knob.** Default is
  no-net (matches Phase 0.1); explicit `--network <name>` opts in.
- **No built-in DNS server.** Resolution uses the host's nameservers
  via `/etc/resolv.conf` (filtering loopback). Container-to-container
  DNS by name is not provided; operators use IP addresses or wire up
  their own resolver.
- **OrbStack VM constraints.** The integration tests assume root +
  Linux + a working `ip` / `iptables-nft` toolchain. Mac dev path
  covers planners only.
- **wisp-image's default capability set is too pared-down for nginx.**
  The runtime drops everything except `KILL` + `NET_BIND_SERVICE`.
  `wisp-cli`'s `cmd_run_image` post-patches the synthesised
  `config.json` to grant `CHOWN + SETUID + SETGID + DAC_OVERRIDE +
  FOWNER + SETPCAP` so real entrypoints (nginx, postgres, etc.) can
  drop privilege from root to a service user. The proper fix is to
  extend `wisp_image::ConfigOverrides` with a capability override;
  tracked for a follow-up.

## Roadmap

- 0.4: Isengard agent integration. Persisted networks across agent
  restarts. Probably IPv6 + a tiny container-to-container DNS shim.
- 0.5: Optional CNI compatibility shim if there's demand. Auth +
  multi-tenant networks. Live reconfig.

For the 0.3 spec + plan see
[`docs/superpowers/specs/2026-05-09-wisp-phase-0-3-networking-design.md`](../../docs/superpowers/specs/2026-05-09-wisp-phase-0-3-networking-design.md)
and
[`docs/superpowers/plans/2026-05-09-wisp-phase-0-3-networking.md`](../../docs/superpowers/plans/2026-05-09-wisp-phase-0-3-networking.md).

## License

MIT (matches the workspace).
