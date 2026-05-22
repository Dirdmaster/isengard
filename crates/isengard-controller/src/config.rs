//! Static schema + dispatcher for `isd configure`.
//!
//! One file, two surfaces:
//!
//! 1. [`Schema`]: the static table of every settable key. Each
//!    [`SchemaEntry`] declares its type ([`KeyType`]), whether the
//!    value should be persisted as a secret, an optional default, and
//!    a one-line operator-facing description. Adding a key here is
//!    the only place needed to make it settable; the CLI fetches the
//!    schema verbatim via `isd configure schema`.
//! 2. [`ConfigDispatcher`]: the controller-side handle that routes a
//!    write to the right backing store based on the schema. Secret
//!    keys land in the [`SecretsStore`]; non-secret keys land in the
//!    [`SettingStore`]. Reads return [`ConfigValue::Set`] when a row
//!    exists or [`ConfigValue::Default`] when the schema entry has a
//!    default and no row exists.
//!
//! No HTTP, no CLI here. PR 2 of the `isd configure` track wires
//! [`ConfigDispatcher`] into the dashboard plugin; PR 3 builds the
//! CLI surface against those routes.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use isengard_storage::SettingStore;
use serde::Serialize;
use serde_json::Value;

use crate::secrets::SecretsStore;

/// Type of a config value.
///
/// Validation at write time rejects mismatches: a [`KeyType::Int`]
/// entry refuses a JSON string, a [`KeyType::Secret`] entry refuses
/// an empty string or anything that is not a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyType {
    /// Plain UTF-8 string, persisted to the `settings` table.
    String,
    /// String persisted to the encrypted `secrets` table.
    ///
    /// The CLI also refuses inline `set <key> <value>` for these; the
    /// value must come from `--stdin` or `--from-file` so it never
    /// lands in shell history.
    Secret,
    /// Signed integer.
    Int,
    /// Boolean.
    Bool,
    /// Ordered list of non-empty UTF-8 strings.
    ///
    /// Persisted as a JSON array. Used for operator-managed multi-value
    /// keys like `routing.zones`. Validation rejects mixed-type arrays,
    /// non-string elements, and empty-string elements so the wire shape
    /// stays predictable for downstream consumers (the cascade picker on
    /// `routing.default_zone`, for instance).
    StringList,
}

/// One row in the static schema.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaEntry {
    /// Dotted lowercase key, e.g. `cloudflare.api_token`.
    pub key: &'static str,
    /// Operator-facing label rendered in menu rows and prompts.
    ///
    /// Example: `Cloudflare API token` for `cloudflare.api_token`. The
    /// CLI menu prefers this over the dotted key so the surface stays
    /// readable as the schema grows (Route53, deSEC, etc. all nest
    /// under one category and need short labels to disambiguate).
    pub display_name: &'static str,
    /// Top-level menu bucket used by the CLI's two-level configure menu.
    ///
    /// Grouped by feature (e.g. `Certificates`, `Routing`, `SSH bastion`)
    /// rather than by provider so future DNS-01 backends slot in without
    /// inventing a new top-level bucket.
    pub category: &'static str,
    /// Type of the value bound to `key`.
    #[serde(rename = "type")]
    pub ty: KeyType,
    /// Default value when no row is present in the backing store.
    ///
    /// `None` means the key is unset by default; readers should treat
    /// an absent row as a hard "not configured".
    pub default: Option<Value>,
    /// Single-line operator-facing description.
    pub doc: &'static str,
    /// Optional fixed choice set. When present, the CLI presents a
    /// select widget instead of free text input, regardless of [`Self::ty`].
    ///
    /// `None` means the key accepts any value of the declared type. A
    /// non-empty slice locks the operator to one of the listed values
    /// (the controller still type-validates via [`Self::validate`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<&'static [Choice]>,
}

/// One option in a fixed [`SchemaEntry::choices`] set.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Choice {
    /// Wire value persisted to the backing store when this choice
    /// is selected.
    pub value: &'static str,
    /// Operator-facing label rendered in the select widget.
    pub label: &'static str,
}

