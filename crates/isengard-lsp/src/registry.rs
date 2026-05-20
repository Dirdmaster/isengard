//! Label registry. The executable mirror of `docs/concepts/labels.md`.
//!
//! Every `isengard.*` (and `io.isengard.*`) label the platform understands
//! gets one [`LabelSpec`] entry. Diagnostics consult the registry to flag
//! unknown keys and validate values against the entry's [`ValueKind`].
//! Hover (Phase 4) reads `summary` and `doc` straight off these entries.
//!
//! The `registry_doc_sync` integration test parses
//! `crates/isengard-lsp/docs/LABELS.md` and asserts every label in that doc
//! has a matching `REGISTRY` entry. Drift fails the build.

/// Static metadata for one label key.
///
/// Drives diagnostics, completion, and hover. `summary` is the one-line
/// hover headline. `doc` is the full Markdown body, surfaced on hover and
/// resolved completion items.
#[derive(Debug, Clone, Copy)]
pub struct LabelSpec {
    /// Match shape: a literal key, or a pattern with a `<name>` wildcard
    /// (the named-rule `isengard.expose.<name>` family).
    pub key: LabelKey,
    /// Hover headline. One sentence, no period.
    pub summary: &'static str,
    /// Hover body. Markdown. Renders on hover and on completion item resolve.
    pub doc: &'static str,
    /// Allowed value shape. Drives value diagnostics.
    pub value: ValueKind,
}

/// Pattern of the label key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKey {
    /// Exact literal match (e.g. `isengard.policy.gate`).
    Literal(&'static str),
    /// Pattern key of the form `<prefix>.<name>[.<suffix>]`. The `<name>`
    /// segment may be any string that is not in [`EXPOSE_RESERVED_PROPS`].
    Pattern {
        /// Everything before the `<name>` segment, with no trailing dot.
        prefix: &'static str,
        /// Optional property segment after `<name>`. `None` matches keys
        /// of the form `<prefix>.<name>`; `Some("port")` matches
        /// `<prefix>.<name>.port`.
        suffix: Option<&'static str>,
    },
}

/// Allowed shape of the value half of a `key: value` label pair.
#[derive(Debug, Clone, Copy)]
pub enum ValueKind {
    /// One of a fixed set of variants.
    Enum(&'static [&'static str]),
    /// Decimal integer in `1..=65535`.
    Port,
    /// Absolute URL (must parse with a scheme).
    Url,
    /// RFC 3339 timestamp (e.g. `2026-05-20T12:00:00Z`).
    Rfc3339,
    /// Any non-empty string.
    String,
    /// Comma-separated list of non-empty strings.
    StringList,
}

/// Reserved second-segment names in the `isengard.expose.*` family.
///
/// Matches `isengard_core::labels::KNOWN_PROPS`. Any second segment in this
/// set resolves to a property of the default rule, not a named rule.
pub const EXPOSE_RESERVED_PROPS: &[&str] = &["port", "tls", "health", "adapter", "auth"];

/// TLS termination modes accepted by `isengard.expose.tls`.
const TLS_MODES: &[&str] = &["acme", "edge", "manual"];
/// Networking adapter ids accepted by `isengard.expose.adapter`.
const ADAPTERS: &[&str] = &["none", "tailscale", "cf-tunnel"];
/// Deploy strategy keywords for `isengard.deploy.strategy`.
const DEPLOY_STRATEGIES: &[&str] = &["recreate", "blue_green", "rolling"];
/// Update policy match strategies.
const POLICY_STRATEGIES: &[&str] = &["pinned", "tag-only", "minor", "any"];
/// Update policy approval gates.
const POLICY_GATES: &[&str] = &["auto", "approval", "never"];
/// Update policy on-failure actions.
const POLICY_ON_FAILURE: &[&str] = &["rollback", "keep", "notify"];
/// System-plane role values.
const SYSTEM_ROLES: &[&str] = &["controller", "agent"];

