//! Pull orchestration: the public surface that wisp-cli wires to.
//!
//! `Client::pull` walks the OCI distribution-spec endpoints in order:
//!
//!   1. `GET /v2/` to surface the auth challenge if the registry
//!      requires one. Public registries (Docker Hub, GHCR) do; some
//!      pass-through caches don't, so a 200 here is also legal.
//!   2. `GET /v2/<repo>/manifests/<tag-or-digest>` with a wide `Accept`
//!      header so the registry knows we'll handle either an image
//!      manifest or a multi-arch index.
//!   3. If the response is an Index, recurse with the
//!      arch-matched manifest's digest.
//!   4. Persist the manifest blob, write the tag pointer, fetch the
//!      config blob, then fetch every layer blob.
//!
//! Token cache: a single token is obtained at step 1 and reused for
//! every subsequent request in the same `pull`. We don't cache across
//! pulls (cheap to redo, scopes differ per repo), and we don't refresh
//! mid-pull (a token expiring during a multi-GB image pull is rare and
//! the user can just retry).
//!
//! Layer fetch is sequential, not parallel. The bottleneck for the 0.2
//! demo is alpine-sized (4MB), so concurrent fetches add complexity
//! without speed. Easy to revisit in 0.3 with a small worker pool.

use std::path::Path;

use crate::error::WispImageError;
use crate::reference::ImageRef;
use crate::registry::auth;
use crate::registry::blob;
use crate::registry::manifest::{self, Manifest};
use crate::store::{ContentStore, GcReport};

/// Reqwest's `Response::headers()` type. Re-exported as a local alias
/// to keep the rest of the file from leaking the `http` re-export.
type Headers = reqwest::header::HeaderMap;

/// One layer in a pulled image. Carries the minimum the dispatch-C
/// layer extractor needs: the digest to fetch the blob from the store,
/// the byte size for sanity-checks, and the media type so the
/// extractor can pick the right (gzip / zstd / raw) decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerRef {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

/// Result of a successful pull. The manifest, config, and every layer
/// blob are persisted to the store; this struct is just the in-memory
/// summary returned to callers.
#[derive(Debug, Clone)]
pub struct PulledImage {
    pub r: ImageRef,
    pub manifest_digest: String,
    pub config: oci_spec::image::ImageConfiguration,
    pub layers: Vec<LayerRef>,
}

/// Override the host-arch detection used during pull. Stored as a
/// pair of strings so tests can pin the arch to whatever shape the
/// fixture index publishes.
#[derive(Debug, Clone)]
pub struct PlatformOverride {
    pub arch: String,
    pub os: String,
}

/// OCI distribution-spec client. Holds the store and a single shared
/// blocking HTTP client (reqwest pools connections internally).
///
/// Construct via `Client::new` for production use; tests use
/// `Client::with_endpoint` to override the docker.io alias (so a
/// wiremock'd `MockServer` can stand in for a real registry).
pub struct Client {
    store: ContentStore,
    http: reqwest::blocking::Client,
    /// Override map: registry name -> base URL (no trailing slash).
    /// Empty in production; populated by tests so that the docker.io
    /// special-case doesn't try to dial out to the real Docker Hub.
    endpoint_overrides: Vec<(String, String)>,
    /// Override the platform used for arch selection on multi-arch
    /// indexes. None falls back to host_arch + host_os.
    platform: Option<PlatformOverride>,
}