impl SchemaEntry {
    /// Returns `Ok(())` when `v` is a valid value for [`Self::ty`].
    pub fn validate(&self, v: &Value) -> Result<()> {
        match (self.ty, v) {
            (KeyType::String, Value::String(_)) => Ok(()),
            (KeyType::Secret, Value::String(s)) if !s.is_empty() => Ok(()),
            (KeyType::Int, Value::Number(n)) if n.is_i64() || n.is_u64() => Ok(()),
            (KeyType::Bool, Value::Bool(_)) => Ok(()),
            (KeyType::StringList, Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    match item {
                        Value::String(s) if !s.is_empty() => {}
                        Value::String(_) => {
                            return Err(anyhow!(
                                "invalid value for {} (StringList): element {i} is empty",
                                self.key
                            ));
                        }
                        _ => {
                            return Err(anyhow!(
                                "invalid value for {} (StringList): element {i} is not a string: {item}",
                                self.key
                            ));
                        }
                    }
                }
                Ok(())
            }
            _ => Err(anyhow!(
                "invalid value for {} (type {:?}): {}",
                self.key,
                self.ty,
                v
            )),
        }
    }

    /// Predicate. True for [`KeyType::Secret`].
    pub fn is_secret(&self) -> bool {
        matches!(self.ty, KeyType::Secret)
    }
}

/// Static schema for `isd configure`.
///
/// Built once at controller boot via [`Schema::v01`]. The dispatcher
/// borrows it for every read and write.
#[derive(Debug, Clone)]
pub struct Schema {
    /// Every settable key, in declaration order.
    entries: Vec<SchemaEntry>,
}

impl Schema {
    /// The v0.1 schema. Adding a key here is the only change needed
    /// to make it settable.
    pub fn v01() -> Self {
        Self {
            entries: vec![
                SchemaEntry {
                    key: "cloudflare.api_token",
                    display_name: "Cloudflare API token",
                    category: "Certificates",
                    ty: KeyType::Secret,
                    default: None,
                    doc: "Cloudflare API token used by cf-tunnel and DNS-01 solver",
                    choices: None,
                },
                SchemaEntry {
                    key: "cloudflare.zone_id",
                    display_name: "Cloudflare zone ID",
                    category: "Certificates",
                    ty: KeyType::String,
                    default: None,
                    doc: "Cloudflare zone UUID (for DNS-01)",
                    choices: None,
                },
                SchemaEntry {
                    key: "routing.zones",
                    display_name: "DNS zones",
                    category: "Routing",
                    ty: KeyType::StringList,
                    default: None,
                    doc: "DNS zones you manage (e.g. weavers.engineering). routing.default_zone picks from this list.",
                    choices: None,
                },
                SchemaEntry {
                    key: "routing.default_zone",
                    display_name: "Default zone",
                    category: "Routing",
                    ty: KeyType::String,
                    default: None,
                    doc: "Default zone appended to bare hostnames in stack manifests",
                    choices: None,
                },
                SchemaEntry {
                    key: "acme.contact_email",
                    display_name: "Contact email",
                    category: "Certificates",
                    ty: KeyType::String,
                    default: None,
                    doc: "ACME account contact email",
                    choices: None,
                },
                SchemaEntry {
                    key: "acme.directory",
                    display_name: "ACME directory (prod vs staging)",
                    category: "Certificates",
                    ty: KeyType::String,
                    default: Some(serde_json::json!(
                        "https://acme-v02.api.letsencrypt.org/directory"
                    )),
                    doc: "ACME directory URL",
                    choices: Some(&[
                        Choice {
                            value: "https://acme-v02.api.letsencrypt.org/directory",
                            label: "Let's Encrypt production",
                        },
                        Choice {
                            value: "https://acme-staging-v02.api.letsencrypt.org/directory",
                            label: "Let's Encrypt staging",
                        },
                    ]),
                },
                SchemaEntry {
                    key: "ssh.max_ttl_seconds",
                    display_name: "Max cert TTL (seconds)",
                    category: "SSH bastion",
                    ty: KeyType::Int,
                    default: Some(serde_json::json!(2_592_000)),
                    doc: "Maximum TTL ceiling for minted SSH operator certs",
                    choices: None,
                },
            ],
        }
    }

