//! Phase 0.4 dispatch A3: BollardBackend, the dockerd-backed
//! [`super::RuntimeBackend`].
//!
//! The agent's existing compose / logs / deploy paths build bollard
//! `Config<String>`, `inspect_container` responses, etc. directly. A3
//! introduces this thin layer that translates the trait's
//! [`super::ContainerCreateSpec`] into bollard's native shape and back.
//!
//! Existing helpers in `compose_apply.rs`, `deployment/driver.rs`, etc.
//! are NOT moved into this file: they remain the canonical implementation
//! of their respective behaviors. A4 keeps those callers using the raw
//! `Arc<bollard::Docker>` (accessible via [`BollardBackend::docker`]) until
//! WispBackend forces them onto the trait in dispatch B. The methods
//! defined here are the surface used by lib.rs / sync.rs and by the
//! WispBackend translation that dispatch B will mirror.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::time::SystemTime;

use async_trait::async_trait;
use bollard::container::{
    CreateContainerOptions, InspectContainerOptions, ListContainersOptions, LogsOptions,
    RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::secret::{
    ContainerInspectResponse, ContainerSummary, HealthStatusEnum, HostConfig, PortBinding,
    RestartPolicy as BollardRestartPolicy, RestartPolicyNameEnum,
};
use bollard::system::EventsOptions;
use futures_util::{Stream, StreamExt};

use super::{
    ContainerCreateSpec, ContainerSnapshot, ContainerState, HealthState, HealthcheckSpec, HostPort,
    ListFilter, LogChunk, LogOptions, LogSource, NetworkSettings, PortProtocol, RestartPolicy,
    RuntimeBackend, RuntimeError, RuntimeEvent, RuntimeEventType,
};

/// Bollard-backed [`super::RuntimeBackend`]. Holds one shared
/// `Arc<bollard::Docker>` reused for every call.
#[derive(Debug)]
pub struct BollardBackend {
    pub(crate) docker: std::sync::Arc<bollard::Docker>,
    #[allow(dead_code)] // wisp backend will need this; bollard backend doesn't yet
    pub(crate) state_dir: std::path::PathBuf,
}

impl BollardBackend {
    /// Connect to dockerd via the local default endpoint (matches the
    /// existing agent.rs behavior).
    pub async fn from_env(state_dir: &Path) -> Result<Self, RuntimeError> {
        let docker = bollard::Docker::connect_with_local_defaults()
            .map_err(|e| RuntimeError::Docker(format!("connect_with_local_defaults: {e}")))?;
        Ok(Self {
            docker: std::sync::Arc::new(docker),
            state_dir: state_dir.to_path_buf(),
        })
    }

    /// Borrow the underlying bollard handle. Phase 0.4 dispatch A4 keeps
    /// internal call sites in compose_apply / deployment/driver using the
    /// raw handle; dispatch B replaces those paths against the trait.
    pub fn docker(&self) -> std::sync::Arc<bollard::Docker> {
        self.docker.clone()
    }
}

/// Translate a backend-agnostic [`ContainerCreateSpec`] into the bollard
/// `Config<String>` + [`HostConfig`] pair `create_container` consumes.
///
/// Mirrors the shape `compose_apply::build_create_config` produces today
/// for the fields that overlap (image, env, labels, ports, restart,
/// binds). Adds entry / cmd / user / working_dir / hostname / healthcheck
/// / linux_resources / secrets that compose_apply doesn't surface yet but
/// the trait callers (deployment driver, future wisp backend) need.
pub(crate) fn spec_to_config(
    spec: &ContainerCreateSpec,
) -> (bollard::container::Config<String>, Vec<String>) {
    // Env: KEY=VALUE pairs, alphabetised for determinism.
    let mut env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    env.sort();

    // Labels: keep the BTreeMap order; bollard takes a HashMap which we
    // translate trivially.
    let labels: HashMap<String, String> = spec
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Ports: build the port_bindings + exposed_ports tables.
    let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
    for p in &spec.ports {
        let proto = match p.protocol {
            PortProtocol::Tcp => "tcp",
            PortProtocol::Udp => "udp",
        };
        let key = format!("{}/{}", p.container_port, proto);
        port_bindings
            .entry(key.clone())
            .or_insert_with(|| Some(Vec::new()))
            .as_mut()
            .unwrap()
            .push(PortBinding {
                host_ip: p.host_ip.map(|ip| ip.to_string()),
                host_port: Some(p.host_port.to_string()),
            });
        exposed_ports.insert(key, HashMap::new());
    }

    // Binds: for Bind + Tmpfs mounts compose_apply already encodes as
    // `src:dst[:ro]` strings. Volume mounts go through host_config.mounts
    // (which we don't synthesise yet; reconciler doesn't either).
    let mut binds: Vec<String> = Vec::new();
    for m in &spec.mounts {
        let suffix = if m.read_only { ":ro" } else { "" };
        let bind = format!("{}:{}{}", m.source, m.target, suffix);
        binds.push(bind);
    }
    // Secrets land as bind-mounts of the agent-materialised tmpfs path.
    for s in &spec.secrets {
        binds.push(format!("{}:{}:ro", s.source, s.target.display()));
    }

    let restart_policy = match spec.restart {
        RestartPolicy::No => Some(BollardRestartPolicy {
            name: Some(RestartPolicyNameEnum::NO),
            maximum_retry_count: None,
        }),
        RestartPolicy::Always => Some(BollardRestartPolicy {
            name: Some(RestartPolicyNameEnum::ALWAYS),
            maximum_retry_count: None,
        }),
        RestartPolicy::UnlessStopped => Some(BollardRestartPolicy {
            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
            maximum_retry_count: None,
        }),
        RestartPolicy::OnFailure { max_retries } => Some(BollardRestartPolicy {
            name: Some(RestartPolicyNameEnum::ON_FAILURE),
            maximum_retry_count: max_retries.map(i64::from),
        }),
    };

    let host_config = HostConfig {
        port_bindings: if port_bindings.is_empty() {
            None
        } else {
            Some(port_bindings)
        },
        restart_policy,
        binds: if binds.is_empty() {
            None
        } else {
            Some(binds.clone())
        },
        memory: spec
            .linux_resources
            .as_ref()
            .and_then(|r| r.memory_max_bytes.map(|v| v as i64)),
        memory_swap: spec
            .linux_resources
            .as_ref()
            .and_then(|r| r.memory_swap_max_bytes.map(|v| v as i64)),
        cpu_quota: spec.linux_resources.as_ref().and_then(|r| r.cpu_quota_us),
        cpu_period: spec
            .linux_resources
            .as_ref()
            .and_then(|r| r.cpu_period_us.map(|v| v as i64)),
        cpu_shares: spec
            .linux_resources
            .as_ref()
            .and_then(|r| r.cpu_shares.map(|v| v as i64)),
        pids_limit: spec.linux_resources.as_ref().and_then(|r| r.pids_max),
        ..Default::default()
    };

    let healthcheck = spec
        .healthcheck
        .as_ref()
        .map(|hc| bollard::secret::HealthConfig {
            test: Some(hc.test.clone()),
            interval: Some(hc.interval.as_nanos() as i64),
            timeout: Some(hc.timeout.as_nanos() as i64),
            retries: Some(hc.retries as i64),
            start_period: Some(hc.start_period.as_nanos() as i64),
            ..Default::default()
        });

    let cfg = bollard::container::Config {
        image: Some(spec.image.clone()),
        cmd: spec.command.clone(),
        entrypoint: spec.entrypoint.clone(),
        env: Some(env),
        labels: if labels.is_empty() {
            None
        } else {
            Some(labels)
        },
        exposed_ports: if exposed_ports.is_empty() {
            None
        } else {
            Some(exposed_ports)
        },
        user: spec.user.clone(),
        working_dir: spec.working_dir.clone(),
        hostname: spec.hostname.clone(),
        healthcheck,
        host_config: Some(host_config),
        ..Default::default()
    };

    (cfg, spec.networks.clone())
}

/// Map a bollard `ContainerSummary` (from list_containers) to a
/// [`ContainerSnapshot`]. List responses don't carry the full inspect
/// detail, so timestamps + exit_code remain `None` until inspected.
pub(crate) fn map_summary(summary: ContainerSummary) -> ContainerSnapshot {
    let id = summary.id.clone().unwrap_or_default();
    let name = summary
        .names
        .as_ref()
        .and_then(|ns| ns.first().cloned())
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_default();
    let image = summary.image.clone().unwrap_or_default();
    let labels = summary
        .labels
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect();
    let stack = summary
        .labels
        .as_ref()
        .and_then(|l| {
            l.get("isengard.stack")
                .or(l.get("com.docker.compose.project"))
        })
        .cloned();
    let service = summary
        .labels
        .as_ref()
        .and_then(|l| {
            l.get("com.docker.compose.service")
                .or(l.get("isengard.service"))
        })
        .cloned();
    let state = summary
        .state
        .as_deref()
        .map(map_state_str)
        .unwrap_or(ContainerState::Created);
    let mut network_settings = NetworkSettings::default();
    if let Some(nets) = summary
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
    {
        for (name, settings) in nets {
            if let Some(ip_str) = settings.ip_address.as_deref().filter(|s| !s.is_empty()) {
                if let Ok(ip) = ip_str.parse() {
                    network_settings.ip_addresses.insert(name.clone(), ip);
                }
            }
        }
    }
    if let Some(ports) = summary.ports.as_ref() {
        for p in ports {
            let proto = p
                .typ
                .map(|t| match t {
                    bollard::secret::PortTypeEnum::TCP => "tcp",
                    bollard::secret::PortTypeEnum::UDP => "udp",
                    bollard::secret::PortTypeEnum::SCTP => "sctp",
                    bollard::secret::PortTypeEnum::EMPTY => "tcp",
                })
                .unwrap_or("tcp");
            let key = format!("{}/{proto}", p.private_port);
            let entry = network_settings.ports.entry(key).or_default();
            if let (Some(host_ip_str), Some(host_port)) = (p.ip.as_deref(), p.public_port) {
                if let Ok(host_ip) = host_ip_str.parse() {
                    entry.push(HostPort { host_ip, host_port });
                }
            }
        }
    }
    ContainerSnapshot {
        id,
        name,
        image,
        state,
        stack,
        service,
        labels,
        created_at: summary
            .created
            .and_then(|c| {
                u64::try_from(c)
                    .ok()
                    .map(|s| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s))
            })
            .unwrap_or(SystemTime::UNIX_EPOCH),
        started_at: None,
        finished_at: None,
        exit_code: None,
        restart_count: 0,
        network_settings,
    }
}

