//! Pure classifier: given a container's spec + an optional label override,
//! decide whether to deploy it via blue-green or in-place.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InPlaceReason {
    NoRoutingRule,
    StatefulVolume,
    NoHealthcheck,
    LabelForced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    BlueGreen,
    InPlace { reason: InPlaceReason },
}

#[derive(Debug, Clone)]
pub struct ContainerSpec<'a> {
    /// True if a routing rule exists pointing at this service:port.
    pub has_routing_rule: bool,
    /// True if the image has HEALTHCHECK or compose has a healthcheck section.
    pub has_healthcheck: bool,
    /// rw bind/named volume mount paths (empty = no stateful state).
    pub rw_volume_mounts: &'a [String],
    /// Value of the `isengard.deploy.strategy` label, if any.
    pub label_strategy: Option<&'a str>,
}

pub fn classify(spec: &ContainerSpec) -> Decision {
    // Label override: explicit user choice wins. "auto" or unknown values
    // fall through to the autodetect cascade.
    match spec.label_strategy {
        Some("blue-green") => return Decision::BlueGreen,
        Some("in-place") => {
            return Decision::InPlace {
                reason: InPlaceReason::LabelForced,
            };
        }
        _ => {}
    }

    if !spec.has_routing_rule {
        return Decision::InPlace {
            reason: InPlaceReason::NoRoutingRule,
        };
    }
    if !spec.rw_volume_mounts.is_empty() {
        return Decision::InPlace {
            reason: InPlaceReason::StatefulVolume,
        };
    }
    if !spec.has_healthcheck {
        return Decision::InPlace {
            reason: InPlaceReason::NoHealthcheck,
        };
    }

    Decision::BlueGreen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> ContainerSpec<'static> {
        ContainerSpec {
            has_routing_rule: true,
            has_healthcheck: true,
            rw_volume_mounts: &[],
            label_strategy: None,
        }
    }

    #[test]
    fn classifies_baseline_as_blue_green() {
        assert_eq!(classify(&baseline()), Decision::BlueGreen);
    }

    #[test]
    fn no_routing_rule_means_in_place() {
        let mut s = baseline();
        s.has_routing_rule = false;
        assert_eq!(
            classify(&s),
            Decision::InPlace {
                reason: InPlaceReason::NoRoutingRule
            }
        );
    }

    #[test]
    fn stateful_volume_means_in_place() {
        let mounts = vec!["/data".to_string()];
        let s = ContainerSpec {
            rw_volume_mounts: &mounts,
            ..baseline()
        };
        assert_eq!(
            classify(&s),
            Decision::InPlace {
                reason: InPlaceReason::StatefulVolume
            }
        );
    }

    #[test]
    fn no_healthcheck_means_in_place() {
        let mut s = baseline();
        s.has_healthcheck = false;
        assert_eq!(
            classify(&s),
            Decision::InPlace {
                reason: InPlaceReason::NoHealthcheck
            }
        );
    }

    #[test]
    fn label_override_wins_over_autodetect() {
        // Container has stateful volume → would normally be in-place,
        // but user explicitly opts into blue-green.
        let mounts = vec!["/data".to_string()];
        let s = ContainerSpec {
            rw_volume_mounts: &mounts,
            label_strategy: Some("blue-green"),
            ..baseline()
        };
        assert_eq!(classify(&s), Decision::BlueGreen);

        // Container is fully BG-eligible but user forces in-place.
        let s2 = ContainerSpec {
            label_strategy: Some("in-place"),
            ..baseline()
        };
        assert_eq!(
            classify(&s2),
            Decision::InPlace {
                reason: InPlaceReason::LabelForced
            }
        );

        // "auto" label falls through to autodetect.
        let s3 = ContainerSpec {
            label_strategy: Some("auto"),
            ..baseline()
        };
        assert_eq!(classify(&s3), Decision::BlueGreen);
    }
}
