#![doc = include_str!("../docs/placement.md")]

use std::fmt;

/// Per-service deploy strategy. Maps to the controller's rollout
/// behavior.
///
/// [`Auto`](Self::Auto) lets the controller pick per-service
/// heuristically. Parsed from the stack file's per-service `strategy:`
/// key. See the 2026-05-15 one-file stack model design spec for the
/// rationale of pushing strategy choice into the stack file itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Controller picks per-service heuristically.
    #[default]
    Auto,
    /// Stand up a fresh "green" deployment alongside the running "blue"
    /// and swap traffic.
    BlueGreen,
    /// Replace replicas one at a time, draining traffic between each.
    Rolling,
    /// Stop everything, then bring up the new version.
    Recreate,
}

impl Strategy {
    /// Parse a strategy keyword.
    ///
    /// Accepts exactly the four kebab-case forms; anything else is an
    /// error naming the allowed set.
    ///
    /// # Errors
    ///
    /// Returns an `anyhow::Error` listing the allowed values when the
    /// input is not one of `auto`, `blue-green`, `rolling`, `recreate`.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "auto" => Ok(Self::Auto),
            "blue-green" => Ok(Self::BlueGreen),
            "rolling" => Ok(Self::Rolling),
            "recreate" => Ok(Self::Recreate),
            other => Err(anyhow::anyhow!(
                "unknown strategy {other:?}; allowed: auto, blue-green, rolling, recreate"
            )),
        }
    }

    /// Inverse of [`parse`](Self::parse): emit the stable kebab-case
    /// keyword.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::BlueGreen => "blue-green",
            Self::Rolling => "rolling",
            Self::Recreate => "recreate",
        }
    }
}

/// Where to place a service's replicas.
///
/// Set by the compose parser; read by the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// One replica on any host matching `selector` (or any healthy host
    /// if the selector is `None`). The default verb when no explicit
    /// placement is set.
    Singleton {
        /// Optional eligibility selector.
        selector: Option<LabelSelector>,
    },
    /// `count` replicas, prefer one-per-host.
    ///
    /// When fleet size < `count`, replicas stack onto already-used hosts
    /// (round-robin).
    Spread {
        /// Number of replicas to maintain.
        count: u32,
        /// Optional eligibility selector.
        selector: Option<LabelSelector>,
    },
    /// Exactly one replica per eligible host.
    ///
    /// Auto-place on new enrollees (after the next reconcile tick).
    Global {
        /// Optional eligibility selector.
        selector: Option<LabelSelector>,
    },
    /// Pinned to a specific hostname. Never relocated.
    ///
    /// `selector` still applies as a sanity check: if the pinned host
    /// doesn't match the selector the scheduler emits
    /// `placement.host_excluded` and the service stays Unavailable.
    On {
        /// Hostname the service is pinned to.
        host: String,
        /// Optional eligibility selector (sanity check).
        selector: Option<LabelSelector>,
    },
}

impl Default for Placement {
    /// [`Placement::Singleton`] with no selector is the scheduler-side
    /// default when the stack file declares no placement verb.
    ///
    /// Keeping the default explicit lets callers (the scheduler, tests)
    /// materialise an "implicit placement" without juggling
    /// `Option<Placement>` shapes.
    fn default() -> Self {
        Self::Singleton { selector: None }
    }
}

/// A parsed label selector with its canonical re-emitted source.
///
/// The `raw` form is what `isd placement explain` shows back to operators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelSelector {
    /// Canonicalised source form (re-emitted from parsed `exprs`).
    pub raw: String,
    /// Parsed clauses, in input order.
    pub exprs: Vec<SelectorExpr>,
}

