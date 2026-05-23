//! Reads the local container list, groups containers into stacks
//! based on the `com.docker.compose.project` label (or `isengard.stack=` override),
//! and converts to the wire-format `StackInfo`.
//!
//! Drives off the [`crate::runtime::RuntimeBackend`] trait
//! so heartbeats from a wisp host stop dialling docker.sock every
//! interval. The legacy `list_container_snapshots()` function (no
//! backend arg) is kept for back-compat but routes through a
//! best-effort BollardBackend factory; new callers pass the live
//! backend Arc via [`list_container_snapshots_via`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use isengard_proto::pb::{ContainerInfo, ServiceInfo, StackInfo};
use tracing::warn;

use crate::runtime::{ContainerState, ListFilter, RuntimeBackend};

/// Lightweight snapshot of one container: the bits we need to derive
/// stacks, services, and (phase 0.18) container rows from a heartbeat.
///
/// Extended with `id`, `created_at`, and `exit_code` so the
/// agent can ship a `ContainerInfo` per container. The runtime id is
/// the backend's native handle (bollard container id, wisp handle).
/// Pre-0.18 callers that consume only `name`/`image`/`state`/`labels`
/// are unaffected.
#[derive(Debug, Clone)]
pub struct ContainerSnapshot {
    /// Container name, leading slash already trimmed.
    pub name: String,
    /// Image reference as the runtime reports it.
    pub image: String,
    /// Lifecycle state string (`running`, `exited`, ...).
    pub state: String,
    /// Container labels.
    pub labels: HashMap<String, String>,
    /// Bollard / wisp native id. Empty when the legacy (no-backend)
    /// fallback path produced this snapshot from a bollard ContainerSummary
    /// without an id field.
    pub id: String,
    /// Unix milliseconds when the container was created. 0 when the
    /// runtime did not record a creation time.
    pub created_at_ms: i64,
    /// Exit code reported by the runtime (only set for `exited` /
    /// `dead`). None for everything else.
    pub exit_code: Option<i32>,
}

/// Query the [`RuntimeBackend`] for all containers
/// (running + stopped) and project to the heartbeat-oriented
/// [`ContainerSnapshot`] shape. Returns an empty Vec on backend error
/// (logged at warn level so the heartbeat still sends).
pub async fn list_container_snapshots_via(backend: &dyn RuntimeBackend) -> Vec<ContainerSnapshot> {
    let snaps = match backend
        .list_containers(ListFilter {
            all: true,
            ..Default::default()
        })
        .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, backend = %backend.name(), "container_snapshot: list_containers failed");
            return Vec::new();
        }
    };
    snaps
        .into_iter()
        .map(|s| {
            let created_at_ms = s
                .created_at
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            ContainerSnapshot {
                name: s.name,
                image: s.image,
                state: state_to_str(s.state).to_string(),
                labels: s.labels.into_iter().collect(),
                id: s.id,
                created_at_ms,
                exit_code: s.exit_code,
            }
        })
        .collect()
}

/// Back-compat wrapper for callers that don't have a backend handle.
/// Equivalent to the pre-0.6 behavior: dial docker.sock every call,
/// return empty on connection failure. Heartbeat code paths now pass
/// the live backend via [`list_container_snapshots_via`]; this remains
/// for the few legacy / test paths that haven't been threaded through.
pub async fn list_container_snapshots() -> Vec<ContainerSnapshot> {
    use bollard::Docker;
    use bollard::container::ListContainersOptions;

    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "container_snapshot: failed to connect to Docker");
            return Vec::new();
        }
    };
    let opts = ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    };
    let containers = match docker.list_containers(Some(opts)).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "container_snapshot: list_containers failed");
            return Vec::new();
        }
    };
    containers
        .into_iter()
        .map(|c| {
            let name = c
                .names
                .as_ref()
                .and_then(|ns| ns.first().cloned())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default();
            let image = c.image.clone().unwrap_or_default();
            let state = c.state.clone().unwrap_or_else(|| "unknown".to_string());
            let labels = c.labels.unwrap_or_default();
            let id = c.id.clone().unwrap_or_default();
            // bollard reports `created` in unix seconds (i64). Convert
            // up to ms; treat negative values (clock skew) as 0.
            let created_at_ms = c.created.map(|s| s.max(0) * 1000).unwrap_or(0);
            ContainerSnapshot {
                name,
                image,
                state,
                labels,
                id,
                created_at_ms,
                exit_code: None,
            }
        })
        .collect()
}

