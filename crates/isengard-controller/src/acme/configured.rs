//! Runtime resolver for wildcard ACME settings stored through `isd configure`.
//!
//! Boot-time env still works as a fallback, but the controller should treat
//! configured settings as the live source of truth so operators do not recreate
//! containers to issue or renew wildcard certs.

use anyhow::Result;
use isengard_storage::Inventory;
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::acme::{
    AcmeConfig, AcmeDns01Client, CloudflareDnsProvider, WildcardCertStore, WildcardGroup,
    parse_acme_domains, resolve_directory, scheduler_tick,
};
use crate::config::{ConfigDispatcher, ConfigValue, Zone};

/// Runtime ACME reconcile cadence for live `isd configure` changes.
const CONFIGURED_TICK_INTERVAL: Duration = Duration::from_secs(60);

/// Fully resolved wildcard ACME settings for one reconciler tick.
#[derive(Debug, Clone)]
pub struct DesiredAcmeConfig {
    /// ACME account contact email.
    pub email: String,
    /// Cloudflare API token used by the DNS-01 provider.
    pub cf_api_token: String,
    /// Wildcard identifier groups to issue or renew.
    pub groups: Vec<WildcardGroup>,
    /// Resolved ACME directory URL.
    pub directory_url: String,
}

/// Result of one configured scheduler reconciliation pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfiguredSchedulerTick {
    /// No complete wildcard ACME config is available.
    Disabled,
    /// A complete config was resolved and the scheduler path ran.
    Ran,
}

/// Spawn the runtime ACME reconciler.
///
/// The loop resolves `isd configure` state on every tick. Boot-time env
/// remains fallback-only through `fallback`, so existing installs keep
/// working while live configure changes take effect without a controller
/// recreate.
pub fn spawn_configured_scheduler(
    inventory: Arc<Inventory>,
    cert_store: Arc<WildcardCertStore>,
    dispatcher: Arc<ConfigDispatcher>,
    fallback: AcmeConfig,
    routing: Arc<crate::routing::RoutingPusher>,
) {
    tokio::spawn(async move {
        loop {
            match tick_configured_scheduler(
                inventory.clone(),
                cert_store.clone(),
                dispatcher.clone(),
                &fallback,
                routing.clone(),
            )
            .await
            {
                Ok(ConfiguredSchedulerTick::Ran) => {
                    debug!("acme: configured scheduler tick complete");
                }
                Ok(ConfiguredSchedulerTick::Disabled) => {
                    debug!("acme: configured scheduler disabled");
                }
                Err(e) => {
                    warn!(error = %e, "acme: configured scheduler tick failed");
                }
            }
            sleep(CONFIGURED_TICK_INTERVAL).await;
        }
    });
}

/// One runtime reconciliation pass using the production DNS-01 scheduler.
pub async fn tick_configured_scheduler(
    inventory: Arc<Inventory>,
    cert_store: Arc<WildcardCertStore>,
    dispatcher: Arc<ConfigDispatcher>,
    fallback: &AcmeConfig,
    routing: Arc<crate::routing::RoutingPusher>,
) -> Result<ConfiguredSchedulerTick> {
    tick_configured_scheduler_with(&dispatcher, fallback, |desired| {
        let inventory = inventory.clone();
        let cert_store = cert_store.clone();
        let routing = routing.clone();
        async move {
            let directory = desired.directory_url.clone();
            let groups = desired.groups.clone();
            let dns_provider = CloudflareDnsProvider::new(desired.cf_api_token);
            let acme_client = Arc::new(AcmeDns01Client::new(
                inventory.clone(),
                desired.email,
                directory.clone(),
                dns_provider,
            ));
            info!(
                directory = %directory,
                groups = groups.len(),
                "acme: configured DNS-01 scheduler running",
            );
            scheduler_tick(
                &inventory,
                &cert_store,
                &acme_client,
                &groups,
                Some(&routing),
            )
            .await
        }
    })
    .await
}

/// Resolve config, then run the injected scheduler path when enabled.
///
/// Kept generic so unit tests can prove runtime gating without touching
/// Cloudflare or Let's Encrypt.
pub async fn tick_configured_scheduler_with<F, Fut>(
    dispatcher: &ConfigDispatcher,
    fallback: &AcmeConfig,
    run_scheduler: F,
) -> Result<ConfiguredSchedulerTick>
where
    F: FnOnce(DesiredAcmeConfig) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let Some(desired) = resolve_desired_config(dispatcher, fallback).await? else {
        return Ok(ConfiguredSchedulerTick::Disabled);
    };
    run_scheduler(desired).await?;
    Ok(ConfiguredSchedulerTick::Ran)
}