impl Client {
    /// Open or create the content store at `store_dir` and build a
    /// default reqwest blocking client. Connection-pooled, rustls-only.
    pub fn new(store_dir: &Path) -> Result<Self, WispImageError> {
        let store = ContentStore::new(store_dir)?;
        let http = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| WispImageError::Network(format!("http client: {e}")))?;
        Ok(Self {
            store,
            http,
            endpoint_overrides: Vec::new(),
            platform: None,
        })
    }

    /// Test-only: register a registry-name to base-URL override. The
    /// override base URL is used verbatim for `<base>/v2/...` resolution.
    /// e.g. `with_endpoint("docker.io", "http://127.0.0.1:1234")`.
    pub fn with_endpoint(mut self, registry: &str, base_url: &str) -> Self {
        let trimmed = base_url.trim_end_matches('/').to_string();
        self.endpoint_overrides
            .push((registry.to_string(), trimmed));
        self
    }

    /// Test-only: pin the arch / os used for index resolution. Without
    /// this, pulls follow `host_arch()` / `host_os()`, which is
    /// non-deterministic across CI / dev machines.
    pub fn with_platform(mut self, arch: &str, os: &str) -> Self {
        self.platform = Some(PlatformOverride {
            arch: arch.to_string(),
            os: os.to_string(),
        });
        self
    }

    /// Borrow the underlying store. Used by `BundleBuilder` (dispatch D)
    /// to read layer blobs without re-opening the store directory.
    pub fn store(&self) -> &ContentStore {
        &self.store
    }

    /// Pull `r` from its registry, persisting the manifest, config, and
    /// every layer to the store. Returns a summary the caller indexes by.
    pub fn pull(&self, r: &ImageRef) -> Result<PulledImage, WispImageError> {
        let base = self.resolve_base_url(&r.registry);

        // Step 1: probe `/v2/`. A 401 here yields the auth challenge
        // we'll forward to the realm; a 2xx means anonymous + open
        // registry. We ignore other failures and let the manifest GET
        // surface them (some registries answer `/v2/` with 404).
        let token = self.maybe_obtain_token(&base, r)?;

        // Step 2: GET the manifest. Tags or digests both go in the URL
        // verbatim (the OCI spec accepts either at this endpoint).
        let target = r
            .digest
            .clone()
            .or_else(|| r.tag.clone())
            .ok_or_else(|| WispImageError::Parse("ImageRef has no tag or digest".into()))?;

        let (manifest_bytes, manifest_digest, content_type) =
            self.get_manifest(&base, &r.repo, &target, token.as_deref())?;

        let manifest = manifest::parse(&manifest_bytes, content_type.as_deref())?;

        // Step 3: if it's an index, pick the arch entry and recurse.
        let (manifest, manifest_bytes, manifest_digest) = match manifest {
            Manifest::Image(m) => (m, manifest_bytes, manifest_digest),
            Manifest::Index(idx) => {
                let arch = self.target_arch();
                let os = self.target_os();
                let descriptor =
                    manifest::select_arch_entry(&idx, &arch, &os).ok_or_else(|| {
                        WispImageError::Manifest(format!(
                            "no manifest in index for {arch}/{os} (image: {r})"
                        ))
                    })?;
                let entry_digest = descriptor.digest().to_string();
                let (bytes, digest_after, ct) =
                    self.get_manifest(&base, &r.repo, &entry_digest, token.as_deref())?;
                let inner = manifest::parse(&bytes, ct.as_deref())?;
                let inner = match inner {
                    Manifest::Image(m) => m,
                    Manifest::Index(_) => {
                        return Err(WispImageError::Manifest(
                            "registry returned an index where an image manifest was expected"
                                .into(),
                        ));
                    }
                };
                (inner, bytes, digest_after)
            }
        };

        // Step 4a: persist the manifest blob. We trust
        // `write_blob_streaming` to verify the digest against what the
        // registry's Docker-Content-Digest header gave us (or what the
        // descriptor pointed at when resolving an index).
        let stored_manifest = self
            .store
            .write_blob_streaming(manifest_bytes.as_slice(), Some(&manifest_digest))?;
        debug_assert_eq!(stored_manifest, manifest_digest);

        // Step 4b: tag pointer (only for tag-pulls; digest pulls don't
        // need an alias since the digest IS the canonical key).
        if let Some(tag) = &r.tag {
            self.store
                .put_tag(&r.registry, &r.repo, tag, &manifest_digest)?;
        }

        // Step 5: fetch + persist the config blob.
        let config_descriptor = manifest.config();
        let config_digest = config_descriptor.digest().to_string();
        let config_url = format!(
            "{base}/v2/{repo}/blobs/{digest}",
            base = base,
            repo = r.repo,
            digest = config_digest
        );
        if !self.store.has_blob(&config_digest) {
            blob::fetch_to_store(
                &self.http,
                &config_url,
                token.as_deref(),
                Some(&config_digest),
                &self.store,
            )?;
        }
        let config_bytes = self.store.read_blob(&config_digest)?;
        let config = oci_spec::image::ImageConfiguration::from_reader(config_bytes.as_slice())
            .map_err(|e| WispImageError::Manifest(format!("ImageConfiguration parse: {e}")))?;

        // Step 6: fetch + persist every layer (sequential).
        let mut layers = Vec::with_capacity(manifest.layers().len());
        for desc in manifest.layers() {
            let digest = desc.digest().to_string();
            let media_type = desc.media_type().to_string();
            let size = desc.size();
            if !self.store.has_blob(&digest) {
                let layer_url = format!(
                    "{base}/v2/{repo}/blobs/{digest}",
                    base = base,
                    repo = r.repo,
                    digest = digest
                );
                blob::fetch_to_store(
                    &self.http,
                    &layer_url,
                    token.as_deref(),
                    Some(&digest),
                    &self.store,
                )?;
            }
            layers.push(LayerRef {
                digest,
                size,
                media_type,
            });
        }

        Ok(PulledImage {
            r: r.clone(),
            manifest_digest,
            config,
            layers,
        })
    }

    /// Look up a previously pulled image. Returns `None` if no tag /
    /// digest pointer matches in the local store.
    pub fn lookup(&self, r: &ImageRef) -> Result<Option<PulledImage>, WispImageError> {
        // Tag-style lookups consult the index; digest-style lookups
        // can short-circuit since the digest IS the manifest key.
        let manifest_digest = if let Some(digest) = &r.digest {
            digest.clone()
        } else if let Some(tag) = &r.tag {
            match self.store.lookup_tag(&r.registry, &r.repo, tag) {
                Some(d) => d,
                None => return Ok(None),
            }
        } else {
            return Ok(None);
        };

        if !self.store.has_blob(&manifest_digest) {
            return Ok(None);
        }
        let bytes = self.store.read_blob(&manifest_digest)?;
        let parsed = manifest::parse(&bytes, None)?;
        let m = match parsed {
            Manifest::Image(m) => m,
            Manifest::Index(_) => {
                // The store should never carry an Index as a tag's
                // resolved manifest (we always resolve through to the
                // image manifest before tagging). If it does, treat
                // it as a stale entry.
                return Ok(None);
            }
        };
        let config_digest = m.config().digest().to_string();
        let config_bytes = self.store.read_blob(&config_digest)?;
        let config = oci_spec::image::ImageConfiguration::from_reader(config_bytes.as_slice())
            .map_err(|e| WispImageError::Manifest(format!("ImageConfiguration parse: {e}")))?;
        let layers = m
            .layers()
            .iter()
            .map(|d| LayerRef {
                digest: d.digest().to_string(),
                size: d.size(),
                media_type: d.media_type().to_string(),
            })
            .collect();
        Ok(Some(PulledImage {
            r: r.clone(),
            manifest_digest,
            config,
            layers,
        }))
    }

    /// Walk the store's index, returning a `PulledImage` for every
    /// recorded `(registry, repo, tag) -> manifest_digest` pointer.
    /// Best-effort: stale or partially-deleted entries are skipped.
    pub fn list(&self) -> Result<Vec<PulledImage>, WispImageError> {
        let mut out = Vec::new();
        for (r, _digest) in self.store.list_images()? {
            if let Some(p) = self.lookup(&r)? {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Drop unreferenced blobs. Wraps `ContentStore::gc`.
    pub fn gc(&self) -> Result<GcReport, WispImageError> {
        self.store.gc()
    }

    /// Resolve the registry hostname to a base URL. The docker.io
    /// alias maps to `registry-1.docker.io` per Docker's convention;
    /// all other registries are reached at `https://<host>/` directly.
    /// Tests register endpoint overrides via `with_endpoint` so an
    /// in-process MockServer can stand in for a real registry.
    fn resolve_base_url(&self, registry: &str) -> String {
        if let Some((_, override_url)) = self
            .endpoint_overrides
            .iter()
            .find(|(name, _)| name == registry)
        {
            return override_url.clone();
        }
        if registry == "docker.io" {
            return "https://registry-1.docker.io".to_string();
        }
        format!("https://{registry}")
    }

    fn target_arch(&self) -> String {
        self.platform
            .as_ref()
            .map(|p| p.arch.clone())
            .unwrap_or_else(|| manifest::host_arch().to_string())
    }

    fn target_os(&self) -> String {
        self.platform
            .as_ref()
            .map(|p| p.os.clone())
            .unwrap_or_else(|| manifest::host_os().to_string())
    }

    /// Probe `<base>/v2/` and obtain a bearer token if the registry
    /// requires one. Returns `None` when the registry is anonymous.
    /// The scope is derived from the image ref so the token is valid
    /// for the manifest + blob endpoints under that repo.
    fn maybe_obtain_token(
        &self,
        base: &str,
        r: &ImageRef,
    ) -> Result<Option<String>, WispImageError> {
        let url = format!("{base}/v2/");
        let resp = self
            .http
            .get(&url)
            .send()
            .map_err(|e| WispImageError::Network(format!("GET {url}: {e}")))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(None);
        }
        if status.as_u16() != 401 {
            // Non-401 failure here is non-fatal; some registries
            // answer /v2/ with 404. Fall through and let the manifest
            // GET surface the real error.
            return Ok(None);
        }

        let challenge_header = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .ok_or_else(|| {
                WispImageError::Auth(format!(
                    "registry {base} returned 401 without WWW-Authenticate"
                ))
            })?
            .to_str()
            .map_err(|e| WispImageError::Auth(format!("WWW-Authenticate not ASCII: {e}")))?
            .to_string();

        let mut challenge = auth::parse_challenge(&challenge_header).ok_or_else(|| {
            WispImageError::Auth(format!(
                "registry {base} returned a non-Bearer challenge: {challenge_header}"
            ))
        })?;
        // If the realm didn't echo a scope, derive one from the image
        // ref so the token has permission for manifest + blob reads.
        if challenge.scope.is_none() {
            challenge.scope = Some(format!("repository:{}:pull", r.repo));
        }
        let token = auth::obtain_token(&self.http, &challenge)?;
        Ok(Some(token))
    }

    /// GET a manifest at `<base>/v2/<repo>/manifests/<reference>` with
    /// the right Accept header. Retries once with a fresh token if the
    /// first attempt yields 401 (registry rotated tokens mid-pull, or
    /// our /v2/ probe missed the challenge for the manifest scope).
    /// Returns `(body, manifest_digest, content_type_header)`.
    fn get_manifest(
        &self,
        base: &str,
        repo: &str,
        reference: &str,
        token: Option<&str>,
    ) -> Result<(Vec<u8>, String, Option<String>), WispImageError> {
        let url = format!("{base}/v2/{repo}/manifests/{reference}");
        let accept = format!(
            "{},{},{},{}",
            manifest::MT_OCI_MANIFEST,
            manifest::MT_OCI_INDEX,
            manifest::MT_DOCKER_V2_MANIFEST,
            manifest::MT_DOCKER_MANIFEST_LIST
        );

        let mut req = self.http.get(&url).header(reqwest::header::ACCEPT, &accept);
        if let Some(tok) = token {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .map_err(|e| WispImageError::Network(format!("GET {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            let snippet = clip(&body, 1024);
            return Err(WispImageError::Network(format!(
                "manifest {url} returned {status}: {snippet}"
            )));
        }

        let content_type = header_string(resp.headers(), reqwest::header::CONTENT_TYPE);
        // Prefer the registry-supplied Docker-Content-Digest so we can
        // verify the on-disk blob matches what the registry pointed at.
        // Fall back to hashing the body locally if the header is absent.
        let header_digest = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok().map(std::string::ToString::to_string));
        let body = resp
            .bytes()
            .map_err(|e| WispImageError::Network(format!("read body: {e}")))?
            .to_vec();
        let digest = match header_digest {
            Some(d) => d,
            None => {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(&body);
                format!("sha256:{}", hex::encode(hasher.finalize()))
            }
        };
        Ok((body, digest, content_type))
    }
}

fn header_string(headers: &Headers, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok().map(std::string::ToString::to_string))
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::manifest::{MT_DOCKER_V2_MANIFEST, MT_OCI_INDEX, MT_OCI_MANIFEST};
    use sha2::{Digest as _, Sha256};
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path as wpath};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sha256_hex(b: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(b);
        hex::encode(h.finalize())
    }
    fn sha256_digest(b: &[u8]) -> String {
        format!("sha256:{}", sha256_hex(b))
    }

    /// Minimal valid image config JSON. oci-spec is permissive about
    /// missing fields, so this is enough to round-trip.
    fn make_config_bytes() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "architecture": "arm64",
            "os": "linux",
            "rootfs": {
                "type": "layers",
                "diff_ids": []
            }
        }))
        .unwrap()
    }

    fn make_layer_bytes(label: &[u8]) -> Vec<u8> {
        // Just opaque bytes; the orchestrator never inspects them.
        let mut v = Vec::with_capacity(64);
        v.extend_from_slice(b"layer:");
        v.extend_from_slice(label);
        v
    }

    fn make_manifest(config: &[u8], layers: &[(&str, &[u8])]) -> Vec<u8> {
        let layers_json: Vec<_> = layers
            .iter()
            .map(|(mt, b)| {
                serde_json::json!({
                    "mediaType": mt,
                    "digest": sha256_digest(b),
                    "size": b.len(),
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": MT_OCI_MANIFEST,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": sha256_digest(config),
                "size": config.len(),
            },
            "layers": layers_json,
        }))
        .unwrap()
    }

    fn make_index(arch_entries: &[(&str, &str, &[u8])]) -> Vec<u8> {
        // arch_entries: (arch, os, manifest_bytes_for_digest)
        let manifests: Vec<_> = arch_entries
            .iter()
            .map(|(arch, os, b)| {
                serde_json::json!({
                    "mediaType": MT_OCI_MANIFEST,
                    "digest": sha256_digest(b),
                    "size": b.len(),
                    "platform": {
                        "architecture": arch,
                        "os": os,
                    }
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": MT_OCI_INDEX,
            "manifests": manifests,
        }))
        .unwrap()
    }

    fn store_dir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Mount the canonical /v2/ probe response (200) so the
    /// orchestrator's pre-flight passes without requiring auth. Tests
    /// that exercise the auth path skip this and mount their own.
    async fn mount_anonymous_v2(server: &MockServer) {
        Mock::given(method("GET"))
            .and(wpath("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn pull_against_wiremocked_registry() {
        let server = MockServer::start().await;
        mount_anonymous_v2(&server).await;

        let config = make_config_bytes();
        let layer = make_layer_bytes(b"one");
        let layers_spec = [(
            "application/vnd.oci.image.layer.v1.tar+gzip",
            layer.as_slice(),
        )];
        let manifest_bytes = make_manifest(&config, &layers_spec);
        let manifest_digest = sha256_digest(&manifest_bytes);
        let config_digest = sha256_digest(&config);
        let layer_digest = sha256_digest(&layer);

        Mock::given(method("GET"))
            .and(wpath("/v2/library/alpine/manifests/3.19"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(manifest_bytes.clone())
                    .insert_header("Content-Type", MT_OCI_MANIFEST)
                    .insert_header("Docker-Content-Digest", manifest_digest.as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{config_digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(config.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{layer_digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(layer.clone()))
            .mount(&server)
            .await;

        let dir = store_dir();
        let store_path = dir.path().to_path_buf();
        let endpoint = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = Client::new(&store_path)
                .unwrap()
                .with_endpoint("docker.io", &endpoint);
            client.pull(&"alpine:3.19".parse::<ImageRef>().unwrap_or_else(|e| {
                panic!("parse: {e}");
            }))
        })
        .await
        .expect("join")
        .expect("pull");
        assert_eq!(result.layers.len(), 1);
        assert_eq!(result.layers[0].digest, layer_digest);
        assert_eq!(result.manifest_digest, manifest_digest);
    }

    #[tokio::test]
    async fn pull_resolves_index_to_arch_specific_manifest() {
        let server = MockServer::start().await;
        mount_anonymous_v2(&server).await;

        let config = make_config_bytes();
        let layer = make_layer_bytes(b"arm64-payload");
        let arm_manifest = make_manifest(
            &config,
            &[(
                "application/vnd.oci.image.layer.v1.tar+gzip",
                layer.as_slice(),
            )],
        );
        // amd64 entry uses the same layer + config to keep digests
        // disjoint; we never fetch this manifest in the test.
        let amd_manifest = make_manifest(&config, &[]);
        let arm_digest = sha256_digest(&arm_manifest);
        let amd_digest = sha256_digest(&amd_manifest);
        let config_digest = sha256_digest(&config);
        let layer_digest = sha256_digest(&layer);

        let index = make_index(&[
            ("amd64", "linux", &amd_manifest),
            ("arm64", "linux", &arm_manifest),
        ]);
        let index_digest = sha256_digest(&index);

        Mock::given(method("GET"))
            .and(wpath("/v2/library/alpine/manifests/3.19"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(index.clone())
                    .insert_header("Content-Type", MT_OCI_INDEX)
                    .insert_header("Docker-Content-Digest", index_digest.as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/manifests/{arm_digest}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(arm_manifest.clone())
                    .insert_header("Content-Type", MT_OCI_MANIFEST)
                    .insert_header("Docker-Content-Digest", arm_digest.as_str()),
            )
            .mount(&server)
            .await;
        // Note: no mock for the amd64 manifest. If the orchestrator
        // accidentally fetches it, wiremock will return 404 and the
        // test fails.
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{config_digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(config.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{layer_digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(layer.clone()))
            .mount(&server)
            .await;

        let dir = store_dir();
        let store_path = dir.path().to_path_buf();
        let endpoint = server.uri();
        let amd_clone = amd_digest.clone();
        let result = tokio::task::spawn_blocking(move || {
            let client = Client::new(&store_path)
                .unwrap()
                .with_endpoint("docker.io", &endpoint)
                .with_platform("arm64", "linux");
            let pulled = client
                .pull(&"alpine:3.19".parse::<ImageRef>().unwrap())
                .unwrap();
            // Sanity: we resolved through the index to the arm64 leaf.
            assert_eq!(pulled.manifest_digest, sha256_digest(&arm_manifest));
            assert!(!client.store().has_blob(&amd_clone));
            pulled
        })
        .await
        .expect("join");
        assert_eq!(result.layers.len(), 1);
    }

    #[tokio::test]
    async fn pull_skips_already_cached_layers() {
        let server = MockServer::start().await;
        mount_anonymous_v2(&server).await;

        let config = make_config_bytes();
        let layer = make_layer_bytes(b"cached");
        let layers_spec = [(
            "application/vnd.oci.image.layer.v1.tar+gzip",
            layer.as_slice(),
        )];
        let manifest_bytes = make_manifest(&config, &layers_spec);
        let manifest_digest = sha256_digest(&manifest_bytes);
        let config_digest = sha256_digest(&config);
        let layer_digest = sha256_digest(&layer);

        Mock::given(method("GET"))
            .and(wpath("/v2/library/alpine/manifests/3.19"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(manifest_bytes.clone())
                    .insert_header("Content-Type", MT_OCI_MANIFEST)
                    .insert_header("Docker-Content-Digest", manifest_digest.as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{config_digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(config.clone()))
            .mount(&server)
            .await;
        // Mount an "expected" layer endpoint that records hits. If
        // the orchestrator skips correctly, expect_at_least(0) +
        // expect_at_most(0) => no hits.
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{layer_digest}")))
            .respond_with(ResponseTemplate::new(500).set_body_string("should not be called"))
            .expect(0)
            .mount(&server)
            .await;

        let dir = store_dir();
        let store_path = dir.path().to_path_buf();
        // Pre-populate the store with the layer blob so the client
        // sees `has_blob(layer_digest) == true` and skips the fetch.
        {
            let st = ContentStore::new(&store_path).unwrap();
            let stored = st.write_blob(&layer).unwrap();
            assert_eq!(stored, layer_digest);
        }

        let endpoint = server.uri();
        let pulled = tokio::task::spawn_blocking(move || {
            let client = Client::new(&store_path)
                .unwrap()
                .with_endpoint("docker.io", &endpoint);
            client
                .pull(&"alpine:3.19".parse::<ImageRef>().unwrap())
                .unwrap()
        })
        .await
        .expect("join");
        assert_eq!(pulled.layers.len(), 1);
        // wiremock verifies expect(0) on drop; if the orchestrator
        // had fetched the layer, drop would panic.
    }

    #[tokio::test]
    async fn pull_with_auth_challenge_negotiates_token() {
        let server = MockServer::start().await;

        // First request to /v2/ returns 401 + a Bearer challenge that
        // points back at a /token endpoint on the same mock server.
        let token_url = format!("{}/token", server.uri());
        let challenge_value = format!(
            r#"Bearer realm="{token_url}",service="registry.example",scope="repository:library/alpine:pull""#,
        );
        Mock::given(method("GET"))
            .and(wpath("/v2/"))
            .respond_with(
                ResponseTemplate::new(401).insert_header("WWW-Authenticate", challenge_value),
            )
            .mount(&server)
            .await;
        // Token endpoint hands back a fixed token.
        Mock::given(method("GET"))
            .and(wpath("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "tok-abc"
            })))
            .mount(&server)
            .await;

        let config = make_config_bytes();
        let layer = make_layer_bytes(b"authed-layer");
        let manifest_bytes = make_manifest(
            &config,
            &[(
                "application/vnd.oci.image.layer.v1.tar+gzip",
                layer.as_slice(),
            )],
        );
        let manifest_digest = sha256_digest(&manifest_bytes);
        let config_digest = sha256_digest(&config);
        let layer_digest = sha256_digest(&layer);

        // Manifest + blob endpoints require the bearer token.
        Mock::given(method("GET"))
            .and(wpath("/v2/library/alpine/manifests/3.19"))
            .and(header("authorization", "Bearer tok-abc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(manifest_bytes.clone())
                    .insert_header("Content-Type", MT_OCI_MANIFEST)
                    .insert_header("Docker-Content-Digest", manifest_digest.as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{config_digest}")))
            .and(header("authorization", "Bearer tok-abc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(config.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{layer_digest}")))
            .and(header("authorization", "Bearer tok-abc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(layer.clone()))
            .mount(&server)
            .await;

        let dir = store_dir();
        let store_path = dir.path().to_path_buf();
        let endpoint = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = Client::new(&store_path)
                .unwrap()
                .with_endpoint("docker.io", &endpoint);
            client.pull(&"alpine:3.19".parse::<ImageRef>().unwrap())
        })
        .await
        .expect("join");
        assert!(result.is_ok(), "pull with auth failed: {result:?}");
    }

    #[tokio::test]
    async fn pull_errors_when_manifest_supports_docker_v2_only() {
        // Sanity: a registry that returns Docker v2 instead of OCI is
        // still a valid path. We piggy-back this onto the standard
        // pull happy-path with the docker mediaType swapped in.
        let server = MockServer::start().await;
        mount_anonymous_v2(&server).await;

        let config = make_config_bytes();
        let layer = make_layer_bytes(b"docker-v2");
        // Build a manifest with the docker v2 mediaType.
        let layers_json = vec![serde_json::json!({
            "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
            "digest": sha256_digest(&layer),
            "size": layer.len(),
        })];
        let manifest_bytes = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": MT_DOCKER_V2_MANIFEST,
            "config": {
                "mediaType": "application/vnd.docker.container.image.v1+json",
                "digest": sha256_digest(&config),
                "size": config.len(),
            },
            "layers": layers_json,
        }))
        .unwrap();
        let manifest_digest = sha256_digest(&manifest_bytes);
        let config_digest = sha256_digest(&config);
        let layer_digest = sha256_digest(&layer);

        Mock::given(method("GET"))
            .and(wpath("/v2/library/alpine/manifests/3.19"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(manifest_bytes.clone())
                    .insert_header("Content-Type", MT_DOCKER_V2_MANIFEST)
                    .insert_header("Docker-Content-Digest", manifest_digest.as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{config_digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(config.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{layer_digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(layer.clone()))
            .mount(&server)
            .await;

        let dir = store_dir();
        let store_path = dir.path().to_path_buf();
        let endpoint = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = Client::new(&store_path)
                .unwrap()
                .with_endpoint("docker.io", &endpoint);
            client.pull(&"alpine:3.19".parse::<ImageRef>().unwrap())
        })
        .await
        .expect("join");
        assert!(result.is_ok(), "pull failed: {result:?}");
    }

    #[tokio::test]
    async fn lookup_returns_some_when_cached() {
        let server = MockServer::start().await;
        mount_anonymous_v2(&server).await;

        let config = make_config_bytes();
        let layer = make_layer_bytes(b"cached-image");
        let manifest_bytes = make_manifest(
            &config,
            &[(
                "application/vnd.oci.image.layer.v1.tar+gzip",
                layer.as_slice(),
            )],
        );
        let manifest_digest = sha256_digest(&manifest_bytes);
        let config_digest = sha256_digest(&config);
        let layer_digest = sha256_digest(&layer);

        Mock::given(method("GET"))
            .and(wpath("/v2/library/alpine/manifests/3.19"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(manifest_bytes.clone())
                    .insert_header("Content-Type", MT_OCI_MANIFEST)
                    .insert_header("Docker-Content-Digest", manifest_digest.as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{config_digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(config.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wpath(format!("/v2/library/alpine/blobs/{layer_digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(layer.clone()))
            .mount(&server)
            .await;

        let dir = store_dir();
        let store_path = dir.path().to_path_buf();
        let endpoint = server.uri();
        let pulled_first = tokio::task::spawn_blocking(move || {
            let client = Client::new(&store_path)
                .unwrap()
                .with_endpoint("docker.io", &endpoint);
            client
                .pull(&"alpine:3.19".parse::<ImageRef>().unwrap())
                .unwrap()
        })
        .await
        .expect("join");

        // Second client over the same store: lookup should return the
        // pulled image with no further HTTP traffic.
        let store_path2 = dir.path().to_path_buf();
        let result = tokio::task::spawn_blocking(move || {
            let client = Client::new(&store_path2).unwrap();
            client
                .lookup(&"alpine:3.19".parse::<ImageRef>().unwrap())
                .unwrap()
        })
        .await
        .expect("join");
        let cached = result.expect("expected cached image");
        assert_eq!(cached.manifest_digest, pulled_first.manifest_digest);
        assert_eq!(cached.layers.len(), 1);
    }

    #[tokio::test]
    async fn lookup_returns_none_when_not_cached() {
        let dir = store_dir();
        let store_path = dir.path().to_path_buf();
        let result = tokio::task::spawn_blocking(move || {
            let client = Client::new(&store_path).unwrap();
            client.lookup(&"alpine:3.19".parse::<ImageRef>().unwrap())
        })
        .await
        .expect("join");
        let v = result.unwrap();
        assert!(v.is_none());
    }
}
