//! Phase 0.4 dispatch B: WispBackend, the wisp-runtime-backed
//! [`super::RuntimeBackend`].
//!
//! Lands in three commits:
//! - B1 (this file, currently): translation helpers between
//!   [`ContainerCreateSpec`] and wisp's native shapes
//!   ([`wisp_image::ConfigOverrides`], [`wisp::NetworkSpec`],
//!   `oci_spec::runtime::Mount`, `oci_spec::runtime::LinuxResources`).
//!   No WispBackend struct yet; `select_backend` still errors on
//!   `ISENGARD_RUNTIME=wisp`.
//! - B2: WispBackend impl + select_backend wiring + persisted spec
//!   helpers.
//! - B3: WispBackend run_healthcheck (HTTP + nsenter probes).
//!
//! The translation helpers are factored out so dispatch A's existing
//! tests continue to work and so dispatch C (logs + events) can re-use
//! them when it wires up the inotify-tail log stream.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use oci_spec::runtime::{
    LinuxCpuBuilder, LinuxMemoryBuilder, LinuxPidsBuilder, LinuxResources as OciLinuxResources,
    LinuxResourcesBuilder, Mount as OciMount, MountBuilder,
};

use super::spec::{
    ContainerCreateSpec, ContainerSnapshot, ContainerState, HealthState, HealthcheckSpec,
    LinuxResources, ListFilter, LogChunk, LogOptions, LogSource, MountKind, MountSpec,
    NetworkSettings, PortProtocol as SpecPortProtocol, PortSpec, RuntimeEvent, RuntimeEventType,
};
use super::{RuntimeBackend, RuntimeError};

/// Translate a backend-agnostic [`ContainerCreateSpec`] into the
/// wisp-image overrides the [`wisp_image::BundleBuilder`] consumes when
/// it materialises `<bundle>/config.json`.
///
/// Field-by-field:
/// - `command` -> `args` (replaces image `Cmd`).
/// - `entrypoint` -> `entrypoint` (replaces image `Entrypoint`).
/// - `env` (BTreeMap) -> Vec<"KEY=VALUE">, alphabetised by key.
/// - `working_dir` -> `cwd`.
/// - `hostname` -> `hostname`.
/// - `mounts` -> Vec<oci_spec::runtime::Mount> via [`mount_spec_to_oci`].
/// - `linux_resources` -> Option<oci_spec::runtime::LinuxResources>
///   via [`linux_resources_to_oci`].
///
/// Note: secrets are NOT included here; the WispBackend itself appends
/// them as bind-mounts in dispatch B2 because the agent's existing
/// `secret_fetch` materializes them on a tmpfs path that's only known
/// at create-time. Labels are persisted separately in `spec.json` (also
/// dispatch B2): wisp doesn't carry labels in its on-disk state, so the
/// backend reads them back from the persisted spec during inspect / list.
pub fn spec_to_config_overrides(spec: &ContainerCreateSpec) -> wisp_image::ConfigOverrides {
    let mut env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    env.sort();
    let mounts: Vec<OciMount> = spec
        .mounts
        .iter()
        .map(mount_spec_to_oci)
        .collect::<Vec<_>>();
    wisp_image::ConfigOverrides {
        args: spec.command.clone(),
        entrypoint: spec.entrypoint.clone(),
        env,
        cwd: spec.working_dir.clone(),
        hostname: spec.hostname.clone(),
        mounts,
        linux_resources: spec.linux_resources.as_ref().map(linux_resources_to_oci),
        capabilities: cap_add_from_labels(&spec.labels),
    }
}

/// Read the `isengard.cap.add` label off a container spec and convert
/// its comma-separated cap names into a [`wisp_image::CapabilityOverride`]
/// that adds the same cap list to all five OCI sets (bounding,
/// effective, permitted, inheritable, ambient). Mirrors the wisp-cli
/// `--cap-add` semantics + docker's behavior.
///
/// Returns `None` when the label is missing or empty so the
/// BundleBuilder keeps its default `CAP_KILL` + `CAP_NET_BIND_SERVICE`
/// allow-list. This is the agent equivalent of the wisp-cli flag: the
/// container author opts in by setting
/// `isengard.cap.add=CAP_CHOWN,CAP_SETUID,CAP_SETGID,CAP_DAC_OVERRIDE,CAP_FOWNER,CAP_SETPCAP`
/// in their compose file (or any deployment metadata that lands in
/// `ContainerCreateSpec::labels`).
pub fn cap_add_from_labels(
    labels: &std::collections::BTreeMap<String, String>,
) -> Option<wisp_image::CapabilityOverride> {
    let raw = labels.get("isengard.cap.add")?;
    let caps: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if caps.is_empty() {
        return None;
    }
    Some(wisp_image::CapabilityOverride {
        bounding: caps.clone(),
        effective: caps.clone(),
        permitted: caps.clone(),
        inheritable: caps.clone(),
        ambient: caps,
    })
}

/// Translate a [`MountSpec`] into the OCI runtime [`OciMount`] shape
/// the wisp bundle config consumes.
///
/// - [`MountKind::Bind`]: type `bind`, options `["bind"]` (+ `"ro"` if
///   `read_only`).
/// - [`MountKind::Tmpfs`]: type `tmpfs`, source `tmpfs`. The `read_only`
///   flag is honoured but tmpfs isn't typically read-only; we still
///   thread the option for spec-fidelity.
/// - [`MountKind::Volume`]: treated as a bind for now (Phase 0.4 has
///   no volume driver). The source is taken as a host path.
pub fn mount_spec_to_oci(m: &MountSpec) -> OciMount {
    let mut options: Vec<String> = Vec::new();
    let typ = match m.kind {
        MountKind::Bind | MountKind::Volume => {
            options.push("bind".to_string());
            "bind".to_string()
        }
        MountKind::Tmpfs => "tmpfs".to_string(),
    };
    if m.read_only {
        options.push("ro".to_string());
    }
    let source = std::path::PathBuf::from(&m.source);
    let destination = std::path::PathBuf::from(&m.target);
    let mut builder = MountBuilder::default()
        .destination(destination)
        .typ(typ)
        .source(source);
    if !options.is_empty() {
        builder = builder.options(options);
    }
    builder.build().expect("mount fields are all set")
}

/// Translate a [`SecretMount`] into an OCI bind-mount entry. Used by
/// the WispBackend impl in dispatch B2 to fold the agent-materialised
/// tmpfs paths into the bundle config.
pub fn secret_mount_to_oci(s: &super::spec::SecretMount) -> OciMount {
    let options = vec!["bind".to_string(), "ro".to_string()];
    MountBuilder::default()
        .destination(s.target.clone())
        .typ("bind".to_string())
        .source(std::path::PathBuf::from(&s.source))
        .options(options)
        .build()
        .expect("secret mount fields are all set")
}

/// Translate the agent's flat [`LinuxResources`] knobs into the
/// nested OCI [`OciLinuxResources`] shape (memory + cpu + pids).
pub fn linux_resources_to_oci(r: &LinuxResources) -> OciLinuxResources {
    let mut builder = LinuxResourcesBuilder::default();
    if r.memory_max_bytes.is_some() || r.memory_swap_max_bytes.is_some() {
        let mut mem_builder = LinuxMemoryBuilder::default();
        if let Some(bytes) = r.memory_max_bytes {
            mem_builder = mem_builder.limit(bytes as i64);
        }
        if let Some(bytes) = r.memory_swap_max_bytes {
            mem_builder = mem_builder.swap(bytes as i64);
        }
        let memory = mem_builder.build().expect("memory fields valid");
        builder = builder.memory(memory);
    }
    if r.cpu_quota_us.is_some() || r.cpu_period_us.is_some() || r.cpu_shares.is_some() {
        let mut cpu_builder = LinuxCpuBuilder::default();
        if let Some(q) = r.cpu_quota_us {
            cpu_builder = cpu_builder.quota(q);
        }
        if let Some(p) = r.cpu_period_us {
            cpu_builder = cpu_builder.period(p);
        }
        if let Some(s) = r.cpu_shares {
            cpu_builder = cpu_builder.shares(s);
        }
        let cpu = cpu_builder.build().expect("cpu fields valid");
        builder = builder.cpu(cpu);
    }
    if let Some(limit) = r.pids_max {
        let pids = LinuxPidsBuilder::default()
            .limit(limit)
            .build()
            .expect("pids fields valid");
        builder = builder.pids(pids);
    }
    builder.build().expect("LinuxResources valid")
}

/// Translate a [`PortSpec`] into wisp's native [`wisp::PortPublish`].
pub fn port_spec_to_wisp(p: &PortSpec) -> wisp::PortPublish {
    let host_ip = p
        .host_ip
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let protocol = match p.protocol {
        SpecPortProtocol::Tcp => wisp::PortProtocol::Tcp,
        SpecPortProtocol::Udp => wisp::PortProtocol::Udp,
    };
    wisp::PortPublish {
        host_ip,
        host_port: p.host_port,
        container_port: p.container_port,
        protocol,
    }
}

/// Translate the network bits of a [`ContainerCreateSpec`] into wisp's
/// [`wisp::NetworkSpec`]. Returns `None` when the agent didn't ask for
/// a network and didn't publish any ports: wisp treats those containers
/// as "no network namespace plumbing" and `Runtime::create` (no
/// `_with_network` variant) is what we want.
///
/// Multi-network handling: wisp's [`wisp::NetworkAttacher`] supports
/// exactly one primary network at create-time. Secondary networks are
/// deferred to dispatch B2's `connect_network`. Here we pick the first
/// declared network as primary; the WispBackend impl iterates the rest
/// and would call `connect_network` on each (which dispatch B2 stubs as
/// "not supported in 0.4; recreate the container"; live network attach
/// is a 0.5 stretch goal).
pub fn spec_to_network_spec(spec: &ContainerCreateSpec) -> Option<wisp::NetworkSpec> {
    if spec.networks.is_empty() && spec.ports.is_empty() {
        return None;
    }
    let network_name = spec
        .networks
        .first()
        .cloned()
        .unwrap_or_else(|| "wisp-default".to_string());
    let ports = spec.ports.iter().map(port_spec_to_wisp).collect();
    Some(wisp::NetworkSpec {
        network_name,
        ports,
        resolv_source: wisp::ResolvSource::HostCopy,
    })
}

/// Default subnet for the wisp default bridge. Operators can override
/// via `WISP_DEFAULT_SUBNET=<cidr>`. Matches the wisp-cli demo subnet.
/// Linux-only: WispNetAttacher does not exist on Mac.
#[cfg(target_os = "linux")]
const DEFAULT_NETWORK_SUBNET: &str = "10.83.0.0/24";

/// Default network name when an agent container declares ports without
/// specifying a network. Matches `wisp run --port 8080:80`'s default.
#[cfg(target_os = "linux")]
const DEFAULT_NETWORK_NAME: &str = "wisp-default";

/// On-disk shape of a persisted network entry. Lives at
/// `<state_dir>/networks/<name>/network.json` and is read on every
/// create-time bridge ensure so subsequent containers reuse the same
/// subnet / gateway / bridge name. The shape is stable: a future field
/// should be optional so prior agent versions keep parsing.
///
/// `dead_code` allow: the registry helpers are exercised by the
/// Linux-only `ensure_bridge` and the cross-platform tests. On Mac
/// `cargo build --lib` doesn't see a non-test caller of the registry
/// path so `dead_code` would fire without this attribute.
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct NetworkRegistryEntry {
    /// Network name as the operator (or the agent's default-naming
    /// rule) chose. Distinct from the bridge name (`wbr-<truncated>`).
    name: String,
    /// IPv4 subnet in CIDR form. Round-trips as a string so older agent
    /// versions that don't yet know this field can still parse the JSON.
    subnet: String,
    /// First usable host address in `subnet`. Stored separately so the
    /// gateway isn't recomputed on every read.
    gateway: std::net::Ipv4Addr,
    /// Bridge interface name (`wbr-<truncated>`). Cached so detach
    /// flows can match `ip link show wbr-...` without recomputing the
    /// truncation.
    bridge: String,
}

/// Default cgroup root for wisp containers. Mirrors
/// `wisp::Runtime::DEFAULT_CGROUP_ROOT`. Tests override via
/// `WISP_CGROUP_ROOT`.
const DEFAULT_CGROUP_ROOT: &str = "/sys/fs/cgroup/wisp";