impl LabelSelector {
    /// Parse a selector string.
    ///
    /// # Errors
    ///
    /// Returns [`SelectorParseError`] when the grammar doesn't match, a
    /// key is empty or has forbidden chars, or a value violates the
    /// `,`/`=` exclusion.
    pub fn parse(s: &str) -> Result<Self, SelectorParseError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(SelectorParseError::Empty);
        }
        let mut exprs = Vec::new();
        for raw_expr in split_top_level_commas(s) {
            let trimmed = raw_expr.trim();
            if trimmed.is_empty() {
                return Err(SelectorParseError::EmptyClause);
            }
            exprs.push(parse_expr(trimmed)?);
        }
        Ok(LabelSelector {
            raw: canonicalize(&exprs),
            exprs,
        })
    }

    /// Return true iff every clause matches the given label map.
    ///
    /// Public so `isengard-controller::scheduler::eligible` can reuse
    /// without forking the matcher.
    pub fn matches(&self, labels: &std::collections::BTreeMap<String, String>) -> bool {
        self.exprs.iter().all(|e| e.matches(labels))
    }
}

impl fmt::Display for LabelSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// One clause in a label selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorExpr {
    /// `key==value` clause.
    Eq {
        /// Label key.
        key: String,
        /// Required label value.
        value: String,
    },
    /// `key!=value` clause. Absent keys count as "not equal" (k8s
    /// semantics).
    Neq {
        /// Label key.
        key: String,
        /// Disallowed label value.
        value: String,
    },
    /// `key in (a, b, c)` clause.
    In {
        /// Label key.
        key: String,
        /// Allowed label values.
        values: Vec<String>,
    },
    /// `key notin (a, b, c)` clause. Absent keys count as "not in" (k8s
    /// semantics).
    NotIn {
        /// Label key.
        key: String,
        /// Disallowed label values.
        values: Vec<String>,
    },
    /// Bare key: matches when the key exists (any value).
    Exists {
        /// Label key.
        key: String,
    },
}

impl SelectorExpr {
    /// Check whether this clause matches the given label map.
    pub fn matches(&self, labels: &std::collections::BTreeMap<String, String>) -> bool {
        match self {
            Self::Eq { key, value } => labels.get(key).map(|v| v == value).unwrap_or(false),
            // k8s semantics: key-absent counts as != (matches).
            Self::Neq { key, value } => labels.get(key).map(|v| v != value).unwrap_or(true),
            Self::In { key, values } => labels
                .get(key)
                .map(|v| values.iter().any(|x| x == v))
                .unwrap_or(false),
            // k8s semantics: key-absent counts as notin (matches).
            Self::NotIn { key, values } => labels
                .get(key)
                .map(|v| !values.iter().any(|x| x == v))
                .unwrap_or(true),
            Self::Exists { key } => labels.contains_key(key),
        }
    }
}

/// Parse failure modes for [`LabelSelector::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorParseError {
    /// Selector string was empty or whitespace-only.
    Empty,
    /// A clause between two commas was empty.
    EmptyClause,
    /// Key violated the `[a-z0-9._-]+` rule or exceeded the length cap.
    InvalidKey {
        /// The offending key.
        key: String,
    },
    /// Value contained `,` or `=`, or was empty.
    InvalidValue {
        /// The offending value.
        value: String,
    },
    /// Clause did not match any grammar production.
    MalformedClause {
        /// The offending clause text.
        clause: String,
    },
    /// `in (...)` or `notin (...)` set was missing its closing paren.
    UnclosedSet {
        /// The offending clause text.
        clause: String,
    },
    /// Recognised the structure but the operator was something other
    /// than `==`, `!=`, `in`, `notin`.
    UnknownOperator {
        /// The offending clause text.
        clause: String,
    },
}

impl fmt::Display for SelectorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty selector (expected at least one clause)"),
            Self::EmptyClause => f.write_str("empty clause between commas"),
            Self::InvalidKey { key } => write!(
                f,
                "invalid selector key {key:?} (allowed: [a-z0-9._-], max 63 chars)"
            ),
            Self::InvalidValue { value } => write!(
                f,
                "invalid selector value {value:?} (forbidden chars: '=' or ',')"
            ),
            Self::MalformedClause { clause } => {
                write!(f, "malformed selector clause {clause:?}")
            }
            Self::UnclosedSet { clause } => {
                write!(f, "unclosed `in (...)` / `notin (...)` set in {clause:?}")
            }
            Self::UnknownOperator { clause } => {
                write!(
                    f,
                    "unknown operator in selector clause {clause:?}: expected ==, !=, in, notin"
                )
            }
        }
    }
}

