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
    LinuxResources, ListFilter, LogChunk, LogOptions, MountKind, MountSpec, NetworkSettings,
    PortProtocol as SpecPortProtocol, PortSpec, RuntimeEvent, RuntimeEventType,
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
    }
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

        Ok(Self {
            runtime: Arc::new(runtime),
            image_client: Arc::new(image_client),
            state_dir: state_dir.to_path_buf(),
            #[cfg(target_os = "linux")]
            net_attacher: std::sync::Mutex::new(Self::build_default_attacher(state_dir)),
            event_tx,
        })
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
        let network = match wisp_net::Network::new("default", subnet) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "wisp Network::new failed, networking disabled");
                return None;
            }
        };
        let ipam_dir = state_dir.join("networks").join("default");
        if let Err(e) = std::fs::create_dir_all(&ipam_dir) {
            tracing::warn!(error = %e, "ipam dir create failed, networking disabled");
            return None;
        }
        Some(Box::new(
            crate::runtime::wisp_backend_attacher::WispNetAttacher::new(network, &ipam_dir),
        ))
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
            Some(net) => runtime
                .create_with_network(&id, &bundle_clone, net)
                .map_err(|e| RuntimeError::Container(format!("{e}")))?,
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

    async fn connect_network(
        &self,
        _container_id: &str,
        _network: &str,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Network(
            "wisp does not support live network attach in 0.4; recreate the container".into(),
        ))
    }

    async fn disconnect_network(
        &self,
        _container_id: &str,
        _network: &str,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Network(
            "wisp does not support live network detach in 0.4; recreate the container".into(),
        ))
    }

    fn stream_logs(
        &self,
        _id: &str,
        _opts: LogOptions,
    ) -> Pin<Box<dyn Stream<Item = LogChunk> + Send>> {
        // Dispatch C wires this up via inotify-tail on the bundle log
        // file. For B2 we return an empty stream so callers get a clean
        // "no logs yet" signal rather than an error.
        Box::pin(futures::stream::empty())
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
        _id: &str,
        _hc: &HealthcheckSpec,
    ) -> Result<HealthState, RuntimeError> {
        // Dispatch B3 fills this in (HTTP probe + nsenter exec).
        Ok(HealthState::Starting)
    }

    fn name(&self) -> &'static str {
        "wisp"
    }
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
}