/// WispBackend: the [`RuntimeBackend`] backed by `wisp`, `wisp-image`,
/// and (Linux only) `wisp-net`.
///
/// Layout:
/// - `runtime`: drives container CRUD against the on-disk state-dir.
/// - `image_client`: pulls + GCs OCI images, materialises bundles.
/// - `state_dir`: agent-owned directory; wisp owns
///   `<state_dir>/wisp/` (containers + bundles), the spec persistence
///   layer owns `<state_dir>/spec/<id>/spec.json`.
/// - `net_attacher` (Linux only): a `Box<dyn NetworkAttacher + Send>`
///   pinned to the default network. Held in an `Option` inside a
///   `Mutex` so we can move it into `spawn_blocking` (the trait's
///   `&mut self` methods preclude shared `Arc` access). Lazily
///   constructed in [`WispBackend::from_env`].
/// - `event_tx`: broadcast channel; `start_container` /
///   `stop_container` push synthetic events here, and
///   [`WispBackend::stream_events`] subscribes.
///
/// Phase 0.4 limitations carried over from prior phases:
/// - Multi-network containers attach the primary network at create-time
///   and would call `connect_network` on the rest. Live attach is not
///   supported in 0.4 (spec says recreate the container instead).
/// - Restart policy is persisted in the spec but not auto-acted-on by
///   the backend; the agent's deployment driver handles it.
/// - Healthchecks run externally (B3) via nsenter; the runtime itself
///   does not run them in-container.
pub struct WispBackend {
    runtime: Arc<wisp::Runtime>,
    image_client: Arc<wisp_image::Client>,
    state_dir: PathBuf,
    #[cfg(target_os = "linux")]
    net_attacher: std::sync::Mutex<Option<Box<dyn wisp::NetworkAttacher + Send>>>,
    event_tx: tokio::sync::broadcast::Sender<RuntimeEvent>,
}

impl std::fmt::Debug for WispBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WispBackend")
            .field("state_dir", &self.state_dir)
            .field("runtime", &"<wisp::Runtime>")
            .field("image_client", &"<wisp_image::Client>")
            .finish()
    }
}

impl WispBackend {
    /// Build a WispBackend rooted at `state_dir`.
    ///
    /// On Linux this also constructs a [`wisp_net::WispNetAttacher`] for
    /// the default network (subnet from `WISP_DEFAULT_SUBNET` or the
    /// hard-coded `10.83.0.0/24`). The attacher is lazily used per
    /// container that asks for a network; containers that don't go
    /// through `Runtime::start_with_attacher`.
    pub async fn from_env(state_dir: &Path) -> Result<Self, RuntimeError> {
        std::fs::create_dir_all(state_dir)?;
        let wisp_state = state_dir.join("wisp");
        std::fs::create_dir_all(&wisp_state)?;
        let bundles_dir = state_dir.join("bundles");
        std::fs::create_dir_all(&bundles_dir)?;
        let store_dir = state_dir.join("images");
        std::fs::create_dir_all(&store_dir)?;
        let spec_dir = state_dir.join("spec");
        std::fs::create_dir_all(&spec_dir)?;

        // Runtime: with_cgroup_root lets tests slot in a tempdir-backed
        // cgroup. Production reads WISP_CGROUP_ROOT or falls back to
        // /sys/fs/cgroup/wisp.
        let cgroup_root =
            std::env::var("WISP_CGROUP_ROOT").unwrap_or_else(|_| DEFAULT_CGROUP_ROOT.to_string());
        let runtime = wisp::Runtime::with_cgroup_root(&wisp_state, Path::new(&cgroup_root))
            .map_err(|e| RuntimeError::Wisp(format!("runtime init: {e}")))?;

        // wisp_image::Client::new builds a reqwest::blocking::Client
        // which owns its own internal tokio runtime. Constructing (and
        // later dropping) it from within a tokio runtime context panics
        // with "Cannot drop a runtime in a context where blocking is not
        // allowed". spawn_blocking gives us a thread that's outside the
        // outer runtime's async context.
        let store_dir_clone = store_dir.clone();
        let image_client =
            tokio::task::spawn_blocking(move || wisp_image::Client::new(&store_dir_clone))
                .await
                .map_err(|e| RuntimeError::Image(format!("join: {e}")))?
                .map_err(|e| RuntimeError::Image(format!("image client init: {e}")))?;

        let (event_tx, _rx) = tokio::sync::broadcast::channel::<RuntimeEvent>(1024);

        // Wave 3.B: replace the 2s poll-and-diff emitter loop with a
        // notify-driven `cgroup.events` watcher. Kernel writes the
        // `populated 0/1` line in `<cgroup_root>/<id>/cgroup.events`
        // whenever the cgroup's process set goes empty / non-empty, so
        // inotify fires within microseconds of a container start or
        // die. Latency drops from ~1s avg (half the poll interval) to
        // <100ms; fast-cycling containers that started + died inside a
        // single 2s window are no longer invisible.
        //
        // Production wires the watcher to the real cgroup root + the
        // wisp Runtime (for exit-code lookup on Die events). Tests
        // substitute a tempdir-backed `CgroupRoot` + a fake exit-code
        // source; see `cgroup_events_loop` and its unit tests.
        let runtime_arc = Arc::new(runtime);
        {
            let rt_for_loop = runtime_arc.clone();
            let tx_for_loop = event_tx.clone();
            let cgroup_root_for_loop = PathBuf::from(&cgroup_root);
            tokio::spawn(async move {
                cgroup_events_loop(
                    cgroup_root_for_loop,
                    WispExitCodeSource(rt_for_loop),
                    tx_for_loop,
                )
                .await;
            });
        }

        Ok(Self {
            runtime: runtime_arc,
            image_client: Arc::new(image_client),
            state_dir: state_dir.to_path_buf(),
            #[cfg(target_os = "linux")]
            net_attacher: std::sync::Mutex::new(Self::build_default_attacher(state_dir)),
            event_tx,
        })
    }

    /// Boot-time orphan cleanup. Walks kernel network state (`wbr-*`
    /// bridges, `wveth-*` veth halves, `wisp:<scope>:*` iptables rules)
    /// against the on-disk network registry and the live wisp runtime,
    /// and best-effort removes anything orphaned (e.g. host reboot or
    /// agent-crash leftovers).
    ///
    /// Called once from `lib.rs::run_agent` before the first compose
    /// reconcile fires, so a fresh container start doesn't race a
    /// stale `wbr-app` from yesterday. The trait shim
    /// [`RuntimeBackend::reconcile_network_orphans`] delegates here
    /// and returns the [`wisp_net::ReconcileReport::total`] count.
    ///
    /// Linux only. Wisp-net's reconcile relies on `ip` + `iptables-save`
    /// + `/sys/class/net`, none of which exist on Mac.
    #[cfg(target_os = "linux")]
    pub fn reconcile_network_inherent(&self) -> Result<wisp_net::ReconcileReport, RuntimeError> {
        let networks_dir = self.state_dir.join("networks");

        // Live container ids the runtime knows about. Reconcile keeps
        // their iptables rules and treats anything else as orphan.
        // Failure to enumerate is non-fatal: prefer "keep all
        // container-scoped rules" over "delete them all".
        let known: std::collections::BTreeSet<String> = match self.runtime.list() {
            Ok(handles) => handles.into_iter().map(|h| h.id).collect(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "reconcile: Runtime::list failed, skipping container-rule cleanup"
                );
                return wisp_net::reconcile(wisp_net::ReconcileInputs {
                    networks_dir: &networks_dir,
                    known_container_ids: None,
                })
                .map_err(|e| RuntimeError::Network(format!("reconcile: {e}")));
            }
        };

        wisp_net::reconcile(wisp_net::ReconcileInputs {
            networks_dir: &networks_dir,
            known_container_ids: Some(&known),
        })
        .map_err(|e| RuntimeError::Network(format!("reconcile: {e}")))
    }

    #[cfg(target_os = "linux")]
    fn build_default_attacher(state_dir: &Path) -> Option<Box<dyn wisp::NetworkAttacher + Send>> {
        let subnet_str = std::env::var("WISP_DEFAULT_SUBNET")
            .unwrap_or_else(|_| DEFAULT_NETWORK_SUBNET.to_string());
        let subnet: ipnet::Ipv4Net = match subnet_str.parse() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "WISP_DEFAULT_SUBNET parse failed, networking disabled"
                );
                return None;
            }
        };
        let network = match wisp_net::Network::new(DEFAULT_NETWORK_NAME, subnet) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "wisp Network::new failed, networking disabled");
                return None;
            }
        };
        let ipam_dir = state_dir.join("networks").join(DEFAULT_NETWORK_NAME);
        if let Err(e) = std::fs::create_dir_all(&ipam_dir) {
            tracing::warn!(error = %e, "ipam dir create failed, networking disabled");
            return None;
        }
        Some(Box::new(
            crate::runtime::wisp_backend_attacher::WispNetAttacher::new(network, &ipam_dir),
        ))
    }

    /// Auto-create the bridge + iptables rules for `network_name` when
    /// a container needs networking and the bridge isn't already
    /// provisioned. Honours an on-disk registry: subsequent ensures
    /// reuse the persisted subnet so the operator can call this
    /// idempotently across many containers.
    ///
    /// Subnet source on first ensure: explicit `subnet_hint` (the
    /// caller's preferred subnet) -> `WISP_DEFAULT_SUBNET` env ->
    /// hard-coded `10.83.0.0/24`. Conflict detection: if the registry
    /// already records a different subnet under the same name, return
    /// a `Network` error rather than silently rebinding.
    ///
    /// Linux only. Returns the freshly-built [`wisp_net::Network`]
    /// the caller can hand to `wisp_net::veth::attach_to_ns`.
    #[cfg(target_os = "linux")]
    fn ensure_bridge(
        &self,
        network_name: &str,
        subnet_hint: Option<&str>,
    ) -> Result<wisp_net::Network, RuntimeError> {
        let registry_path = self
            .state_dir
            .join("networks")
            .join(network_name)
            .join("network.json");

        // 1. Look up the on-disk entry so subsequent ensures reuse the
        //    existing subnet. Treat ENOENT / parse failure as "first
        //    ensure"; surface real IO errors to the caller.
        let existing = read_network_registry(&registry_path)?;

        // 2. Derive the subnet for this ensure: registry > caller hint
        //    > env > hard-coded default.
        let want_subnet_str = match (existing.as_ref(), subnet_hint) {
            (Some(entry), _) => entry.subnet.clone(),
            (None, Some(hint)) => hint.to_string(),
            (None, None) => std::env::var("WISP_DEFAULT_SUBNET")
                .unwrap_or_else(|_| DEFAULT_NETWORK_SUBNET.to_string()),
        };
        let want_subnet: ipnet::Ipv4Net = want_subnet_str
            .parse()
            .map_err(|e| RuntimeError::Network(format!("parse subnet {want_subnet_str:?}: {e}")))?;

        // 3. Conflict: registry says X, caller passed Y.
        if let (Some(entry), Some(hint)) = (existing.as_ref(), subnet_hint) {
            if entry.subnet != hint {
                return Err(RuntimeError::Network(format!(
                    "network {network_name:?} already exists with subnet {} but caller asked for {hint}",
                    entry.subnet,
                )));
            }
        }

        let network = wisp_net::Network::new(network_name, want_subnet)
            .map_err(|e| RuntimeError::Network(format!("build network {network_name:?}: {e}")))?;

        // 4. Best-effort host-global ip_forward toggle. Without this
        //    the FORWARD rules accept the packet but the kernel still
        //    drops it; curl hangs. The helper is itself idempotent.
        if let Err(e) = wisp::lifecycle::ensure_global_ip_forward() {
            tracing::warn!(error = %e, "ensure_global_ip_forward (best-effort)");
        }

        // 5. Bridge + iptables (idempotent: pre-existing matches succeed).
        wisp_net::bridge::ensure(&network)
            .map_err(|e| RuntimeError::Network(format!("bridge::ensure: {e}")))?;
        let net_rs = wisp_net::iptables::plan_for_network(&network);
        let _ = wisp_net::iptables::revoke(&net_rs);
        wisp_net::iptables::apply(&net_rs)
            .map_err(|e| RuntimeError::Network(format!("iptables::apply (net): {e}")))?;

        // 6. Persist on first ensure. Re-writes are no-ops if the
        //    contents match.
        if existing.is_none() {
            let entry = NetworkRegistryEntry {
                name: network.name.clone(),
                subnet: network.subnet.to_string(),
                gateway: network.gateway,
                bridge: network.bridge.clone(),
            };
            write_network_registry(&registry_path, &entry)?;
        }

        Ok(network)
    }

    /// Persist the [`ContainerCreateSpec`] under
    /// `<state_dir>/spec/<id>/spec.json`. Wisp's native `state.json`
    /// doesn't carry labels / healthcheck / restart info; the WispBackend
    /// reads them back from this file during [`Self::inspect_container`]
    /// + [`Self::list_containers`] + [`Self::run_healthcheck`].
    fn persist_spec(&self, id: &str, spec: &ContainerCreateSpec) -> Result<(), RuntimeError> {
        let dir = self.state_dir.join("spec").join(id);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("spec.json");
        let json = serde_json::to_vec_pretty(spec)
            .map_err(|e| RuntimeError::Container(format!("spec serialize: {e}")))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Read back the [`ContainerCreateSpec`] persisted by
    /// [`Self::persist_spec`].
    fn read_spec(&self, id: &str) -> Result<ContainerCreateSpec, RuntimeError> {
        let path = self.state_dir.join("spec").join(id).join("spec.json");
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| RuntimeError::Container(format!("spec parse {id}: {e}")))
    }

    /// Drop the persisted spec. Idempotent.
    fn remove_spec(&self, id: &str) -> Result<(), RuntimeError> {
        let dir = self.state_dir.join("spec").join(id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(RuntimeError::Io(e)),
        }
    }

    /// Translate a wisp `ContainerHandle` + the persisted
    /// [`ContainerCreateSpec`] into the agent's shared
    /// [`ContainerSnapshot`].
    fn handle_to_snapshot(
        &self,
        handle: &wisp::ContainerHandle,
    ) -> Result<ContainerSnapshot, RuntimeError> {
        let spec = self.read_spec(&handle.id).ok();
        let state = match handle.state {
            wisp::ContainerState::Created => ContainerState::Created,
            wisp::ContainerState::Running => ContainerState::Running,
            wisp::ContainerState::Stopped => ContainerState::Exited,
        };
        let mut network_settings = NetworkSettings::default();
        if let Some(rec) = handle.network_attachment.as_ref() {
            network_settings
                .ip_addresses
                .insert(rec.network_name.clone(), std::net::IpAddr::V4(rec.ipv4));
            for p in &rec.ports {
                let proto = match p.protocol {
                    wisp::PortProtocol::Tcp => "tcp",
                    wisp::PortProtocol::Udp => "udp",
                };
                let key = format!("{}/{proto}", p.container_port);
                network_settings
                    .ports
                    .entry(key)
                    .or_default()
                    .push(super::spec::HostPort {
                        host_ip: p.host_ip,
                        host_port: p.host_port,
                    });
            }
        }
        // Phase 0.6: surface env / port_bindings / restart so the
        // compose reconciler can detect drift through the trait without
        // reaching for native runtime types. Bollard fills these from
        // inspect; wisp from the persisted spec.
        let env = spec.as_ref().map(|s| s.env.clone()).unwrap_or_default();
        let port_bindings: Vec<String> = spec
            .as_ref()
            .map(|s| {
                let mut out: Vec<String> = s
                    .ports
                    .iter()
                    .map(|p| match p.host_ip {
                        Some(ip) => format!("{ip}:{}:{}", p.host_port, p.container_port),
                        None => format!("{}:{}", p.host_port, p.container_port),
                    })
                    .collect();
                out.sort();
                out
            })
            .unwrap_or_default();
        let restart = spec.as_ref().map(|s| match s.restart {
            super::spec::RestartPolicy::Always => "always".to_string(),
            super::spec::RestartPolicy::UnlessStopped => "unless-stopped".to_string(),
            super::spec::RestartPolicy::OnFailure { .. } => "on-failure".to_string(),
            super::spec::RestartPolicy::No => "no".to_string(),
        });
        Ok(ContainerSnapshot {
            id: handle.id.clone(),
            name: handle.id.clone(),
            image: spec.as_ref().map(|s| s.image.clone()).unwrap_or_default(),
            state,
            stack: spec.as_ref().map(|s| s.stack.clone()),
            service: spec.as_ref().map(|s| s.service.clone()),
            labels: spec.as_ref().map(|s| s.labels.clone()).unwrap_or_default(),
            created_at: handle.created_at,
            started_at: None,
            finished_at: None,
            exit_code: None,
            restart_count: 0,
            network_settings,
            env,
            port_bindings,
            restart,
        })
    }
}

