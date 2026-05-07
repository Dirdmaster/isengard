//! Phase 12b: container-scope lifecycle-hook ingest from Docker labels (#54).
//!
//! Mirrors `policy_ingest.rs`: tail `ContainerLabelsReport` /
//! `ContainerLabelsRemoved` agent messages, parse `isengard.hooks.*` labels,
//! upsert / delete rows in `container_hooks`. The lifecycle subscriber on
//! the webhooks plugin reads this table when deployment events fire.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use isengard_core::hooks::parse_hook_labels;
use isengard_proto::pb::{ContainerLabelsRemoved, ContainerLabelsReport};
use isengard_storage::{HostId, Inventory, UpsertContainerHooks};
use tokio::sync::Mutex;

/// Owns the controller-side ingest of container lifecycle-hook labels.
///
/// Holds an in-memory `(host_id, container_id) -> container_name` map so
/// the remove path can find the right row to delete (the wire payload only
/// carries `container_id`).
pub struct HookLabelIngest {
    inv: Arc<Inventory>,
    by_container: Mutex<HashMap<(HostId, String), String>>,
}

impl HookLabelIngest {
    pub fn new(inv: Arc<Inventory>) -> Self {
        Self {
            inv,
            by_container: Mutex::new(HashMap::new()),
        }
    }

    /// Ingest a `ContainerLabelsReport` into `container_hooks`.
    ///
    /// - Parse `isengard.hooks.*`. If at least one URL is present, upsert
    ///   the row by `(host_id, container_name)`.
    /// - If no URLs are present, delete any existing row.
    /// - Track the `(host_id, container_id) -> container_name` mapping so
    ///   the remove path can find the row.
    pub async fn ingest(&self, host_id: HostId, report: &ContainerLabelsReport) -> Result<()> {
        let parsed = parse_hook_labels(&report.labels);

        {
            let mut guard = self.by_container.lock().await;
            guard.insert(
                (host_id, report.container_id.clone()),
                report.container_name.clone(),
            );
        }

        if parsed.has_any_url() {
            self.inv
                .upsert_container_hooks(UpsertContainerHooks {
                    host_id,
                    container_id: report.container_id.clone(),
                    container_name: report.container_name.clone(),
                    pre_deploy_url: parsed.pre_deploy_url,
                    post_deploy_url: parsed.post_deploy_url,
                    on_failure_url: parsed.on_failure_url,
                    secret: parsed.secret,
                })
                .await?;
            tracing::debug!(
                host_id = %host_id,
                container_name = %report.container_name,
                "hook labels: upserted container_hooks row",
            );
        } else {
            let deleted = self
                .inv
                .delete_container_hooks_by_name(host_id, &report.container_name)
                .await?;
            if deleted {
                tracing::debug!(
                    host_id = %host_id,
                    container_name = %report.container_name,
                    "hook labels: deleted container_hooks row (labels removed)",
                );
            }
        }
        Ok(())
    }

