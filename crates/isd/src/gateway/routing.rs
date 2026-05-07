//! Gateway routing-table cache.
//!
//! Loaded from `GET /api/v1/routing/rules` at startup, refreshed by a
//! background poller every [`REFRESH_INTERVAL`]. The spec calls for
//! `/ws/events`-driven refresh; v0.3.5 keeps the polling path because it
//! is simpler + deterministic and the dev refresh latency (30s) is
//! tolerable. WS-driven refresh is a follow-up.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::credentials::ContextEntry;
use crate::login::verify_pinned_response;

/// Cadence at which we re-pull rules from the controller. 30s matches the
/// spec's "30s poll fallback".
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Subset of the controller's [`isengard_storage::RoutingRule`] we care about.
/// Tagged `#[serde(default)]` on the optional fields so the struct survives
/// future controller-side schema additions.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RoutingRuleDto {
    /// `id` from the controller's storage row.
    #[serde(default)]
    pub id: i64,
    /// e.g. `hello.isd`. Lower-cased before insertion into the table so
    /// Host-header lookup is case-insensitive.
    pub public_hostname: String,
    /// In-container port the proxy should forward to.
    pub container_port: u16,
    /// `pending|active|draining|failed` (rule lifecycle in storage). The
    /// proxy treats `pending` and `active` as "serve traffic"; the others
    /// fall through to a 502 with a hint.
    #[serde(default)]
    pub state: Option<String>,
    /// Optional container IP populated by the agent for production traffic.
    /// The controller currently does NOT populate this on the rule list
    /// endpoint, so we treat it as an opportunistic hint and fall back to
    /// the gateway's `--backend-host` when missing or unparseable.
    #[serde(default)]
    pub container_ip: Option<String>,
}

/// Rule-as-the-proxy-cares: just the bits the data path needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    pub container_port: u16,
    pub container_ip: Option<String>,
    pub state: RuleState,
    pub rule_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleState {
    /// Rule is in `pending` or `active`. Forward traffic.
    Servable,
    /// Rule exists but is in `draining` / `failed` / `disabled` / unknown.
    /// Don't forward; respond with a hint.
    NotServing,
}

/// Hostname-keyed routing table. Empty by default so an unreachable
/// controller still lets the gateway boot.
#[derive(Debug, Default, Clone)]
pub struct RoutingTable {
    by_host: HashMap<String, Backend>,
}

impl RoutingTable {
    /// Build a table from a controller-rule slice. Hostnames are lowered
    /// for case-insensitive lookup; duplicates keep the lowest `id` (the
    /// stable "primary" rule, mirroring `isd open`).
    pub fn from_rules(rules: &[RoutingRuleDto]) -> Self {
        let mut by_host: HashMap<String, Backend> = HashMap::new();
        let mut sorted = rules.to_vec();
        sorted.sort_by_key(|r| r.id);
        for r in sorted {
            let host = r.public_hostname.trim().to_ascii_lowercase();
            if host.is_empty() {
                continue;
            }
            // Keep the first (lowest-id) rule per hostname; later rules
            // for the same hostname are dropped silently.
            by_host.entry(host).or_insert_with(|| Backend {
                container_port: r.container_port,
                container_ip: r
                    .container_ip
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                state: classify_state(r.state.as_deref()),
                rule_id: r.id,
            });
        }
        Self { by_host }
    }

    /// Look up a backend by Host-header hostname (case-insensitive). The
    /// caller is responsible for stripping the `:port` suffix before
    /// calling.
    pub fn lookup(&self, host: &str) -> Option<&Backend> {
        self.by_host.get(&host.trim().to_ascii_lowercase())
    }

    /// Number of rules in the table.
    pub fn len(&self) -> usize {
        self.by_host.len()
    }

    /// True when the table holds no rules. Useful for empty-table guards
    /// in callers + sanity assertions in tests.
    pub fn is_empty(&self) -> bool {
        self.by_host.is_empty()
    }

    /// Sorted list of hostnames currently routable. Used by the proxy's
    /// 404 hint to point operators at what IS available.
    pub fn hostnames(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_host.keys().cloned().collect();
        v.sort();
        v
    }
}

fn classify_state(state: Option<&str>) -> RuleState {
    match state.map(|s| s.to_ascii_lowercase()).as_deref() {
        // Treat missing state as "servable" so older controllers (or rule
        // shapes that don't carry state) still work.
        None | Some("active") | Some("pending") | Some("enabled") => RuleState::Servable,
        _ => RuleState::NotServing,
    }
}

/// One pull from the controller. Returns the parsed rule list.
pub async fn fetch_rules(
    client: &reqwest::Client,
    ctx: &ContextEntry,
) -> Result<Vec<RoutingRuleDto>> {
    let resp = client
        .get(format!("{}/api/v1/routing/rules", ctx.controller_url))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .context("GET /api/v1/routing/rules")?;
    verify_pinned_response(&resp, &ctx.ca_fingerprint_sha256)?;
    let rules: Vec<RoutingRuleDto> = resp
        .error_for_status()
        .context("listing routing rules")?
        .json()
        .await
        .context("decoding routing rules JSON")?;
    Ok(rules)
}

