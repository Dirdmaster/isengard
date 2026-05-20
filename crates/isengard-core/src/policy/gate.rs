//! External-action gate types (#55).
//!
//! Pure types only: the HTTP evaluator that turns these into actual
//! decisions lives in `isengard-plugin-updater::gate` so this crate stays
//! free of `reqwest` / `hmac`.
//!
//! A "gate" is a per-policy webhook that the updater consults BEFORE
//! applying any update. The gate replies with one of four decisions:
//!
//! - `approve`: proceed with the existing post-policy logic.
//! - `reject`: skip; emit `update.gated_reject`.
//! - `defer`: set the service's `paused_until` to the supplied time.
//! - `manual`: escalate to the existing approval queue.
//!
//! Failure modes are mapped by the evaluator (5xx becomes `Manual`,
//! timeout becomes `Manual`, connection refused becomes
//! [`GateDecision::Unreachable`] which the caller treats as a 1h defer).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Configuration for one external gate.
///
/// Lives on a [`super::Policy`] row at any scope; resolution merges per
/// the standard layered rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalGate {
    /// HTTPS URL the updater POSTs to.
    pub url: String,
    /// Optional HMAC-SHA256 signing secret.
    ///
    /// Uses the same scheme as 12a webhooks
    /// (`X-Isengard-Signature: sha256=<hex>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Per-evaluation timeout in seconds.
    ///
    /// The evaluator caps the total outbound request duration to this
    /// value; a timeout maps to [`GateDecision::Manual`] (escalate) per
    /// the spec.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u32,
}

/// Default value for [`ExternalGate::timeout_secs`] when the field is
/// omitted in JSON.
fn default_timeout_secs() -> u32 {
    10
}

impl Default for ExternalGate {
    fn default() -> Self {
        Self {
            url: String::new(),
            secret: None,
            timeout_secs: default_timeout_secs(),
        }
    }
}

/// JSON shape POSTed to the gate URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatePayload {
    /// Always `"update.gate"`.
    ///
    /// Lets receivers reuse one endpoint for multiple Isengard event
    /// sources.
    pub kind: String,
    /// Host the candidate container is on.
    pub host_id: String,
    /// Compose stack name.
    pub stack: String,
    /// Service name within the stack.
    pub service: String,
    /// Docker container name.
    pub container_name: String,
    /// Image reference (with tag).
    pub image: String,
    /// Digest currently running.
    pub current_digest: String,
    /// Digest the updater wants to apply.
    pub proposed_digest: String,
    /// When the gate request was minted, in UTC.
    pub timestamp: DateTime<Utc>,
}

/// Response body the gate is expected to return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GateResponse {
    /// Proceed with the existing post-policy logic.
    Approve,
    /// Skip; emit `update.gated_reject` with the optional reason.
    Reject {
        /// Human-readable reason the gate rejected (e.g. "incident in
        /// progress").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Pause until the supplied UTC time. Sets the service's
    /// `paused_until`.
    Defer {
        /// When the deferral expires.
        until: DateTime<Utc>,
    },
    /// Escalate to the existing approval queue.
    Manual,
}

/// Outcome the evaluator returns to the cycle.
///
/// Distinct from [`GateResponse`] because the evaluator collapses
/// transport-level errors into specific decisions per the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Proceed with the existing post-policy logic.
    Approve,
    /// Skip; emit `update.gated_reject` with an optional reason.
    Reject {
        /// Optional reason text from the gate.
        reason: Option<String>,
    },
    /// Set the service's `paused_until` to `until`.
    Defer {
        /// When the deferral expires.
        until: DateTime<Utc>,
    },
    /// Escalate to the existing approval queue.
    Manual,
    /// Connection refused / DNS / network unreachable.
    ///
    /// The cycle emits `update.gated_unreachable` and applies a 1h
    /// `paused_until` rather than escalating to a human.
    Unreachable,
}

impl GateDecision {
    /// Stable string for events.
    ///
    /// Matches the spec's `update.gated_<x>` suffix vocabulary.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject { .. } => "reject",
            Self::Defer { .. } => "defer",
            Self::Manual => "manual",
            Self::Unreachable => "unreachable",
        }
    }
}

impl From<GateResponse> for GateDecision {
    fn from(r: GateResponse) -> Self {
        match r {
            GateResponse::Approve => Self::Approve,
            GateResponse::Reject { reason } => Self::Reject { reason },
            GateResponse::Defer { until } => Self::Defer { until },
            GateResponse::Manual => Self::Manual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_gate_round_trips_with_secret() {
        let g = ExternalGate {
            url: "https://gate.example.com".into(),
            secret: Some("shh".into()),
            timeout_secs: 30,
        };
        let s = serde_json::to_string(&g).unwrap();
        let back: ExternalGate = serde_json::from_str(&s).unwrap();
        assert_eq!(g, back);
    }

    #[test]
    fn external_gate_round_trips_without_secret() {
        let g = ExternalGate {
            url: "https://gate.example.com".into(),
            secret: None,
            timeout_secs: 10,
        };
        let s = serde_json::to_string(&g).unwrap();
        // secret is omitted from the wire form.
        assert!(!s.contains("secret"));
        let back: ExternalGate = serde_json::from_str(&s).unwrap();
        assert_eq!(g, back);
    }

    #[test]
    fn missing_timeout_defaults_to_10() {
        let g: ExternalGate =
            serde_json::from_str(r#"{"url":"https://gate.example.com"}"#).unwrap();
        assert_eq!(g.timeout_secs, 10);
    }

    #[test]
    fn gate_response_approve_parses() {
        let r: GateResponse = serde_json::from_str(r#"{"decision":"approve"}"#).unwrap();
        assert_eq!(r, GateResponse::Approve);
    }

    #[test]
    fn gate_response_reject_with_reason_parses() {
        let r: GateResponse =
            serde_json::from_str(r#"{"decision":"reject","reason":"too risky"}"#).unwrap();
        assert_eq!(
            r,
            GateResponse::Reject {
                reason: Some("too risky".into()),
            }
        );
    }

    #[test]
    fn gate_response_defer_with_until_parses() {
        let r: GateResponse =
            serde_json::from_str(r#"{"decision":"defer","until":"2026-06-01T00:00:00Z"}"#).unwrap();
        match r {
            GateResponse::Defer { until } => {
                assert_eq!(until.to_rfc3339(), "2026-06-01T00:00:00+00:00")
            }
            other => panic!("expected Defer, got {other:?}"),
        }
    }

    #[test]
    fn gate_response_manual_parses() {
        let r: GateResponse = serde_json::from_str(r#"{"decision":"manual"}"#).unwrap();
        assert_eq!(r, GateResponse::Manual);
    }

    #[test]
    fn gate_decision_as_str_stable() {
        assert_eq!(GateDecision::Approve.as_str(), "approve");
        assert_eq!(GateDecision::Reject { reason: None }.as_str(), "reject");
        assert_eq!(GateDecision::Defer { until: Utc::now() }.as_str(), "defer");
        assert_eq!(GateDecision::Manual.as_str(), "manual");
        assert_eq!(GateDecision::Unreachable.as_str(), "unreachable");
    }
}
