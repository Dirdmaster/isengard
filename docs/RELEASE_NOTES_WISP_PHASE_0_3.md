# Wisp Phase 0.3: networking (`wisp-net`)

> Phase 0.3 of wisp. Branch: `wisp/phase-0-1`. Builds on 0.1's runtime + 0.2's image pulling; not in `next` yet, no operator-facing change for v0.3.x users.

## What this is

`crates/wisp-net/` is the local-fabric networking library that sits next to `crates/wisp/` (the runtime). Operator picks a network name; the crate stands up a wisp-managed bridge (`wbr-<name>`), allocates an IPv4 from a per-network bitmap, drops a veth pair into the container's netns, applies iptables NAT + DNAT rules, and templates `/etc/resolv.conf` + `/etc/hosts` into the bundle's rootfs before the child pivot_roots.

`wisp` deliberately does not depend on `wisp-net`: the runtime owns the data shape (`NetworkSpec`, `PortPublish`, `NetworkAttachmentRecord`) and exposes a `NetworkAttacher` trait. `wisp-cli` provides the production attacher (`WispNetAttacher` in `crates/wisp-cli/src/net_attacher.rs`); the runtime test in `crates/wisp/tests/runtime_with_network.rs` ships a parallel impl with the same shape.

`wisp-cli` grows two surfaces: `--network <name>` + `--port <[host_ip:]host:container[/proto]>` flags on `wisp run` (port flag repeatable, default protocol tcp, default host_ip 0.0.0.0); and a `wisp net create | ls | rm | inspect` subcommand group for managing bridges.

This is the third of four phases on the way to v0.4. 0.1 was the runc-equivalent (run a hand-prepared bundle); 0.2 was image pulling; 0.3 is networking; 0.4 wires it into the Isengard agent.

## What's NOT in 0.3

- **No IPv6.** Single-stack IPv4. The data shape carries `IpAddr` so an additive IPv6 upgrade will not break state.json, but the planners + IPAM are v4 only.
- **No cross-host networking.** wisp-net is a single-host bridge layer. Cross-host overlays continue to go through the Isengard agent's networking adapter, not wisp.
- **No CNI.** Direct shell-out to `ip` + `iptables-nft`. CNI is a v1.x line item.
- **No `--network host` / `--network none`.** Default is no-net (matches Phase 0.1); explicit `--network <name>` opts in.
- **No live reconfig.** Adding a port to a running container is delete + recreate. Network attach happens once between `clone3` and the container's first `execve`.
- **No built-in DNS server.** Resolution uses the host's nameservers (via `/etc/resolv.conf` copied at attach time, filtering loopback). Container-to-container DNS by name is not provided.
- **No registry of networks on disk.** `wisp net create app --subnet <cidr>` stands up a bridge; the subnet is implicit in the bridge's address. `wisp net rm` and `--network <name>` on `wisp run` use the default `10.83.0.0/24` if they need to derive a gateway. Pre-creating the network with the desired subnet is the workaround. A 0.4-era network registry is the proper fix.

## Done bar

`wisp net create app && wisp run --image docker.io/library/nginx:alpine --network app --port 18080:80 --detach --id web && curl http://127.0.0.1:18080/` from inside the OrbStack `wisp` VM as root prints HTTP/1.1 200 OK and the nginx welcome HTML. Verified end-to-end on Ubuntu 24.04, kernel 6.x, cgroup v2, arm64 (the same VM that proves out 0.1 + 0.2).

Verbatim demo session:

```text
$ orb -m wisp -u root bash
$ PATH=/home/dirdmaster/.cargo/bin:$PATH
$ cd /Users/dirdmaster/Projects/isengard/.worktrees/next

=== step 1: net create ===
created: app (bridge wbr-app subnet 10.83.0.0/24 gateway 10.83.0.1)

=== step 2: run nginx ===
/docker-entrypoint.sh: /docker-entrypoint.d/ is not empty, will attempt to perform configuration
/docker-entrypoint.sh: Looking for shell scripts in /docker-entrypoint.d/
/docker-entrypoint.sh: Launching /docker-entrypoint.d/10-listen-on-ipv6-by-default.sh
10-listen-on-ipv6-by-default.sh: info: Getting the checksum of /etc/nginx/conf.d/default.conf
10-listen-on-ipv6-by-default.sh: info: Enabled listen on IPv6 in /etc/nginx/conf.d/default.conf
/docker-entrypoint.sh: Sourcing /docker-entrypoint.d/15-local-resolvers.envsh
/docker-entrypoint.sh: Launching /docker-entrypoint.d/20-envsubst-on-templates.sh
/docker-entrypoint.sh: Launching /docker-entrypoint.d/30-tune-worker-processes.sh
/docker-entrypoint.sh: Configuration complete; ready for start up
web

=== step 3: curl ===
HTTP/1.1 200 OK
Server: nginx/1.29.8
Date: Sat, 09 May 2026 23:31:33 GMT
Content-Type: text/html
Content-Length: 896
Last-Modified: Sat, 09 May 2026 23:31:31 GMT
Connection: keep-alive
ETag: "69ffc3d3-380"
Accept-Ranges: bytes

<!DOCTYPE html>
<html>
<head>
<title>Welcome to nginx!</title>
<style>
html { color-scheme: light dark; }
body { width: 35em; margin: 0 auto;
font-family: Tahoma, Verdana, Arial, sans-serif; }
</style>
</head>

=== step 4: kill + delete ===
removed: app
```