    /// Drop the container_hooks row for a removed container. Looks up the
    /// container_name from the in-memory map; if absent, the row is
    /// deleted-by-id which still works because the table indexes
    /// `(host_id, container_id)`.
    pub async fn ingest_removed(&self, host_id: HostId, ev: &ContainerLabelsRemoved) -> Result<()> {
        let name_opt = {
            let mut guard = self.by_container.lock().await;
            guard.remove(&(host_id, ev.container_id.clone()))
        };

        if let Some(name) = &name_opt {
            let _ = self.inv.delete_container_hooks_by_name(host_id, name).await;
        }
        // Belt-and-braces: also delete by container_id in case we missed
        // the prior report (controller restart between report + remove).
        let _ = self
            .inv
            .delete_container_hooks_by_id(host_id, &ev.container_id)
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::EnrollHost;
    use std::collections::HashMap;

    async fn setup() -> (Arc<Inventory>, HostId) {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let host = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp".into(),
                hostname: "h".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0".into(),
                docker_version: "27".into(),
                fleet: "default".into(),
            })
            .await
            .unwrap();
        (inv, host)
    }

    fn report(cid: &str, name: &str, pairs: &[(&str, &str)]) -> ContainerLabelsReport {
        let labels: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        ContainerLabelsReport {
            container_id: cid.into(),
            container_name: name.into(),
            image: "test".into(),
            labels,
        }
    }

    #[tokio::test]
    async fn ingest_with_pre_deploy_url_upserts_row() {
        let (inv, host) = setup().await;
        let ingest = HookLabelIngest::new(inv.clone());
        let r = report(
            "cid-1",
            "web",
            &[("isengard.hooks.pre_deploy", "https://h.example/pre")],
        );
        ingest.ingest(host, &r).await.unwrap();

        let row = inv
            .get_container_hooks(host, "web")
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.pre_deploy_url.as_deref(), Some("https://h.example/pre"));
        assert!(row.post_deploy_url.is_none());
    }

    #[tokio::test]
    async fn ingest_with_no_hook_labels_deletes_existing_row() {
        let (inv, host) = setup().await;
        let ingest = HookLabelIngest::new(inv.clone());

        // First populate.
        let r1 = report(
            "cid-1",
            "web",
            &[("isengard.hooks.pre_deploy", "https://h.example/pre")],
        );
        ingest.ingest(host, &r1).await.unwrap();
        assert!(
            inv.get_container_hooks(host, "web")
                .await
                .unwrap()
                .is_some()
        );

        // Now ingest a report with no hooks labels.
        let r2 = report("cid-1", "web", &[("isengard.expose", "x.test")]);
        ingest.ingest(host, &r2).await.unwrap();
        assert!(
            inv.get_container_hooks(host, "web")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn ingest_removed_drops_row_by_name() {
        let (inv, host) = setup().await;
        let ingest = HookLabelIngest::new(inv.clone());

        let r = report(
            "cid-1",
            "web",
            &[("isengard.hooks.post_deploy", "https://h.example/post")],
        );
        ingest.ingest(host, &r).await.unwrap();

        ingest
            .ingest_removed(
                host,
                &ContainerLabelsRemoved {
                    container_id: "cid-1".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            inv.get_container_hooks(host, "web")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn ingest_removed_with_no_prior_report_falls_back_to_id_lookup() {
        let (inv, host) = setup().await;
        let ingest = HookLabelIngest::new(inv.clone());

        // Pre-seed a row directly.
        inv.upsert_container_hooks(UpsertContainerHooks {
            host_id: host,
            container_id: "cid-orphan".into(),
            container_name: "orphan".into(),
            pre_deploy_url: Some("https://h".into()),
            post_deploy_url: None,
            on_failure_url: None,
            secret: None,
        })
        .await
        .unwrap();

        // Remove without prior report: id-based delete must still clean up.
        ingest
            .ingest_removed(
                host,
                &ContainerLabelsRemoved {
                    container_id: "cid-orphan".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            inv.get_container_hooks(host, "orphan")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn ingest_with_secret_round_trips() {
        let (inv, host) = setup().await;
        let ingest = HookLabelIngest::new(inv.clone());
        let r = report(
            "cid-1",
            "web",
            &[
                ("isengard.hooks.pre_deploy", "https://h.example/pre"),
                ("isengard.hooks.secret", "shared"),
            ],
        );
        ingest.ingest(host, &r).await.unwrap();
        let row = inv
            .get_container_hooks(host, "web")
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.secret.as_deref(), Some("shared"));
    }

    #[tokio::test]
    async fn upsert_overwrites_when_url_changes() {
        let (inv, host) = setup().await;
        let ingest = HookLabelIngest::new(inv.clone());

        let r1 = report(
            "cid-1",
            "web",
            &[("isengard.hooks.pre_deploy", "https://old")],
        );
        ingest.ingest(host, &r1).await.unwrap();

        let r2 = report(
            "cid-1",
            "web",
            &[("isengard.hooks.pre_deploy", "https://new")],
        );
        ingest.ingest(host, &r2).await.unwrap();

        let row = inv.get_container_hooks(host, "web").await.unwrap().unwrap();
        assert_eq!(row.pre_deploy_url.as_deref(), Some("https://new"));
    }
}