/// Every label the platform recognises.
///
/// Ordering follows `docs/concepts/labels.md`: core, routing default,
/// routing named, policy, hooks, system plane. The doc-sync test does not
/// require this order, but humans reading the registry should find it.
pub const REGISTRY: &[LabelSpec] = &[
    // Core opt-in + naming.
    LabelSpec {
        key: LabelKey::Literal("isengard.enable"),
        summary: "Opt the container into the updater and policy pipeline",
        doc: "Set to `true` to make the agent track this container for image \
updates and apply the policy + hook + expose labels on it. Any other value \
(or absence) means Isengard ignores the container.",
        value: ValueKind::Enum(&["true"]),
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.stack"),
        summary: "Override the stack name (defaults to the compose project)",
        doc: "Stack name reported to the controller. Defaults to the compose \
project name. Set this when one compose project contains containers that \
belong to logically different stacks.",
        value: ValueKind::String,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.service"),
        summary: "Override the service name (defaults to the container name)",
        doc: "Service name reported to the controller. Defaults to the \
compose service name. Set this when the container name does not match the \
service identity the rest of the platform should see.",
        value: ValueKind::String,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.cap.add"),
        summary: "Mirror of compose `cap_add:` for protection-layer checks",
        doc: "Comma-separated list of Linux capabilities the container \
requests. The agent mirrors this onto the container snapshot so the \
protection layer can reason about elevation without re-reading compose.",
        value: ValueKind::StringList,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.deploy.strategy"),
        summary: "Per-service deploy strategy override",
        doc: "Override the deploy strategy for this service.\n\n\
- `recreate`: stop the old container, start the new one. Brief downtime.\n\
- `blue_green`: bring up the new container alongside, swap routes, drain \
the old.\n\
- `rolling`: roll instance by instance for replicated services.",
        value: ValueKind::Enum(DEPLOY_STRATEGIES),
    },
    // Default-rule routing (`isengard.expose` + `isengard.expose.<prop>`).
    LabelSpec {
        key: LabelKey::Literal("isengard.expose"),
        summary: "Default routing rule: public hostname",
        doc: "Public hostname for the default routing rule. The controller \
provisions a route to this hostname via Pingora; the adapter (Tailscale, \
Cloudflare Tunnel, raw IP) attaches it to the right ingress.",
        value: ValueKind::String,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.expose.port"),
        summary: "Default routing rule: upstream port",
        doc: "Upstream container port the default rule forwards to. \
Range `1..=65535`.",
        value: ValueKind::Port,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.expose.tls"),
        summary: "Default routing rule: TLS termination mode",
        doc: "TLS strategy for the default rule.\n\n\
- `acme`: controller issues a Let's Encrypt cert and terminates at the edge.\n\
- `edge`: controller terminates with an operator-supplied cert.\n\
- `manual`: pass TLS through to the container; the container terminates.",
        value: ValueKind::Enum(TLS_MODES),
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.expose.health"),
        summary: "Default routing rule: healthcheck path",
        doc: "HTTP path the controller probes to decide the upstream is \
healthy (e.g. `/healthz`). Free-form; no scheme or host.",
        value: ValueKind::String,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.expose.adapter"),
        summary: "Default routing rule: networking adapter",
        doc: "Networking adapter that exposes the rule outside the host.\n\n\
- `none`: route over the host's local network only.\n\
- `tailscale`: attach via the tailnet.\n\
- `cf-tunnel`: attach via a Cloudflare Tunnel.",
        value: ValueKind::Enum(ADAPTERS),
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.expose.auth"),
        summary: "Default routing rule: auth requirement",
        doc: "Auth keyword for the default rule. Today the value is a \
notifier channel id used for approval; in the future this will accept \
adapter-specific auth tags.",
        value: ValueKind::String,
    },
    // Named-rule routing. Each entry below also matches the default-rule
    // equivalent above, but with a `<name>` capture so multiple rules per
    // container work.
    LabelSpec {
        key: LabelKey::Pattern {
            prefix: "isengard.expose",
            suffix: None,
        },
        summary: "Named routing rule: public hostname",
        doc: "Public hostname for a named routing rule (e.g. \
`isengard.expose.web = plex.vallee.casa`). The name is anything except a \
reserved property (`port`, `tls`, `health`, `adapter`, `auth`).",
        value: ValueKind::String,
    },
    LabelSpec {
        key: LabelKey::Pattern {
            prefix: "isengard.expose",
            suffix: Some("port"),
        },
        summary: "Named routing rule: upstream port",
        doc: "Upstream container port for a named rule. Range `1..=65535`.",
        value: ValueKind::Port,
    },
    LabelSpec {
        key: LabelKey::Pattern {
            prefix: "isengard.expose",
            suffix: Some("tls"),
        },
        summary: "Named routing rule: TLS termination mode",
        doc: "TLS strategy for a named rule. See `isengard.expose.tls` for \
the variant semantics.",
        value: ValueKind::Enum(TLS_MODES),
    },
    LabelSpec {
        key: LabelKey::Pattern {
            prefix: "isengard.expose",
            suffix: Some("health"),
        },
        summary: "Named routing rule: healthcheck path",
        doc: "HTTP path the controller probes to decide the upstream is \
healthy for a named rule.",
        value: ValueKind::String,
    },
    LabelSpec {
        key: LabelKey::Pattern {
            prefix: "isengard.expose",
            suffix: Some("adapter"),
        },
        summary: "Named routing rule: networking adapter",
        doc: "Networking adapter for a named rule. See \
`isengard.expose.adapter` for variant semantics.",
        value: ValueKind::Enum(ADAPTERS),
    },
    LabelSpec {
        key: LabelKey::Pattern {
            prefix: "isengard.expose",
            suffix: Some("auth"),
        },
        summary: "Named routing rule: auth requirement",
        doc: "Auth keyword for a named rule. See `isengard.expose.auth` for \
the value shape.",
        value: ValueKind::String,
    },
    // Update policy.
    LabelSpec {
        key: LabelKey::Literal("isengard.policy.strategy"),
        summary: "Image-version match strategy",
        doc: "How permissive the updater is about new image tags.\n\n\
- `pinned`: never update on its own; only redeploys.\n\
- `tag-only`: same tag, new digest. SHA bumps only.\n\
- `minor`: same major version, new minor/patch.\n\
- `any`: take any newer tag.",
        value: ValueKind::Enum(POLICY_STRATEGIES),
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.policy.gate"),
        summary: "Approval gate for updates",
        doc: "What gating the updater applies before applying a new image.\n\n\
- `auto`: roll forward without operator input.\n\
- `approval`: pause and notify; require operator approval.\n\
- `never`: hard-block updates; surface the candidate but never apply.",
        value: ValueKind::Enum(POLICY_GATES),
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.policy.paused_until"),
        summary: "Pause updates until the given timestamp",
        doc: "Skip update evaluation for this container until the wall \
clock passes the RFC 3339 timestamp. After the timestamp the policy \
resumes normally.",
        value: ValueKind::Rfc3339,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.policy.on_failure"),
        summary: "Recovery action on deploy failure",
        doc: "What the updater does when a deploy fails its healthcheck.\n\n\
- `rollback`: bring the previous image back.\n\
- `keep`: leave the failed container in place.\n\
- `notify`: leave it in place and ping the approver channel.",
        value: ValueKind::Enum(POLICY_ON_FAILURE),
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.policy.approver_channel"),
        summary: "Notifier channel to ping on approval / failure",
        doc: "Channel id the notifier plugin pings when the policy needs \
operator input (e.g. `oncall`, `homelab-alerts`).",
        value: ValueKind::String,
    },
    // Lifecycle hooks.
    LabelSpec {
        key: LabelKey::Literal("isengard.hooks.pre_deploy"),
        summary: "Webhook URL fired before recreate",
        doc: "URL the agent POSTs to before stopping the current container. \
Useful for draining external load balancers or quiescing background work.",
        value: ValueKind::Url,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.hooks.post_deploy"),
        summary: "Webhook URL fired after the new container is healthy",
        doc: "URL the agent POSTs to once the new container passes its \
healthcheck. Typical use: trigger smoke tests or post a release note.",
        value: ValueKind::Url,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.hooks.on_failure"),
        summary: "Webhook URL fired on deploy failure",
        doc: "URL the agent POSTs to when a deploy fails. Body includes \
the failure stage and the agent's recovery action.",
        value: ValueKind::Url,
    },
    LabelSpec {
        key: LabelKey::Literal("isengard.hooks.secret"),
        summary: "Shared secret for webhook HMAC signing",
        doc: "Shared secret used to sign hook payloads with HMAC-SHA256. \
Receivers verify the signature in the `X-Isengard-Signature` header.",
        value: ValueKind::String,
    },
    // System plane.
    LabelSpec {
        key: LabelKey::Literal("io.isengard.role"),
        summary: "System-plane role of an Isengard container",
        doc: "Set by the platform on its own containers so discovery can \
find the controller and agent without trial RPC. Workload containers \
should not set this label.",
        value: ValueKind::Enum(SYSTEM_ROLES),
    },
    LabelSpec {
        key: LabelKey::Literal("io.isengard.api.version"),
        summary: "API contract version of the controller image",
        doc: "Major version of the controller's REST/gRPC contract. Bumped \
when the wire shape changes in a way clients must care about.",
        value: ValueKind::String,
    },
];