/// Map a bollard `ContainerInspectResponse` to a [`ContainerSnapshot`].
pub(crate) fn map_inspect(inspect: ContainerInspectResponse) -> ContainerSnapshot {
    let id = inspect.id.clone().unwrap_or_default();
    let name = inspect
        .name
        .clone()
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string();
    let image = inspect
        .config
        .as_ref()
        .and_then(|c| c.image.clone())
        .or_else(|| inspect.image.clone())
        .unwrap_or_default();
    let labels: std::collections::BTreeMap<String, String> = inspect
        .config
        .as_ref()
        .and_then(|c| c.labels.clone())
        .unwrap_or_default()
        .into_iter()
        .collect();
    let stack = labels
        .get("isengard.stack")
        .or_else(|| labels.get("com.docker.compose.project"))
        .cloned();
    let service = labels
        .get("com.docker.compose.service")
        .or_else(|| labels.get("isengard.service"))
        .cloned();
    let state_enum = inspect
        .state
        .as_ref()
        .and_then(|s| s.status)
        .map(|s| match s {
            bollard::secret::ContainerStateStatusEnum::CREATED => ContainerState::Created,
            bollard::secret::ContainerStateStatusEnum::RUNNING => ContainerState::Running,
            bollard::secret::ContainerStateStatusEnum::PAUSED => ContainerState::Paused,
            bollard::secret::ContainerStateStatusEnum::RESTARTING => ContainerState::Restarting,
            bollard::secret::ContainerStateStatusEnum::EXITED => ContainerState::Exited,
            bollard::secret::ContainerStateStatusEnum::DEAD => ContainerState::Dead,
            bollard::secret::ContainerStateStatusEnum::EMPTY
            | bollard::secret::ContainerStateStatusEnum::REMOVING => ContainerState::Created,
        })
        .unwrap_or(ContainerState::Created);
    let exit_code = inspect
        .state
        .as_ref()
        .and_then(|s| s.exit_code)
        .map(|c| c as i32);
    let restart_count = inspect.restart_count.map(|c| c as u32).unwrap_or(0);

    let mut network_settings = NetworkSettings::default();
    if let Some(ns) = inspect.network_settings.as_ref() {
        if let Some(nets) = ns.networks.as_ref() {
            for (net_name, settings) in nets {
                if let Some(ip_str) = settings.ip_address.as_deref().filter(|s| !s.is_empty()) {
                    if let Ok(ip) = ip_str.parse() {
                        network_settings.ip_addresses.insert(net_name.clone(), ip);
                    }
                }
            }
        }
        if let Some(ports) = ns.ports.as_ref() {
            for (key, bindings) in ports {
                let mut entry = Vec::new();
                if let Some(bs) = bindings {
                    for b in bs {
                        if let (Some(host_ip_str), Some(host_port_str)) =
                            (b.host_ip.as_deref(), b.host_port.as_deref())
                        {
                            if let (Ok(host_ip), Ok(host_port)) =
                                (host_ip_str.parse(), host_port_str.parse::<u16>())
                            {
                                entry.push(HostPort { host_ip, host_port });
                            }
                        }
                    }
                }
                network_settings.ports.insert(key.clone(), entry);
            }
        }
    }

    ContainerSnapshot {
        id,
        name,
        image,
        state: state_enum,
        stack,
        service,
        labels,
        created_at: inspect
            .created
            .as_deref()
            .and_then(parse_rfc3339)
            .unwrap_or(SystemTime::UNIX_EPOCH),
        started_at: inspect
            .state
            .as_ref()
            .and_then(|s| s.started_at.as_deref())
            .and_then(parse_rfc3339),
        finished_at: inspect
            .state
            .as_ref()
            .and_then(|s| s.finished_at.as_deref())
            .and_then(parse_rfc3339),
        exit_code,
        restart_count,
        network_settings,
    }
}

fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp() as u64))
}

fn map_state_str(state: &str) -> ContainerState {
    match state.to_lowercase().as_str() {
        "running" => ContainerState::Running,
        "paused" => ContainerState::Paused,
        "restarting" => ContainerState::Restarting,
        "exited" => ContainerState::Exited,
        "dead" => ContainerState::Dead,
        _ => ContainerState::Created,
    }
}

#[async_trait]
impl RuntimeBackend for BollardBackend {
    /// Pull the image if missing. Drains the pull stream and returns the
    /// resolved image's manifest digest from a follow-up inspect.
    async fn ensure_image(&self, reference: &str) -> Result<String, RuntimeError> {
        let opts = CreateImageOptions {
            from_image: reference.to_string(),
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(opts), None, None);
        while let Some(item) = stream.next().await {
            item.map_err(|e| RuntimeError::Image(format!("pulling {reference}: {e}")))?;
        }
        match self.docker.inspect_image(reference).await {
            Ok(img) => {
                let digest = img
                    .repo_digests
                    .as_ref()
                    .and_then(|ds| ds.first().cloned())
                    .or(img.id)
                    .unwrap_or_default();
                Ok(digest)
            }
            Err(e) => Err(RuntimeError::Image(format!(
                "inspect_image {reference}: {e}"
            ))),
        }
    }

