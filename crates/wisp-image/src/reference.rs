//! Image reference parsing: registry / repo / tag / digest decomposition.
//!
//! The parser is hand-rolled rather than regex-based; the grammar is
//! short enough that explicit positional logic reads more clearly than a
//! pattern. Defaults follow the conventions Docker / containerd use:
//!
//!   - bare `alpine`               -> `docker.io/library/alpine:latest`
//!   - `nginx:alpine`              -> `docker.io/library/nginx:alpine`
//!   - `library/redis:7.4`         -> `docker.io/library/redis:7.4`
//!   - `ghcr.io/foo/bar:baz`       -> `ghcr.io/foo/bar:baz`
//!   - `alpine@sha256:<hex>`       -> `docker.io/library/alpine@sha256:<hex>`
//!
//! A `/`-separated first component is treated as a registry only if it
//! contains a `.` (DNS dot) or a `:` (port). Anything else lives in the
//! Docker Hub `library/` namespace.

use crate::error::WispImageError;

/// Parsed image reference. Either `tag` or `digest` is set; they are
/// mutually exclusive (matches the OCI distribution spec, which lets you
/// pin by digest XOR resolve by tag, never both).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// Registry hostname (with optional `:port`). Defaulted to `docker.io`
    /// when the input doesn't contain a recognisable registry component.
    pub registry: String,
    /// Repository path within the registry (e.g. `library/nginx`,
    /// `dirdmaster/foo`). Always populated.
    pub repo: String,
    /// Tag, if the ref was tag-based. `Some("latest")` for inputs that
    /// omit both tag and digest.
    pub tag: Option<String>,
    /// Digest, if the ref was digest-based. Always starts with `sha256:`
    /// in Phase 0.2; other algorithms get rejected at parse time.
    pub digest: Option<String>,
}

impl ImageRef {
    /// Parse a reference string into structured components. Errors on
    /// empty input, multiple `@`, both tag and digest set, or unsupported
    /// digest algorithms.
    pub fn parse(s: &str) -> Result<Self, WispImageError> {
        if s.is_empty() {
            return Err(WispImageError::Parse("empty image reference".into()));
        }

        // Split off digest if present. Multiple `@` is illegal; image
        // names never contain `@` outside the digest separator.
        let at_count = s.matches('@').count();
        if at_count > 1 {
            return Err(WispImageError::Parse(format!(
                "image reference has multiple '@' separators: {s:?}"
            )));
        }

        let (image_part, digest) = if let Some((head, tail)) = s.split_once('@') {
            if !tail.starts_with("sha256:") {
                return Err(WispImageError::Parse(format!(
                    "unsupported digest algorithm in {s:?} (only sha256 is accepted in 0.2)"
                )));
            }
            // Reject `sha256:` with no hex payload, and any whitespace.
            let hex = &tail["sha256:".len()..];
            if hex.is_empty() || hex.chars().any(|c| !c.is_ascii_hexdigit()) {
                return Err(WispImageError::Parse(format!(
                    "malformed sha256 digest in {s:?}"
                )));
            }
            (head, Some(tail.to_string()))
        } else {
            (s, None)
        };

        if image_part.is_empty() {
            return Err(WispImageError::Parse("empty image reference".into()));
        }

        // Tag detection: find the last `:` that comes after the last `/`.
        // The leading `/`-component may contain a `:` for `host:port`;
        // that `:` is part of the registry, not a tag separator.
        let last_slash = image_part.rfind('/');
        let scan_start = last_slash.map_or(0, |i| i + 1);
        let last_segment = &image_part[scan_start..];
        let (repo_part, tag) = if let Some(colon_in_seg) = last_segment.find(':') {
            let split_at = scan_start + colon_in_seg;
            let repo_str = &image_part[..split_at];
            let tag_str = &image_part[split_at + 1..];
            if tag_str.is_empty() {
                return Err(WispImageError::Parse(format!("empty tag in {s:?}")));
            }
            // After the digest split a tag must be plain; reject embedded
            // `@` or `:` so we surface obvious malformations explicitly.
            if tag_str.contains('@') || tag_str.contains(':') {
                return Err(WispImageError::Parse(format!(
                    "invalid characters in tag {tag_str:?}"
                )));
            }
            (repo_str, Some(tag_str.to_string()))
        } else {
            (image_part, None)
        };

        if tag.is_some() && digest.is_some() {
            return Err(WispImageError::Parse(format!(
                "image reference has both tag and digest: {s:?}"
            )));
        }

        // Registry vs. namespace detection. The first `/`-separated
        // component is a registry if and only if it contains `.` or `:`.
        // Otherwise the input is in the Docker Hub namespace.
        let (registry, repo) = if let Some((head, rest)) = repo_part.split_once('/') {
            if head.contains('.') || head.contains(':') {
                (head.to_string(), rest.to_string())
            } else {
                ("docker.io".to_string(), format!("{head}/{rest}"))
            }
        } else {
            // Single-component name. Default to docker.io/library/<name>.
            ("docker.io".to_string(), format!("library/{repo_part}"))
        };

        if repo.is_empty() {
            return Err(WispImageError::Parse(format!("empty repo in {s:?}")));
        }

        // If neither tag nor digest were supplied, default to `:latest`.
        let tag = if tag.is_none() && digest.is_none() {
            Some("latest".to_string())
        } else {
            tag
        };

        Ok(ImageRef {
            registry,
            repo,
            tag,
            digest,
        })
    }
}