/// Map the trait's typed state enum into the wire-format state string the
/// controller persists via `ServiceState::from_str`.
///
/// v0.5.3: `Created` now reports as `"creating"` (was `"created"`), so
/// `ServiceState::from_str` lands on `ServiceState::Creating` rather than
/// the pre-extension `Unknown` fallback. `from_str` still accepts the
/// legacy `"created"` string so heartbeats from older agents stay green.
fn state_to_str(state: ContainerState) -> &'static str {
    match state {
        ContainerState::Created => "creating",
        ContainerState::Running => "running",
        ContainerState::Restarting => "restarting",
        ContainerState::Paused => "paused",
        ContainerState::Exited => "stopped",
        ContainerState::Dead => "failed",
    }
}

/// Heartbeat hook that prefers the live backend when one is
/// available; falls back to the legacy bollard probe otherwise.
/// Callers pass the same `Option<Arc<dyn RuntimeBackend>>` they already
/// hold for the rest of the agent.
pub async fn snapshots_via_backend_or_legacy(
    backend: Option<&Arc<dyn RuntimeBackend>>,
) -> Vec<ContainerSnapshot> {
    match backend {
        Some(b) => list_container_snapshots_via(b.as_ref()).await,
        None => list_container_snapshots().await,
    }
}

/// Build per-container ServiceInfo entries with stack association.
/// Mirrors derive_stacks naming: same precedence (isengard.stack > compose label > inferred).
pub fn derive_services(containers: &[ContainerSnapshot]) -> Vec<ServiceInfo> {
    containers
        .iter()
        .map(|c| {
            let stack = c
                .labels
                .get("isengard.stack")
                .or_else(|| c.labels.get("com.docker.compose.project"))
                .cloned()
                .unwrap_or_else(|| c.name.clone());

            ServiceInfo {
                name: c.name.clone(),
                image: c.image.clone(),
                state: c.state.clone(),
                stack: Some(stack),
            }
        })
        .collect()
}