The library-only `attach-busybox` example proves out the same primitives without the image pull or the runtime:

```text
$ cargo run -p wisp-net --example attach-busybox
demo network: name=wisp-demo bridge=wbr-wisp-demo subnet=10.111.0.0/24 gateway=10.111.0.1
bridge ensured: wbr-wisp-demo
iptables network rules applied: 5 rule(s)
unshare child pid=53673
allocated ip=10.111.0.2 for demo-busybox
veth pair: host=wveth-h-7db595 ctr=wveth-c-7db595
veth attached + addr/route configured inside ns
eth0 visible inside ns
ping 10.111.0.1 succeeded from inside ns
demo OK: bridge + veth + iptables + IPAM round-trip clean
```

Test counts:

- `cargo test -p wisp-cli` (Mac + VM): 20 unit tests; covers the new `parse_port_publish` shapes (`HOST:CONTAINER`, `HOST_IP:HOST:CONTAINER`, `/tcp`, `/udp`) plus rejection cases plus the existing CLI-shape `Cli::command().debug_assert()`.
- `cargo test -p wisp` (Mac): 67 unit tests, all green.
- `cargo test -p wisp-net` (Mac): 46 unit tests, all green. Mac runs the planner + IPAM + sanitization tests; the four `#[ignore]` integration tests (`bridge_lifecycle`, `veth_pair_into_ns`, `iptables_nat_round_trip`, `ports_publish`) need root + Linux and pass on the VM.
- `cargo test -p wisp` (VM, with `--ignored`): the `runtime_with_network.rs` end-to-end driving `Runtime::create_with_network` + `start_with_attacher` + `delete_with_attacher` was green at the close of dispatch C; D doesn't add new tests at this layer.

## Public API

```rust
use wisp::{NetworkSpec, PortProtocol, PortPublish, ResolvSource, Runtime};

let net_spec = NetworkSpec::new("app").with_ports(vec![
    PortPublish::v4_any(18080, 80, PortProtocol::Tcp),
]);

let rt = Runtime::new(&state_dir)?;
let handle = rt.create_with_network("web", &bundle, net_spec)?;

let mut attacher = WispNetAttacher::new(net, &ipam_dir);
rt.start_with_attacher(&handle.id, &mut attacher)?;
// ... container is now running with eth0, IP, default route, port published ...
rt.delete_with_attacher(&handle.id, true, &mut attacher)?;
```

Or via the CLI:

```sh
wisp net create app --subnet 10.83.0.0/24
wisp run --image nginx:alpine --network app --port 18080:80 --detach --id web
curl http://127.0.0.1:18080/
wisp delete web --force        # routes through delete_with_attacher automatically
wisp net rm app
```

## Disk layout

```text
<state-dir>/
  containers/<id>/state.json     # now carries optional `network_spec` + `network_attachment`
  networks/<network-name>/
    allocs.json                  # StaticBitmapIpam: { version: 1, allocs: { <id>: <ipv4> } }
  bundles/<id>/                  # Phase 0.2 unchanged
  images/                        # Phase 0.2 unchanged
```

iptables-side: every wisp rule carries a `wisp:<scope>:<purpose>` comment. Scopes are the sanitised network name for network-level rules (`wisp:app:masq`, `wisp:app:bridge-out`, `wisp:app:loopback-snat`...) and the sanitised container id for per-attachment rules (`wisp:web:dnat`, `wisp:web:loopback-dnat`, `wisp:web:fwd-accept`). `iptables-save | grep wisp:` enumerates everything wisp owns; that's how a future `wisp net reconcile` will find drift.