    async fn create_container(&self, spec: &ContainerCreateSpec) -> Result<String, RuntimeError> {
        let (cfg, _networks) = spec_to_config(spec);
        let create = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: spec.container_name.clone(),
                    platform: None,
                }),
                cfg,
            )
            .await
            .map_err(|e| {
                RuntimeError::Container(format!("create_container {}: {e}", spec.container_name))
            })?;
        Ok(create.id)
    }

    async fn start_container(&self, id: &str) -> Result<(), RuntimeError> {
        self.docker
            .start_container(id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| RuntimeError::Container(format!("start_container {id}: {e}")))
    }

    async fn stop_container(&self, id: &str, timeout_s: u32) -> Result<(), RuntimeError> {
        self.docker
            .stop_container(
                id,
                Some(StopContainerOptions {
                    t: timeout_s as i64,
                }),
            )
            .await
            .map_err(|e| RuntimeError::Container(format!("stop_container {id}: {e}")))
    }

    async fn remove_container(&self, id: &str, force: bool) -> Result<(), RuntimeError> {
        match self
            .docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force,
                    v: false,
                    link: false,
                }),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                let s = e.to_string();
                if s.contains("404") || s.to_lowercase().contains("no such container") {
                    Err(RuntimeError::NotFound(id.to_string()))
                } else {
                    Err(RuntimeError::Container(format!(
                        "remove_container {id}: {e}"
                    )))
                }
            }
        }
    }

    async fn list_containers(
        &self,
        filter: ListFilter,
    ) -> Result<Vec<ContainerSnapshot>, RuntimeError> {
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(stack) = filter.stack.as_ref() {
            filters.insert(
                "label".to_string(),
                vec![
                    format!("com.docker.compose.project={stack}"),
                    format!("isengard.stack={stack}"),
                ],
            );
        }
        if let Some(label_key) = filter.label_key.as_ref() {
            filters
                .entry("label".to_string())
                .or_default()
                .push(label_key.clone());
        }
        let opts = ListContainersOptions::<String> {
            all: filter.all,
            filters,
            ..Default::default()
        };
        let summaries = self
            .docker
            .list_containers(Some(opts))
            .await
            .map_err(|e| RuntimeError::Docker(format!("list_containers: {e}")))?;
        Ok(summaries.into_iter().map(map_summary).collect())
    }

    async fn inspect_container(&self, id: &str) -> Result<Option<ContainerSnapshot>, RuntimeError> {
        match self
            .docker
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
        {
            Ok(inspect) => Ok(Some(map_inspect(inspect))),
            Err(e) => {
                let s = e.to_string();
                if s.contains("404") || s.to_lowercase().contains("no such container") {
                    Ok(None)
                } else {
                    Err(RuntimeError::Docker(format!("inspect_container {id}: {e}")))
                }
            }
        }
    }

    async fn connect_network(&self, container_id: &str, network: &str) -> Result<(), RuntimeError> {
        self.docker
            .connect_network(
                network,
                bollard::network::ConnectNetworkOptions {
                    container: container_id.to_string(),
                    endpoint_config: Default::default(),
                },
            )
            .await
            .map_err(|e| {
                RuntimeError::Network(format!("connect_network {network} -> {container_id}: {e}"))
            })
    }

    async fn disconnect_network(
        &self,
        container_id: &str,
        network: &str,
    ) -> Result<(), RuntimeError> {
        self.docker
            .disconnect_network(
                network,
                bollard::network::DisconnectNetworkOptions {
                    container: container_id.to_string(),
                    force: false,
                },
            )
            .await
            .map_err(|e| {
                RuntimeError::Network(format!(
                    "disconnect_network {network} <- {container_id}: {e}"
                ))
            })
    }

    fn stream_logs(
        &self,
        id: &str,
        opts: LogOptions,
    ) -> Pin<Box<dyn Stream<Item = LogChunk> + Send>> {
        let bollard_opts = LogsOptions::<String> {
            follow: opts.follow,
            stdout: true,
            stderr: true,
            timestamps: opts.timestamps,
            tail: opts
                .tail
                .map(|n| n.to_string())
                .unwrap_or_else(|| "all".to_string()),
            since: opts.since_seconds.unwrap_or(0),
            ..Default::default()
        };
        let raw = self.docker.logs(id, Some(bollard_opts));
        let mapped = raw.filter_map(|r| async move {
            match r {
                Ok(out) => Some(map_log_output(out)),
                Err(_) => None,
            }
        });
        Box::pin(mapped)
    }

    fn stream_events(&self) -> Pin<Box<dyn Stream<Item = RuntimeEvent> + Send>> {
        let mut filters = HashMap::new();
        filters.insert("type".to_string(), vec!["container".to_string()]);
        let stream = self.docker.events(Some(EventsOptions::<String> {
            since: None,
            until: None,
            filters,
        }));
        let mapped = stream.filter_map(|r| async move {
            match r {
                Ok(ev) => map_docker_event(ev),
                Err(_) => None,
            }
        });
        Box::pin(mapped)
    }

    async fn run_healthcheck(
        &self,
        id: &str,
        _hc: &HealthcheckSpec,
    ) -> Result<HealthState, RuntimeError> {
        // Bollard impl: docker runs the healthcheck in-container; we just
        // read whatever state docker has recorded. WispBackend (dispatch
        // C) will run the probe externally.
        let inspect = self
            .docker
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| RuntimeError::Healthcheck(format!("inspect for healthcheck {id}: {e}")))?;
        let status = inspect
            .state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.status);
        Ok(match status {
            Some(HealthStatusEnum::HEALTHY) => HealthState::Healthy,
            Some(HealthStatusEnum::UNHEALTHY) => HealthState::Unhealthy,
            Some(HealthStatusEnum::STARTING) | Some(HealthStatusEnum::NONE) => {
                HealthState::Starting
            }
            None | Some(HealthStatusEnum::EMPTY) => HealthState::Starting,
        })
    }

    fn name(&self) -> &'static str {
        "docker"
    }

    fn as_bollard(&self) -> Option<std::sync::Arc<bollard::Docker>> {
        Some(self.docker.clone())
    }
}

