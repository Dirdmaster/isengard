#![doc = include_str!("../../docs/policy-labels.md")]

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::{FailureHandling, Policy, UpdateGate, UpdateStrategy};

/// Common prefix for all policy labels.
///
/// Useful for cheap "does this container carry any policy labels" checks
/// at the agent watcher.
pub const LABEL_PREFIX: &str = "isengard.policy.";

/// Label key for [`super::Policy::strategy`].
pub const LABEL_STRATEGY: &str = "isengard.policy.strategy";
/// Label key for [`super::Policy::gate`].
pub const LABEL_GATE: &str = "isengard.policy.gate";
/// Label key for [`super::Policy::paused_until`].
pub const LABEL_PAUSED_UNTIL: &str = "isengard.policy.paused_until";
/// Label key for [`super::Policy::on_failure`].
pub const LABEL_ON_FAILURE: &str = "isengard.policy.on_failure";
/// Label key for [`super::Policy::approver_channel`].
pub const LABEL_APPROVER_CHANNEL: &str = "isengard.policy.approver_channel";

/// Returned when a label value cannot be coerced into the typed field.
///
/// Carries the offending label key + value so the ingest caller can log
/// something operators can act on. The `reason` is short and
/// human-readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseLabelError {
    /// The label key that failed.
    pub label: String,
    /// The offending value as it appeared on the container.
    pub value: String,
    /// Short, human-readable reason the value was rejected.
    pub reason: String,
}

impl std::fmt::Display for ParseLabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "policy label '{}' has invalid value '{}': {}",
            self.label, self.value, self.reason
        )
    }
}

impl std::error::Error for ParseLabelError {}

/// Parse the `isengard.policy.*` subset of a Docker label map.
///
/// Returns `Ok(Policy::default())` when no policy labels are present (so
/// the caller can use that as the "delete the row" signal). Unknown
/// `isengard.policy.<future_field>` keys are ignored so adding new fields
/// in later phases stays backward-compatible.
///
/// # Errors
///
/// Returns [`ParseLabelError`] on the first malformed value, with the
/// offending label name attached.
pub fn parse_policy_labels(labels: &HashMap<String, String>) -> Result<Policy, ParseLabelError> {
    let mut p = Policy::default();

    if let Some(v) = labels.get(LABEL_STRATEGY) {
        p.strategy = Some(parse_strategy(LABEL_STRATEGY, v)?);
    }
    if let Some(v) = labels.get(LABEL_GATE) {
        p.gate = Some(parse_gate(LABEL_GATE, v)?);
    }
    if let Some(v) = labels.get(LABEL_PAUSED_UNTIL) {
        p.paused_until = Some(parse_paused_until(LABEL_PAUSED_UNTIL, v)?);
    }
    if let Some(v) = labels.get(LABEL_ON_FAILURE) {
        p.on_failure = Some(parse_on_failure(LABEL_ON_FAILURE, v)?);
    }
    if let Some(v) = labels.get(LABEL_APPROVER_CHANNEL) {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            p.approver_channel = Some(trimmed.to_string());
        }
    }

    Ok(p)
}

/// Convenience predicate: does this label map carry any
/// `isengard.policy.*` key?
///
/// Used by the agent watcher to decide whether a container without
/// `isengard.expose*` is still worth reporting.
pub fn has_any_policy_label(labels: &HashMap<String, String>) -> bool {
    labels.keys().any(|k| k.starts_with(LABEL_PREFIX))
}

/// Normalize an enum value: trim, lowercase, then map `_` to `-` so
/// `tag_only` and `tag-only` are equivalent.
fn normalize_enum(v: &str) -> String {
    v.trim().to_ascii_lowercase().replace('_', "-")
}

/// Parse the `strategy` label value into [`UpdateStrategy`].
fn parse_strategy(label: &str, v: &str) -> Result<UpdateStrategy, ParseLabelError> {
    match normalize_enum(v).as_str() {
        "pinned" => Ok(UpdateStrategy::Pinned),
        "tag-only" => Ok(UpdateStrategy::TagOnly),
        "minor" => Ok(UpdateStrategy::Minor),
        "any" => Ok(UpdateStrategy::Any),
        _ => Err(ParseLabelError {
            label: label.to_string(),
            value: v.to_string(),
            reason: "expected one of: pinned, tag-only, minor, any".to_string(),
        }),
    }
}