    /// Lookup by exact key.
    pub fn get(&self, key: &str) -> Option<&SchemaEntry> {
        self.entries.iter().find(|e| e.key == key)
    }

    /// Iterator over every key in declaration order.
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.iter().map(|e| e.key)
    }

    /// Borrowed view of every entry, in declaration order.
    pub fn entries(&self) -> &[SchemaEntry] {
        &self.entries
    }

    /// Top-3 closest schema keys to `unknown` by Levenshtein distance,
    /// capped at distance 4. Used to power did-you-mean hints on
    /// `isd configure set <typo>`.
    pub fn did_you_mean(&self, unknown: &str) -> Vec<String> {
        let mut scored: Vec<(usize, &'static str)> = self
            .entries
            .iter()
            .map(|e| (levenshtein(e.key, unknown), e.key))
            .collect();
        scored.sort_by_key(|(d, _)| *d);
        scored
            .into_iter()
            .filter(|(d, _)| *d <= 4)
            .take(3)
            .map(|(_, k)| k.to_string())
            .collect()
    }
}

/// Levenshtein distance with O(min(|a|,|b|)) extra memory.
///
/// Used by [`Schema::did_you_mean`]. Plain text only; the key
/// alphabet is `[a-zA-Z0-9._-]` so we lean on `chars()`.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Result of a [`ConfigDispatcher::get`].
///
/// Distinguishes a value that the operator explicitly set
/// ([`Self::Set`]) from one falling back to the schema's default
/// ([`Self::Default`]). The HTTP and CLI layers use this to print the
/// `(default)` marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValue {
    /// Value persisted in the backing store.
    Set(Value),
    /// Value from the schema's default; no row in the backing store.
    Default(Value),
}

impl ConfigValue {
    /// Unwrap to the inner JSON value, ignoring the set-vs-default
    /// distinction.
    pub fn value(&self) -> &Value {
        match self {
            ConfigValue::Set(v) | ConfigValue::Default(v) => v,
        }
    }

    /// True when this value came from a backing-store row.
    pub fn is_set(&self) -> bool {
        matches!(self, ConfigValue::Set(_))
    }
}

/// Routes `isd configure` reads and writes to the right backing
/// store based on the static schema.
///
/// Secret keys go to the [`SecretsStore`] (encrypted with the master
/// key); everything else goes to the [`SettingStore`] (plain JSON).
/// The CLI never picks the storage target.
#[derive(Clone)]
pub struct ConfigDispatcher {
    /// Static schema; declared once at boot.
    schema: Arc<Schema>,
    /// Secrets backing store.
    secrets: Arc<SecretsStore>,
    /// Non-secret backing store.
    settings: Arc<SettingStore>,
}

impl ConfigDispatcher {
    /// Bundles a [`Schema`], [`SecretsStore`], and [`SettingStore`]
    /// into one dispatcher.
    pub fn new(schema: Schema, secrets: Arc<SecretsStore>, settings: Arc<SettingStore>) -> Self {
        Self {
            schema: Arc::new(schema),
            secrets,
            settings,
        }
    }