/// Look up the [`LabelSpec`] for an exact label key.
///
/// Walks the registry. Literal entries match by string equality; pattern
/// entries match by prefix-plus-suffix decomposition.
///
/// Returns `None` for unknown keys; the caller decides whether to emit a
/// diagnostic (we do, but the parser stays opinion-free).
///
/// # Examples
///
/// ```
/// use isengard_lsp::registry::{lookup, ValueKind};
///
/// let spec = lookup("isengard.policy.gate").unwrap();
/// assert!(matches!(spec.value, ValueKind::Enum(_)));
///
/// let named = lookup("isengard.expose.web.port").unwrap();
/// assert!(matches!(named.value, ValueKind::Port));
///
/// assert!(lookup("isengard.unknown").is_none());
/// ```
pub fn lookup(key: &str) -> Option<&'static LabelSpec> {
    REGISTRY.iter().find(|spec| spec_matches(spec, key))
}

/// True when `spec` applies to `key`.
///
/// Literal specs match by string equality. Pattern specs split `key` into
/// `<prefix>.<name>[.<suffix>]` and check that `<name>` is not a reserved
/// property; otherwise the literal entry for the property wins.
fn spec_matches(spec: &LabelSpec, key: &str) -> bool {
    match spec.key {
        LabelKey::Literal(lit) => lit == key,
        LabelKey::Pattern { prefix, suffix } => match_pattern(prefix, suffix, key),
    }
}