#[async_trait]
impl RuntimeBackend for WispBackend {
    async fn ensure_image(&self, reference: &str) -> Result<String, RuntimeError> {
        let r: wisp_image::ImageRef = reference
            .parse()
            .map_err(|e: wisp_image::WispImageError| RuntimeError::Image(format!("{e}")))?;
        // Pull is blocking (reqwest blocking + content-store flock); run
        // it on a blocking pool slot so we don't stall the tokio runtime.
        let client = self.image_client.clone();
        let pulled = tokio::task::spawn_blocking(move || client.pull(&r))
            .await
            .map_err(|e| RuntimeError::Image(format!("join: {e}")))?
            .map_err(|e| RuntimeError::Image(format!("{e}")))?;
        Ok(pulled.manifest_digest)
    }

    async fn create_container(&self, spec: &ContainerCreateSpec) -> Result<String, RuntimeError> {
        let r: wisp_image::ImageRef = spec
            .image
            .parse()
            .map_err(|e: wisp_image::WispImageError| RuntimeError::Image(format!("{e}")))?;
        let client = self.image_client.clone();
        let pulled = tokio::task::spawn_blocking(move || match client.lookup(&r) {
            Ok(Some(p)) => Ok(p),
            Ok(None) => client.pull(&r),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| RuntimeError::Image(format!("join: {e}")))?
        .map_err(|e| RuntimeError::Image(format!("{e}")))?;

        let bundle_dir = self.state_dir.join("bundles").join(&spec.container_name);
        if bundle_dir.exists() {
            return Err(RuntimeError::Container(format!(
                "bundle dir already exists: {}",
                bundle_dir.display()
            )));
        }

        // Build the override set: spec_to_config_overrides + secrets as
        // bind-mounts. The secret_fetch path materialises secrets onto
        // tmpfs paths before this call; we just turn each into a Mount.
        let mut overrides = spec_to_config_overrides(spec);
        for s in &spec.secrets {
            overrides.mounts.push(secret_mount_to_oci(s));
        }

        // Run the assemble + write_config off-thread (tar extraction +
        // file IO; would block the tokio runtime).
        let pulled_clone = pulled.clone();
        let store = self.image_client.store().clone();
        let bundle_dir_clone = bundle_dir.clone();
        let overrides_clone = overrides.clone();
        tokio::task::spawn_blocking(move || -> Result<(), wisp_image::WispImageError> {
            let builder = wisp_image::BundleBuilder::new(&pulled_clone, &store, &bundle_dir_clone);
            builder.assemble_rootfs()?;
            builder.write_config(overrides_clone)?;
            Ok(())
        })
        .await
        .map_err(|e| RuntimeError::Image(format!("join: {e}")))?
        .map_err(|e| RuntimeError::Image(format!("{e}")))?;

        // Layer ref for GC.
        let layer_digests: Vec<String> = pulled.layers.iter().map(|l| l.digest.clone()).collect();
        self.image_client
            .store()
            .add_ref(&spec.container_name, &layer_digests)
            .map_err(|e| RuntimeError::Image(format!("{e}")))?;

        // Persist agent-side spec for inspect / list / restart.
        self.persist_spec(&spec.container_name, spec)?;

        // Hand off to the wisp runtime. The native `Runtime::create*`
        // calls are file-only on Mac and clone3-prep on Linux, both
        // synchronous. They don't fork the entrypoint (that's `start`),
        // so calling them on the tokio runtime thread is safe: no clone3
        // here.
        let network_spec = spec_to_network_spec(spec);
        let runtime = self.runtime.clone();
        let id = spec.container_name.clone();
        let bundle_clone = bundle_dir.clone();
        match network_spec {
            Some(net) => {
                // Phase 0.5: pre-flight ensure the bridge + iptables for
                // this network exist on the host before clone3 fires.
                // The persisted registry under
                // `<state_dir>/networks/<name>/network.json` lets later
                // ensures reuse the same subnet, and detects an
                // operator picking a conflicting subnet for the same
                // network name.
                #[cfg(target_os = "linux")]
                {
                    self.ensure_bridge(&net.network_name, None)?;
                }
                runtime
                    .create_with_network(&id, &bundle_clone, net)
                    .map_err(|e| RuntimeError::Container(format!("{e}")))?
            }
            None => runtime
                .create(&id, &bundle_clone)
                .map_err(|e| RuntimeError::Container(format!("{e}")))?,
        };
        Ok(spec.container_name.clone())
    }

    async fn start_container(&self, id: &str) -> Result<(), RuntimeError> {
        let spec = self.read_spec(id)?;
        let needs_network = spec_to_network_spec(&spec).is_some();
        let runtime = self.runtime.clone();
        let id_owned = id.to_string();

        // clone3 invariant: must be in spawn_blocking. The runtime's
        // `start_with_attacher` and `start` both eventually clone3.
        #[cfg(target_os = "linux")]
        {
            if needs_network {
                // Take ownership of the attacher so the trait's
                // `&mut self` requirement is satisfied without holding a
                // lock across `await`. Put it back when start returns.
                let attacher_opt = self
                    .net_attacher
                    .lock()
                    .expect("net_attacher mutex poisoned")
                    .take();
                let mut attacher = attacher_opt.ok_or_else(|| {
                    RuntimeError::Network("wisp default network attacher not available".to_string())
                })?;
                let join_result = tokio::task::spawn_blocking(move || {
                    let res = runtime.start_with_attacher(&id_owned, attacher.as_mut());
                    (attacher, res)
                })
                .await
                .map_err(|e| RuntimeError::Container(format!("join: {e}")))?;
                let (attacher_back, res) = join_result;
                *self
                    .net_attacher
                    .lock()
                    .expect("net_attacher mutex poisoned") = Some(attacher_back);
                res.map_err(|e| RuntimeError::Container(format!("{e}")))?;
            } else {
                tokio::task::spawn_blocking(move || runtime.start(&id_owned))
                    .await
                    .map_err(|e| RuntimeError::Container(format!("join: {e}")))?
                    .map_err(|e| RuntimeError::Container(format!("{e}")))?;
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            if needs_network {
                return Err(RuntimeError::Network(
                    "wisp networking requires linux".to_string(),
                ));
            }
            tokio::task::spawn_blocking(move || runtime.start(&id_owned))
                .await
                .map_err(|e| RuntimeError::Container(format!("join: {e}")))?
                .map_err(|e| RuntimeError::Container(format!("{e}")))?;
        }

        let _ = self.event_tx.send(RuntimeEvent {
            container_id: id.to_string(),
            event_type: RuntimeEventType::Start,
            timestamp: SystemTime::now(),
        });
        Ok(())
    }

    async fn stop_container(&self, id: &str, timeout_s: u32) -> Result<(), RuntimeError> {
        // SIGTERM first, then poll for `Stopped`, then SIGKILL on
        // timeout. wisp's `state` call already handles the
        // running-pid-gone repair, so the loop sees `Stopped` as soon
        // as the process exits.
        self.runtime
            .kill(id, nix::sys::signal::Signal::SIGTERM)
            .map_err(|e| RuntimeError::Container(format!("{e}")))?;

        let start = std::time::Instant::now();
        loop {
            let handle = self
                .runtime
                .state(id)
                .map_err(|e| RuntimeError::Container(format!("{e}")))?;
            if handle.state == wisp::ContainerState::Stopped {
                break;
            }
            if start.elapsed().as_secs() >= timeout_s as u64 {
                let _ = self.runtime.kill(id, nix::sys::signal::Signal::SIGKILL);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        let _ = self.event_tx.send(RuntimeEvent {
            container_id: id.to_string(),
            event_type: RuntimeEventType::Stop,
            timestamp: SystemTime::now(),
        });
        Ok(())
    }

    async fn remove_container(&self, id: &str, force: bool) -> Result<(), RuntimeError> {
        let runtime = self.runtime.clone();
        let id_owned = id.to_string();

        #[cfg(target_os = "linux")]
        {
            // Same dance as start: take the attacher, run delete, put
            // it back.
            let attacher_opt = self
                .net_attacher
                .lock()
                .expect("net_attacher mutex poisoned")
                .take();
            if let Some(mut attacher) = attacher_opt {
                let join_result = tokio::task::spawn_blocking(move || {
                    let res = runtime.delete_with_attacher(&id_owned, force, attacher.as_mut());
                    (attacher, res)
                })
                .await
                .map_err(|e| RuntimeError::Container(format!("join: {e}")))?;
                let (attacher_back, res) = join_result;
                *self
                    .net_attacher
                    .lock()
                    .expect("net_attacher mutex poisoned") = Some(attacher_back);
                res.map_err(|e| RuntimeError::Container(format!("{e}")))?;
            } else {
                tokio::task::spawn_blocking(move || runtime.delete(&id_owned, force))
                    .await
                    .map_err(|e| RuntimeError::Container(format!("join: {e}")))?
                    .map_err(|e| RuntimeError::Container(format!("{e}")))?;
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            tokio::task::spawn_blocking(move || runtime.delete(&id_owned, force))
                .await
                .map_err(|e| RuntimeError::Container(format!("join: {e}")))?
                .map_err(|e| RuntimeError::Container(format!("{e}")))?;
        }

        // Drop the bundle dir, layer ref, persisted spec.
        let bundle_dir = self.state_dir.join("bundles").join(id);
        if bundle_dir.exists() {
            std::fs::remove_dir_all(&bundle_dir)?;
        }
        let _ = self.image_client.store().drop_ref(id);
        let _ = self.remove_spec(id);
        Ok(())
    }

    async fn list_containers(
        &self,
        filter: ListFilter,
    ) -> Result<Vec<ContainerSnapshot>, RuntimeError> {
        let handles = self
            .runtime
            .list()
            .map_err(|e| RuntimeError::Container(format!("{e}")))?;
        let mut out = Vec::new();
        for h in handles {
            let snap = self.handle_to_snapshot(&h)?;
            if let Some(stack) = filter.stack.as_deref() {
                if snap.stack.as_deref() != Some(stack) {
                    continue;
                }
            }
            if let Some(key) = filter.label_key.as_deref() {
                if !snap.labels.contains_key(key) {
                    continue;
                }
            }
            if !filter.all && snap.state == ContainerState::Exited {
                continue;
            }
            out.push(snap);
        }
        Ok(out)
    }

    async fn inspect_container(&self, id: &str) -> Result<Option<ContainerSnapshot>, RuntimeError> {
        match self.runtime.state(id) {
            Ok(handle) => Ok(Some(self.handle_to_snapshot(&handle)?)),
            Err(_) => Ok(None),
        }
    }

    async fn connect_network(&self, container_id: &str, network: &str) -> Result<(), RuntimeError> {
        // wisp wires the primary network at create_container time via
        // spec.networks[0]. The compose_apply path then loops the
        // declared networks calling connect_network for each. Treat the
        // already-attached primary as a no-op so the canonical
        // single-network compose case succeeds. Live attach for
        // additional networks remains unimplemented.
        let spec = self.read_spec(container_id)?;
        if spec.networks.first().map(String::as_str) == Some(network) {
            return Ok(());
        }
        Err(RuntimeError::Network(
            "wisp does not support live network attach in 0.4; recreate the container".into(),
        ))
    }

    async fn disconnect_network(
        &self,
        _container_id: &str,
        network: &str,
    ) -> Result<(), RuntimeError> {
        // The compose_apply path issues a best-effort `disconnect_network
        // bridge` before connecting declared networks. Accept that as a
        // no-op since wisp containers never join Docker's `bridge`. The
        // surrounding caller already swallows errors here, but returning
        // Ok keeps the trace clean for operators reading logs.
        if network == "bridge" {
            return Ok(());
        }
        Err(RuntimeError::Network(
            "wisp does not support live network detach in 0.4; recreate the container".into(),
        ))
    }

    fn stream_logs(
        &self,
        id: &str,
        opts: LogOptions,
    ) -> Pin<Box<dyn Stream<Item = LogChunk> + Send>> {
        // Dispatch C2: inotify-tail the per-container stdout.log /
        // stderr.log files written by wisp lifecycle. Backfill existing
        // content first (capped by opts.tail), then watch each file for
        // Modify events and emit deltas as they land.
        //
        // The wisp state-dir layout puts each container under
        // `<state_dir>/wisp/containers/<id>/`. (See `WispBackend::from_env`:
        // `let wisp_state = state_dir.join("wisp")`.)
        let stdout_path = self
            .state_dir
            .join("wisp")
            .join("containers")
            .join(id)
            .join("stdout.log");
        let stderr_path = self
            .state_dir
            .join("wisp")
            .join("containers")
            .join(id)
            .join("stderr.log");
        Box::pin(inotify_tail_logs(stdout_path, stderr_path, opts))
    }

    fn stream_events(&self) -> Pin<Box<dyn Stream<Item = RuntimeEvent> + Send>> {
        let rx = self.event_tx.subscribe();
        Box::pin(
            tokio_stream::wrappers::BroadcastStream::new(rx)
                .filter_map(|r| futures::future::ready(r.ok())),
        )
    }

    async fn run_healthcheck(
        &self,
        id: &str,
        hc: &HealthcheckSpec,
    ) -> Result<HealthState, RuntimeError> {
        run_healthcheck_impl(self, id, hc).await
    }

    fn name(&self) -> &'static str {
        "wisp"
    }

    /// Wisp-specific orphan sweep. See
    /// [`Self::reconcile_network_inherent`].
    ///
    /// On Mac the wisp backend's reconcile path is a noop (the kernel
    /// helpers wisp-net wraps don't exist), so the inherent method only
    /// compiles on Linux; we surface the same total-actions number
    /// through the trait by gating the body.
    async fn reconcile_network_orphans(&self) -> Result<usize, RuntimeError> {
        #[cfg(target_os = "linux")]
        {
            let report = self.reconcile_network_inherent()?;
            Ok(report.total())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(0)
        }
    }
}

/// External-process healthcheck for wisp containers.
///
/// Wisp does not run healthchecks in-container the way docker does;
/// the wisp container has no init that would cycle a probe between
/// `interval` ticks. Instead the agent's deployment driver calls
/// [`RuntimeBackend::run_healthcheck`] on its own cadence, and this
/// function shells out to `nsenter` to execute the test inside the
/// container's namespaces.
///
/// Recognised test shapes (matching docker's HealthConfig.test):
/// - `["NONE"]` -> always Healthy. Matches docker's "skip the check".
/// - `["CMD", arg0, arg1, ...]` -> exec arg0 + args inside the
///   container's mount + uts + ipc + pid + network namespaces. Exit 0
///   = Healthy, anything else = Unhealthy.
/// - `["CMD-SHELL", "<cmd line>"]` -> exec /bin/sh -c "<cmd line>"
///   inside those same namespaces.
/// - Anything else (bare list) is treated as `CMD` + the list.
///
/// The empty-test case returns Healthy (matches docker: "no test
/// configured" is reported as healthy by `docker inspect`).
async fn run_healthcheck_impl(
    backend: &WispBackend,
    id: &str,
    hc: &HealthcheckSpec,
) -> Result<HealthState, RuntimeError> {
    if hc.test.is_empty() {
        return Ok(HealthState::Healthy);
    }
    if hc.test[0] == "NONE" {
        return Ok(HealthState::Healthy);
    }
    // Validate test shape BEFORE looking up the pid: a malformed
    // CMD / CMD-SHELL spec is a configuration bug, not a runtime
    // failure, and a clear error is more useful than "no pid".
    let mode = hc.test[0].as_str();
    if mode == "CMD" && hc.test.len() < 2 {
        return Err(RuntimeError::Healthcheck(
            "CMD requires at least one arg".into(),
        ));
    }
    if mode == "CMD-SHELL" && hc.test.len() < 2 {
        return Err(RuntimeError::Healthcheck(
            "CMD-SHELL requires a command string".into(),
        ));
    }

    let pid = backend
        .runtime
        .container_pid(id)
        .ok_or_else(|| RuntimeError::Container(format!("no pid for {id}")))?;
    let timeout = hc.timeout;

    match mode {
        "CMD-SHELL" => {
            let cmdline = hc.test[1..].join(" ");
            run_nsenter_shell(pid, timeout, &cmdline).await
        }
        "CMD" => run_nsenter_exec(pid, timeout, &hc.test[1], &hc.test[2..]).await,
        _ => {
            // Bare list: treat first element as the binary, remainder as
            // args. Matches docker's "test is just argv" fallback.
            run_nsenter_exec(pid, timeout, &hc.test[0], &hc.test[1..]).await
        }
    }
}

/// Spawn `nsenter -t <pid> -m -u -i -n -p -- <prog> <args...>` and
/// translate exit code into [`HealthState`]. Times out per `hc.timeout`.
async fn run_nsenter_exec(
    pid: u32,
    timeout: std::time::Duration,
    prog: &str,
    args: &[String],
) -> Result<HealthState, RuntimeError> {
    let mut cmd = tokio::process::Command::new("nsenter");
    cmd.arg("-t")
        .arg(pid.to_string())
        .args(["-m", "-u", "-i", "-n", "-p"])
        .arg("--")
        .arg(prog)
        .args(args);
    let fut = cmd.output();
    let output = match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(RuntimeError::Healthcheck(format!("nsenter spawn: {e}"))),
        Err(_) => return Err(RuntimeError::Healthcheck("timeout".into())),
    };
    if output.status.success() {
        Ok(HealthState::Healthy)
    } else {
        Ok(HealthState::Unhealthy)
    }
}

/// `nsenter ... -- /bin/sh -c "<command>"` flavour. Same five-namespace
/// entry as [`run_nsenter_exec`].
async fn run_nsenter_shell(
    pid: u32,
    timeout: std::time::Duration,
    command: &str,
) -> Result<HealthState, RuntimeError> {
    let mut cmd = tokio::process::Command::new("nsenter");
    cmd.arg("-t")
        .arg(pid.to_string())
        .args(["-m", "-u", "-i", "-n", "-p"])
        .args(["--", "/bin/sh", "-c"])
        .arg(command);
    let fut = cmd.output();
    let output = match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(RuntimeError::Healthcheck(format!("nsenter spawn: {e}"))),
        Err(_) => return Err(RuntimeError::Healthcheck("timeout".into())),
    };
    if output.status.success() {
        Ok(HealthState::Healthy)
    } else {
        Ok(HealthState::Unhealthy)
    }
}

/// Read from `path` starting at `*offset` to EOF, advancing `*offset`
/// past whatever was read. Returns the bytes; an empty Vec if the file
/// is missing or the offset is already at EOF. We tolerate ENOENT
/// because the wisp lifecycle creates the log files lazily on
/// `start_container`; callers may invoke `stream_logs` before the
/// container has started and we want a clean empty backfill in that
/// case rather than an error.
fn read_tail(path: &Path, offset: &mut u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => return Vec::new(),
    };
    if file.seek(SeekFrom::Start(*offset)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let n = match file.read_to_end(&mut buf) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    *offset += n as u64;
    buf
}

/// Trim `bytes` to its last `tail_lines` newline-terminated segments.
/// Used by [`inotify_tail_logs`] when LogOptions.tail is set: only the
/// final N lines from the backfill phase are emitted, matching docker /
/// kubectl `--tail`.
///
/// Treats a trailing newline as the terminator of the last line (not
/// an empty extra line); a buffer without a trailing newline still
/// counts its dangling-final segment as one line.
fn last_n_lines(bytes: &[u8], tail_lines: u32) -> Vec<u8> {
    if tail_lines == 0 {
        return Vec::new();
    }
    let want = tail_lines as usize;
    // Strip a single trailing `\n` for counting purposes so the final
    // line and a dangling-no-trailing-newline final line are treated
    // uniformly.
    let scan_end = if bytes.last() == Some(&b'\n') {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    let scan = &bytes[..scan_end];
    // Walk backwards counting newlines. Each newline within `scan`
    // closes one prior line; once we've passed `want - 1` of them, the
    // next byte after that newline is the cut point for the last
    // `want` lines.
    let mut count = 0usize;
    let mut cut = 0usize;
    for (i, b) in scan.iter().enumerate().rev() {
        if *b == b'\n' {
            count += 1;
            if count >= want {
                cut = i + 1;
                break;
            }
        }
    }
    bytes[cut..].to_vec()
}

/// Wave 3.B: source of per-container exit codes, used by
/// [`cgroup_events_loop`] to enrich Die events.
///
/// A trait so tests can substitute a fake without spinning up a real
/// `wisp::Runtime`. Production wires this to
/// [`WispExitCodeSource`], which calls `Runtime::state(id)`: that's
/// the call that triggers the lazy `/proc/<pid>` check + reads the
/// per-container `exit_status` file the lifecycle reaper writes.
trait ExitCodeSource: Send + Sync + 'static {
    /// Best-effort lookup of `id`'s exit code. Returns `None` when:
    /// - the runtime has no record of `id` (e.g. test fakes that
    ///   don't model the container)
    /// - the reaper hasn't written `exit_status` yet (race between
    ///   `/proc/<pid>` going away and the reaper's tick)
    /// - any IO / state read error: we'd rather emit a Die with
    ///   `None` than block the event loop on a transient read.
    fn exit_code(&self, id: &str) -> Option<i32>;
}

/// Production [`ExitCodeSource`] backed by the real wisp runtime.
struct WispExitCodeSource(Arc<wisp::Runtime>);

impl ExitCodeSource for WispExitCodeSource {
    fn exit_code(&self, id: &str) -> Option<i32> {
        self.0.state(id).ok().and_then(|h| h.exit_code)
    }
}

/// Wave 3.B: parse the kernel's `cgroup.events` file content and
/// return whether the cgroup is populated (has at least one process).
///
/// Format (cgroup v2, kernel >= 4.15):
/// ```text
/// populated <0|1>
/// frozen <0|1>          # kernel >= 5.2
/// ```
/// The fields are space-separated; any unknown lines are ignored.
/// Returns `None` when the `populated` line is missing or unparseable
/// (caller treats this as "no state change to emit").
fn parse_cgroup_events(content: &str) -> Option<bool> {
    for line in content.lines() {
        let mut parts = line.split_ascii_whitespace();
        match (parts.next(), parts.next()) {
            (Some("populated"), Some("1")) => return Some(true),
            (Some("populated"), Some("0")) => return Some(false),
            _ => {}
        }
    }
    None
}

/// Best-effort read + parse of `<dir>/cgroup.events`. Returns `None`
/// when the file is missing (container dir was just removed) or
/// unreadable (transient IO error: the next notify event will retry).
fn read_cgroup_events(dir: &Path) -> Option<bool> {
    let content = std::fs::read_to_string(dir.join("cgroup.events")).ok()?;
    parse_cgroup_events(&content)
}

/// Wave 3.B: notify-driven event emitter. Replaces the Phase 0.4 2s
/// poll loop with an inotify-backed watcher on `<cgroup_root>` so
/// container state changes surface within ~milliseconds rather than
/// up to 2s after the kernel saw them.
///
/// Mechanics:
/// 1. Initial sweep: walks `<cgroup_root>/*/cgroup.events`, records
///    each container's last-seen populated bit, and emits a Start for
///    every cgroup that's already populated (covers agent restart
///    with running containers).
/// 2. Recursive notify watcher on `<cgroup_root>` catches:
///    - Modify events on `<id>/cgroup.events` -> re-read, compare,
///      emit Start (populated 0 -> 1) or Die (populated 1 -> 0)
///    - Remove events on `<id>` or `<id>/cgroup.events` -> emit Stop
///      (the wisp lifecycle removes the cgroup dir after reap)
/// 3. On Die: the exit code comes from `source.exit_code(id)`. The
///    reaper writes `exit_status` shortly after PID 1 exits but it
///    can lag the kernel's `populated 0` write by a few hundred ms;
///    we retry the lookup up to `EXIT_CODE_BACKFILL_BUDGET` to give
///    the reaper time to land the file before the Die event fires.
///
/// The loop runs forever; the spawned task is dropped when
/// `WispBackend` itself drops (the broadcast Sender goes away).
///
/// Linux-only mechanics: cgroup.events is a kernel pseudo-file only
/// on Linux. On Mac the cgroup root won't exist, so the watcher
/// returns an error from notify::watch and the loop exits cleanly:
/// the agent's wisp backend is itself linux-only past `create`, so
/// the only state changes that could happen on Mac come from
/// `start_container` / `stop_container` directly emitting events.
async fn cgroup_events_loop<S: ExitCodeSource>(
    cgroup_root: PathBuf,
    source: S,
    event_tx: tokio::sync::broadcast::Sender<RuntimeEvent>,
) {
    // Canonicalize so paths returned by notify (which canonicalize on
    // both inotify and FSEvents) line up with the prefix we strip when
    // identifying the container id. Without this, macOS's
    // `/var/folders/...` -> `/private/var/folders/...` symlink resolution
    // makes `strip_prefix` fail and every event gets dropped.
    let cgroup_root = std::fs::canonicalize(&cgroup_root).unwrap_or(cgroup_root);

    // 1. Initial sweep so a restart-with-running-containers agent
    //    doesn't lose state. populated_state[id] = last-seen bool.
    let mut populated_state: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    if let Ok(entries) = std::fs::read_dir(&cgroup_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let id = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !path.is_dir() {
                continue;
            }
            if let Some(populated) = read_cgroup_events(&path) {
                populated_state.insert(id.clone(), populated);
                if populated {
                    let _ = event_tx.send(RuntimeEvent {
                        container_id: id,
                        event_type: RuntimeEventType::Start,
                        timestamp: SystemTime::now(),
                    });
                }
            }
        }
    }

    // 2. Set up the notify watcher. The mpsc channel bridges notify's
    //    sync callback to our async loop. UnboundedSender::send never
    //    blocks; events that arrive while we're processing a previous
    //    one queue up cleanly.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "cgroup events watcher: failed to construct, falling back to no event stream"
                );
                return;
            }
        };
    if let Err(e) =
        notify::Watcher::watch(&mut watcher, &cgroup_root, notify::RecursiveMode::Recursive)
    {
        // On Mac the cgroup root doesn't exist; on Linux without
        // cgroup v2 it might not either. Either way: nothing to
        // watch, exit cleanly. The agent's lifecycle hooks still
        // emit Start/Stop events directly.
        tracing::debug!(
            cgroup_root = %cgroup_root.display(),
            error = %e,
            "cgroup events watcher: watch failed, event loop exiting"
        );
        return;
    }

    // 3. Event-processing loop. Each notify event tells us a path
    //    changed; we re-read that container's cgroup.events file and
    //    compare populated against last seen.
    while let Some(event) = rx.recv().await {
        for path in &event.paths {
            // Identify the container dir: skip events that don't fall
            // under cgroup_root or aren't at depth 1 (cgroup.events
            // lives at `<root>/<id>/cgroup.events`, so we care about
            // depth-1 directories and their direct children).
            let rel = match path.strip_prefix(&cgroup_root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let id = match rel.components().next() {
                Some(std::path::Component::Normal(n)) => match n.to_str() {
                    Some(s) => s.to_string(),
                    None => continue,
                },
                _ => continue,
            };
            let container_dir = cgroup_root.join(&id);

            // Remove events: container dir going away. Emit Stop if
            // we ever saw it populated; either way, forget it.
            if matches!(event.kind, notify::EventKind::Remove(_)) && !container_dir.exists() {
                if populated_state.remove(&id).is_some() {
                    let _ = event_tx.send(RuntimeEvent {
                        container_id: id.clone(),
                        event_type: RuntimeEventType::Stop,
                        timestamp: SystemTime::now(),
                    });
                }
                continue;
            }

            // For Create/Modify, read the current populated bit.
            let now_populated = match read_cgroup_events(&container_dir) {
                Some(p) => p,
                None => continue,
            };
            let was_populated = populated_state.get(&id).copied();
            match (was_populated, now_populated) {
                (None, true) | (Some(false), true) => {
                    populated_state.insert(id.clone(), true);
                    let _ = event_tx.send(RuntimeEvent {
                        container_id: id.clone(),
                        event_type: RuntimeEventType::Start,
                        timestamp: SystemTime::now(),
                    });
                }
                (Some(true), false) => {
                    populated_state.insert(id.clone(), false);
                    // The reaper writes `exit_status` shortly after
                    // PID 1 exits but the kernel's populated flip can
                    // race ahead by a few hundred ms. Retry the
                    // lookup briefly so the Die event carries the
                    // code in the common case.
                    let exit_code = wait_for_exit_code(&source, &id).await;
                    let _ = event_tx.send(RuntimeEvent {
                        container_id: id.clone(),
                        event_type: RuntimeEventType::Die { exit_code },
                        timestamp: SystemTime::now(),
                    });
                }
                (None, false) => {
                    // First sight of an empty cgroup: record state
                    // but don't emit (no transition happened).
                    populated_state.insert(id.clone(), false);
                }
                _ => {}
            }
        }
    }
}

