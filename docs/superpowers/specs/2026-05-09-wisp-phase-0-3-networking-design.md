# Wisp Phase 0.3: `wisp-net` MVP (design)

Status: proposed
Phase: v0.4 foundation, wisp 0.3
Author: 2026-05-09
Depends on: wisp 0.1 (runtime), wisp 0.2 (image pulling)

## Problem

Wisp 0.1 puts each container in its own network namespace, but doesn't wire it up. The container has `lo` and nothing else: no DNS, no outbound connectivity, no inter-container routing, no port publishing. Real workloads need:

- Outbound: container hits `https://api.example.com/...` and the response comes back. NAT through the host.
- Inter-container: postgres listens on 5432, the web container connects to it by service name. Both on a shared bridge.
- Inbound: container listens on 8080, host publishes it on `:80` so external traffic reaches it.
- DNS: container's `getaddrinfo("api.example.com")` resolves. Inside the cluster, `getaddrinfo("postgres")` resolves to the postgres container's IP.

Phase 0.3 owns the first three. DNS is a cooperating-module question (the agent's existing `isengard-controller`-driven DNS and Pingora layer already exist for v0.3.x); 0.3 wires the container side so the agent can plug those into the synthesised network.

Without this, wisp can run a busybox `echo` (current 0.2 demo) but can't run `nginx`, `redis`, `postgres`, or anything that talks to anything.

## Goal

Land `crates/wisp-net/` plus the agent-side hooks the runtime calls during create/start/delete. Also, an extension to `wisp-cli` so an operator can debug a container's networking without setns dancing by hand.

After this phase:

```
wisp run --image nginx:alpine --port 8080:80 web
curl http://127.0.0.1:8080/                  # nginx default page reaches us
docker run -d --name pg postgres:17
wisp run --image alpine:3.19 --network <existing-bridge> debug
# inside debug: ping pg by IP; if /etc/hosts is wired, ping pg by name
```

Phase 0.3 done bar: a 2-container demo where container A serves on `:8080` (published to the host as `:18080`), container B in the same network namespace name-resolves A and curls it. Both containers have outbound internet via NAT. `curl http://127.0.0.1:18080/` from the host hits container A's nginx and gets HTML back.

## Non-goals (Phase 0.3)

- IPv6. v4 only for the MVP. Most homelab traffic is v4-NAT'd anyway.
- Cross-host networking. Bridge per agent only. Cross-host overlay (tailscale, wireguard) is Phase 0.4+ via the existing `networking-tailscale` adapter on the agent; wisp-net just provides the local fabric.
- Custom CNI plugins. Inline bridge / veth / iptables logic. CNI compatibility is a possible 0.5 wedge if real demand surfaces.
- IPAM beyond a single bridge with static-ish allocation. No DHCP. No external IPAM coordination.
- Egress IP assignment per container. Containers share the agent's egress IP via NAT.
- nftables. iptables (legacy or nf_tables backend, whatever the host's `iptables` wrapper uses) only. The systemd-default on Ubuntu 24.04 routes through nftables under the hood; we use the `iptables-nft` shim CLI which is binary-compatible.
- DNS server inside the agent. Containers get `/etc/resolv.conf` populated either from the host's resolver or from the agent's own listening DNS (controlled by 0.3's "resolv-source" knob); the actual DNS server is the agent's existing controller-DNS (out of scope for this spec).
- Live network reconfiguration of running containers. Network is configured at create-time; changing it requires recreate.

## Design

### Crate layout

```
crates/
  wisp-net/
    Cargo.toml          # NO isengard-* deps; depends on wisp + wisp-image (for ContentStore type? no, doesn't need it)
    src/
      lib.rs
      error.rs          # WispNetError
      bridge.rs         # bridge create / delete / list
      veth.rs           # veth pair create / move-into-ns
      ipam.rs           # bridge-scoped IP allocation
      iptables.rs       # NAT + port-forward rule install / remove
      resolv.rs         # /etc/resolv.conf templating
      hosts.rs          # /etc/hosts templating
      ns.rs             # netns helpers (open by pid, run-in-ns)
      lifecycle.rs      # NetworkAttachment lifecycle: attach / detach / configure
    examples/
      attach-busybox.rs
    tests/
      bridge_lifecycle.rs       # ignored, root only
      veth_pair_into_ns.rs      # ignored, root only
      iptables_nat_round_trip.rs # ignored, root only
```

### Public API

```rust
pub struct Network {
    pub name: String,                   // "wisp-default", "myapp-net"
    pub bridge: String,                 // "wbr-<name>" derived
    pub subnet: ipnet::Ipv4Net,         // 10.83.0.0/24 default
    pub gateway: std::net::Ipv4Addr,    // 10.83.0.1
}

impl Network {
    /// Create the bridge, assign the gateway IP, set up the iptables
    /// FORWARD + MASQUERADE rules. Idempotent; existing bridge with the
    /// same name + subnet is a no-op.
    pub fn ensure(&self) -> Result<(), WispNetError>;

    /// Remove iptables rules + delete the bridge. Errors if any veth is
    /// still attached (caller must detach all containers first).
    pub fn delete(&self) -> Result<(), WispNetError>;

    /// Report state for debugging.
    pub fn inspect(&self) -> Result<NetworkInspect, WispNetError>;
}

pub struct NetworkAttachment {
    pub container_id: String,
    pub network: String,
    pub ipv4: std::net::Ipv4Addr,
    pub veth_host: String,         // "wveth-h-<6 hex>"
    pub veth_container: String,    // "wveth-c-<6 hex>" (inside ns)
    pub mac: macaddr::MacAddr6,
    pub ports: Vec<PortPublish>,
}

pub struct PortPublish {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,    // Tcp or Udp
    pub host_ip: std::net::IpAddr, // 0.0.0.0 default
}

pub fn attach(
    net: &Network,
    container_id: &str,
    ipam: &mut dyn Ipam,
    container_ns_pid: u32,
    ports: Vec<PortPublish>,
) -> Result<NetworkAttachment, WispNetError>;

pub fn detach(att: &NetworkAttachment, ipam: &mut dyn Ipam) -> Result<(), WispNetError>;

/// Write /etc/resolv.conf and /etc/hosts inside the container's rootfs
/// before pivot_root. Called by wisp-runtime during the start sequence
/// IF the operator opted into a network. Honors `resolv_source`:
///   - HostCopy: copy the host's /etc/resolv.conf
///   - Static(Vec<IpAddr>): write nameserver lines for each
///   - None: don't write resolv.conf (operator handles it)
pub fn populate_resolv_and_hosts(
    rootfs: &Path,
    spec: &NetworkSpec,
    attachment: &NetworkAttachment,
) -> Result<(), WispNetError>;
```

### Bridge management (bridge.rs)

Operations via `nix::ifaddrs` for read-side and `rtnetlink` (or shelling out to `ip` for the MVP) for write-side. The plan: shell out to `ip` for 0.3 (simpler, stable CLI; performance penalty is fine for create/delete/start ops). 0.5+ revisit with native rtnetlink.

```
ip link add name wbr-<net> type bridge
ip addr add <gateway>/<prefix> dev wbr-<net>
ip link set wbr-<net> up
```

For delete: `ip link delete wbr-<net> type bridge`. Errors if any interface is enslaved; cleanup is the operator's job in 0.3 (5+ revisit auto-detach).

`bridge::list_wisp_bridges() -> Vec<String>` walks `/sys/class/net/` looking for `wbr-*` prefixes (so we don't trip on docker0, br0, etc.).

### IP allocation (ipam.rs)

Trait + simple bitmap impl:

```rust
pub trait Ipam: Send {
    fn alloc(&mut self, network: &Network) -> Result<Ipv4Addr, WispNetError>;
    fn release(&mut self, network: &Network, addr: Ipv4Addr) -> Result<(), WispNetError>;
    fn list(&self, network: &Network) -> Result<Vec<Ipv4Addr>, WispNetError>;
}

pub struct StaticBitmapIpam {
    state_path: PathBuf,         // <state-dir>/networks/<net-name>/allocs.json
}

impl Ipam for StaticBitmapIpam { ... }
```

Simple algorithm: for a /24 subnet (default 10.83.0.0/24), the gateway takes .1, broadcast is .255, container range is .2 through .254 (253 slots). State is a JSON file mapping container_id -> ipv4. Picks the lowest free address. Persistence is atomic write on each alloc/release.

253 containers per network is the MVP cap. For real homelab fleets this is plenty (the deferred isengard fleet is ~5 hosts each running ~20 containers). Bigger subnets are an operator-config knob in 0.5.

### Veth pair (veth.rs)

```
ip link add wveth-h-<6hex> type veth peer name wveth-c-<6hex>
ip link set wveth-h-<6hex> master wbr-<net>
ip link set wveth-c-<6hex> netns <pid>
ip link set wveth-h-<6hex> up
nsenter -t <pid> -n ip link set wveth-c-<6hex> name eth0
nsenter -t <pid> -n ip link set lo up
nsenter -t <pid> -n ip link set eth0 up
nsenter -t <pid> -n ip addr add <container-ip>/<prefix> dev eth0
nsenter -t <pid> -n ip route add default via <gateway>
```

`nsenter -t <pid> -n` runs the next command in the target's network namespace. The container's pid is the value `wisp::Runtime::start` already records. wisp-net needs the runtime to expose this PID; we add `Runtime::container_pid(&id) -> Option<u32>` if it's not already there.

Random hex suffixes avoid collisions; if a name's taken, retry with a new suffix.

### iptables rules (iptables.rs)

Three rule groups, all anchored on a wisp-specific chain so cleanup is easy:

1. **MASQUERADE for outbound NAT** on the bridge subnet:
   ```
   iptables -t nat -A POSTROUTING -s <subnet> ! -o <bridge> -j MASQUERADE
   ```

2. **FORWARD allow** between the bridge and the rest of the world:
   ```
   iptables -A FORWARD -i <bridge> -o <bridge> -j ACCEPT      # intra-bridge
   iptables -A FORWARD -i <bridge> ! -o <bridge> -j ACCEPT    # outbound
   iptables -A FORWARD ! -i <bridge> -o <bridge> -m state --state RELATED,ESTABLISHED -j ACCEPT  # inbound responses
   ```

3. **Per-port DNAT for `--port` published ports**:
   ```
   iptables -t nat -A PREROUTING  -p tcp --dport <host-port> -j DNAT --to-destination <container-ip>:<container-port>
   iptables -t nat -A OUTPUT      -p tcp -d 127.0.0.1 --dport <host-port> -j DNAT --to-destination <container-ip>:<container-port>
   iptables -A FORWARD -p tcp -d <container-ip> --dport <container-port> -j ACCEPT
   ```

To make cleanup tractable, all rules go into a `WISP-<network-id>` chain where possible (POSTROUTING/PREROUTING jump to it). On `Network::delete` we flush + delete the chain. Per-container DNAT rules carry a `--comment "wisp:<container-id>"` so we can grep + delete on detach.

The implementation talks to iptables through the `iptables` (or `iptables-nft`) command-line. Each rule add/delete is its own subprocess; bulk operations build a list and use `iptables-restore` for atomicity.

### resolv.conf + /etc/hosts (resolv.rs, hosts.rs)

Templating, called pre-pivot from within the cloned child (it has the rootfs mounted but not yet pivot_rooted; resolv.conf is just a file write).

`/etc/resolv.conf` policy:
- `ResolvSource::HostCopy`: read `/etc/resolv.conf`, filter local-only nameservers (127.0.0.x without a forwarder), write to rootfs.
- `ResolvSource::Static(vec)`: write nameserver lines.
- `ResolvSource::None`: skip; operator handles via mounts.

`/etc/hosts` minimal template (always written unless `ResolvSource::None`):
```
127.0.0.1 localhost
::1 localhost ip6-localhost ip6-loopback
<container-ip> <container-id>
```

If wisp-net later integrates with the agent's controller-DNS (Phase 0.4 territory), additional entries can be templated for known sibling services. Phase 0.3 stops at the local-bind.

### Network-spec extension to wisp-runtime

Today's spec validation rejects bundles that don't declare exactly the five required namespaces. Adding networking doesn't change that; the network namespace is already there. What we add is operator-controllable network attachment OUTSIDE the spec:

- `Runtime::create(id, bundle)` gains an optional `network: Option<NetworkSpec>` parameter (or new `create_with_network` method).
- `NetworkSpec { network_name, ports, resolv_source }`.
- During `Runtime::start`, after `clone3` returns the child PID, the parent (which already attaches cgroup) ALSO calls `wisp_net::attach(...)`. The child's `signal_ready` waits for the network to be configured (parent signals back through a second pipe before the child execs the entrypoint).

The pre-exec sync gets a second hop:
```
parent: cgroup add_pid
parent: wisp_net::attach
parent: signal_ready_to_exec
child: wait_ready_to_exec
child: exec
```

This means the child's exec'd PID 1 sees the network already plumbed in.

`Runtime::delete` calls `wisp_net::detach` before tearing down the cgroup.

For containers without a network attachment (the busybox demo): no behavior change; the network namespace stays empty as today.

### wisp-cli changes

```
wisp net create <name> [--subnet 10.83.0.0/24]    # Network::ensure
wisp net list                                      # bridge::list_wisp_bridges + state
wisp net rm <name>                                 # Network::delete
wisp net inspect <name>                            # NetworkInspect
wisp run ... --network <name> --port 80:8080      # NetworkSpec on run
wisp inspect <id>                                  # exposed network attachment in state
```

Default network: if `--network` is omitted but `--port` is provided, auto-create `wisp-default` (10.83.0.0/24) on demand and attach there.

## Test strategy

### Unit tests (Mac OK)

- `bridge::commands_for_create` returns the right `ip link add`, `ip addr add`, `ip link set up` invocations for a given Network.
- `iptables::rule_set_for_network` returns the correct text-form rules.
- `ipam::StaticBitmapIpam::alloc + release` against tempdir-backed state.
- `resolv::render_for_host_copy` filters 127.0.0.x correctly.
- `hosts::render_minimal` produces the expected three-line file.

The plan-but-don't-execute pattern is the same as Phase 0.1's `mount::plan_mounts`: side-effect-free planners are testable; the actual `nix`/CLI calls happen in `setup_*`/`apply_*` functions gated on Linux + root.

### Integration tests (Linux as root, on the OrbStack VM)

- `bridge_lifecycle.rs`: ensure -> list shows it -> delete -> list shows it gone.
- `veth_pair_into_ns.rs`: clone3 a child to get a netns; attach a veth pair; assert the container side has the named iface.
- `iptables_nat_round_trip.rs`: configure a bridge + container; in the container, curl `http://example.com/`; assert the request went out (use the OrbStack VM's egress).
- `ports_publish.rs`: container nginx on :80; bridge with publish 8080:80; from the VM (as root): `curl http://127.0.0.1:8080/`; assert the response is nginx default.
- `inter_container_curl.rs`: two containers attached to the same network; container B curls container A's published port.

OrbStack containers + VMs by default share the host's network in unusual ways (orb's "shared mode"). Validate the demo on a setup that doesn't conflict with orb's own NAT (the OrbStack VM has its own kernel + bridge; iptables there is a clean slate).

### Demo

```
orb -m wisp -u root bash
PATH=/home/dirdmaster/.cargo/bin:$PATH
cd /Users/dirdmaster/Projects/isengard/.worktrees/next

cargo run -p wisp-cli -- net create app
cargo run -p wisp-cli -- run --image nginx:alpine --network app --port 18080:80 web
# in another shell:
curl http://127.0.0.1:18080/
# expected: nginx welcome HTML
```

Two-container demo (stretch):
```
cargo run -p wisp-cli -- run --image nginx:alpine --network app web --detach
cargo run -p wisp-cli -- run --image alpine:3.19 --network app probe \
  /bin/sh -c 'apk add --no-cache curl && curl http://web/'
```

The second container needs hostname resolution for `web`, which falls out of `/etc/hosts` templating once we extend hosts to include all sibling containers in the same network. That's a stretch for 0.3.

## Risks

- **iptables ordering vs the agent's existing rules.** The Isengard agent is running in the VM (or will be) with its own iptables rules (Pingora's port mapping for the v0.3 wildcard cert flow). We add into wisp-specific chains and avoid the default chains. Concrete pattern: `iptables -t nat -N WISP-app && iptables -t nat -A POSTROUTING -j WISP-app`. Cleanup is `iptables -t nat -F WISP-app && iptables -t nat -X WISP-app`.
- **OrbStack's network model.** Orb VMs share the Mac's network in shared mode; VM-local iptables changes are real and persist. Test isolation: each integration test uses a unique bridge name + subnet.
- **MAC address generation.** Random 46-bit suffix into `02:42:` prefix (matches docker's). Document the ABI choice.
- **`ip` CLI dependency.** Ubuntu 24.04 ships `iproute2` + `iptables-nft`. Document and validate at `Network::ensure` time. Error loudly with install instructions if missing.
- **Race between netns set + addr add.** `nsenter -t <pid> -n ip ...` invocations are subprocesses; if the container's PID dies before we finish wiring, we leak the host-side veth. Mitigation: catch errors during attach; if any step fails, delete the host-side veth in the rollback path.
- **Bridge MTU.** The OrbStack VM's eth0 MTU is 1500; bridges default to 1500; should match. Veth peers default to MTU 1500. No explicit setting needed for 0.3.
- **Inbound DNAT loopback.** Without the OUTPUT chain DNAT, `curl 127.0.0.1:18080` from the host doesn't reach the container (loopback packets bypass PREROUTING). The OUTPUT rule fixes this; document why.

## Stretch goals (only if 0.3 lands fast)

- **`/etc/hosts` cross-container population**: write all sibling-container names + IPs into each container's hosts file at attach time. Fragile (needs reattach on new container join); works for static test setups.
- **Bridge isolation between networks**: by default, containers in network `foo` can't talk to network `bar`. Implement via a chain that drops cross-bridge traffic at FORWARD.
- **`wisp net connect <ctr-id> <network>`**: attach an already-running container to a second network. Requires opening the netns and adding a second veth.
- **IPv6**: dual-stack with a /64 ULA per network.

## Out of scope, explicitly

- CNI plugin compatibility.
- Calico / flannel / cilium-style overlay drivers.
- BGP peering, encryption, mesh.
- Ingress controllers as part of wisp; that's the Pingora story already in Isengard.

## Open questions

- **State persistence on agent restart.** Bridges + iptables rules survive; the IPAM allocs file does too. But what about veth pairs? When the host reboots, the netns goes away, the host-side veth is dangling. Mitigation: at agent boot, walk the IPAM file, check each `/proc/<pid>/ns/net` for the recorded PID; if the PID is gone or its ns inode differs, garbage-collect that allocation + the host-side veth. Phase 0.3 implements this cleanup as a `Runtime::reconcile` startup step.
- **Where the MAC address lives.** `macaddr::MacAddr6` field on NetworkAttachment for now. If anyone wants to fix the MAC for predictable WoL, we add an operator override.
- **DNS server inside the agent.** The Isengard controller has DNS already; for wisp-as-orchestrator-runtime we can just template the controller's IP into resolv.conf and let it answer. Phase 0.4 (agent integration) is when this gets wired.
- **`--network host`.** Operator wants the container to share the host's netns directly. Phase 0.3 doesn't support this; document the gap. Phase 0.5 if real demand surfaces.
- **`--network none`.** Today's default (no network attachment) is effectively this; we can make it explicit so operators can opt out cleanly.