/// Group container snapshots into stacks based on Docker Compose labels,
/// the optional `isengard.stack` override, or fall back to single-service
/// inferred stacks.
pub fn derive_stacks(containers: &[ContainerSnapshot]) -> Vec<StackInfo> {
    // BTreeMap for deterministic output ordering (helps tests + diffs).
    let mut grouped: BTreeMap<(String, &'static str), Vec<String>> = BTreeMap::new();

    for c in containers {
        let (name, source) = if let Some(n) = c.labels.get("isengard.stack") {
            (n.clone(), "manual")
        } else if let Some(n) = c.labels.get("com.docker.compose.project") {
            (n.clone(), "compose")
        } else {
            (c.name.clone(), "inferred")
        };

        grouped
            .entry((name, source))
            .or_default()
            .push(c.name.clone());
    }

    grouped
        .into_iter()
        .map(|((name, source), mut services)| {
            services.sort();
            StackInfo {
                name,
                source: source.to_string(),
                services,
            }
        })
        .collect()
}

/// State vocabulary the agent emits in [`ContainerInfo`].
/// Distinct from [`state_to_str`] which targets the storage-side
/// `ServiceState` enum. The container vocabulary is fixed: `running`,
/// `restarting`, `paused`, `created`, `exited`, `dead`. `removing` is
/// not modelled by [`ContainerState`] today; the legacy fallback path
/// may produce arbitrary strings from bollard's `State` field, which
/// the controller treats as-is.
fn container_state_to_str(state: &str) -> &str {
    // The bollard-fallback path passes the raw State string (e.g.
    // `running`, `exited`, `dead`, `paused`, `removing`, `restarting`,
    // `created`). The backend-driven path passes one of the legacy
    // service strings (`creating`, `stopped`, `failed`). Normalise both
    // onto the vocabulary so the controller sees a single
    // dictionary.
    match state {
        "creating" => "created",
        "stopped" => "exited",
        "failed" => "dead",
        // running / restarting / paused / created / exited / dead /
        // removing pass through unchanged.
        other => other,
    }
}

/// Render the per-container `STATUS` column the way docker
/// ps does. Inputs are the container's state vocabulary (post
/// `container_state_to_str`), the unix-ms creation time (0 when the
/// runtime didn't record one), the optional exit code, and the
/// reference `now` (unix ms) used to compute "Up 5m".
pub fn render_status_message(
    state: &str,
    created_at_ms: i64,
    exit_code: Option<i32>,
    now_ms: i64,
) -> String {
    match state {
        "running" => format!("Up {}", humanize_age_ms(now_ms, created_at_ms)),
        "exited" => match exit_code {
            Some(code) => format!(
                "Exited ({code}) {} ago",
                humanize_age_ms(now_ms, created_at_ms)
            ),
            None => format!("Exited {} ago", humanize_age_ms(now_ms, created_at_ms)),
        },
        "paused" => "Paused".to_string(),
        "restarting" => "Restarting".to_string(),
        "created" => "Created".to_string(),
        "dead" => "Dead".to_string(),
        "removing" => "Removing".to_string(),
        // Anything else: pass the raw vocabulary through with a capital
        // letter. Defensive; the agent shouldn't emit anything else.
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Format `now_ms - then_ms` like `15s`, `5m`, `2h`, `3d`. Negative
/// deltas (the reference is BEFORE the event) collapse to `0s`.
fn humanize_age_ms(now_ms: i64, then_ms: i64) -> String {
    if then_ms <= 0 {
        // Unknown creation time: don't lie with a duration.
        return "?".to_string();
    }
    let delta_secs = ((now_ms - then_ms).max(0)) / 1000;
    if delta_secs < 60 {
        format!("{delta_secs}s")
    } else if delta_secs < 3600 {
        format!("{}m", delta_secs / 60)
    } else if delta_secs < 86_400 {
        format!("{}h", delta_secs / 3600)
    } else {
        format!("{}d", delta_secs / 86_400)
    }
}

/// Project a slice of snapshots to the wire-format
/// [`ContainerInfo`] vec carried on every heartbeat. Each row gets its
/// `observed_at_ms` stamped from `now_ms` so the controller's
/// `last_seen_at` derivation can clamp to the agent's clock.
///
/// Containers without a runtime id are skipped: without a stable id the
/// controller would mint a synthetic operator id every heartbeat,
/// defeating the deduplication. This only happens on the legacy
/// no-backend bollard fallback path which doesn't carry container ids.
pub fn derive_containers(snapshots: &[ContainerSnapshot], now_ms: i64) -> Vec<ContainerInfo> {
    snapshots
        .iter()
        .filter(|s| !s.id.is_empty())
        .map(|s| {
            let stack = s
                .labels
                .get("isengard.stack")
                .or_else(|| s.labels.get("com.docker.compose.project"))
                .cloned()
                .unwrap_or_default();
            let service = s
                .labels
                .get("com.docker.compose.service")
                .cloned()
                .unwrap_or_default();
            let state = container_state_to_str(&s.state).to_string();
            let status_message =
                render_status_message(&state, s.created_at_ms, s.exit_code, now_ms);
            ContainerInfo {
                runtime_container_id: s.id.clone(),
                image: s.image.clone(),
                command: String::new(),
                state,
                status_message,
                names: s.name.clone(),
                stack,
                service,
                created_at_ms: s.created_at_ms,
                observed_at_ms: now_ms,
            }
        })
        .collect()
}

/// Convenience: stamp `observed_at_ms` from the system clock. Splits
/// the wall-clock call from [`derive_containers`] so tests can inject a
/// deterministic `now_ms`.
pub fn derive_containers_now(snapshots: &[ContainerSnapshot]) -> Vec<ContainerInfo> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    derive_containers(snapshots, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.5.3: `state_to_str` now maps `Created` -> `"creating"` and
    /// `Exited` / `Dead` -> `"stopped"` / `"failed"` so the controller's
    /// `ServiceState::from_str` lands on the matching variant instead of
    /// `Unknown`. Backstops the bug fix that surfaced live on the
    /// lausanne v0.5.1 deploy (4/8 services rendering `unknown`).
    #[test]
    fn state_to_str_maps_wisp_created_to_creating() {
        assert_eq!(state_to_str(ContainerState::Created), "creating");
        assert_eq!(state_to_str(ContainerState::Running), "running");
        assert_eq!(state_to_str(ContainerState::Restarting), "restarting");
        assert_eq!(state_to_str(ContainerState::Paused), "paused");
        assert_eq!(state_to_str(ContainerState::Exited), "stopped");
        assert_eq!(state_to_str(ContainerState::Dead), "failed");
    }

    /// The runtime-level mapping must compose with the storage-level
    /// `ServiceState::from_str` so that no runtime state ends up as
    /// `Unknown` at the controller boundary.
    #[test]
    fn agent_state_str_decodes_into_concrete_service_state() {
        use isengard_storage::ServiceState;
        for cs in [
            ContainerState::Created,
            ContainerState::Running,
            ContainerState::Restarting,
            ContainerState::Exited,
            ContainerState::Dead,
        ] {
            let s = state_to_str(cs);
            let decoded = ServiceState::from_str(s);
            assert_ne!(
                decoded,
                ServiceState::Unknown,
                "ContainerState::{cs:?} -> {s:?} -> Unknown (regression)"
            );
        }
    }

    fn snap(name: &str, labels: &[(&str, &str)]) -> ContainerSnapshot {
        ContainerSnapshot {
            name: name.into(),
            image: format!("{name}:latest"),
            state: "running".into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            id: format!("rt-{name}"),
            created_at_ms: 0,
            exit_code: None,
        }
    }

    #[test]
    fn derives_stack_info_from_compose_label() {
        let containers = vec![
            snap("web", &[("com.docker.compose.project", "wordpress")]),
            snap("db", &[("com.docker.compose.project", "wordpress")]),
            snap("homer", &[]),
        ];

        let stacks = derive_stacks(&containers);
        assert_eq!(stacks.len(), 2);

        let wp = stacks.iter().find(|s| s.name == "wordpress").unwrap();
        assert_eq!(wp.source, "compose");
        assert_eq!(wp.services.len(), 2);
        assert!(wp.services.contains(&"db".to_string()));
        assert!(wp.services.contains(&"web".to_string()));

        let homer = stacks.iter().find(|s| s.name == "homer").unwrap();
        assert_eq!(homer.source, "inferred");
        assert_eq!(homer.services, vec!["homer".to_string()]);
    }

    #[test]
    fn isengard_stack_label_overrides_compose_label() {
        let containers = vec![snap(
            "x",
            &[
                ("com.docker.compose.project", "default-name"),
                ("isengard.stack", "override-name"),
            ],
        )];

        let stacks = derive_stacks(&containers);
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].name, "override-name");
        assert_eq!(stacks[0].source, "manual");
    }

    #[test]
    fn derives_service_info_with_state_and_stack_link() {
        let mut compose_label = std::collections::HashMap::new();
        compose_label.insert("com.docker.compose.project".to_string(), "blog".to_string());

        let containers = vec![
            ContainerSnapshot {
                name: "web".into(),
                image: "nginx:1.25-alpine".into(),
                state: "running".into(),
                labels: compose_label.clone(),
                id: "rt-web".into(),
                created_at_ms: 0,
                exit_code: None,
            },
            ContainerSnapshot {
                name: "homer".into(),
                image: "b4bz/homer:latest".into(),
                state: "stopped".into(),
                labels: std::collections::HashMap::new(),
                id: "rt-homer".into(),
                created_at_ms: 0,
                exit_code: None,
            },
        ];

        let services = derive_services(&containers);
        assert_eq!(services.len(), 2);

        let web = services.iter().find(|s| s.name == "web").unwrap();
        assert_eq!(web.image, "nginx:1.25-alpine");
        assert_eq!(web.state, "running");
        assert_eq!(web.stack.as_deref(), Some("blog"));

        let homer = services.iter().find(|s| s.name == "homer").unwrap();
        assert_eq!(homer.state, "stopped");
        assert_eq!(homer.stack.as_deref(), Some("homer"));
    }

    // Derive_containers shape tests.

    fn rich_snap(name: &str, id: &str, state: &str, created_at_ms: i64) -> ContainerSnapshot {
        let mut labels = HashMap::new();
        labels.insert("com.docker.compose.project".into(), "hello".into());
        labels.insert("com.docker.compose.service".into(), "web".into());
        ContainerSnapshot {
            name: name.into(),
            image: "nginx:alpine".into(),
            state: state.into(),
            labels,
            id: id.into(),
            created_at_ms,
            exit_code: None,
        }
    }

    /// Every populated field on the source snapshot lands
    /// on the wire-format `ContainerInfo`. Runtime id, image, names,
    /// stack, service, created_at_ms, observed_at_ms.
    #[test]
    fn derive_containers_round_trip_preserves_fields() {
        let snap = rich_snap("hello-web.1", "rt-abc", "running", 1_700_000_000_000);
        let infos = derive_containers(std::slice::from_ref(&snap), 1_700_000_300_000);
        assert_eq!(infos.len(), 1);
        let info = &infos[0];
        assert_eq!(info.runtime_container_id, "rt-abc");
        assert_eq!(info.image, "nginx:alpine");
        assert_eq!(info.names, "hello-web.1");
        assert_eq!(info.stack, "hello");
        assert_eq!(info.service, "web");
        assert_eq!(info.state, "running");
        assert_eq!(info.created_at_ms, 1_700_000_000_000);
        assert_eq!(info.observed_at_ms, 1_700_000_300_000);
    }

    /// Status_message renders consistently per state with a
    /// deterministic clock. Up uses humanized age, Exited adds an exit
    /// code if present, terminal states (Paused / Restarting / Created /
    /// Dead / Removing) render verbatim.
    #[test]
    fn status_message_renders_per_state_with_mock_clock() {
        let now = 1_700_000_300_000;
        let created = 1_700_000_000_000; // 300s = 5m ago

        assert_eq!(
            render_status_message("running", created, None, now),
            "Up 5m"
        );
        assert_eq!(
            render_status_message("exited", created, Some(0), now),
            "Exited (0) 5m ago"
        );
        assert_eq!(
            render_status_message("exited", created, Some(137), now),
            "Exited (137) 5m ago"
        );
        assert_eq!(
            render_status_message("paused", created, None, now),
            "Paused"
        );
        assert_eq!(
            render_status_message("restarting", created, None, now),
            "Restarting"
        );
        assert_eq!(
            render_status_message("created", created, None, now),
            "Created"
        );
        assert_eq!(render_status_message("dead", created, None, now), "Dead");
        assert_eq!(
            render_status_message("removing", created, None, now),
            "Removing"
        );

        // Created_at unset -> render with `?` rather than a bogus number.
        assert_eq!(render_status_message("running", 0, None, now), "Up ?");
    }

    /// `observed_at_ms` is stamped from the explicit `now_ms`
    /// arg so callers can inject deterministic clocks. Two snapshots
    /// derived at different `now_ms` get distinct observed_at_ms values.
    #[test]
    fn observed_at_ms_is_stamped_from_now_arg() {
        let snap = rich_snap("hello-web.1", "rt-abc", "running", 1_700_000_000_000);
        let first = derive_containers(std::slice::from_ref(&snap), 1_700_000_300_000);
        let later = derive_containers(&[snap], 1_700_000_900_000);
        assert_eq!(first[0].observed_at_ms, 1_700_000_300_000);
        assert_eq!(later[0].observed_at_ms, 1_700_000_900_000);
    }

    /// Stack + service derive from compose labels when
    /// present. Falls back to empty string (NOT the container name) so
    /// the controller can distinguish "no stack" from "stack named X".
    #[test]
    fn label_derived_stack_and_service_with_fallbacks() {
        // No labels: empty stack + empty service.
        let bare = ContainerSnapshot {
            name: "ad-hoc".into(),
            image: "alpine".into(),
            state: "running".into(),
            labels: HashMap::new(),
            id: "rt-bare".into(),
            created_at_ms: 0,
            exit_code: None,
        };
        let infos = derive_containers(&[bare], 1_700_000_000_000);
        assert_eq!(infos[0].stack, "");
        assert_eq!(infos[0].service, "");

        // `isengard.stack` label overrides compose project.
        let mut labels = HashMap::new();
        labels.insert("com.docker.compose.project".into(), "compose-name".into());
        labels.insert("isengard.stack".into(), "override-name".into());
        labels.insert("com.docker.compose.service".into(), "svc-name".into());
        let labelled = ContainerSnapshot {
            name: "labelled".into(),
            image: "alpine".into(),
            state: "running".into(),
            labels,
            id: "rt-labelled".into(),
            created_at_ms: 0,
            exit_code: None,
        };
        let infos = derive_containers(&[labelled], 1_700_000_000_000);
        assert_eq!(infos[0].stack, "override-name");
        assert_eq!(infos[0].service, "svc-name");

        // Containers without a runtime id are dropped (we can't mint a
        // stable operator id without one).
        let idless = ContainerSnapshot {
            name: "no-id".into(),
            image: "alpine".into(),
            state: "running".into(),
            labels: HashMap::new(),
            id: String::new(),
            created_at_ms: 0,
            exit_code: None,
        };
        let infos = derive_containers(&[idless], 1_700_000_000_000);
        assert!(infos.is_empty());
    }
}