/// Decompose `key` against the pattern `<prefix>.<name>[.<suffix>]`.
///
/// The `<name>` segment is the one captured wildcard; it must be non-empty
/// and not appear in [`EXPOSE_RESERVED_PROPS`] (otherwise the matching
/// literal entry covers the key).
fn match_pattern(prefix: &str, suffix: Option<&str>, key: &str) -> bool {
    let Some(after_prefix) = key.strip_prefix(prefix) else {
        return false;
    };
    let Some(rest) = after_prefix.strip_prefix('.') else {
        return false;
    };
    match suffix {
        None => {
            // `<prefix>.<name>` only: rest must be a single non-reserved segment.
            !rest.is_empty() && !rest.contains('.') && !EXPOSE_RESERVED_PROPS.contains(&rest)
        }
        Some(s) => {
            // `<prefix>.<name>.<suffix>`: rest must end in `.<suffix>`.
            let Some(name) = rest.strip_suffix(s) else {
                return false;
            };
            let Some(name) = name.strip_suffix('.') else {
                return false;
            };
            !name.is_empty() && !name.contains('.') && !EXPOSE_RESERVED_PROPS.contains(&name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_lookup_hits() {
        assert!(lookup("isengard.enable").is_some());
        assert!(lookup("isengard.policy.gate").is_some());
        assert!(lookup("io.isengard.role").is_some());
    }

    #[test]
    fn pattern_lookup_hits_named_rule() {
        let spec = lookup("isengard.expose.web").expect("named hostname");
        assert!(matches!(spec.value, ValueKind::String));
        let spec = lookup("isengard.expose.web.port").expect("named port");
        assert!(matches!(spec.value, ValueKind::Port));
        let spec = lookup("isengard.expose.api.tls").expect("named tls");
        assert!(matches!(spec.value, ValueKind::Enum(_)));
    }

    #[test]
    fn pattern_does_not_eat_reserved_props() {
        // `isengard.expose.port` is the literal default-rule prop, not a
        // named-rule hostname; the literal entry must win.
        let spec = lookup("isengard.expose.port").expect("literal port");
        assert!(matches!(spec.value, ValueKind::Port));
    }

    #[test]
    fn unknown_keys_miss() {
        assert!(lookup("isengard.unknown").is_none());
        assert!(lookup("isengard.expose.web.unknown").is_none());
        // Empty name segment.
        assert!(lookup("isengard.expose..port").is_none());
    }
}