/// Resolve wildcard ACME settings from `isd configure` with boot env fallback.
pub async fn resolve_desired_config(
    dispatcher: &ConfigDispatcher,
    fallback: &AcmeConfig,
) -> Result<Option<DesiredAcmeConfig>> {
    let email = configured_string(dispatcher, "acme.contact_email")
        .await?
        .or_else(|| fallback.email.clone());
    let cf_api_token = configured_string(dispatcher, "cloudflare.api_token")
        .await?
        .or_else(|| fallback.cf_api_token.clone());
    let directory_raw = configured_string(dispatcher, "acme.directory")
        .await?
        .or_else(|| non_empty(fallback.directory_url.clone()))
        .unwrap_or_default();

    let configured_groups = configured_wildcard_groups(dispatcher).await?;
    let groups = match configured_groups {
        Some(groups) => groups,
        None => parse_acme_domains(&fallback.domains),
    };

    let Some(email) = email else {
        return Ok(None);
    };
    let Some(cf_api_token) = cf_api_token else {
        return Ok(None);
    };
    if groups.is_empty() {
        return Ok(None);
    }

    Ok(Some(DesiredAcmeConfig {
        email,
        cf_api_token,
        groups,
        directory_url: resolve_directory(&directory_raw),
    }))
}

/// Read a configured string key and normalize blank values to unset.
async fn configured_string(dispatcher: &ConfigDispatcher, key: &str) -> Result<Option<String>> {
    match dispatcher.get(key).await? {
        Some(ConfigValue::Set(Value::String(value))) => Ok(non_empty(value)),
        _ => Ok(None),
    }
}

/// Resolve wildcard-enabled routing zones into ACME identifier groups.
async fn configured_wildcard_groups(
    dispatcher: &ConfigDispatcher,
) -> Result<Option<Vec<WildcardGroup>>> {
    let Some(ConfigValue::Set(value)) = dispatcher.get("routing.zones").await? else {
        return Ok(None);
    };
    let zones: Vec<Zone> = serde_json::from_value(value)?;
    let domains = zones
        .into_iter()
        .filter(|zone| zone.wildcard)
        .flat_map(|zone| {
            let name = zone.name.trim().trim_start_matches("*.").to_string();
            [format!("*.{name}"), name]
        })
        .collect::<Vec<_>>()
        .join(",");
    Ok(Some(parse_acme_domains(&domains)))
}

