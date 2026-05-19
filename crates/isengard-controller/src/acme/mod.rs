//! Controller-side ACME with DNS-01 challenge for wildcard cert issuance.
//!
//! Shipped HTTP-01: each agent owns the cert for the
//! hostnames it routes. That works for per-host names but not for wildcards:
//! `*.vallee.casa` cannot be validated via HTTP-01 because the wildcard
//! covers names without a corresponding HTTP responder. Let's Encrypt
//! requires DNS-01 for any wildcard.
//!
//! This module is the controller-side complement. The flow:
//!   1. Operator sets `ISENGARD_ACME_EMAIL`, `ISENGARD_CF_DNS_API_TOKEN`,
//!      `ISENGARD_ACME_DOMAINS`, optionally `ISENGARD_ACME_DIRECTORY`.
//!   2. On boot the controller spawns the renewal scheduler (`scheduler.rs`).
//!   3. The scheduler issues / renews via `dns01_cf.rs`, persists the cert
//!      in `WildcardCertStore` (`store.rs`), and stores metadata (validity,
//!      next renewal time) in the existing `tls_certs` table.
//!   4. The routing pusher (`routing.rs`) snapshots the store on each push
//!      so every connected agent receives the cert in its `ProxyConfig`.
//!   5. Agents install the cert in their existing pingora cert resolver;
//!      SNI for any name covered by the wildcard now serves this cert.
//!
//! Coexistence with HTTP-01: additive. Per-host certs continue to work via
//! the agent's HTTP-01 path; wildcard certs come from here. The agent's
//! cert callback resolves SNI by hostname, so it sees one merged store.

pub mod cf_api;
pub mod dns01_cf;
pub mod scheduler;
pub mod store;

pub use cf_api::CloudflareApi;
pub use dns01_cf::{
    AcmeDns01Client, CloudflareDnsProvider, DnsProvider, DnsRecordHandle, IssuedCert,
    LE_PRODUCTION_URL, LE_STAGING_URL,
};
pub use scheduler::{
    RENEW_DAYS_BEFORE_EXPIRY, WildcardGroup, parse_acme_domains, parse_cert_validity, should_retry,
    spawn as spawn_renewal_scheduler, tick as scheduler_tick,
};
pub use store::{WildcardCert, WildcardCertStore};

/// Controller boot config for the ACME subsystem. Empty means disabled.
#[derive(Debug, Clone, Default)]
pub struct AcmeConfig {
    pub email: Option<String>,
    pub cf_api_token: Option<String>,
    pub domains: String,
    /// Defaults to `LE_PRODUCTION_URL` when empty.
    pub directory_url: String,
}

impl AcmeConfig {
    /// Returns Some(...) when all required fields are present, else None
    /// (the ACME subsystem stays disabled).
    pub fn validated(self) -> Option<ValidatedAcmeConfig> {
        let email = self.email?;
        let token = self.cf_api_token?;
        let groups = parse_acme_domains(&self.domains);
        if groups.is_empty() {
            return None;
        }
        let directory = resolve_directory(&self.directory_url);
        Some(ValidatedAcmeConfig {
            email,
            cf_api_token: token,
            groups,
            directory_url: directory,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ValidatedAcmeConfig {
    pub email: String,
    pub cf_api_token: String,
    pub groups: Vec<WildcardGroup>,
    pub directory_url: String,
}

/// Resolve `ISENGARD_ACME_DIRECTORY` into a real URL.
///
/// Accepts three forms (case-insensitive, whitespace-trimmed):
/// - empty / unset / `production` / `prod` → LE production
/// - `staging` / `stage` → LE staging
/// - anything else → used as-is (treated as a directory URL)
///
/// Lets operators write `ISENGARD_ACME_DIRECTORY=production` instead of
/// memorizing the LE URL when flipping between staging and prod.
pub fn resolve_directory(input: &str) -> String {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "production" | "prod" => LE_PRODUCTION_URL.to_string(),
        "staging" | "stage" => LE_STAGING_URL.to_string(),
        _ => input.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_requires_email() {
        let cfg = AcmeConfig {
            email: None,
            cf_api_token: Some("t".into()),
            domains: "*.x.com".into(),
            directory_url: "".into(),
        };
        assert!(cfg.validated().is_none());
    }

    #[test]
    fn validated_requires_token() {
        let cfg = AcmeConfig {
            email: Some("a@b".into()),
            cf_api_token: None,
            domains: "*.x.com".into(),
            directory_url: "".into(),
        };
        assert!(cfg.validated().is_none());
    }

    #[test]
    fn validated_requires_at_least_one_domain() {
        let cfg = AcmeConfig {
            email: Some("a@b".into()),
            cf_api_token: Some("t".into()),
            domains: "".into(),
            directory_url: "".into(),
        };
        assert!(cfg.validated().is_none());
    }

    #[test]
    fn validated_defaults_to_production_directory() {
        let cfg = AcmeConfig {
            email: Some("a@b".into()),
            cf_api_token: Some("t".into()),
            domains: "*.x.com,x.com".into(),
            directory_url: "".into(),
        };
        let v = cfg.validated().unwrap();
        assert_eq!(v.directory_url, LE_PRODUCTION_URL);
        assert_eq!(v.groups.len(), 1);
        assert_eq!(v.groups[0].primary(), "*.x.com");
    }

    #[test]
    fn validated_respects_explicit_directory() {
        let cfg = AcmeConfig {
            email: Some("a@b".into()),
            cf_api_token: Some("t".into()),
            domains: "*.x.com".into(),
            directory_url: LE_STAGING_URL.into(),
        };
        let v = cfg.validated().unwrap();
        assert_eq!(v.directory_url, LE_STAGING_URL);
    }

    #[test]
    fn resolve_directory_accepts_aliases() {
        assert_eq!(resolve_directory(""), LE_PRODUCTION_URL);
        assert_eq!(resolve_directory("production"), LE_PRODUCTION_URL);
        assert_eq!(resolve_directory("PROD"), LE_PRODUCTION_URL);
        assert_eq!(resolve_directory("  prod  "), LE_PRODUCTION_URL);
        assert_eq!(resolve_directory("staging"), LE_STAGING_URL);
        assert_eq!(resolve_directory("STAGE"), LE_STAGING_URL);
    }

    #[test]
    fn resolve_directory_passes_through_urls() {
        let custom = "https://acme.internal.example/directory";
        assert_eq!(resolve_directory(custom), custom);
    }

    #[test]
    fn validated_resolves_alias_to_url() {
        let cfg = AcmeConfig {
            email: Some("a@b".into()),
            cf_api_token: Some("t".into()),
            domains: "*.x.com".into(),
            directory_url: "staging".into(),
        };
        let v = cfg.validated().unwrap();
        assert_eq!(v.directory_url, LE_STAGING_URL);
    }
}