fn map_log_output(out: bollard::container::LogOutput) -> LogChunk {
    let (source, bytes) = match out {
        bollard::container::LogOutput::StdOut { message } => (LogSource::Stdout, message),
        bollard::container::LogOutput::StdErr { message } => (LogSource::Stderr, message),
        bollard::container::LogOutput::StdIn { message } => (LogSource::Stdout, message),
        bollard::container::LogOutput::Console { message } => (LogSource::Stdout, message),
    };
    LogChunk { source, bytes }
}

fn map_docker_event(ev: bollard::secret::EventMessage) -> Option<RuntimeEvent> {
    let actor = ev.actor.as_ref()?;
    let container_id = actor.id.clone()?;
    let action = ev.action.as_deref()?;
    let event_type = match action {
        "start" => RuntimeEventType::Start,
        "stop" | "kill" => RuntimeEventType::Stop,
        "die" => {
            let exit_code = actor
                .attributes
                .as_ref()
                .and_then(|a| a.get("exitCode"))
                .and_then(|s| s.parse::<i32>().ok());
            RuntimeEventType::Die { exit_code }
        }
        "health_status: healthy" => RuntimeEventType::HealthcheckPassed,
        "health_status: unhealthy" => RuntimeEventType::HealthcheckFailed,
        _ => return None,
    };
    let timestamp = ev
        .time
        .and_then(|t| u64::try_from(t).ok())
        .map(|s| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s))
        .unwrap_or_else(SystemTime::now);
    Some(RuntimeEvent {
        container_id,
        event_type,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{MountKind, MountSpec, PortProtocol, PortSpec};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn base_spec() -> ContainerCreateSpec {
        ContainerCreateSpec {
            container_name: "hello-web".into(),
            image: "nginx:1.25-alpine".into(),
            stack: "hello".into(),
            service: "web".into(),
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
    fn spec_to_config_sets_image_and_returns_networks() {
        let mut spec = base_spec();
        spec.networks = vec!["isengard-proxy".into(), "hello_default".into()];
        let (cfg, networks) = spec_to_config(&spec);
        assert_eq!(cfg.image.as_deref(), Some("nginx:1.25-alpine"));
        assert_eq!(networks, spec.networks);
    }

    #[test]
    fn spec_to_config_sorts_env_alphabetically() {
        let mut spec = base_spec();
        spec.env.insert("FOO".into(), "1".into());
        spec.env.insert("BAR".into(), "2".into());
        let (cfg, _) = spec_to_config(&spec);
        let env = cfg.env.unwrap();
        assert_eq!(env, vec!["BAR=2", "FOO=1"]);
    }

    #[test]
    fn spec_to_config_translates_unless_stopped_restart() {
        let mut spec = base_spec();
        spec.restart = RestartPolicy::UnlessStopped;
        let (cfg, _) = spec_to_config(&spec);
        let rp = cfg.host_config.unwrap().restart_policy.unwrap();
        assert!(matches!(
            rp.name,
            Some(RestartPolicyNameEnum::UNLESS_STOPPED)
        ));
        assert!(rp.maximum_retry_count.is_none());
    }

    #[test]
    fn spec_to_config_translates_on_failure_max_retries() {
        let mut spec = base_spec();
        spec.restart = RestartPolicy::OnFailure {
            max_retries: Some(3),
        };
        let (cfg, _) = spec_to_config(&spec);
        let rp = cfg.host_config.unwrap().restart_policy.unwrap();
        assert!(matches!(rp.name, Some(RestartPolicyNameEnum::ON_FAILURE)));
        assert_eq!(rp.maximum_retry_count, Some(3));
    }

    #[test]
    fn spec_to_config_emits_port_bindings_with_proto() {
        let mut spec = base_spec();
        spec.ports.push(PortSpec {
            host_ip: None,
            host_port: 8080,
            container_port: 80,
            protocol: PortProtocol::Tcp,
        });
        let (cfg, _) = spec_to_config(&spec);
        let pb = cfg.host_config.unwrap().port_bindings.unwrap();
        let bindings = pb.get("80/tcp").unwrap().as_ref().unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].host_port.as_deref(), Some("8080"));
        let exposed = cfg.exposed_ports.unwrap();
        assert!(exposed.contains_key("80/tcp"));
    }

    #[test]
    fn spec_to_config_emits_binds_for_mounts() {
        let mut spec = base_spec();
        spec.mounts.push(MountSpec {
            source: "/host/data".into(),
            target: "/data".into(),
            kind: MountKind::Bind,
            read_only: true,
        });
        let (cfg, _) = spec_to_config(&spec);
        let binds = cfg.host_config.unwrap().binds.unwrap();
        assert_eq!(binds, vec!["/host/data:/data:ro"]);
    }

    #[test]
    fn spec_to_config_translates_secrets_to_binds() {
        let mut spec = base_spec();
        spec.secrets.push(crate::runtime::SecretMount {
            source: "/run/isengard-secrets/web/cf".into(),
            target: std::path::PathBuf::from("/run/secrets/cf"),
            mode: 0o400,
        });
        let (cfg, _) = spec_to_config(&spec);
        let binds = cfg.host_config.unwrap().binds.unwrap();
        assert_eq!(
            binds,
            vec!["/run/isengard-secrets/web/cf:/run/secrets/cf:ro"]
        );
    }

    #[test]
    fn spec_to_config_translates_healthcheck_durations() {
        let mut spec = base_spec();
        spec.healthcheck = Some(HealthcheckSpec {
            test: vec!["CMD".into(), "true".into()],
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(1),
            retries: 4,
            start_period: Duration::from_secs(5),
        });
        let (cfg, _) = spec_to_config(&spec);
        let hc = cfg.healthcheck.unwrap();
        assert_eq!(hc.test, Some(vec!["CMD".into(), "true".into()]));
        assert_eq!(hc.interval, Some(Duration::from_secs(2).as_nanos() as i64));
        assert_eq!(hc.retries, Some(4));
    }

    #[test]
    fn map_inspect_extracts_running_state_and_labels() {
        use bollard::secret::{
            ContainerConfig, ContainerInspectResponse, ContainerState as BollardContainerState,
            ContainerStateStatusEnum,
        };
        let mut labels = HashMap::new();
        labels.insert("isengard.stack".to_string(), "hello".to_string());
        labels.insert("com.docker.compose.service".to_string(), "web".to_string());
        let resp = ContainerInspectResponse {
            id: Some("abc123".into()),
            name: Some("/hello-web".into()),
            config: Some(ContainerConfig {
                image: Some("nginx".into()),
                labels: Some(labels),
                ..Default::default()
            }),
            state: Some(BollardContainerState {
                status: Some(ContainerStateStatusEnum::RUNNING),
                exit_code: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let snap = map_inspect(resp);
        assert_eq!(snap.id, "abc123");
        assert_eq!(snap.name, "hello-web");
        assert_eq!(snap.image, "nginx");
        assert_eq!(snap.state, ContainerState::Running);
        assert_eq!(snap.stack.as_deref(), Some("hello"));
        assert_eq!(snap.service.as_deref(), Some("web"));
    }
}