/// Convert a string into `None` when it is blank after trimming.
fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme::{AcmeConfig, LE_PRODUCTION_URL, LE_STAGING_URL};
    use crate::config::{ConfigDispatcher, Schema};
    use crate::secrets::SecretsStore;
    use isengard_storage::{Inventory, SettingStore};
    use serde_json::json;
    use std::sync::Arc;

    fn fixed_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        key
    }

    async fn test_dispatcher() -> Arc<ConfigDispatcher> {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let secrets = Arc::new(SecretsStore::new(inv.clone(), fixed_key()));
        let settings = Arc::new(SettingStore::new(inv));
        Arc::new(ConfigDispatcher::new(Schema::v01(), secrets, settings))
    }

    #[tokio::test]
    async fn configured_acme_uses_wildcard_zones_from_configure() {
        let dispatcher = test_dispatcher().await;
        dispatcher
            .put(
                "routing.zones",
                json!([
                    {"name": "vallee.casa", "wildcard": true},
                    {"name": "ignored.dev", "wildcard": false}
                ]),
                Some("test"),
            )
            .await
            .unwrap();
        dispatcher
            .put("cloudflare.api_token", json!("cf-token"), Some("test"))
            .await
            .unwrap();
        dispatcher
            .put("acme.contact_email", json!("ops@example.com"), Some("test"))
            .await
            .unwrap();
        dispatcher
            .put("acme.directory", json!("staging"), Some("test"))
            .await
            .unwrap();

        let desired = resolve_desired_config(&dispatcher, &AcmeConfig::default())
            .await
            .unwrap()
            .expect("wildcard config should be active");

        assert_eq!(desired.email, "ops@example.com");
        assert_eq!(desired.cf_api_token, "cf-token");
        assert_eq!(desired.directory_url, LE_STAGING_URL);
        assert_eq!(desired.groups.len(), 1);
        assert_eq!(
            desired.groups[0].identifiers,
            vec!["*.vallee.casa".to_string(), "vallee.casa".to_string()]
        );
    }

    #[tokio::test]
    async fn configured_acme_falls_back_to_boot_config_when_configure_is_unset() {
        let dispatcher = test_dispatcher().await;
        let fallback = AcmeConfig {
            email: Some("env@example.com".into()),
            cf_api_token: Some("env-token".into()),
            domains: "*.env.test,env.test".into(),
            directory_url: "prod".into(),
        };

        let desired = resolve_desired_config(&dispatcher, &fallback)
            .await
            .unwrap()
            .expect("fallback config should be active");

        assert_eq!(desired.email, "env@example.com");
        assert_eq!(desired.cf_api_token, "env-token");
        assert_eq!(desired.directory_url, LE_PRODUCTION_URL);
        assert_eq!(desired.groups.len(), 1);
        assert_eq!(
            desired.groups[0].identifiers,
            vec!["*.env.test".to_string(), "env.test".to_string()]
        );
    }

    #[tokio::test]
    async fn configured_acme_configure_values_override_boot_fallback() {
        let dispatcher = test_dispatcher().await;
        dispatcher
            .put(
                "routing.zones",
                json!([{"name": "configured.test", "wildcard": true}]),
                Some("test"),
            )
            .await
            .unwrap();
        dispatcher
            .put(
                "cloudflare.api_token",
                json!("configured-token"),
                Some("test"),
            )
            .await
            .unwrap();
        dispatcher
            .put(
                "acme.contact_email",
                json!("configured@example.com"),
                Some("test"),
            )
            .await
            .unwrap();
        let fallback = AcmeConfig {
            email: Some("env@example.com".into()),
            cf_api_token: Some("env-token".into()),
            domains: "*.env.test,env.test".into(),
            directory_url: "staging".into(),
        };

        let desired = resolve_desired_config(&dispatcher, &fallback)
            .await
            .unwrap()
            .expect("configured wildcard should be active");

        assert_eq!(desired.email, "configured@example.com");
        assert_eq!(desired.cf_api_token, "configured-token");
        assert_eq!(desired.directory_url, LE_STAGING_URL);
        assert_eq!(
            desired.groups[0].identifiers,
            vec![
                "*.configured.test".to_string(),
                "configured.test".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn configured_acme_without_domains_stays_disabled() {
        let dispatcher = test_dispatcher().await;
        dispatcher
            .put("cloudflare.api_token", json!("cf-token"), Some("test"))
            .await
            .unwrap();
        dispatcher
            .put("acme.contact_email", json!("ops@example.com"), Some("test"))
            .await
            .unwrap();

        let desired = resolve_desired_config(&dispatcher, &AcmeConfig::default())
            .await
            .unwrap();

        assert!(desired.is_none());
    }

    #[tokio::test]
    async fn configured_acme_tick_calls_scheduler_only_when_groups_exist() {
        let dispatcher = test_dispatcher().await;
        let fallback = AcmeConfig::default();

        let disabled = tick_configured_scheduler_with(&dispatcher, &fallback, |_desired| async {
            panic!("scheduler must not run without wildcard groups");
            #[allow(unreachable_code)]
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(disabled, ConfiguredSchedulerTick::Disabled);

        dispatcher
            .put(
                "routing.zones",
                json!([{"name": "configured.test", "wildcard": true}]),
                Some("test"),
            )
            .await
            .unwrap();
        dispatcher
            .put(
                "cloudflare.api_token",
                json!("configured-token"),
                Some("test"),
            )
            .await
            .unwrap();
        dispatcher
            .put(
                "acme.contact_email",
                json!("configured@example.com"),
                Some("test"),
            )
            .await
            .unwrap();

        let enabled =
            tick_configured_scheduler_with(&dispatcher, &fallback, |desired| async move {
                assert_eq!(desired.email, "configured@example.com");
                assert_eq!(desired.cf_api_token, "configured-token");
                assert_eq!(
                    desired.groups[0].identifiers,
                    vec![
                        "*.configured.test".to_string(),
                        "configured.test".to_string()
                    ]
                );
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(enabled, ConfiguredSchedulerTick::Ran);
    }
}