impl std::error::Error for SelectorParseError {}

/// Maximum length of a selector key.
const MAX_KEY_LEN: usize = 63;

/// Split `s` on top-level commas, leaving commas inside parens alone.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut out = Vec::new();
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
            }
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Parse one clause `s` (no top-level commas) into a [`SelectorExpr`].
fn parse_expr(s: &str) -> Result<SelectorExpr, SelectorParseError> {
    // Detect `in (...)` / `notin (...)` first; they contain commas.
    if let Some((key, rest)) = match_set_op(s, " in ") {
        return parse_set(key, rest, false);
    }
    if let Some((key, rest)) = match_set_op(s, " notin ") {
        return parse_set(key, rest, true);
    }
    if let Some(idx) = s.find("==") {
        let key = s[..idx].trim();
        let value = s[idx + 2..].trim();
        return finalize_eq(key, value);
    }
    if let Some(idx) = s.find("!=") {
        let key = s[..idx].trim();
        let value = s[idx + 2..].trim();
        return finalize_neq(key, value);
    }
    // No operator: bare key = Exists.
    let key = s.trim();
    validate_key(key)?;
    Ok(SelectorExpr::Exists {
        key: key.to_string(),
    })
}

/// Match `<key>(<delim>)(<rest>)`. Returns `(key, rest)` if `delim`
/// appears outside any parens.
fn match_set_op<'a>(s: &'a str, delim: &str) -> Option<(&'a str, &'a str)> {
    let bytes = s.as_bytes();
    let needle = delim.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i + needle.len() <= bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && bytes[i..i + needle.len()] == *needle {
            let key = &s[..i];
            let rest = &s[i + needle.len()..];
            return Some((key, rest));
        }
        i += 1;
    }
    None
}

/// Parse a set operator: `key in (a, b)` or `key notin (a, b)`.
fn parse_set(key: &str, rest: &str, negate: bool) -> Result<SelectorExpr, SelectorParseError> {
    let key = key.trim();
    validate_key(key)?;
    let rest = rest.trim();
    let Some(inner) = rest.strip_prefix('(').and_then(|x| x.strip_suffix(')')) else {
        return Err(SelectorParseError::UnclosedSet {
            clause: rest.to_string(),
        });
    };
    let values: Result<Vec<String>, SelectorParseError> = inner
        .split(',')
        .map(|v| {
            let trimmed = v.trim();
            validate_value(trimmed)?;
            Ok(trimmed.to_string())
        })
        .collect();
    let values = values?;
    if values.is_empty() {
        return Err(SelectorParseError::MalformedClause {
            clause: rest.to_string(),
        });
    }
    Ok(if negate {
        SelectorExpr::NotIn {
            key: key.to_string(),
            values,
        }
    } else {
        SelectorExpr::In {
            key: key.to_string(),
            values,
        }
    })
}

/// Build a [`SelectorExpr::Eq`] after validating both halves.
fn finalize_eq(key: &str, value: &str) -> Result<SelectorExpr, SelectorParseError> {
    if key.is_empty() {
        return Err(SelectorParseError::MalformedClause {
            clause: format!("=={value}"),
        });
    }
    validate_key(key)?;
    validate_value(value)?;
    Ok(SelectorExpr::Eq {
        key: key.to_string(),
        value: value.to_string(),
    })
}

/// Build a [`SelectorExpr::Neq`] after validating both halves.
fn finalize_neq(key: &str, value: &str) -> Result<SelectorExpr, SelectorParseError> {
    if key.is_empty() {
        return Err(SelectorParseError::MalformedClause {
            clause: format!("!={value}"),
        });
    }
    validate_key(key)?;
    validate_value(value)?;
    Ok(SelectorExpr::Neq {
        key: key.to_string(),
        value: value.to_string(),
    })
}

/// Validate a selector key against the `[a-z0-9._-]+` + max-length rule.
fn validate_key(k: &str) -> Result<(), SelectorParseError> {
    if k.is_empty() || k.len() > MAX_KEY_LEN {
        return Err(SelectorParseError::InvalidKey { key: k.to_string() });
    }
    if !k.bytes().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'_'
    }) {
        return Err(SelectorParseError::InvalidKey { key: k.to_string() });
    }
    Ok(())
}

