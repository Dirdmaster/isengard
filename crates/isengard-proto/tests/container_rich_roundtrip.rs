//! Round-trip the new `ContainerRich` block riding on `ContainerInfo`
//! at field 11. Older agents leave `rich` unset; older controllers
//! ignore unknown fields. Both directions must stay additive.

use isengard_proto::pb::{
    ContainerHealthcheck, ContainerInfo, ContainerMount, ContainerPortMapping, ContainerRich,
};
use prost::Message;

#[test]
fn container_info_without_rich_round_trips() {
    // Older agents emit no `rich` field. Encode + decode must yield the
    // identical message back, with `rich` still `None`.
    let info = ContainerInfo {
        runtime_container_id: "abc123".into(),
        image: "nginx:alpine".into(),
        command: "nginx -g 'daemon off;'".into(),
        state: "running".into(),
        status_message: "Up 5m".into(),
        names: "hello-web.1".into(),
        stack: "hello".into(),
        service: "web".into(),
        created_at_ms: 1_700_000_000_000,
        observed_at_ms: 1_700_000_300_000,
        rich: None,
    };
    let bytes = info.encode_to_vec();
    let back = ContainerInfo::decode(&*bytes).unwrap();
    assert!(back.rich.is_none());
    assert_eq!(back.runtime_container_id, "abc123");
}

#[test]
fn container_info_with_rich_round_trips() {
    // Newer agents populate `rich` with every sub-message. Every field
    // must survive the encode + decode loop.
    let rich = ContainerRich {
        ports: vec![
            ContainerPortMapping {
                host_ip: "0.0.0.0".into(),
                host_port: 8080,
                container_port: 80,
                protocol: "tcp".into(),
            },
            ContainerPortMapping {
                host_ip: "127.0.0.1".into(),
                host_port: 5353,
                container_port: 53,
                protocol: "udp".into(),
            },
        ],
        env: vec!["FOO=bar".into(), "BAZ=qux".into()],
        mounts: vec![
            ContainerMount {
                kind: "bind".into(),
                source: "/host/data".into(),
                target: "/data".into(),
                read_only: true,
            },
            ContainerMount {
                kind: "volume".into(),
                source: "dbvol".into(),
                target: "/var/lib/mysql".into(),
                read_only: false,
            },
        ],
        networks: vec!["frontend".into(), "backend".into()],
        restart_policy: "on-failure:5".into(),
        command: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
        entrypoint: vec!["/docker-entrypoint.sh".into()],
        working_dir: "/srv".into(),
        user_spec: "nginx".into(),
        healthcheck: Some(ContainerHealthcheck {
            test: vec!["CMD".into(), "curl".into(), "-f".into(), "/".into()],
            interval_ns: 30_000_000_000,
            timeout_ns: 5_000_000_000,
            retries: 3,
            start_period_ns: 10_000_000_000,
        }),
    };
    let info = ContainerInfo {
        runtime_container_id: "abc123".into(),
        image: "nginx:alpine".into(),
        command: "nginx -g 'daemon off;'".into(),
        state: "running".into(),
        status_message: "Up 5m".into(),
        names: "hello-web.1".into(),
        stack: "hello".into(),
        service: "web".into(),
        created_at_ms: 1_700_000_000_000,
        observed_at_ms: 1_700_000_300_000,
        rich: Some(rich.clone()),
    };
    let bytes = info.encode_to_vec();
    let back = ContainerInfo::decode(&*bytes).unwrap();
    let got = back.rich.expect("rich preserved across round-trip");
    assert_eq!(got.ports.len(), 2);
    assert_eq!(got.ports[0].host_port, 8080);
    assert_eq!(got.ports[0].container_port, 80);
    assert_eq!(got.ports[1].protocol, "udp");
    assert_eq!(got.env, rich.env);
    assert_eq!(got.mounts.len(), 2);
    assert_eq!(got.mounts[1].source, "dbvol");
    assert_eq!(got.networks, vec!["frontend", "backend"]);
    assert_eq!(got.restart_policy, "on-failure:5");
    assert_eq!(got.command, rich.command);
    assert_eq!(got.entrypoint, rich.entrypoint);
    assert_eq!(got.working_dir, "/srv");
    assert_eq!(got.user_spec, "nginx");
    let hc = got.healthcheck.expect("healthcheck preserved");
    assert_eq!(hc.test, vec!["CMD", "curl", "-f", "/"]);
    assert_eq!(hc.interval_ns, 30_000_000_000);
    assert_eq!(hc.timeout_ns, 5_000_000_000);
    assert_eq!(hc.retries, 3);
    assert_eq!(hc.start_period_ns, 10_000_000_000);
}
