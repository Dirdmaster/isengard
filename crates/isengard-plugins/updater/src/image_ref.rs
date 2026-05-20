//! Parse Docker image references into (registry, repository, tag).
//!
//! Handles the common forms:
//!   nginx                          → docker.io / library/nginx / latest
//!   nginx:1.25                     → docker.io / library/nginx / 1.25
//!   library/nginx:1.25             → docker.io / library/nginx / 1.25
//!   ghcr.io/foo/bar:latest         → ghcr.io   / foo/bar       / latest
//!   localhost:5000/baz             → localhost:5000 / baz      / latest
//!
//! A "registry" is detected as the first path component if it contains a `.`,
//! a `:`, or is exactly `localhost`. Otherwise the registry is `docker.io` and
//! single-component repos are prefixed with `library/`.
//!
//! Digest references (`name@sha256:...`) are not parsed here — for the
//! updater's purposes we only need the tag form, since digest-pinned images
//! never need an update.

use std::fmt;

/// Parsed Docker image reference.
///
/// Three pieces: registry host, repository (org/name), tag.
/// Digest-pinned references aren't represented here: those never
/// need updates and the parser returns `None` for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// Registry host (e.g. `docker.io`, `ghcr.io`, `localhost:5000`).
    pub registry: String,
    /// Repository path (e.g. `library/nginx`, `foo/bar`).
    pub repository: String,
    /// Tag (`latest` when unspecified).
    pub tag: String,
}

impl ImageRef {
    /// Parse a reference string. Returns `None` for digest-pinned refs
    /// (`name@sha256:...`) since those are never out-of-date.
    pub fn parse(input: &str) -> Option<Self> {
        if input.contains('@') {
            return None;
        }

        let (head, tag) = match input.rsplit_once(':') {
            // If the part after `:` contains `/`, it was a port number, not a tag.
            Some((h, t)) if !t.contains('/') => (h, t.to_string()),
            _ => (input, "latest".to_string()),
        };

        let (registry, repo_path) = match head.split_once('/') {
            Some((maybe_registry, rest))
                if maybe_registry.contains('.')
                    || maybe_registry.contains(':')
                    || maybe_registry == "localhost" =>
            {
                (maybe_registry.to_string(), rest.to_string())
            }
            _ => ("docker.io".to_string(), head.to_string()),
        };

        // On docker.io, single-component names live under `library/`.
        let repository = if registry == "docker.io" && !repo_path.contains('/') {
            format!("library/{repo_path}")
        } else {
            repo_path
        };

        Some(Self {
            registry,
            repository,
            tag,
        })
    }

    /// Manifest URL for a HEAD request: `https://<registry>/v2/<repo>/manifests/<tag>`.
    /// `docker.io` is rewritten to `registry-1.docker.io` (the actual host).
    pub fn manifest_url(&self) -> String {
        let host = if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        };
        format!(
            "https://{host}/v2/{repo}/manifests/{tag}",
            repo = self.repository,
            tag = self.tag
        )
    }

    /// Tags-list URL for a GET request: `https://<registry>/v2/<repo>/tags/list`.
    /// `docker.io` is rewritten to `registry-1.docker.io`. Used
    /// (`Minor` strategy) to enumerate semver candidates on the registry.
    pub fn tags_list_url(&self) -> String {
        let host = if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        };
        format!("https://{host}/v2/{repo}/tags/list", repo = self.repository,)
    }

    /// Returns a copy of this ref with the tag swapped to `new_tag`.
    /// Convenience for the bumped-tag path.
    pub fn with_tag(&self, new_tag: impl Into<String>) -> Self {
        Self {
            registry: self.registry.clone(),
            repository: self.repository.clone(),
            tag: new_tag.into(),
        }
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}:{}", self.registry, self.repository, self.tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_defaults_to_dockerhub_library_latest() {
        let r = ImageRef::parse("nginx").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn name_with_tag() {
        let r = ImageRef::parse("nginx:1.25").unwrap();
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, "1.25");
    }

    #[test]
    fn dockerhub_user_repo() {
        let r = ImageRef::parse("foo/bar").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "foo/bar");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn ghcr_with_tag() {
        let r = ImageRef::parse("ghcr.io/foo/bar:v2").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "foo/bar");
        assert_eq!(r.tag, "v2");
    }

    #[test]
    fn localhost_with_port_is_a_registry_not_a_tag() {
        let r = ImageRef::parse("localhost:5000/baz").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "baz");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn digest_pinned_returns_none() {
        assert!(ImageRef::parse("nginx@sha256:abc123").is_none());
    }

    #[test]
    fn manifest_url_rewrites_dockerhub() {
        let r = ImageRef::parse("nginx:1.25").unwrap();
        assert_eq!(
            r.manifest_url(),
            "https://registry-1.docker.io/v2/library/nginx/manifests/1.25"
        );
    }

    #[test]
    fn manifest_url_passes_other_registries_through() {
        let r = ImageRef::parse("ghcr.io/foo/bar:v2").unwrap();
        assert_eq!(r.manifest_url(), "https://ghcr.io/v2/foo/bar/manifests/v2");
    }

    #[test]
    fn display_round_trips() {
        let r = ImageRef::parse("ghcr.io/foo/bar:v2").unwrap();
        assert_eq!(r.to_string(), "ghcr.io/foo/bar:v2");
    }

    #[test]
    fn tags_list_url_rewrites_dockerhub() {
        let r = ImageRef::parse("nginx:1.25").unwrap();
        assert_eq!(
            r.tags_list_url(),
            "https://registry-1.docker.io/v2/library/nginx/tags/list"
        );
    }

    #[test]
    fn tags_list_url_passes_other_registries_through() {
        let r = ImageRef::parse("ghcr.io/foo/bar:v2").unwrap();
        assert_eq!(r.tags_list_url(), "https://ghcr.io/v2/foo/bar/tags/list");
    }

    #[test]
    fn with_tag_swaps_only_the_tag() {
        let r = ImageRef::parse("ghcr.io/foo/bar:1.2.3").unwrap();
        let bumped = r.with_tag("1.3.0");
        assert_eq!(bumped.registry, "ghcr.io");
        assert_eq!(bumped.repository, "foo/bar");
        assert_eq!(bumped.tag, "1.3.0");
    }
}