/// Maximum time `cgroup_events_loop` will wait for `exit_status` to
/// land on disk before emitting a Die with `exit_code: None`. The
/// reaper polls at 500ms so 600ms covers the worst case without
/// holding the event loop hostage on a stuck reaper.
const EXIT_CODE_BACKFILL_BUDGET: std::time::Duration = std::time::Duration::from_millis(600);

/// Poll `source.exit_code(id)` up to [`EXIT_CODE_BACKFILL_BUDGET`],
/// returning the first `Some` seen or `None` on timeout.
async fn wait_for_exit_code<S: ExitCodeSource>(source: &S, id: &str) -> Option<i32> {
    let deadline = tokio::time::Instant::now() + EXIT_CODE_BACKFILL_BUDGET;
    loop {
        if let Some(code) = source.exit_code(id) {
            return Some(code);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// State carried across [`futures::stream::unfold`] poll calls for the
/// inotify-backed log tail.
///
/// We use unfold rather than `async_stream` (not in our deps) so the
/// watcher's lifetime can be threaded through each poll cleanly. The
/// `RecommendedWatcher` is kept alive in `_watcher` until the consumer
/// drops the stream; dropping releases the inotify subscription.
struct LogTailState {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout_offset: u64,
    stderr_offset: u64,
    /// Backfill items emitted before we start receiving live events.
    /// Each `Some(LogChunk)` is yielded immediately; once empty we
    /// transition into the watcher-event loop.
    backfill: std::collections::VecDeque<LogChunk>,
    follow: bool,
    /// Receiver fed by the `notify` callback. Each path hint is one of
    /// `stdout_path` / `stderr_path` (other paths are filtered in the
    /// callback before being sent).
    events: tokio::sync::mpsc::UnboundedReceiver<PathBuf>,
    /// Held to keep the watcher alive for the lifetime of the stream.
    /// Dropped when the consumer drops the unfold stream.
    _watcher: Option<notify::RecommendedWatcher>,
}

/// Stream backfill + inotify-tail of stdout / stderr log files.
///
/// Both files are watched via a single `notify::recommended_watcher`;
/// each Modify / Create event on the matching path triggers a
/// read-from-offset and emits a [`LogChunk`] tagged with the
/// [`LogSource`].
///
/// The watcher is sync; we bridge to async via a tokio
/// `mpsc::unbounded_channel`. The `RecommendedWatcher` value lives in
/// the unfold state and drops when the consumer stops polling.
///
/// LogOptions semantics (Phase 0.4):
/// - `tail`: cap the backfill phase to the last N lines.
/// - `follow`: when false, the stream completes after backfill.
/// - `since_seconds` / `timestamps`: not honored in 0.4 (wisp doesn't
///   write per-line timestamps; documented limitation).
fn inotify_tail_logs(
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    opts: LogOptions,
) -> impl Stream<Item = LogChunk> + Send {
    // Phase 1: backfill. Read whatever's on disk before arming the
    // watcher so events that arrive between read + watch can't be
    // missed.
    let mut stdout_offset: u64 = 0;
    let mut stderr_offset: u64 = 0;
    let mut stdout_initial = read_tail(&stdout_path, &mut stdout_offset);
    let mut stderr_initial = read_tail(&stderr_path, &mut stderr_offset);
    if let Some(tail_n) = opts.tail {
        stdout_initial = last_n_lines(&stdout_initial, tail_n);
        stderr_initial = last_n_lines(&stderr_initial, tail_n);
    }
    let mut backfill = std::collections::VecDeque::new();
    if !stdout_initial.is_empty() {
        backfill.push_back(LogChunk {
            source: LogSource::Stdout,
            bytes: bytes::Bytes::from(stdout_initial),
        });
    }
    if !stderr_initial.is_empty() {
        backfill.push_back(LogChunk {
            source: LogSource::Stderr,
            bytes: bytes::Bytes::from(stderr_initial),
        });
    }

    // Phase 2: arm the watcher (only when follow=true). The callback
    // forwards path hints through an unbounded mpsc; the unfold loop
    // below drains them.
    //
    // We canonicalize both the parent dir and the per-file paths
    // before storing them as match keys: macOS FSEvents reports
    // `/private/var/folders/...` even when we hand it `/var/folders/...`
    // (the latter is a symlink), so the path comparison in the
    // callback would otherwise miss every event in tempdir-backed
    // tests. Canonicalize is a noop on Linux when no symlinks
    // intervene.
    let parent = stdout_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"));
    let _ = std::fs::create_dir_all(&parent);
    let parent_canon = std::fs::canonicalize(&parent).unwrap_or(parent);
    let stdout_canon = std::fs::canonicalize(&stdout_path).unwrap_or_else(|_| stdout_path.clone());
    let stderr_canon = std::fs::canonicalize(&stderr_path).unwrap_or_else(|_| stderr_path.clone());

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
    let stdout_path_cb = stdout_canon.clone();
    let stderr_path_cb = stderr_canon.clone();
    let mut watcher_opt: Option<notify::RecommendedWatcher> = None;
    if opts.follow {
        if let Ok(mut w) = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let event = match res {
                Ok(e) => e,
                Err(_) => return,
            };
            if !matches!(
                event.kind,
                notify::EventKind::Modify(_) | notify::EventKind::Create(_)
            ) {
                return;
            }
            for p in event.paths {
                if p == stdout_path_cb {
                    let _ = tx.send(stdout_path_cb.clone());
                } else if p == stderr_path_cb {
                    let _ = tx.send(stderr_path_cb.clone());
                }
            }
        }) {
            if notify::Watcher::watch(&mut w, &parent_canon, notify::RecursiveMode::NonRecursive)
                .is_ok()
            {
                watcher_opt = Some(w);
            }
        }
    }

    let state = LogTailState {
        stdout_path: stdout_canon,
        stderr_path: stderr_canon,
        stdout_offset,
        stderr_offset,
        backfill,
        follow: opts.follow,
        events: rx,
        _watcher: watcher_opt,
    };

    futures::stream::unfold(state, |mut s| async move {
        // 1. Drain pre-buffered backfill chunks first.
        if let Some(chunk) = s.backfill.pop_front() {
            return Some((chunk, s));
        }
        // 2. End of stream when we're not following.
        if !s.follow {
            return None;
        }
        // 3. Wait for the next inotify hit; stop if the channel closed.
        loop {
            let path = s.events.recv().await?;
            let (source, offset_field, target_path) = if path == s.stdout_path {
                (
                    LogSource::Stdout,
                    &mut s.stdout_offset,
                    s.stdout_path.clone(),
                )
            } else if path == s.stderr_path {
                (
                    LogSource::Stderr,
                    &mut s.stderr_offset,
                    s.stderr_path.clone(),
                )
            } else {
                continue;
            };
            let delta = read_tail(&target_path, offset_field);
            if delta.is_empty() {
                // Spurious event (e.g. directory metadata touched) -
                // keep waiting.
                continue;
            }
            let chunk = LogChunk {
                source,
                bytes: bytes::Bytes::from(delta),
            };
            return Some((chunk, s));
        }
    })
}

/// Read a [`NetworkRegistryEntry`] from `path`. Returns `Ok(None)`
/// when the file is missing OR when its bytes fail to parse (treat
/// corrupt files as "first ensure" so the caller can rewrite). Real
/// IO errors (permission denied, etc.) propagate as
/// [`RuntimeError::Io`].
#[allow(dead_code)]
fn read_network_registry(path: &Path) -> Result<Option<NetworkRegistryEntry>, RuntimeError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<NetworkRegistryEntry>(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RuntimeError::Io(e)),
    }
}

