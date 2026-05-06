//! Parse `isengard.hooks.*` Docker labels into a `ParsedHooks` struct.
//! Phase 12b (#54).
//!
//! Pure module: `HashMap<String, String>` in, `ParsedHooks` out. The ingest
//! caller (controller's `HookLabelIngest`) decides what to upsert.
//!
//! Recognized labels:
//!
//! | Label                                | Field           |
//! |--------------------------------------|-----------------|
//! | `isengard.hooks.pre_deploy`          | pre_deploy_url  |
//! | `isengard.hooks.post_deploy`         | post_deploy_url |
//! | `isengard.hooks.on_failure`          | on_failure_url  |
//! | `isengard.hooks.secret`              | secret          |
//!
//! Unset / empty values are treated as "no hook for this kind". Malformed
//! URLs are NOT rejected here: the worker surfaces the parse error in
//! `last_error` when it tries to dispatch, which is more visible to operators.

use std::collections::HashMap;

/// Common prefix for all hook labels.
pub const HOOK_LABEL_PREFIX: &str = "isengard.hooks.";

pub const LABEL_PRE_DEPLOY: &str = "isengard.hooks.pre_deploy";
pub const LABEL_POST_DEPLOY: &str = "isengard.hooks.post_deploy";
pub const LABEL_ON_FAILURE: &str = "isengard.hooks.on_failure";
pub const LABEL_SECRET: &str = "isengard.hooks.secret";

/// Pure parsed result. Empty (`has_any_url() == false`) means the container
/// carries no usable hooks; the ingest treats that as a delete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedHooks {
    pub pre_deploy_url: Option<String>,
    pub post_deploy_url: Option<String>,
    pub on_failure_url: Option<String>,
    pub secret: Option<String>,
}

impl ParsedHooks {
    /// Returns true iff at least one URL field is set. Used to gate
    /// "is this row worth keeping at all" decisions in the ingest path.
    pub fn has_any_url(&self) -> bool {
        self.pre_deploy_url.is_some()
            || self.post_deploy_url.is_some()
            || self.on_failure_url.is_some()
    }
}

/// Parse the `isengard.hooks.*` subset of a Docker label map.
///
/// Returns `ParsedHooks::default()` when no hook labels are present. The
/// caller can treat that as the "delete the row" signal.
pub fn parse_hook_labels(labels: &HashMap<String, String>) -> ParsedHooks {
    ParsedHooks {
        pre_deploy_url: nonempty(labels.get(LABEL_PRE_DEPLOY)),
        post_deploy_url: nonempty(labels.get(LABEL_POST_DEPLOY)),
        on_failure_url: nonempty(labels.get(LABEL_ON_FAILURE)),
        secret: nonempty(labels.get(LABEL_SECRET)),
    }
}

/// Convenience predicate: does this label map carry any `isengard.hooks.*`
/// key at all? Used by the agent watcher to decide whether to ship a
/// `ContainerLabelsReport` for an otherwise-unlabeled container.
pub fn has_any_hook_label(labels: &HashMap<String, String>) -> bool {
    labels.keys().any(|k| k.starts_with(HOOK_LABEL_PREFIX))
}

fn nonempty(v: Option<&String>) -> Option<String> {
    v.map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn empty_map_returns_default() {
        let p = parse_hook_labels(&map(&[]));
        assert_eq!(p, ParsedHooks::default());
        assert!(!p.has_any_url());
    }

    #[test]
    fn unrelated_labels_are_ignored() {
        let labels = map(&[
            ("isengard.expose", "x.test"),
            ("isengard.policy.strategy", "pinned"),
            ("com.docker.compose.project", "blog"),
        ]);
        let p = parse_hook_labels(&labels);
        assert_eq!(p, ParsedHooks::default());
        assert!(!has_any_hook_label(&labels));
    }

    #[test]
    fn all_three_urls_round_trip() {
        let labels = map(&[
            (LABEL_PRE_DEPLOY, "https://hooks.example.com/pre"),
            (LABEL_POST_DEPLOY, "https://hooks.example.com/post"),
            (LABEL_ON_FAILURE, "https://hooks.example.com/fail"),
        ]);
        let p = parse_hook_labels(&labels);
        assert_eq!(
            p.pre_deploy_url.as_deref(),
            Some("https://hooks.example.com/pre")
        );
        assert_eq!(
            p.post_deploy_url.as_deref(),
            Some("https://hooks.example.com/post")
        );
        assert_eq!(
            p.on_failure_url.as_deref(),
            Some("https://hooks.example.com/fail")
        );
        assert!(has_any_hook_label(&labels));
        assert!(p.has_any_url());
    }

    #[test]
    fn just_pre_deploy_only() {
        let labels = map(&[(LABEL_PRE_DEPLOY, "https://hooks.example.com/pre")]);
        let p = parse_hook_labels(&labels);
        assert!(p.pre_deploy_url.is_some());
        assert!(p.post_deploy_url.is_none());
        assert!(p.on_failure_url.is_none());
        assert!(p.has_any_url());
    }

    #[test]
    fn empty_url_value_is_treated_as_unset() {
        let labels = map(&[(LABEL_PRE_DEPLOY, "   ")]);
        let p = parse_hook_labels(&labels);
        assert!(p.pre_deploy_url.is_none());
        assert!(!p.has_any_url());
    }

    #[test]
    fn secret_round_trips() {
        let labels = map(&[
            (LABEL_PRE_DEPLOY, "https://hooks.example.com/pre"),
            (LABEL_SECRET, "shared-secret"),
        ]);
        let p = parse_hook_labels(&labels);
        assert_eq!(p.secret.as_deref(), Some("shared-secret"));
    }

    #[test]
    fn malformed_url_is_passed_through_unchanged() {
        // The worker surfaces the parse error in last_error; the parser
        // doesn't pre-validate.
        let labels = map(&[(LABEL_PRE_DEPLOY, "not-a-url")]);
        let p = parse_hook_labels(&labels);
        assert_eq!(p.pre_deploy_url.as_deref(), Some("not-a-url"));
    }

    #[test]
    fn has_any_hook_label_detects_secret_only_case() {
        // Secret without URL is unusual but should still register as "this
        // container carries hook labels" for label-watch decisions.
        let labels = map(&[(LABEL_SECRET, "shh")]);
        assert!(has_any_hook_label(&labels));
        let p = parse_hook_labels(&labels);
        assert!(!p.has_any_url());
        assert!(p.secret.is_some());
    }
}
