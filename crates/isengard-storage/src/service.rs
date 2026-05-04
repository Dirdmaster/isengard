//! Service entity: one container belonging to a host (and optionally a stack).

use crate::host::HostId;
use crate::stack::StackId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServiceId(pub i64);

impl std::fmt::Display for ServiceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    Running,
    Stopped,
    Restarting,
    Unknown,
}

impl ServiceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Restarting => "restarting",
            Self::Unknown => "unknown",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "stopped" => Self::Stopped,
            "restarting" => Self::Restarting,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    pub id: ServiceId,
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub name: String,
    pub image: String,
    pub state: ServiceState,
    pub last_seen_at: DateTime<Utc>,
    pub deploy_strategy_override: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertService {
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub name: String,
    pub image: String,
    pub state: ServiceState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::EnrollHost;
    use crate::inventory::Inventory;
    use crate::stack::{InsertStack, StackSource};

    async fn setup() -> (Inventory, ServiceId) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                hostname: "h".into(),
                fingerprint: "fp".into(),
                fleet: "default".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "24.0".into(),
            })
            .await
            .unwrap();
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        let svc_id = inv
            .insert_service(InsertService {
                host_id,
                stack_id: Some(stack_id),
                name: "web".into(),
                image: "nginx:alpine".into(),
                state: ServiceState::Running,
            })
            .await
            .unwrap();
        (inv, svc_id)
    }

    #[tokio::test]
    async fn override_starts_null() {
        let (inv, svc_id) = setup().await;
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        assert_eq!(svc.deploy_strategy_override, None);
    }

    #[tokio::test]
    async fn set_and_get_override() {
        let (inv, svc_id) = setup().await;
        inv.set_service_deploy_strategy_override(svc_id, Some("blue-green"))
            .await
            .unwrap();
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        assert_eq!(svc.deploy_strategy_override.as_deref(), Some("blue-green"));

        inv.set_service_deploy_strategy_override(svc_id, None)
            .await
            .unwrap();
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        assert_eq!(svc.deploy_strategy_override, None);
    }

    #[tokio::test]
    async fn lookup_by_name_returns_matching_service() {
        let (inv, svc_id) = setup().await;
        let svc = inv.get_service(svc_id).await.unwrap().unwrap();
        let by_name = inv
            .get_service_by_name(svc.host_id, svc.stack_id, "web")
            .await
            .unwrap()
            .expect("service found by name");
        assert_eq!(by_name.id, svc_id);

        let missing = inv
            .get_service_by_name(svc.host_id, svc.stack_id, "doesnotexist")
            .await
            .unwrap();
        assert!(missing.is_none());
    }
}