/// Parse the `gate` label value into [`UpdateGate`].
fn parse_gate(label: &str, v: &str) -> Result<UpdateGate, ParseLabelError> {
    match normalize_enum(v).as_str() {
        "auto" => Ok(UpdateGate::Auto),
        "approval" => Ok(UpdateGate::Approval),
        "never" => Ok(UpdateGate::Never),
        _ => Err(ParseLabelError {
            label: label.to_string(),
            value: v.to_string(),
            reason: "expected one of: auto, approval, never".to_string(),
        }),
    }
}

/// Parse the `on_failure` label value into [`FailureHandling`].
fn parse_on_failure(label: &str, v: &str) -> Result<FailureHandling, ParseLabelError> {
    match normalize_enum(v).as_str() {
        "rollback" => Ok(FailureHandling::Rollback),
        "keep" => Ok(FailureHandling::Keep),
        "notify" => Ok(FailureHandling::Notify),
        _ => Err(ParseLabelError {
            label: label.to_string(),
            value: v.to_string(),
            reason: "expected one of: rollback, keep, notify".to_string(),
        }),
    }
}

/// Parse the `paused_until` label value into [`DateTime<Utc>`].
fn parse_paused_until(label: &str, v: &str) -> Result<DateTime<Utc>, ParseLabelError> {
    DateTime::parse_from_rfc3339(v.trim())
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| ParseLabelError {
            label: label.to_string(),
            value: v.to_string(),
            reason: format!("expected RFC 3339 timestamp: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn all_unset_returns_default_policy() {
        let labels = map(&[]);
        let p = parse_policy_labels(&labels).expect("parse");
        assert_eq!(p, Policy::default());
    }

    #[test]
    fn unrelated_labels_are_ignored() {
        let labels = map(&[
            ("isengard.enable", "true"),
            ("isengard.expose", "x.test"),
            ("com.docker.compose.project", "blog"),
        ]);
        let p = parse_policy_labels(&labels).expect("parse");
        assert_eq!(p, Policy::default());
        assert!(!has_any_policy_label(&labels));
    }

    #[test]
    fn strategy_kebab_case_parses() {
        let labels = map(&[(LABEL_STRATEGY, "tag-only")]);
        let p = parse_policy_labels(&labels).unwrap();
        assert_eq!(p.strategy, Some(UpdateStrategy::TagOnly));
        assert!(has_any_policy_label(&labels));
    }

    #[test]
    fn strategy_snake_case_parses() {
        let labels = map(&[(LABEL_STRATEGY, "tag_only")]);
        let p = parse_policy_labels(&labels).unwrap();
        assert_eq!(p.strategy, Some(UpdateStrategy::TagOnly));
    }

    #[test]
    fn strategy_uppercase_normalizes() {
        let labels = map(&[(LABEL_STRATEGY, "PINNED")]);
        let p = parse_policy_labels(&labels).unwrap();
        assert_eq!(p.strategy, Some(UpdateStrategy::Pinned));
    }

    #[test]
    fn gate_kebab_and_snake() {
        for v in ["approval", "Approval", "approval"] {
            let labels = map(&[(LABEL_GATE, v)]);
            let p = parse_policy_labels(&labels).unwrap();
            assert_eq!(p.gate, Some(UpdateGate::Approval));
        }
    }

    #[test]
    fn paused_until_rfc3339_parses() {
        let labels = map(&[(LABEL_PAUSED_UNTIL, "2026-06-15T00:00:00Z")]);
        let p = parse_policy_labels(&labels).unwrap();
        assert_eq!(
            p.paused_until,
            Some(Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap()),
        );
    }

    #[test]
    fn on_failure_all_three_values() {
        for (v, expected) in [
            ("rollback", FailureHandling::Rollback),
            ("keep", FailureHandling::Keep),
            ("notify", FailureHandling::Notify),
        ] {
            let labels = map(&[(LABEL_ON_FAILURE, v)]);
            let p = parse_policy_labels(&labels).unwrap();
            assert_eq!(p.on_failure, Some(expected));
        }
    }

    #[test]
    fn approver_channel_round_trips() {
        let labels = map(&[(LABEL_APPROVER_CHANNEL, "ops-team")]);
        let p = parse_policy_labels(&labels).unwrap();
        assert_eq!(p.approver_channel.as_deref(), Some("ops-team"));
    }

    #[test]
    fn approver_channel_empty_string_treated_as_unset() {
        let labels = map(&[(LABEL_APPROVER_CHANNEL, "  ")]);
        let p = parse_policy_labels(&labels).unwrap();
        assert!(p.approver_channel.is_none());
    }

    #[test]
    fn malformed_strategy_returns_err_with_label_name() {
        let labels = map(&[(LABEL_STRATEGY, "pinneded")]);
        let err = parse_policy_labels(&labels).unwrap_err();
        assert_eq!(err.label, LABEL_STRATEGY);
        assert_eq!(err.value, "pinneded");
        assert!(err.reason.contains("pinned"));
    }

    #[test]
    fn malformed_gate_returns_err() {
        let labels = map(&[(LABEL_GATE, "yes")]);
        let err = parse_policy_labels(&labels).unwrap_err();
        assert_eq!(err.label, LABEL_GATE);
    }

    #[test]
    fn malformed_paused_until_returns_err() {
        let labels = map(&[(LABEL_PAUSED_UNTIL, "tomorrow")]);
        let err = parse_policy_labels(&labels).unwrap_err();
        assert_eq!(err.label, LABEL_PAUSED_UNTIL);
        assert!(err.reason.to_lowercase().contains("rfc 3339"));
    }

    #[test]
    fn malformed_on_failure_returns_err() {
        let labels = map(&[(LABEL_ON_FAILURE, "explode")]);
        let err = parse_policy_labels(&labels).unwrap_err();
        assert_eq!(err.label, LABEL_ON_FAILURE);
    }

    #[test]
    fn full_set_round_trips() {
        let labels = map(&[
            (LABEL_STRATEGY, "pinned"),
            (LABEL_GATE, "approval"),
            (LABEL_PAUSED_UNTIL, "2026-12-31T23:59:59Z"),
            (LABEL_ON_FAILURE, "rollback"),
            (LABEL_APPROVER_CHANNEL, "ops"),
        ]);
        let p = parse_policy_labels(&labels).unwrap();
        assert_eq!(p.strategy, Some(UpdateStrategy::Pinned));
        assert_eq!(p.gate, Some(UpdateGate::Approval));
        assert_eq!(
            p.paused_until,
            Some(Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap()),
        );
        assert_eq!(p.on_failure, Some(FailureHandling::Rollback));
        assert_eq!(p.approver_channel.as_deref(), Some("ops"));
    }

    #[test]
    fn unknown_policy_subkey_is_ignored_for_forward_compat() {
        let labels = map(&[
            (LABEL_STRATEGY, "pinned"),
            ("isengard.policy.future_field", "whatever"),
        ]);
        let p = parse_policy_labels(&labels).unwrap();
        assert_eq!(p.strategy, Some(UpdateStrategy::Pinned));
        // The unknown key is ignored, but still counts as a policy label for
        // `has_any_policy_label` (good: we still report the container).
        assert!(has_any_policy_label(&labels));
    }

    #[test]
    fn first_malformed_field_short_circuits_with_its_label() {
        // Even if other fields are well-formed, the first error wins.
        let labels = map(&[
            (LABEL_STRATEGY, "pinned"),
            (LABEL_GATE, "yes"), // malformed
            (LABEL_ON_FAILURE, "rollback"),
        ]);
        let err = parse_policy_labels(&labels).unwrap_err();
        assert_eq!(err.label, LABEL_GATE);
    }
}