/// Validate a selector value: non-empty and free of `=` / `,`.
fn validate_value(v: &str) -> Result<(), SelectorParseError> {
    if v.is_empty() {
        return Err(SelectorParseError::InvalidValue {
            value: v.to_string(),
        });
    }
    if v.contains('=') || v.contains(',') {
        return Err(SelectorParseError::InvalidValue {
            value: v.to_string(),
        });
    }
    Ok(())
}

/// Render a list of clauses back to the canonical selector source string.
fn canonicalize(exprs: &[SelectorExpr]) -> String {
    let mut parts = Vec::with_capacity(exprs.len());
    for e in exprs {
        parts.push(match e {
            SelectorExpr::Eq { key, value } => format!("{key}=={value}"),
            SelectorExpr::Neq { key, value } => format!("{key}!={value}"),
            SelectorExpr::In { key, values } => format!("{key} in ({})", values.join(",")),
            SelectorExpr::NotIn { key, values } => format!("{key} notin ({})", values.join(",")),
            SelectorExpr::Exists { key } => key.clone(),
        });
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn lbl(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn parse_eq() {
        let s = LabelSelector::parse("role==worker").unwrap();
        assert_eq!(s.exprs.len(), 1);
        assert!(
            matches!(s.exprs[0], SelectorExpr::Eq { ref key, ref value } if key == "role" && value == "worker")
        );
    }

    #[test]
    fn parse_neq() {
        let s = LabelSelector::parse("zone!=eu-west").unwrap();
        assert_eq!(s.exprs.len(), 1);
        assert!(matches!(s.exprs[0], SelectorExpr::Neq { .. }));
    }

    #[test]
    fn parse_in_set() {
        let s = LabelSelector::parse("tier in (gpu, fast)").unwrap();
        match &s.exprs[0] {
            SelectorExpr::In { key, values } => {
                assert_eq!(key, "tier");
                assert_eq!(values, &vec!["gpu".to_string(), "fast".to_string()]);
            }
            _ => panic!("expected In"),
        }
    }

    #[test]
    fn parse_notin_set() {
        let s = LabelSelector::parse("tier notin (gpu, fast)").unwrap();
        match &s.exprs[0] {
            SelectorExpr::NotIn { key, values } => {
                assert_eq!(key, "tier");
                assert_eq!(values.len(), 2);
            }
            _ => panic!("expected NotIn"),
        }
    }

    #[test]
    fn parse_bare_key_is_exists() {
        let s = LabelSelector::parse("preempt").unwrap();
        assert!(matches!(s.exprs[0], SelectorExpr::Exists { ref key } if key == "preempt"));
    }

    #[test]
    fn parse_compound_and() {
        let s = LabelSelector::parse("role==worker, zone!=eu-west").unwrap();
        assert_eq!(s.exprs.len(), 2);
    }

    #[test]
    fn parse_compound_with_set() {
        let s = LabelSelector::parse("tier in (gpu, fast), role==worker").unwrap();
        assert_eq!(s.exprs.len(), 2);
    }

    #[test]
    fn parse_rejects_empty_string() {
        assert_eq!(
            LabelSelector::parse("").unwrap_err(),
            SelectorParseError::Empty
        );
        assert_eq!(
            LabelSelector::parse("   ").unwrap_err(),
            SelectorParseError::Empty
        );
    }

    #[test]
    fn parse_rejects_value_only() {
        let err = LabelSelector::parse("==value").unwrap_err();
        assert!(matches!(err, SelectorParseError::MalformedClause { .. }));
    }

    #[test]
    fn parse_rejects_bad_key_chars() {
        let err = LabelSelector::parse("Role==worker").unwrap_err();
        assert!(matches!(err, SelectorParseError::InvalidKey { .. }));
        let err = LabelSelector::parse("a!bang==worker").unwrap_err();
        assert!(matches!(err, SelectorParseError::InvalidKey { .. }));
    }

    #[test]
    fn parse_rejects_empty_value() {
        let err = LabelSelector::parse("role==").unwrap_err();
        assert!(matches!(err, SelectorParseError::InvalidValue { .. }));
    }

    #[test]
    fn parse_rejects_unclosed_set() {
        let err = LabelSelector::parse("tier in (gpu, fast").unwrap_err();
        assert!(matches!(err, SelectorParseError::UnclosedSet { .. }));
    }

    #[test]
    fn parse_rejects_empty_set() {
        let err = LabelSelector::parse("tier in ()").unwrap_err();
        assert!(matches!(err, SelectorParseError::InvalidValue { .. }));
    }

    #[test]
    fn canonical_form_round_trip() {
        let s = LabelSelector::parse("role==worker, tier in (gpu, fast)").unwrap();
        assert_eq!(s.raw, "role==worker, tier in (gpu,fast)");
    }

    #[test]
    fn matches_eq_present() {
        let s = LabelSelector::parse("role==worker").unwrap();
        assert!(s.matches(&lbl(&[("role", "worker")])));
        assert!(!s.matches(&lbl(&[("role", "control")])));
        assert!(!s.matches(&lbl(&[])));
    }

    #[test]
    fn matches_neq_absent_is_match() {
        let s = LabelSelector::parse("role!=control").unwrap();
        assert!(s.matches(&lbl(&[("role", "worker")])));
        // k8s semantics: key absent matches a `!=` clause.
        assert!(s.matches(&lbl(&[])));
        assert!(!s.matches(&lbl(&[("role", "control")])));
    }

    #[test]
    fn matches_in_set() {
        let s = LabelSelector::parse("tier in (gpu, fast)").unwrap();
        assert!(s.matches(&lbl(&[("tier", "gpu")])));
        assert!(s.matches(&lbl(&[("tier", "fast")])));
        assert!(!s.matches(&lbl(&[("tier", "slow")])));
        assert!(!s.matches(&lbl(&[])));
    }

    #[test]
    fn matches_notin_set_absent_is_match() {
        let s = LabelSelector::parse("tier notin (gpu)").unwrap();
        assert!(s.matches(&lbl(&[("tier", "cpu")])));
        // k8s semantics: key absent matches a notin clause.
        assert!(s.matches(&lbl(&[])));
        assert!(!s.matches(&lbl(&[("tier", "gpu")])));
    }

    #[test]
    fn matches_exists() {
        let s = LabelSelector::parse("preempt").unwrap();
        assert!(s.matches(&lbl(&[("preempt", "true")])));
        assert!(s.matches(&lbl(&[("preempt", "")])));
        assert!(!s.matches(&lbl(&[])));
    }

    #[test]
    fn matches_conjunction() {
        let s = LabelSelector::parse("role==worker, tier in (gpu, fast)").unwrap();
        assert!(s.matches(&lbl(&[("role", "worker"), ("tier", "gpu")])));
        assert!(!s.matches(&lbl(&[("role", "worker"), ("tier", "slow")])));
        assert!(!s.matches(&lbl(&[("role", "control"), ("tier", "gpu")])));
    }

    #[test]
    fn strategy_parses_known_values() {
        assert_eq!(Strategy::parse("auto").unwrap(), Strategy::Auto);
        assert_eq!(Strategy::parse("blue-green").unwrap(), Strategy::BlueGreen);
        assert_eq!(Strategy::parse("rolling").unwrap(), Strategy::Rolling);
        assert_eq!(Strategy::parse("recreate").unwrap(), Strategy::Recreate);
        assert!(Strategy::parse("sideways").is_err());
    }

    #[test]
    fn strategy_default_is_auto() {
        assert_eq!(Strategy::default(), Strategy::Auto);
    }

    #[test]
    fn strategy_as_str_roundtrip() {
        for kw in ["auto", "blue-green", "rolling", "recreate"] {
            assert_eq!(Strategy::parse(kw).unwrap().as_str(), kw);
        }
    }

    #[test]
    fn placement_default_is_singleton_no_selector() {
        assert_eq!(
            Placement::default(),
            Placement::Singleton { selector: None }
        );
    }
}