/// Background poller. Re-pulls rules every [`REFRESH_INTERVAL`] and swaps
/// the table atomically. Errors are warn-logged and the loop continues so
/// a transient controller hiccup doesn't sink the gateway.
pub async fn refresh_loop(
    table: Arc<RwLock<RoutingTable>>,
    client: reqwest::Client,
    ctx: ContextEntry,
) {
    let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
    // Skip the immediate first tick: the boot-time pull already populated
    // the table.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match fetch_rules(&client, &ctx).await {
            Ok(rules) => {
                let new = RoutingTable::from_rules(&rules);
                let mut guard = table.write().await;
                let changed = guard.by_host != new.by_host;
                *guard = new;
                if changed {
                    tracing::info!(rule_count = guard.len(), "gateway: routing table refreshed");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "gateway: routing refresh failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(
        id: i64,
        host: &str,
        port: u16,
        state: Option<&str>,
        ip: Option<&str>,
    ) -> RoutingRuleDto {
        RoutingRuleDto {
            id,
            public_hostname: host.into(),
            container_port: port,
            state: state.map(str::to_string),
            container_ip: ip.map(str::to_string),
        }
    }

    #[test]
    fn builder_lowercases_hostnames() {
        let table = RoutingTable::from_rules(&[rule(1, "Hello.ISD", 80, Some("active"), None)]);
        assert!(!table.is_empty());
        assert_eq!(table.len(), 1);
        assert!(table.lookup("hello.isd").is_some());
        assert!(table.lookup("HELLO.ISD").is_some());
    }

    #[test]
    fn empty_input_yields_empty_table() {
        let table = RoutingTable::from_rules(&[]);
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.hostnames().is_empty());
    }

    #[test]
    fn builder_keeps_lowest_id_among_duplicates() {
        let rules = vec![
            rule(7, "hello.isd", 8081, Some("active"), None),
            rule(3, "hello.isd", 8082, Some("active"), None),
            rule(5, "hello.isd", 8083, Some("active"), None),
        ];
        let table = RoutingTable::from_rules(&rules);
        let b = table.lookup("hello.isd").unwrap();
        assert_eq!(b.rule_id, 3);
        assert_eq!(b.container_port, 8082);
    }

    #[test]
    fn unknown_state_is_not_serving() {
        let table = RoutingTable::from_rules(&[rule(1, "ghost.isd", 80, Some("draining"), None)]);
        let b = table.lookup("ghost.isd").unwrap();
        assert_eq!(b.state, RuleState::NotServing);
    }

    #[test]
    fn missing_state_field_is_servable() {
        let table = RoutingTable::from_rules(&[rule(1, "ok.isd", 80, None, None)]);
        let b = table.lookup("ok.isd").unwrap();
        assert_eq!(b.state, RuleState::Servable);
    }

    #[test]
    fn pending_and_active_both_servable() {
        let table = RoutingTable::from_rules(&[
            rule(1, "pend.isd", 80, Some("pending"), None),
            rule(2, "act.isd", 81, Some("active"), None),
        ]);
        assert_eq!(table.lookup("pend.isd").unwrap().state, RuleState::Servable);
        assert_eq!(table.lookup("act.isd").unwrap().state, RuleState::Servable);
    }

    #[test]
    fn empty_or_blank_container_ip_is_treated_as_none() {
        let table = RoutingTable::from_rules(&[
            rule(1, "a.isd", 80, Some("active"), Some("")),
            rule(2, "b.isd", 80, Some("active"), Some("   ")),
            rule(3, "c.isd", 80, Some("active"), Some("10.0.0.5")),
        ]);
        assert!(table.lookup("a.isd").unwrap().container_ip.is_none());
        assert!(table.lookup("b.isd").unwrap().container_ip.is_none());
        assert_eq!(
            table.lookup("c.isd").unwrap().container_ip.as_deref(),
            Some("10.0.0.5")
        );
    }

    #[test]
    fn hostnames_returns_sorted_list() {
        let table = RoutingTable::from_rules(&[
            rule(1, "z.isd", 80, Some("active"), None),
            rule(2, "a.isd", 80, Some("active"), None),
            rule(3, "m.isd", 80, Some("active"), None),
        ]);
        assert_eq!(
            table.hostnames(),
            vec!["a.isd".to_string(), "m.isd".into(), "z.isd".into()]
        );
    }

    #[test]
    fn dto_round_trips_through_serde() {
        let json = r#"[{
            "id": 1,
            "public_hostname": "hello.isd",
            "container_port": 80,
            "state": "active"
        }]"#;
        let parsed: Vec<RoutingRuleDto> = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].public_hostname, "hello.isd");
        assert_eq!(parsed[0].container_port, 80);
        assert_eq!(parsed[0].state.as_deref(), Some("active"));
    }

    #[test]
    fn dto_tolerates_extra_fields() {
        // The real controller payload carries fields like `host_id`,
        // `tls_mode`, `adapter`, `source` that the gateway doesn't need.
        // Make sure deserialization ignores them gracefully.
        let json = r#"[{
            "id": 99,
            "fleet": "default",
            "host_id": "01HX8AAAAAAAAAAAAAAAAAAAAA",
            "service_name": "hello",
            "container_port": 80,
            "public_hostname": "hello.isd",
            "protocol": "http",
            "adapter": "none",
            "tls_mode": "edge",
            "state": "active",
            "source": "label"
        }]"#;
        let parsed: Vec<RoutingRuleDto> = serde_json::from_str(json).unwrap();
        assert_eq!(parsed[0].id, 99);
    }
}