    /// Borrowed view of the schema. Used by route handlers that
    /// echo the schema verbatim.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Writes `value` for `key`, routing to the right backing store.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `key` is unknown (with did-you-mean
    /// suggestions when there is a close match), when `value` fails
    /// the schema's type check, or when the backing store rejects
    /// the row.
    pub async fn put(&self, key: &str, value: Value, by: Option<&str>) -> Result<()> {
        let entry = self.schema.get(key).ok_or_else(|| {
            let suggestions = self.schema.did_you_mean(key);
            if suggestions.is_empty() {
                anyhow!("unknown config key: {key}")
            } else {
                anyhow!(
                    "unknown config key: {key}; did you mean: {}?",
                    suggestions.join(", ")
                )
            }
        })?;
        entry.validate(&value)?;
        if entry.is_secret() {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow!("secret value for {key} must be a string"))?;
            self.secrets
                .put(key, s.as_bytes(), by)
                .await
                .map_err(|e| anyhow!("secrets put for {key}: {e}"))?;
        } else {
            self.settings.upsert(key, &value, by).await?;
        }
        Ok(())
    }

    /// Reads `key`. Returns [`ConfigValue::Set`] when a row exists,
    /// [`ConfigValue::Default`] when the schema has a default and no
    /// row, and `None` otherwise.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `key` is unknown, or when the backing store
    /// fails with anything other than not-found.
    pub async fn get(&self, key: &str) -> Result<Option<ConfigValue>> {
        let entry = self
            .schema
            .get(key)
            .ok_or_else(|| anyhow!("unknown config key: {key}"))?;
        if entry.is_secret() {
            match self.secrets.fetch(key).await {
                Ok(bytes) => {
                    let s = String::from_utf8(bytes)
                        .map_err(|e| anyhow!("secret {key} is not valid utf-8: {e}"))?;
                    Ok(Some(ConfigValue::Set(Value::String(s))))
                }
                Err(crate::secrets::SecretsError::NotFound(_)) => {
                    Ok(entry.default.clone().map(ConfigValue::Default))
                }
                Err(e) => Err(anyhow!("secrets fetch for {key}: {e}")),
            }
        } else {
            match self.settings.get(key).await? {
                Some(v) => Ok(Some(ConfigValue::Set(v))),
                None => Ok(entry.default.clone().map(ConfigValue::Default)),
            }
        }
    }

    /// Deletes `key` from its backing store. Returns `true` when a
    /// row was removed.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `key` is unknown or when the backing store
    /// fails.
    pub async fn delete(&self, key: &str) -> Result<bool> {
        let entry = self
            .schema
            .get(key)
            .ok_or_else(|| anyhow!("unknown config key: {key}"))?;
        if entry.is_secret() {
            self.secrets
                .delete(key)
                .await
                .map_err(|e| anyhow!("secrets delete for {key}: {e}"))
        } else {
            Ok(self.settings.delete(key).await?)
        }
    }

    /// Snapshots every schema key with its current value (set or
    /// default). Used by the `list` route handler.
    ///
    /// # Errors
    ///
    /// Bubbles up any backing-store failure encountered while reading
    /// a particular key.
    pub async fn list(&self) -> Result<Vec<(SchemaEntry, Option<ConfigValue>)>> {
        let mut out = Vec::with_capacity(self.schema.entries.len());
        for entry in self.schema.entries() {
            let value = self.get(entry.key).await?;
            out.push((entry.clone(), value));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::Inventory;
    use serde_json::json;

    fn fixed_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    async fn test_dispatcher() -> ConfigDispatcher {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let secrets = Arc::new(SecretsStore::new(inv.clone(), fixed_key()));
        let settings = Arc::new(SettingStore::new(inv));
        ConfigDispatcher::new(Schema::v01(), secrets, settings)
    }

    // --- schema ----------------------------------------------------------

    #[test]
    fn schema_lists_all_v01_keys() {
        let schema = Schema::v01();
        let keys: Vec<&str> = schema.keys().collect();
        assert!(keys.contains(&"cloudflare.api_token"));
        assert!(keys.contains(&"cloudflare.zone_id"));
        assert!(keys.contains(&"routing.default_zone"));
        assert!(keys.contains(&"acme.contact_email"));
        assert!(keys.contains(&"acme.directory"));
        assert!(keys.contains(&"ssh.max_ttl_seconds"));
        assert!(keys.contains(&"routing.zones"));
        assert_eq!(keys.len(), 7);
    }

    #[test]
    fn schema_marks_api_token_as_secret() {
        let schema = Schema::v01();
        let entry = schema.get("cloudflare.api_token").unwrap();
        assert_eq!(entry.ty, KeyType::Secret);
        assert!(entry.is_secret());
    }

    #[test]
    fn schema_provides_acme_directory_default() {
        let schema = Schema::v01();
        let entry = schema.get("acme.directory").unwrap();
        assert_eq!(
            entry.default.as_ref().unwrap(),
            &json!("https://acme-v02.api.letsencrypt.org/directory")
        );
    }

    #[test]
    fn schema_provides_ssh_max_ttl_default() {
        let schema = Schema::v01();
        let entry = schema.get("ssh.max_ttl_seconds").unwrap();
        assert_eq!(entry.default.as_ref().unwrap(), &json!(2_592_000));
    }

    #[test]
    fn schema_validates_int_type() {
        let schema = Schema::v01();
        let entry = schema.get("ssh.max_ttl_seconds").unwrap();
        assert_eq!(entry.ty, KeyType::Int);
        assert!(entry.validate(&json!(3600)).is_ok());
        assert!(entry.validate(&json!("not-an-int")).is_err());
    }

    #[test]
    fn schema_validates_string_list_accepts_non_empty_strings() {
        let schema = Schema::v01();
        let entry = schema.get("routing.zones").unwrap();
        assert_eq!(entry.ty, KeyType::StringList);
        assert!(entry.validate(&json!([])).is_ok());
        assert!(
            entry
                .validate(&json!(["a.com", "b.com", "weavers.engineering"]))
                .is_ok()
        );
    }

    #[test]
    fn schema_validates_string_list_rejects_non_array() {
        let schema = Schema::v01();
        let entry = schema.get("routing.zones").unwrap();
        assert!(entry.validate(&json!("a.com")).is_err());
        assert!(entry.validate(&json!(42)).is_err());
        assert!(entry.validate(&json!({"zones": ["a.com"]})).is_err());
    }

    #[test]
    fn schema_validates_string_list_rejects_non_string_elements() {
        let schema = Schema::v01();
        let entry = schema.get("routing.zones").unwrap();
        let err = entry.validate(&json!(["a.com", 42])).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("element 1"), "msg: {msg}");
        assert!(msg.contains("not a string"), "msg: {msg}");
    }

    #[test]
    fn schema_validates_string_list_rejects_empty_element() {
        let schema = Schema::v01();
        let entry = schema.get("routing.zones").unwrap();
        let err = entry.validate(&json!(["a.com", ""])).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("element 1"), "msg: {msg}");
        assert!(msg.contains("empty"), "msg: {msg}");
    }

    #[test]
    fn schema_validates_secret_rejects_empty_string() {
        let schema = Schema::v01();
        let entry = schema.get("cloudflare.api_token").unwrap();
        assert!(entry.validate(&json!("ok")).is_ok());
        assert!(entry.validate(&json!("")).is_err());
        assert!(entry.validate(&json!(42)).is_err());
    }

    #[test]
    fn schema_entries_carry_display_name_and_category() {
        let schema = Schema::v01();
        let allowed = ["Certificates", "Routing", "SSH bastion"];
        for entry in schema.entries() {
            assert!(
                !entry.display_name.is_empty(),
                "{} missing display_name",
                entry.key
            );
            assert!(!entry.category.is_empty(), "{} missing category", entry.key);
            assert!(
                allowed.contains(&entry.category),
                "{} has unexpected category {:?}",
                entry.key,
                entry.category
            );
        }
    }

    #[test]
    fn schema_serializes_display_name_and_category() {
        // The dashboard echoes SchemaEntry verbatim via serde. If a
        // `skip` attribute ever creeps in on these fields, the CLI menu
        // breaks silently. Guard that here so the wire shape stays
        // stable.
        let schema = Schema::v01();
        let entry = schema.get("cloudflare.api_token").unwrap();
        let raw = serde_json::to_string(entry).unwrap();
        assert!(raw.contains("display_name"), "raw: {raw}");
        assert!(raw.contains("Cloudflare API token"), "raw: {raw}");
        assert!(raw.contains("category"), "raw: {raw}");
        assert!(raw.contains("Certificates"), "raw: {raw}");
    }

    #[test]
    fn schema_choices_well_formed_for_every_entry() {
        // Every entry has `choices` set to either None or a non-empty
        // slice. An empty slice would render as a select widget with
        // zero options, which is unreachable for the operator.
        let schema = Schema::v01();
        for entry in schema.entries() {
            if let Some(choices) = entry.choices {
                assert!(
                    !choices.is_empty(),
                    "{} has an empty choices slice",
                    entry.key
                );
                // Every label and value must be non-empty so the select
                // widget renders a readable row.
                for c in choices {
                    assert!(!c.value.is_empty(), "{} has empty choice value", entry.key);
                    assert!(!c.label.is_empty(), "{} has empty choice label", entry.key);
                }
            }
        }
    }

    #[test]
    fn acme_directory_exposes_prod_and_staging_choices() {
        let schema = Schema::v01();
        let entry = schema.get("acme.directory").unwrap();
        let choices = entry.choices.expect("acme.directory carries choices");
        assert_eq!(choices.len(), 2);
        assert!(
            choices
                .iter()
                .any(|c| c.value == "https://acme-v02.api.letsencrypt.org/directory")
        );
        assert!(
            choices
                .iter()
                .any(|c| c.value == "https://acme-staging-v02.api.letsencrypt.org/directory")
        );
    }

    #[test]
    fn schema_serializes_choices_for_acme_directory() {
        // The dashboard echoes SchemaEntry verbatim via serde. Operator
        // CLI relies on `choices` propagating over the wire to decide
        // between text input and select widget.
        let schema = Schema::v01();
        let entry = schema.get("acme.directory").unwrap();
        let raw = serde_json::to_string(entry).unwrap();
        assert!(raw.contains("choices"), "raw: {raw}");
        assert!(raw.contains("Let's Encrypt production"), "raw: {raw}");
        assert!(raw.contains("Let's Encrypt staging"), "raw: {raw}");
    }

    #[test]
    fn schema_skips_choices_serialization_when_none() {
        // Keys without choices must not emit `"choices": null` over the
        // wire. We rely on serde's skip_serializing_if so the wire shape
        // for keys lacking a choices set stays unchanged.
        let schema = Schema::v01();
        let entry = schema.get("cloudflare.api_token").unwrap();
        let raw = serde_json::to_string(entry).unwrap();
        assert!(!raw.contains("choices"), "raw: {raw}");
    }

    #[test]
    fn did_you_mean_finds_close_match() {
        let schema = Schema::v01();
        let suggestions = schema.did_you_mean("acme.contac_email");
        assert!(suggestions.contains(&"acme.contact_email".to_string()));
    }

    #[test]
    fn did_you_mean_returns_empty_for_far_match() {
        let schema = Schema::v01();
        let suggestions = schema.did_you_mean("completely.unrelated.identifier.path");
        assert!(suggestions.is_empty());
    }

    // --- dispatcher ------------------------------------------------------

    #[tokio::test]
    async fn put_string_routes_to_setting_store() {
        let d = test_dispatcher().await;
        d.put("acme.contact_email", json!("ops@example.com"), Some("test"))
            .await
            .unwrap();
        let v = d.get("acme.contact_email").await.unwrap();
        assert_eq!(v, Some(ConfigValue::Set(json!("ops@example.com"))));
        // secret store should not have the key.
        assert!(d.secrets.fetch("acme.contact_email").await.is_err());
    }

    #[tokio::test]
    async fn put_string_list_round_trips_through_setting_store() {
        let d = test_dispatcher().await;
        d.put(
            "routing.zones",
            json!(["weavers.engineering", "vallee.casa"]),
            Some("test"),
        )
        .await
        .unwrap();
        let v = d.get("routing.zones").await.unwrap();
        assert_eq!(
            v,
            Some(ConfigValue::Set(json!([
                "weavers.engineering",
                "vallee.casa"
            ])))
        );
    }

    #[tokio::test]
    async fn put_string_list_rejects_mixed_types() {
        let d = test_dispatcher().await;
        let err = d
            .put("routing.zones", json!(["ok.com", 42]), None)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not a string"));
    }

    #[tokio::test]
    async fn put_secret_routes_to_secrets_store() {
        let d = test_dispatcher().await;
        d.put("cloudflare.api_token", json!("cf-token-xyz"), None)
            .await
            .unwrap();
        // settings store should not have it.
        assert_eq!(d.settings.get("cloudflare.api_token").await.unwrap(), None);
        // secrets fetch should return the bytes.
        let bytes = d.secrets.fetch("cloudflare.api_token").await.unwrap();
        assert_eq!(bytes, b"cf-token-xyz");
        // dispatcher get echoes the value as Set.
        let v = d.get("cloudflare.api_token").await.unwrap();
        assert_eq!(v, Some(ConfigValue::Set(json!("cf-token-xyz"))));
    }

    #[tokio::test]
    async fn put_unknown_key_returns_did_you_mean() {
        let d = test_dispatcher().await;
        let err = d
            .put("cloudflrae.api_token", json!("x"), None)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("did you mean"), "msg: {msg}");
        assert!(msg.contains("cloudflare.api_token"), "msg: {msg}");
    }

    #[tokio::test]
    async fn put_rejects_wrong_type() {
        let d = test_dispatcher().await;
        let err = d
            .put("ssh.max_ttl_seconds", json!("not-an-int"), None)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid value"));
    }

    #[tokio::test]
    async fn get_returns_default_when_unset() {
        let d = test_dispatcher().await;
        let v = d.get("acme.directory").await.unwrap();
        assert_eq!(
            v,
            Some(ConfigValue::Default(json!(
                "https://acme-v02.api.letsencrypt.org/directory"
            )))
        );
    }

    #[tokio::test]
    async fn get_returns_none_when_unset_and_no_default() {
        let d = test_dispatcher().await;
        let v = d.get("acme.contact_email").await.unwrap();
        assert_eq!(v, None);
    }

    #[tokio::test]
    async fn delete_removes_setting() {
        let d = test_dispatcher().await;
        d.put("routing.default_zone", json!("weavers.engineering"), None)
            .await
            .unwrap();
        assert!(d.delete("routing.default_zone").await.unwrap());
        assert_eq!(d.get("routing.default_zone").await.unwrap(), None);
    }

    #[tokio::test]
    async fn delete_removes_secret() {
        let d = test_dispatcher().await;
        d.put("cloudflare.api_token", json!("v"), None)
            .await
            .unwrap();
        assert!(d.delete("cloudflare.api_token").await.unwrap());
        // Default is None for this key, so get returns None.
        assert_eq!(d.get("cloudflare.api_token").await.unwrap(), None);
    }

    #[tokio::test]
    async fn list_includes_defaults_and_set_values() {
        let d = test_dispatcher().await;
        d.put("routing.default_zone", json!("weavers.engineering"), None)
            .await
            .unwrap();
        let rows = d.list().await.unwrap();
        assert_eq!(rows.len(), 7);
        // routing.default_zone is Set.
        let routing = rows
            .iter()
            .find(|(e, _)| e.key == "routing.default_zone")
            .unwrap();
        assert_eq!(
            routing.1,
            Some(ConfigValue::Set(json!("weavers.engineering")))
        );
        // acme.directory is Default.
        let acme_dir = rows
            .iter()
            .find(|(e, _)| e.key == "acme.directory")
            .unwrap();
        assert!(matches!(acme_dir.1, Some(ConfigValue::Default(_))));
        // acme.contact_email is None (unset, no default).
        let contact = rows
            .iter()
            .find(|(e, _)| e.key == "acme.contact_email")
            .unwrap();
        assert_eq!(contact.1, None);
    }
}