## Notable design choices

- **Operator-managed bridges.** `WispNetAttacher::detach` does NOT auto-delete the bridge or revoke network-level iptables rules just because the last container left. The operator created the network via `wisp net create` and tears it down with `wisp net rm`. Sticky bridges keep the next `wisp run --network app` cheap (no bridge re-create + sysctl + iptables apply) and avoid races between rapid restart cycles.
- **`NetworkAttacher` trait in `wisp`, impl in `wisp-cli`.** The runtime owns the protocol; `wisp-net` consumes it. This direction means `wisp` ships without depending on `wisp-net` (so a future no-net consumer of the runtime crate doesn't pay for the bridge / veth / iptables surface), and the test fake in `tests/runtime_with_network.rs` mirrors the production attacher one-to-one.
- **Loopback DNAT requires `route_localnet=1` + a matching MASQUERADE.** Without these two, `curl 127.0.0.1:<host-port>` hangs: the kernel emits ARPs with src=127.0.0.1 onto the bridge and the container can't reply. `bridge::ensure` writes `/proc/sys/net/ipv4/conf/<bridge>/route_localnet=1` at create time; `plan_for_network` emits the loopback-source MASQUERADE rule.
- **`ensure_global_ip_forward` is best-effort.** Lives in `wisp::lifecycle` (the runtime crate, not wisp-net). The CLI calls it once before any network-aware `wisp run` or `wisp net create`. Failures only log via tracing: a real misconfiguration is caught by the integration test that ping/curl through the bridge.
- **Sticky vs idempotent.** Bridge create is idempotent (a second `wisp net create app` with the same subnet is a no-op; a different subnet is a `WispNetError::Conflict`). iptables apply is NOT idempotent at the kernel level (every `-A` is an append), so the attacher revokes the network ruleset before re-applying it. Per-attachment rules are tied to a single attach lifecycle so that race doesn't apply.
- **wisp-image capability default is too narrow for nginx.** Phase 0.1's busybox demo only needed `KILL + NET_BIND_SERVICE`; `wisp-image::ConfigOverrides` doesn't yet support a capability override, so `cmd_run_image` post-patches the synthesised config.json to grant `CHOWN + SETUID + SETGID + DAC_OVERRIDE + FOWNER + SETPCAP`. This is what lets nginx fork its worker as the `nginx` user without EPERM. The proper fix (extending `ConfigOverrides`) is tracked for a follow-up; the demo is unblocked today.

## Known limitations

- **No IPv6.** Tracked above. v4-only data path; protocol surfaces use `IpAddr` to leave room.
- **No cross-host networking.** Single-host bridge fabric only.
- **OrbStack VM constraints.** The demo + integration tests assume Ubuntu 24.04 + kernel 6.x + cgroup v2 + iptables-nft. The Mac dev loop only covers the side-effect-free planners.
- **No on-disk subnet registry.** `wisp net rm <name>` doesn't know what subnet `name` was created with, so it uses the default `10.83.0.0/24` to derive iptables rule shapes for revoke. Stale rules from a non-default subnet may need `iptables -F` by hand. v0.4 will add a `<state>/networks/<name>/network.json` so subnet round-trips cleanly.
- **No built-in DNS server.** `/etc/resolv.conf` is host-copied (loopback nameservers filtered). Container-to-container DNS by name is not provided.
- **wisp-image capabilities default is busybox-shaped.** Worked around by `wisp-cli` for `--image` runs as noted above; out of scope for raw `wisp run <bundle>` flows.
- **No automatic GC of orphaned veths / iptables rules on agent restart.** The integration test exercises the happy detach path; a `wisp net reconcile` is a v0.4 item.

## Spec + plan

- Spec: [`docs/superpowers/specs/2026-05-09-wisp-phase-0-3-networking-design.md`](superpowers/specs/2026-05-09-wisp-phase-0-3-networking-design.md)
- Plan: [`docs/superpowers/plans/2026-05-09-wisp-phase-0-3-networking.md`](superpowers/plans/2026-05-09-wisp-phase-0-3-networking.md)
- 0.1 release notes: [`docs/RELEASE_NOTES_WISP_PHASE_0_1.md`](RELEASE_NOTES_WISP_PHASE_0_1.md)
- 0.2 release notes: [`docs/RELEASE_NOTES_WISP_PHASE_0_2.md`](RELEASE_NOTES_WISP_PHASE_0_2.md)
