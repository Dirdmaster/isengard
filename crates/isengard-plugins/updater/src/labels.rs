//! Label-based filter. v1 is opt-in: only containers with
//! `isengard.enable=true` are considered candidates for update.
//!
//! Why opt-in: the friend's fleet runs containers we don't own (host metrics,
//! a tunneling daemon, system services). A watch-all default would update
//! those out from under their owner. v1.x can revisit with a per-host
//! `--watch-all` flag if real demand surfaces.

use std::collections::HashMap;

/// Opt-in label key. Set this to `true` on any container the
/// updater should manage.
pub const ENABLE_LABEL: &str = "isengard.enable";

/// Returns true if the labels indicate the container opted in.
/// Accepts `true`, `True`, `TRUE` (case-insensitive). Anything else → false.
pub fn isengard_enabled(labels: Option<&HashMap<String, String>>) -> bool {
    labels
        .and_then(|m| m.get(ENABLE_LABEL))
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(k: &str, v: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(k.into(), v.into());
        m
    }

    #[test]
    fn missing_labels_means_disabled() {
        assert!(!isengard_enabled(None));
    }

    #[test]
    fn empty_labels_means_disabled() {
        assert!(!isengard_enabled(Some(&HashMap::new())));
    }

    #[test]
    fn no_isengard_label_means_disabled() {
        let l = label("other.label", "true");
        assert!(!isengard_enabled(Some(&l)));
    }

    #[test]
    fn label_true_lowercase_enables() {
        let l = label(ENABLE_LABEL, "true");
        assert!(isengard_enabled(Some(&l)));
    }

    #[test]
    fn label_true_uppercase_enables() {
        let l = label(ENABLE_LABEL, "TRUE");
        assert!(isengard_enabled(Some(&l)));
    }

    #[test]
    fn label_false_disables() {
        let l = label(ENABLE_LABEL, "false");
        assert!(!isengard_enabled(Some(&l)));
    }

    #[test]
    fn label_random_value_disables() {
        let l = label(ENABLE_LABEL, "yes");
        assert!(!isengard_enabled(Some(&l)));
    }
}