/// Round-trippable display form: always emits `<registry>/<repo>` plus
/// either `:tag` or `@digest`. Inputs that defaulted (bare `alpine`)
/// re-emit in canonical form (`docker.io/library/alpine:latest`); the
/// canonical form re-parses to an identical `ImageRef`.
impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.registry, self.repo)?;
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        } else if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_name() {
        let r = ImageRef::parse("alpine").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repo, "library/alpine");
        assert_eq!(r.tag.as_deref(), Some("latest"));
        assert_eq!(r.digest, None);
    }

    #[test]
    fn parse_name_with_tag() {
        let r = ImageRef::parse("nginx:alpine").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repo, "library/nginx");
        assert_eq!(r.tag.as_deref(), Some("alpine"));
        assert_eq!(r.digest, None);
    }

    #[test]
    fn parse_namespaced() {
        // First slash component has no `.` or `:`, so it's a Docker Hub
        // namespace, not a registry.
        let r = ImageRef::parse("foo/bar:baz").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repo, "foo/bar");
        assert_eq!(r.tag.as_deref(), Some("baz"));
    }

    #[test]
    fn parse_namespaced_library_explicit() {
        let r = ImageRef::parse("library/redis:7.4").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repo, "library/redis");
        assert_eq!(r.tag.as_deref(), Some("7.4"));
    }

    #[test]
    fn parse_explicit_registry() {
        let r = ImageRef::parse("ghcr.io/foo/bar:baz").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repo, "foo/bar");
        assert_eq!(r.tag.as_deref(), Some("baz"));
    }

    #[test]
    fn parse_registry_with_port() {
        let r = ImageRef::parse("localhost:5000/foo:bar").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repo, "foo");
        assert_eq!(r.tag.as_deref(), Some("bar"));
    }

    #[test]
    fn parse_digest_pull() {
        let r = ImageRef::parse(
            "docker.io/library/alpine@sha256:abc1234567890def0000000000000000000000000000000000000000000000abc",
        )
        .unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repo, "library/alpine");
        assert_eq!(r.tag, None);
        assert_eq!(
            r.digest.as_deref(),
            Some("sha256:abc1234567890def0000000000000000000000000000000000000000000000abc")
        );
    }

    #[test]
    fn parse_digest_pull_implicit_registry() {
        let r = ImageRef::parse(
            "alpine@sha256:abc1234567890def0000000000000000000000000000000000000000000000abc",
        )
        .unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repo, "library/alpine");
        assert_eq!(r.tag, None);
        assert!(r.digest.is_some());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(ImageRef::parse("").is_err());
    }

    #[test]
    fn parse_rejects_multiple_at() {
        assert!(ImageRef::parse("alpine@sha256:abc@sha256:def").is_err());
    }

    #[test]
    fn parse_rejects_tag_with_digest() {
        // Tag and digest on the same ref is illegal per the OCI dist spec.
        let res = ImageRef::parse(
            "alpine:3.19@sha256:abc1234567890def0000000000000000000000000000000000000000000000abc",
        );
        assert!(res.is_err());
    }

    #[test]
    fn parse_rejects_invalid_digest_algo() {
        assert!(ImageRef::parse("alpine@md5:abc").is_err());
        assert!(ImageRef::parse("alpine@sha512:abc").is_err());
    }

    #[test]
    fn parse_rejects_malformed_digest() {
        assert!(ImageRef::parse("alpine@sha256:").is_err());
        assert!(ImageRef::parse("alpine@sha256:not-hex!").is_err());
    }

    #[test]
    fn display_round_trips_for_all_valid_forms() {
        // Bare names re-emit in canonical form; that canonical form
        // round-trips to an identical struct.
        let cases = [
            "alpine",
            "nginx:alpine",
            "foo/bar:baz",
            "library/redis:7.4",
            "ghcr.io/foo/bar:baz",
            "localhost:5000/foo:bar",
            "docker.io/library/alpine@sha256:abc1234567890def0000000000000000000000000000000000000000000000abc",
            "alpine@sha256:abc1234567890def0000000000000000000000000000000000000000000000abc",
        ];
        for input in cases {
            let parsed = ImageRef::parse(input).expect(input);
            let displayed = format!("{parsed}");
            let re_parsed = ImageRef::parse(&displayed)
                .unwrap_or_else(|e| panic!("display form {displayed:?} did not re-parse: {e}"));
            assert_eq!(
                parsed, re_parsed,
                "round-trip mismatch for input {input:?} via {displayed:?}"
            );
        }
    }
}
