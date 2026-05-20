//! `isengard://` URI parsing and construction.
//!
//! Three URI families, one per content tree:
//!
//! - `isengard://docs/<path>` for operator guides. Path is the
//!   relative path under `docs/` with the `.md` extension stripped.
//!   Example: `isengard://docs/concepts/labels` resolves to
//!   `docs/concepts/labels.md`.
//! - `isengard://api/<crate>/<symbol>` for per-crate API reference.
//!   Resolves to `crates/<crate>/docs/<symbol>.md`.
//! - `isengard://skill/<name>` for AI playbooks. Name is the
//!   filename stem under `skills/`. Resolves to `skills/<name>.md`.
//!
//! The scheme prefix is `isengard://`. Parsers reject anything that
//! does not start with that prefix or that uses an unknown segment.

/// Parsed [`isengard://`] URI.
///
/// Constructed by [`ResourceUri::parse`]. Each variant holds the
/// pieces needed to look up the corresponding file in the embedded
/// trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceUri {
    /// Operator-facing guide. Path is the relative path under
    /// `docs/`, without the `.md` extension.
    Docs(String),
    /// Per-crate API reference. Crate is the workspace member
    /// directory name; symbol is the `.md` stem under that crate's
    /// `docs/` directory.
    Api { krate: String, symbol: String },
    /// AI playbook. Name is the `.md` stem under `skills/`.
    Skill(String),
}

impl ResourceUri {
    /// Parse an [`isengard://`] URI into a typed variant.
    ///
    /// Returns `None` when the scheme is wrong or the segment count
    /// does not match any of the three URI families.
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("isengard://")?;
        let (kind, tail) = rest.split_once('/')?;
        match kind {
            "docs" => Some(Self::Docs(tail.to_string())),
            "api" => {
                let (krate, symbol) = tail.split_once('/')?;
                if krate.is_empty() || symbol.is_empty() {
                    return None;
                }
                Some(Self::Api {
                    krate: krate.to_string(),
                    symbol: symbol.to_string(),
                })
            }
            "skill" => {
                if tail.is_empty() || tail.contains('/') {
                    return None;
                }
                Some(Self::Skill(tail.to_string()))
            }
            _ => None,
        }
    }
}

/// Build the canonical [`isengard://docs/`] URI for a relative doc
/// path. The `.md` extension is stripped to match the URI scheme.
pub fn build_docs_uri(rel_path: &str) -> String {
    let stem = rel_path.strip_suffix(".md").unwrap_or(rel_path);
    format!("isengard://docs/{stem}")
}

/// Build the canonical [`isengard://api/`] URI for a per-crate API
/// doc. `krate` is the workspace member directory name; `symbol` is
/// the `.md` stem under that crate's `docs/`.
pub fn build_api_uri(krate: &str, symbol: &str) -> String {
    let stem = symbol.strip_suffix(".md").unwrap_or(symbol);
    format!("isengard://api/{krate}/{stem}")
}

/// Build the canonical [`isengard://skill/`] URI for a playbook.
/// `name` is the `.md` stem under `skills/`.
pub fn build_skill_uri(name: &str) -> String {
    let stem = name.strip_suffix(".md").unwrap_or(name);
    format!("isengard://skill/{stem}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docs_uri() {
        let parsed = ResourceUri::parse("isengard://docs/concepts/labels");
        assert_eq!(parsed, Some(ResourceUri::Docs("concepts/labels".into())));
    }

    #[test]
    fn parses_api_uri() {
        let parsed = ResourceUri::parse("isengard://api/isengard-core/join-token");
        assert_eq!(
            parsed,
            Some(ResourceUri::Api {
                krate: "isengard-core".into(),
                symbol: "join-token".into(),
            }),
        );
    }

    #[test]
    fn parses_skill_uri() {
        let parsed = ResourceUri::parse("isengard://skill/add-a-route");
        assert_eq!(parsed, Some(ResourceUri::Skill("add-a-route".into())));
    }

    #[test]
    fn rejects_wrong_scheme() {
        assert_eq!(ResourceUri::parse("file:///etc/passwd"), None);
        assert_eq!(ResourceUri::parse("isengard://unknown/x"), None);
    }

    #[test]
    fn rejects_malformed_api_uri() {
        // missing symbol segment
        assert_eq!(ResourceUri::parse("isengard://api/isengard-core"), None);
    }

    #[test]
    fn rejects_skill_uri_with_slash() {
        // skills/<name>.md is flat; nested skill paths are invalid
        assert_eq!(ResourceUri::parse("isengard://skill/nested/path"), None);
    }

    #[test]
    fn builds_docs_uri_with_and_without_md() {
        assert_eq!(
            build_docs_uri("concepts/labels.md"),
            "isengard://docs/concepts/labels",
        );
        assert_eq!(
            build_docs_uri("concepts/labels"),
            "isengard://docs/concepts/labels",
        );
    }

    #[test]
    fn builds_api_uri() {
        assert_eq!(
            build_api_uri("isengard-core", "join-token.md"),
            "isengard://api/isengard-core/join-token",
        );
    }

    #[test]
    fn builds_skill_uri() {
        assert_eq!(
            build_skill_uri("add-a-route.md"),
            "isengard://skill/add-a-route",
        );
    }
}
