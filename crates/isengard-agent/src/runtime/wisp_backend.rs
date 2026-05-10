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

use oci_spec::runtime::{
    LinuxCpuBuilder, LinuxMemoryBuilder, LinuxPidsBuilder, LinuxResources as OciLinuxResources,
    LinuxResourcesBuilder, Mount as OciMount, MountBuilder,
};

use super::spec::{
    ContainerCreateSpec, LinuxResources, MountKind, MountSpec, PortProtocol as SpecPortProtocol,
    PortSpec,
};

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
}