/// Atomic write of a [`NetworkRegistryEntry`] to `path`. Creates the
/// parent directory chain if it doesn't already exist. Uses the
/// write-then-rename idiom so a concurrent reader either sees the
/// pre-existing contents or the fully-written new entry, never a
/// torn file.
#[allow(dead_code)]
fn write_network_registry(path: &Path, entry: &NetworkRegistryEntry) -> Result<(), RuntimeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(entry)
        .map_err(|e| RuntimeError::Network(format!("serialize network registry: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::spec::{ContainerCreateSpec, MountKind, MountSpec, RestartPolicy};
    use std::collections::BTreeMap;

    fn empty_spec(name: &str, image: &str) -> ContainerCreateSpec {
        ContainerCreateSpec {
            container_name: name.to_string(),
            image: image.to_string(),
            stack: "stack".into(),
            service: "svc".into(),
            command: None,
            entrypoint: None,
            env: BTreeMap::new(),
            labels: BTreeMap::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            networks: Vec::new(),
            restart: RestartPolicy::No,
            healthcheck: None,
            user: None,
            working_dir: None,
            hostname: None,
            linux_resources: None,
            secrets: Vec::new(),
        }
    }

    #[test]
    fn spec_to_config_overrides_round_trips_basic_fields() {
        let mut s = empty_spec("c", "alpine:3.19");
        s.command = Some(vec!["/bin/sh".into(), "-c".into(), "echo hi".into()]);
        s.entrypoint = Some(vec!["/usr/bin/env".into()]);
        s.working_dir = Some("/app".into());
        s.hostname = Some("myhost".into());

        let o = spec_to_config_overrides(&s);
        assert_eq!(
            o.args,
            Some(vec!["/bin/sh".into(), "-c".into(), "echo hi".into()])
        );
        assert_eq!(o.entrypoint, Some(vec!["/usr/bin/env".into()]));
        assert_eq!(o.cwd.as_deref(), Some("/app"));
        assert_eq!(o.hostname.as_deref(), Some("myhost"));
        assert!(o.linux_resources.is_none());
        assert!(o.mounts.is_empty());
    }

    #[test]
    fn spec_to_config_overrides_translates_env_to_key_equals_value() {
        let mut s = empty_spec("c", "alpine:3.19");
        s.env.insert("FOO".into(), "bar".into());
        s.env.insert("BAZ".into(), "qux".into());
        let o = spec_to_config_overrides(&s);
        // BTreeMap iteration is alphabetised; env vec mirrors that.
        assert_eq!(o.env, vec!["BAZ=qux".to_string(), "FOO=bar".to_string()]);
    }

    #[test]
    fn spec_to_network_spec_returns_none_when_no_network() {
        let s = empty_spec("c", "alpine:3.19");
        assert!(spec_to_network_spec(&s).is_none());
    }

    #[test]
    fn spec_to_network_spec_picks_first_network_as_primary() {
        let mut s = empty_spec("c", "alpine:3.19");
        s.networks = vec!["app".into(), "db".into(), "logs".into()];
        let n = spec_to_network_spec(&s).expect("first network is primary");
        assert_eq!(n.network_name, "app");
        assert!(n.ports.is_empty());
    }

    #[test]
    fn spec_to_network_spec_synthesises_default_when_only_ports_declared() {
        // An operator that publishes ports without naming a network gets
        // the wisp-default network; this matches docker's behavior of
        // attaching to the default bridge when --network is unspecified.
        let mut s = empty_spec("c", "alpine:3.19");
        s.ports = vec![PortSpec {
            host_ip: None,
            host_port: 18080,
            container_port: 80,
            protocol: SpecPortProtocol::Tcp,
        }];
        let n = spec_to_network_spec(&s).expect("ports imply a network");
        assert_eq!(n.network_name, "wisp-default");
        assert_eq!(n.ports.len(), 1);
        assert_eq!(n.ports[0].host_port, 18080);
    }

    #[test]
    fn mount_spec_to_oci_translates_bind_mount() {
        let m = MountSpec {
            source: "/host/data".into(),
            target: "/data".into(),
            kind: MountKind::Bind,
            read_only: true,
        };
        let oci = mount_spec_to_oci(&m);
        assert_eq!(
            oci.destination(),
            &std::path::PathBuf::from("/data"),
            "destination"
        );
        assert_eq!(oci.typ().as_deref(), Some("bind"), "typ");
        assert_eq!(
            oci.source().as_ref().map(|p| p.to_path_buf()),
            Some(std::path::PathBuf::from("/host/data")),
            "source"
        );
        let opts = oci.options().clone().unwrap_or_default();
        assert!(opts.contains(&"bind".to_string()));
        assert!(opts.contains(&"ro".to_string()));
    }

    #[test]
    fn mount_spec_to_oci_translates_tmpfs_mount() {
        let m = MountSpec {
            source: "tmpfs".into(),
            target: "/tmp".into(),
            kind: MountKind::Tmpfs,
            read_only: false,
        };
        let oci = mount_spec_to_oci(&m);
        assert_eq!(oci.typ().as_deref(), Some("tmpfs"));
        assert_eq!(oci.destination(), &std::path::PathBuf::from("/tmp"));
        let opts = oci.options().clone().unwrap_or_default();
        // Tmpfs mounts shouldn't carry the `bind` option.
        assert!(!opts.contains(&"bind".to_string()));
        assert!(!opts.contains(&"ro".to_string()));
    }

    #[test]
    fn wisp_backend_reads_cap_add_label() {
        // Phase 0.5: agent containers opt in to a richer cap set by
        // setting `isengard.cap.add=CAP_CHOWN,CAP_SETUID,...` on the
        // spec's labels. The override fans the same list to all five
        // OCI sets (matching wisp-cli's --cap-add).
        let mut s = empty_spec("c", "nginx:alpine");
        s.labels.insert(
            "isengard.cap.add".into(),
            "CAP_CHOWN,CAP_SETUID,CAP_SETGID,CAP_DAC_OVERRIDE,CAP_FOWNER,CAP_SETPCAP".into(),
        );
        let o = spec_to_config_overrides(&s);
        let cap_override = o.capabilities.expect("cap override present");
        assert_eq!(cap_override.bounding.len(), 6);
        assert!(cap_override.bounding.contains(&"CAP_CHOWN".to_string()));
        assert!(cap_override.bounding.contains(&"CAP_SETPCAP".to_string()));
        // Same list across all five sets.
        assert_eq!(cap_override.bounding, cap_override.effective);
        assert_eq!(cap_override.bounding, cap_override.permitted);
        assert_eq!(cap_override.bounding, cap_override.inheritable);
        assert_eq!(cap_override.bounding, cap_override.ambient);
    }

    #[test]
    fn wisp_backend_no_cap_label_keeps_defaults() {
        // Without the label, `capabilities` stays `None` so the bundle
        // synthesiser falls back to its default cap allow-list.
        let s = empty_spec("c", "alpine:3.19");
        let o = spec_to_config_overrides(&s);
        assert!(o.capabilities.is_none());
    }

    #[test]
    fn wisp_backend_empty_cap_label_keeps_defaults() {
        // An empty / whitespace-only label is treated the same as no
        // label: don't emit an override that would replace the default
        // set with an empty allow-list.
        let mut s = empty_spec("c", "alpine:3.19");
        s.labels.insert("isengard.cap.add".into(), " , ,".into());
        let o = spec_to_config_overrides(&s);
        assert!(o.capabilities.is_none());
    }

    /// Phase 0.5: ports declared with no `--network` selection should
    /// still imply attachment to `wisp-default`, matching the wisp-cli
    /// `--port` semantics.
    #[test]
    fn auto_create_default_network_when_port_without_network() {
        let mut s = empty_spec("c", "alpine:3.19");
        s.ports = vec![PortSpec {
            host_ip: None,
            host_port: 18080,
            container_port: 80,
            protocol: SpecPortProtocol::Tcp,
        }];
        // Networks vec is empty: only ports declared.
        assert!(s.networks.is_empty());
        let n = spec_to_network_spec(&s).expect("ports imply a network");
        assert_eq!(n.network_name, "wisp-default");
        assert_eq!(n.ports.len(), 1);
    }

    /// Phase 0.5: the on-disk registry round-trips a full
    /// [`NetworkRegistryEntry`] across read / write so subsequent
    /// `ensure_bridge` calls pick up the same subnet without
    /// recomputing from environment.
    #[test]
    fn network_registry_persists_subnet() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nets/wisp-default/network.json");
        // First read: missing -> Ok(None).
        assert!(read_network_registry(&path).unwrap().is_none());

        let entry = NetworkRegistryEntry {
            name: "wisp-default".into(),
            subnet: "10.83.0.0/24".into(),
            gateway: "10.83.0.1".parse().unwrap(),
            bridge: "wbr-wisp-default".into(),
        };
        write_network_registry(&path, &entry).unwrap();
        let back = read_network_registry(&path).unwrap().expect("persisted");
        assert_eq!(back, entry);
        // Re-write with the same contents is a no-op (idempotent).
        write_network_registry(&path, &entry).unwrap();
        let back = read_network_registry(&path).unwrap().expect("persisted");
        assert_eq!(back, entry);
    }

    /// Phase 0.5: a corrupt registry file is treated as "first
    /// ensure" so the caller can rewrite it. This keeps the agent
    /// from getting wedged after a partial-write crash; real IO
    /// errors (permission denied, ...) still propagate.
    #[test]
    fn network_registry_corrupt_file_treated_as_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("network.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        let res = read_network_registry(&path).unwrap();
        assert!(res.is_none(), "corrupt file should be skipped");
    }

    /// Phase 0.5: the `ensure_bridge` helper rejects a subnet hint
    /// that conflicts with an already-persisted entry. Linux only
    /// because the helper itself touches `wisp_net::bridge::ensure`;
    /// we exercise the conflict path on Mac by writing a registry
    /// entry by hand and asserting the helper short-circuits before
    /// the syscall.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "needs root + linux + cgroup v2"]
    async fn network_registry_conflict_on_subnet_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = WispBackend::from_env(tmp.path()).await.unwrap();

        // Pre-seed the registry with one subnet.
        let path = tmp.path().join("networks/wisp-default/network.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let entry = NetworkRegistryEntry {
            name: "wisp-default".into(),
            subnet: "10.83.0.0/24".into(),
            gateway: "10.83.0.1".parse().unwrap(),
            bridge: "wbr-wisp-default".into(),
        };
        write_network_registry(&path, &entry).unwrap();

        // Caller passes a different subnet: surface a Network error.
        let err = backend
            .ensure_bridge("wisp-default", Some("10.99.0.0/24"))
            .unwrap_err();
        match err {
            RuntimeError::Network(msg) => {
                assert!(msg.contains("already exists"), "got: {msg}");
                assert!(msg.contains("10.83.0.0/24"), "got: {msg}");
            }
            other => panic!("expected Network error, got {other:?}"),
        }

        tokio::task::spawn_blocking(move || drop(backend))
            .await
            .unwrap();
    }

    #[test]
    fn linux_resources_to_oci_translates_all_fields() {
        let r = LinuxResources {
            memory_max_bytes: Some(512 * 1024 * 1024),
            memory_swap_max_bytes: Some(1024 * 1024 * 1024),
            cpu_quota_us: Some(50_000),
            cpu_period_us: Some(100_000),
            cpu_shares: Some(1024),
            pids_max: Some(2048),
        };
        let oci = linux_resources_to_oci(&r);
        let mem = oci.memory().expect("memory present");
        assert_eq!(mem.limit(), Some(512 * 1024 * 1024));
        assert_eq!(mem.swap(), Some(1024 * 1024 * 1024));
        let cpu = oci.cpu().clone().expect("cpu present");
        assert_eq!(cpu.quota(), Some(50_000));
        assert_eq!(cpu.period(), Some(100_000));
        assert_eq!(cpu.shares(), Some(1024));
        let pids = oci.pids().expect("pids present");
        assert_eq!(pids.limit(), 2048);
    }

    /// Integration-ish test for the persisted-spec round-trip. Exercises
    /// [`WispBackend::persist_spec`] + [`WispBackend::read_spec`] without
    /// touching the wisp runtime (which on Mac would error on
    /// create_container the moment it tried to clone3). Constructs a
    /// WispBackend manually so we don't need a real cgroup tree.
    ///
    /// Drops the backend via spawn_blocking on test exit because
    /// [`wisp_image::Client`] holds a `reqwest::blocking::Client` whose
    /// internal tokio runtime cannot be dropped inside a tokio async
    /// context (panics: "Cannot drop a runtime in a context where
    /// blocking is not allowed"). Production agents drop the backend
    /// when the host process shuts down, where this isn't an issue.
    #[tokio::test]
    #[ignore = "needs root + linux + cgroup v2"]
    async fn wisp_backend_persist_then_read_spec_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = WispBackend::from_env(tmp.path())
            .await
            .expect("from_env on tempdir");

        let mut spec = empty_spec("ctr-1", "alpine:3.19");
        spec.labels.insert("isengard.stack".into(), "demo".into());
        spec.labels
            .insert("com.docker.compose.service".into(), "web".into());
        spec.command = Some(vec!["/bin/sh".into()]);
        spec.healthcheck = Some(super::HealthcheckSpec {
            test: vec!["CMD".into(), "true".into()],
            interval: std::time::Duration::from_secs(5),
            timeout: std::time::Duration::from_secs(2),
            retries: 3,
            start_period: std::time::Duration::from_secs(0),
        });

        backend.persist_spec("ctr-1", &spec).expect("persist");
        let read = backend.read_spec("ctr-1").expect("read");
        assert_eq!(read.container_name, "ctr-1");
        assert_eq!(read.image, "alpine:3.19");
        assert_eq!(read.labels.len(), 2);
        assert_eq!(read.command.as_deref(), Some(&["/bin/sh".to_string()][..]));
        assert!(read.healthcheck.is_some());

        // remove_spec is idempotent.
        backend.remove_spec("ctr-1").unwrap();
        backend.remove_spec("ctr-1").unwrap();
        let err = backend.read_spec("ctr-1").unwrap_err();
        match err {
            RuntimeError::Io(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected Io NotFound, got {other:?}"),
        }

        tokio::task::spawn_blocking(move || drop(backend))
            .await
            .unwrap();
    }

    /// Pure-translation: build the bundle by hand against a temp dir
    /// store + tempdir bundle, then verify the rootfs + config.json
    /// land in the right places. This proves the
    /// `spec_to_config_overrides + secret_mount_to_oci` plumbing wires
    /// up correctly without invoking the full wisp runtime.
    #[tokio::test]
    async fn wisp_backend_create_container_assembles_bundle() {
        // Hand-crafted PulledImage with no layers (rootfs ends up empty
        // but valid). We bypass `WispBackend::create_container` so the
        // wisp runtime's clone3-prep doesn't run; we only want to prove
        // the override translation lands on disk.
        use oci_spec::image::{ConfigBuilder, ImageConfigurationBuilder, RootFsBuilder};

        let store_tmp = tempfile::tempdir().unwrap();
        let bundle_tmp = tempfile::tempdir().unwrap();
        let store = wisp_image::ContentStore::new(store_tmp.path()).unwrap();

        let cfg = ConfigBuilder::default()
            .entrypoint(vec!["/bin/sh".to_string()])
            .build()
            .unwrap();
        let rootfs = RootFsBuilder::default()
            .typ("layers".to_string())
            .diff_ids(Vec::<String>::new())
            .build()
            .unwrap();
        let image_cfg = ImageConfigurationBuilder::default()
            .architecture(oci_spec::image::Arch::ARM64)
            .os(oci_spec::image::Os::Linux)
            .config(cfg)
            .rootfs(rootfs)
            .build()
            .unwrap();
        let pulled = wisp_image::PulledImage {
            r: "alpine:3.19".parse::<wisp_image::ImageRef>().unwrap(),
            manifest_digest: "sha256:abc".to_string(),
            config: image_cfg,
            layers: Vec::new(),
        };

        let bundle_dir = bundle_tmp.path().join("ctr-1");
        let mut spec = empty_spec("ctr-1", "alpine:3.19");
        spec.command = Some(vec!["/bin/echo".into(), "hi".into()]);
        spec.env.insert("FOO".into(), "bar".into());

        let overrides = spec_to_config_overrides(&spec);
        let builder = wisp_image::BundleBuilder::new(&pulled, &store, &bundle_dir);
        builder.assemble_rootfs().unwrap();
        builder.write_config(overrides).unwrap();

        assert!(bundle_dir.join("rootfs").exists(), "rootfs assembled");
        assert!(
            bundle_dir.join("config.json").exists(),
            "config.json written"
        );
        // Re-load the spec to make sure overrides landed.
        let reloaded =
            oci_spec::runtime::Spec::load(bundle_dir.join("config.json")).expect("spec load");
        let args = reloaded.process().as_ref().unwrap().args().clone().unwrap();
        // Image entrypoint /bin/sh + override args [/bin/echo hi].
        assert!(
            args.iter().any(|a| a == "/bin/echo"),
            "override args present: {args:?}"
        );
    }

    fn hc(test: Vec<&str>) -> super::HealthcheckSpec {
        super::HealthcheckSpec {
            test: test.into_iter().map(String::from).collect(),
            interval: std::time::Duration::from_secs(5),
            timeout: std::time::Duration::from_millis(500),
            retries: 3,
            start_period: std::time::Duration::from_secs(0),
        }
    }

    /// `["NONE"]` short-circuits to Healthy without touching nsenter.
    /// Verifies the docker-compatible "skip the check" semantics.
    ///
    /// Ignored by default: WispBackend::from_env requires writable cgroup v2
    /// (real /sys/fs/cgroup access), which CI runners and unprivileged dev
    /// environments don't have. Run as root on a wisp host:
    ///   cargo test -p isengard-agent --lib -- --ignored healthcheck
    #[tokio::test]
    #[ignore = "needs root + linux + cgroup v2"]
    async fn healthcheck_test_none_returns_healthy() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = WispBackend::from_env(tmp.path()).await.unwrap();
        let res = backend
            .run_healthcheck("any-id", &hc(vec!["NONE"]))
            .await
            .unwrap();
        assert_eq!(res, HealthState::Healthy);
        tokio::task::spawn_blocking(move || drop(backend))
            .await
            .unwrap();
    }

    /// Empty `test` is treated as "no healthcheck configured" -> Healthy.
    /// Matches docker's `inspect` output for containers without a
    /// HealthConfig.
    #[tokio::test]
    #[ignore = "needs root + linux + cgroup v2"]
    async fn healthcheck_test_empty_returns_healthy() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = WispBackend::from_env(tmp.path()).await.unwrap();
        let res = backend
            .run_healthcheck("any-id", &hc(Vec::new()))
            .await
            .unwrap();
        assert_eq!(res, HealthState::Healthy);
        tokio::task::spawn_blocking(move || drop(backend))
            .await
            .unwrap();
    }

    /// `CMD-SHELL` with no command string errors with a clear
    /// healthcheck-flavored error. Same for `CMD` with no argv.
    #[tokio::test]
    #[ignore = "needs root + linux + cgroup v2"]
    async fn healthcheck_invalid_test_shape_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = WispBackend::from_env(tmp.path()).await.unwrap();
        // CMD-SHELL with only the keyword: no command string.
        let err = backend
            .run_healthcheck("any-id", &hc(vec!["CMD-SHELL"]))
            .await
            .unwrap_err();
        match err {
            RuntimeError::Healthcheck(msg) => {
                assert!(
                    msg.contains("CMD-SHELL"),
                    "msg should mention CMD-SHELL: {msg}"
                );
            }
            other => panic!("expected Healthcheck error, got {other:?}"),
        }
        // CMD with only the keyword: no argv.
        let err = backend
            .run_healthcheck("any-id", &hc(vec!["CMD"]))
            .await
            .unwrap_err();
        match err {
            RuntimeError::Healthcheck(msg) => {
                assert!(msg.contains("CMD"), "msg should mention CMD: {msg}");
            }
            other => panic!("expected Healthcheck error, got {other:?}"),
        }
        tokio::task::spawn_blocking(move || drop(backend))
            .await
            .unwrap();
    }

    /// Phase 0.4 dispatch C2: existing log content backfills before
    /// the watcher starts. Pre-write into the wisp container layout
    /// so `stream_logs` finds something to read.
    #[tokio::test]
    #[ignore = "needs root + linux + cgroup v2"]
    async fn stream_logs_backfills_existing_file_content() {
        use futures_util::StreamExt;

        let tmp = tempfile::tempdir().unwrap();
        let backend = WispBackend::from_env(tmp.path()).await.unwrap();
        let cdir = tmp.path().join("wisp/containers/ctr-1");
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(cdir.join("stdout.log"), b"line-one\nline-two\n").unwrap();
        std::fs::write(cdir.join("stderr.log"), b"err-one\n").unwrap();

        // follow=false so the stream completes after backfill.
        let opts = LogOptions {
            follow: false,
            tail: None,
            since_seconds: None,
            timestamps: false,
        };
        let mut s = backend.stream_logs("ctr-1", opts);

        let mut got_stdout = Vec::new();
        let mut got_stderr = Vec::new();
        while let Some(chunk) = s.next().await {
            match chunk.source {
                LogSource::Stdout => got_stdout.extend_from_slice(&chunk.bytes),
                LogSource::Stderr => got_stderr.extend_from_slice(&chunk.bytes),
            }
        }

        assert_eq!(got_stdout, b"line-one\nline-two\n");
        assert_eq!(got_stderr, b"err-one\n");

        tokio::task::spawn_blocking(move || drop(backend))
            .await
            .unwrap();
    }

    /// Phase 0.4 dispatch C2: appended bytes after backfill arrive via
    /// inotify. Spawn the stream with follow=true, append a line,
    /// then assert we receive it within a generous timeout.
    #[tokio::test]
    #[ignore = "needs root + linux + cgroup v2"]
    async fn stream_logs_emits_appended_lines_on_modify() {
        use futures_util::StreamExt;
        use std::io::Write;
        use tokio::time::{Duration, timeout};

        let tmp = tempfile::tempdir().unwrap();
        let backend = WispBackend::from_env(tmp.path()).await.unwrap();
        let cdir = tmp.path().join("wisp/containers/ctr-2");
        std::fs::create_dir_all(&cdir).unwrap();
        // Pre-create the log files so the watcher's directory watch
        // picks up future writes immediately.
        std::fs::write(cdir.join("stdout.log"), b"").unwrap();
        std::fs::write(cdir.join("stderr.log"), b"").unwrap();

        let opts = LogOptions {
            follow: true,
            tail: None,
            since_seconds: None,
            timestamps: false,
        };
        let mut s = backend.stream_logs("ctr-2", opts);

        // Append after a longer delay so notify's FSEvents backend
        // (mac) / inotify (linux) has time to register the watch.
        // FSEvents on macOS takes ~500ms to settle in some kernel
        // versions; inotify on Linux is sub-millisecond. The 1.5s
        // delay is conservative for both.
        let stdout_path = cdir.join("stdout.log");
        let writer_handle = tokio::task::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&stdout_path)
                .unwrap();
            f.write_all(b"appended-line\n").unwrap();
            f.sync_all().unwrap();
        });

        // Wait up to 10s for the appended bytes (notify on Mac uses
        // FSEvents which has higher latency than Linux inotify).
        let chunk = timeout(Duration::from_secs(10), s.next())
            .await
            .expect("stream timed out")
            .expect("stream ended without emitting");
        assert_eq!(chunk.source, LogSource::Stdout);
        // FSEvents on Mac can coalesce write+attr-change events; the
        // delta we read may include the appended line plus possibly
        // more (zero-byte writes). We assert the line is present
        // rather than insisting on exact equality, since the test
        // really cares about "we got the bytes".
        let received = String::from_utf8_lossy(&chunk.bytes).to_string();
        assert!(
            received.contains("appended-line"),
            "expected appended-line in chunk, got {received:?}"
        );

        writer_handle.await.unwrap();
        // Drop the stream first to release the watcher, then drop the
        // backend off the runtime as the other tests do.
        drop(s);
        tokio::task::spawn_blocking(move || drop(backend))
            .await
            .unwrap();
    }

    /// Phase 0.4 dispatch C2: `last_n_lines` keeps the trailing N
    /// newline-terminated segments of a buffer (matches docker
    /// --tail).
    #[test]
    fn last_n_lines_returns_only_trailing_segments() {
        let buf = b"a\nb\nc\nd\n";
        assert_eq!(last_n_lines(buf, 2), b"c\nd\n".to_vec());
        assert_eq!(last_n_lines(buf, 0), Vec::<u8>::new());
        assert_eq!(last_n_lines(buf, 99), buf.to_vec());
        // No trailing newline: last "line" is the dangling segment.
        assert_eq!(last_n_lines(b"a\nb\nc", 1), b"c".to_vec());
    }

    /// Wave 3.B: parse the kernel-format `cgroup.events` content.
    /// Format is `populated <0|1>` followed by other key/value
    /// pairs we ignore. Whitespace and unknown lines must not
    /// confuse the parser.
    #[test]
    fn parse_cgroup_events_returns_populated_bit() {
        assert_eq!(parse_cgroup_events("populated 1\nfrozen 0\n"), Some(true));
        assert_eq!(parse_cgroup_events("populated 0\nfrozen 0\n"), Some(false));
        // Just populated, no frozen line (kernel < 5.2).
        assert_eq!(parse_cgroup_events("populated 1\n"), Some(true));
        // Out-of-order; populated still wins.
        assert_eq!(parse_cgroup_events("frozen 1\npopulated 0\n"), Some(false));
        // Trailing whitespace, multiple spaces.
        assert_eq!(parse_cgroup_events("populated  1  \n"), Some(true));
        // Missing populated -> None.
        assert_eq!(parse_cgroup_events("frozen 0\n"), None);
        // Garbage -> None.
        assert_eq!(parse_cgroup_events(""), None);
        assert_eq!(parse_cgroup_events("populated yes\n"), None);
    }

    /// Wave 3.B: a fake [`ExitCodeSource`] backed by a `HashMap` so
    /// tests can assert Die events carry the right exit code without
    /// spinning up a real `wisp::Runtime`.
    #[derive(Clone, Default)]
    struct FakeExitCodes(std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, i32>>>);

    impl FakeExitCodes {
        fn set(&self, id: &str, code: i32) {
            self.0.lock().unwrap().insert(id.to_string(), code);
        }
    }

    impl ExitCodeSource for FakeExitCodes {
        fn exit_code(&self, id: &str) -> Option<i32> {
            self.0.lock().unwrap().get(id).copied()
        }
    }

    /// Helper: write a `cgroup.events` file under
    /// `<root>/<id>/cgroup.events` with the given populated bit.
    fn write_cgroup_events(root: &Path, id: &str, populated: bool) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cgroup.events"),
            format!("populated {}\nfrozen 0\n", if populated { 1 } else { 0 }),
        )
        .unwrap();
    }

    /// Wave 3.B: a populated cgroup that already exists at startup
    /// produces a Start event during the initial sweep. Mirrors the
    /// "agent restart with running containers" case.
    #[tokio::test]
    async fn cgroup_events_initial_sweep_emits_start_for_populated() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_cgroup_events(&root, "ctr-a", true);

        let (tx, mut rx) = tokio::sync::broadcast::channel::<RuntimeEvent>(64);
        let _loop = tokio::spawn(cgroup_events_loop(root, FakeExitCodes::default(), tx));

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("recv timed out")
            .expect("recv");
        assert_eq!(event.container_id, "ctr-a");
        assert!(matches!(event.event_type, RuntimeEventType::Start));
    }

    /// Wave 3.B: creating a new `<root>/<id>/cgroup.events` with
    /// populated=1 after the loop is running fires a Start event via
    /// the notify watcher. This is the fast-cycling-container case
    /// the old 2s poll loop would miss.
    #[tokio::test]
    async fn cgroup_events_detects_start_on_populated_flip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        let (tx, mut rx) = tokio::sync::broadcast::channel::<RuntimeEvent>(64);
        let root_for_loop = root.clone();
        let _loop = tokio::spawn(cgroup_events_loop(
            root_for_loop,
            FakeExitCodes::default(),
            tx,
        ));

        // Give notify a beat to install the inotify/FSEvents watch.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        write_cgroup_events(&root, "ctr-b", true);

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("recv timed out (Start)")
            .expect("recv (Start)");
        assert_eq!(event.container_id, "ctr-b");
        assert!(matches!(event.event_type, RuntimeEventType::Start));
    }

    /// Wave 3.B: flipping `populated 1` -> `populated 0` in an
    /// existing file produces a Die event carrying the exit code
    /// the source returns.
    #[tokio::test]
    async fn cgroup_events_detects_die_with_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_cgroup_events(&root, "ctr-c", true);

        let exits = FakeExitCodes::default();
        exits.set("ctr-c", 42);

        let (tx, mut rx) = tokio::sync::broadcast::channel::<RuntimeEvent>(64);
        let root_for_loop = root.clone();
        let _loop = tokio::spawn(cgroup_events_loop(root_for_loop, exits, tx));

        // Drain the initial Start emitted by the sweep.
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("recv timed out (initial Start)")
            .expect("recv (initial Start)");
        assert!(matches!(first.event_type, RuntimeEventType::Start));

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        write_cgroup_events(&root, "ctr-c", false);

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("recv timed out (Die)")
            .expect("recv (Die)");
        assert_eq!(event.container_id, "ctr-c");
        match event.event_type {
            RuntimeEventType::Die { exit_code } => assert_eq!(exit_code, Some(42)),
            other => panic!("expected Die, got {other:?}"),
        }
    }

    /// Wave 3.B: when the reaper hasn't written `exit_status` yet
    /// (source returns `None` throughout the backfill budget), the
    /// Die event still fires but with `exit_code: None`. Documents
    /// the SIGKILL race the old loop also surfaced.
    #[tokio::test]
    async fn cgroup_events_die_carries_none_when_exit_code_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_cgroup_events(&root, "ctr-d", true);

        let (tx, mut rx) = tokio::sync::broadcast::channel::<RuntimeEvent>(64);
        let root_for_loop = root.clone();
        let _loop = tokio::spawn(cgroup_events_loop(
            root_for_loop,
            FakeExitCodes::default(),
            tx,
        ));

        // Drain Start.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        write_cgroup_events(&root, "ctr-d", false);

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("recv timed out (Die)")
            .expect("recv (Die)");
        match event.event_type {
            RuntimeEventType::Die { exit_code } => assert_eq!(exit_code, None),
            other => panic!("expected Die, got {other:?}"),
        }
    }

    /// Wave 3.B: removing the container's cgroup dir after the
    /// kernel saw it populated emits Stop. Mirrors
    /// `remove_container` cleanup.
    #[tokio::test]
    async fn cgroup_events_detects_stop_when_dir_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_cgroup_events(&root, "ctr-e", true);

        let (tx, mut rx) = tokio::sync::broadcast::channel::<RuntimeEvent>(64);
        let root_for_loop = root.clone();
        let _loop = tokio::spawn(cgroup_events_loop(
            root_for_loop,
            FakeExitCodes::default(),
            tx,
        ));

        // Drain initial Start.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        std::fs::remove_dir_all(root.join("ctr-e")).unwrap();

        // We expect Stop within a few hundred ms (notify+fsync) but
        // give the test a generous timeout to account for slower CI
        // FS event backends.
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("recv timed out (Stop)")
            .expect("recv (Stop)");
        assert_eq!(event.container_id, "ctr-e");
        assert!(matches!(event.event_type, RuntimeEventType::Stop));
    }

    /// Wave 3.B: fast-cycling container: start and die both happen
    /// inside a single 100ms window. The old 2s poll loop would
    /// observe only one of the two transitions; the notify-driven
    /// loop captures both. Asserts:
    /// 1. A Start event fires.
    /// 2. A Die event fires.
    /// 3. The total latency for both is well under 2s (the old
    ///    poll interval), proving the new loop is sub-poll-latency.
    #[tokio::test]
    async fn cgroup_events_catches_fast_cycle_both_events() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        let exits = FakeExitCodes::default();
        exits.set("ctr-fast", 0);

        let (tx, mut rx) = tokio::sync::broadcast::channel::<RuntimeEvent>(64);
        let root_for_loop = root.clone();
        let _loop = tokio::spawn(cgroup_events_loop(root_for_loop, exits, tx));

        // Let notify install the watcher.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let started_at = std::time::Instant::now();
        write_cgroup_events(&root, "ctr-fast", true);
        // Sleep < 100ms then flip to populated 0. The old 2s poll
        // would see only the final state on its next tick (~2s
        // later); this test asserts both events come out fast.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        write_cgroup_events(&root, "ctr-fast", false);

        let first = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("recv timed out (Start)")
            .expect("recv (Start)");
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("recv timed out (Die)")
            .expect("recv (Die)");

        assert!(matches!(first.event_type, RuntimeEventType::Start));
        assert!(matches!(second.event_type, RuntimeEventType::Die { .. }));

        // Both events landed well inside the old 2s poll window.
        // The exit-code backfill adds up to ~600ms but the kernel
        // populated=0 already happened, so the test ceiling is
        // tighter than the old loop would manage.
        let elapsed = started_at.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(2000),
            "fast-cycle events should land in <2s (was {elapsed:?})"
        );
    }

    /// Phase 0.4 dispatch C2: `read_tail` advances offsets and is
    /// tolerant of missing files. Backfill semantics rely on this.
    #[test]
    fn read_tail_advances_offset_and_tolerates_missing() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.log");
        let mut offset = 0u64;
        let bytes = read_tail(&path, &mut offset);
        assert!(bytes.is_empty());
        assert_eq!(offset, 0);

        let path = tmp.path().join("present.log");
        std::fs::write(&path, b"hello").unwrap();
        let mut offset = 0u64;
        let bytes = read_tail(&path, &mut offset);
        assert_eq!(bytes, b"hello".to_vec());
        assert_eq!(offset, 5);

        // Subsequent read at the same offset returns nothing until
        // someone appends.
        let bytes = read_tail(&path, &mut offset);
        assert!(bytes.is_empty());
        assert_eq!(offset, 5);

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"-more")
            .unwrap();
        let bytes = read_tail(&path, &mut offset);
        assert_eq!(bytes, b"-more".to_vec());
        assert_eq!(offset, 10);
    }
}
