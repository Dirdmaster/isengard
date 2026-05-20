//! Backup destination trait and implementations.
//!
//! Two backends ship in 11a:
//!
//! - [`LocalDestination`] writes encrypted snapshots to a directory
//!   on the controller host. Useful for self-hosters with a NAS
//!   mount or external disk; also the integration-test substrate.
//! - [`S3Destination`] PUTs / GETs / LISTs / DELETEs against any
//!   S3-compatible endpoint (Cloudflare R2 is the documented default;
//!   AWS S3, Wasabi, B2, MinIO work through the same trait). SigV4
//!   signing is hand-rolled with `hmac` + `sha2` so the dependency
//!   tree stays small (no `aws-sdk-s3`).

use std::path::PathBuf;

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tokio::fs;

/// One snapshot object we know about on the destination.
///
/// Used by retention to enumerate candidates for deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObject {
    /// Object key (without any configured prefix).
    pub name: String,
    /// Object size in bytes.
    pub size: i64,
}

/// Errors raised while talking to a backup destination.
#[derive(Debug, thiserror::Error)]
pub enum DestinationError {
    /// Filesystem IO failure (local destination, tempfile, etc.).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP transport failure.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// Failed to parse a URL.
    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),

    /// XML parse failure (currently only the S3 list response).
    #[error("xml parse: {0}")]
    Xml(String),

    /// Destination returned a non-success HTTP status.
    #[error("destination returned status {status}: {body}")]
    Status {
        /// Numeric HTTP status code.
        status: u16,
        /// Response body, included verbatim for diagnostics.
        body: String,
    },

    /// Object name contains path-traversal characters.
    #[error("invalid object name: {0}")]
    InvalidName(String),
}

/// What the runner uses to talk to remote storage.
///
/// One implementation per supported backend. The trait is
/// intentionally small: upload, list, delete, download.
#[async_trait]
pub trait BackupDestination: Send + Sync {
    /// Stable name shown in the UI (e.g. `"local"`, `"s3"`).
    fn kind(&self) -> &'static str;

    /// Uploads `bytes` under `name`. The runner picks the name
    /// (timestamp-based).
    async fn upload(&self, name: &str, bytes: &[u8]) -> Result<(), DestinationError>;

    /// Lists every object the destination is responsible for.
    ///
    /// Used by retention pruning. Returns objects with the prefix
    /// stripped (so retention sees names that match what
    /// [`Self::upload`] wrote).
    async fn list(&self) -> Result<Vec<RemoteObject>, DestinationError>;

    /// Deletes an object by name. No-op when the object is missing.
    async fn delete(&self, name: &str) -> Result<(), DestinationError>;

    /// Downloads an object's bytes by name.
    ///
    /// Used by tests and by the restore flow.
    async fn download(&self, name: &str) -> Result<Vec<u8>, DestinationError>;
}

// =================== LocalDestination ===================

/// Filesystem-backed destination.
///
/// Writes under `root/prefix/<name>`. Used by self-hosters with a
/// NAS mount or external disk, and by integration tests.
pub struct LocalDestination {
    /// Filesystem root the plugin writes under.
    pub root: PathBuf,
    /// Optional sub-prefix inside `root`.
    pub prefix: String,
}

impl LocalDestination {
    /// Builds a destination from a root path and prefix.
    pub fn new(root: impl Into<PathBuf>, prefix: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            prefix: prefix.into(),
        }
    }

    /// Returns the effective directory: `root` when `prefix` is
    /// empty, `root/prefix` otherwise.
    fn dir(&self) -> PathBuf {
        if self.prefix.is_empty() {
            self.root.clone()
        } else {
            self.root.join(&self.prefix)
        }
    }

    /// Rejects names that would escape [`Self::dir`].
    fn validate_name(name: &str) -> Result<(), DestinationError> {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(DestinationError::InvalidName(name.into()));
        }
        Ok(())
    }
}

#[async_trait]
impl BackupDestination for LocalDestination {
    fn kind(&self) -> &'static str {
        "local"
    }

    async fn upload(&self, name: &str, bytes: &[u8]) -> Result<(), DestinationError> {
        Self::validate_name(name)?;
        fs::create_dir_all(self.dir()).await?;
        fs::write(self.dir().join(name), bytes).await?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<RemoteObject>, DestinationError> {
        let dir = self.dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut rd = fs::read_dir(dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let meta = entry.metadata().await?;
            if !meta.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            out.push(RemoteObject {
                name,
                size: meta.len() as i64,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn delete(&self, name: &str) -> Result<(), DestinationError> {
        Self::validate_name(name)?;
        let path = self.dir().join(name);
        match fs::remove_file(&path).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn download(&self, name: &str) -> Result<Vec<u8>, DestinationError> {
        Self::validate_name(name)?;
        let bytes = fs::read(self.dir().join(name)).await?;
        Ok(bytes)
    }
}

// =================== S3Destination ===================

/// Configuration for an S3-compatible destination.
///
/// `endpoint` is the bucket-relative endpoint (e.g.
/// `https://<accountid>.r2.cloudflarestorage.com`). For path-style
/// endpoints like MinIO, use the host URL and rely on `bucket` being
/// appended in the request path.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Bucket-relative endpoint URL.
    pub endpoint: String,
    /// AWS-style region string.
    pub region: String,
    /// Bucket name.
    pub bucket: String,
    /// Optional key prefix inside the bucket.
    pub prefix: String,
    /// IAM-style access key id.
    pub access_key_id: String,
    /// IAM-style secret. Never logged.
    pub secret_access_key: String,
}

/// S3-compatible destination.
///
/// Uses a hand-rolled SigV4-S3 signer: payload SHA-256 in header,
/// no chunked encoding.
pub struct S3Destination {
    /// Resolved config block.
    pub cfg: S3Config,
    /// HTTP client used for every API call.
    client: reqwest::Client,
}

impl S3Destination {
    /// Builds a destination with a fresh reqwest client.
    pub fn new(cfg: S3Config) -> Self {
        Self {
            cfg,
            client: reqwest::Client::new(),
        }
    }

    /// Builds the path-style URL for `key`.
    ///
    /// R2, AWS S3, and MinIO all accept
    /// `<endpoint>/<bucket>/<prefix>/<key>`.
    fn url_for_key(&self, key: &str) -> Result<reqwest::Url, DestinationError> {
        let base = self.cfg.endpoint.trim_end_matches('/').to_string();
        let prefix = self.cfg.prefix.trim_matches('/');
        let key_part = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}/{key}")
        };
        let url_str = format!("{base}/{}/{key_part}", self.cfg.bucket);
        Ok(reqwest::Url::parse(&url_str)?)
    }

    /// Builds the SigV4 Authorization header and `x-amz-date` value
    /// for a request.
    ///
    /// Pure SigV4-S3 signing: payload SHA-256 in header, no chunked
    /// encoding. Returns `(authorization, x-amz-date)`.
    fn sign(
        &self,
        method: &str,
        url: &reqwest::Url,
        payload_sha256: &str,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> (String, String) {
        let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = timestamp.format("%Y%m%d").to_string();
        let host = url.host_str().unwrap_or("").to_string();
        let canonical_uri = url.path().to_string();
        let canonical_query = url.query().unwrap_or("").to_string();

        let canonical_headers = format!(
            "host:{host}\n\
             x-amz-content-sha256:{payload_sha256}\n\
             x-amz-date:{amz_date}\n",
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";

        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256}"
        );

        let cr_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
        let scope = format!("{date_stamp}/{}/s3/aws4_request", self.cfg.region);
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{cr_hash}");

        let signature = self.derive_signature(&date_stamp, &string_to_sign);

        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.cfg.access_key_id,
        );
        (auth, amz_date)
    }

    /// Performs the SigV4 four-step HMAC chain to derive a signing
    /// key, then signs `string_to_sign`.
    fn derive_signature(&self, date_stamp: &str, string_to_sign: &str) -> String {
        type HmacSha256 = Hmac<Sha256>;
        let k_secret = format!("AWS4{}", self.cfg.secret_access_key);
        let k_date = HmacSha256::new_from_slice(k_secret.as_bytes())
            .unwrap()
            .chain_update(date_stamp.as_bytes())
            .finalize()
            .into_bytes();
        let k_region = HmacSha256::new_from_slice(&k_date)
            .unwrap()
            .chain_update(self.cfg.region.as_bytes())
            .finalize()
            .into_bytes();
        let k_service = HmacSha256::new_from_slice(&k_region)
            .unwrap()
            .chain_update(b"s3")
            .finalize()
            .into_bytes();
        let k_signing = HmacSha256::new_from_slice(&k_service)
            .unwrap()
            .chain_update(b"aws4_request")
            .finalize()
            .into_bytes();
        let sig = HmacSha256::new_from_slice(&k_signing)
            .unwrap()
            .chain_update(string_to_sign.as_bytes())
            .finalize()
            .into_bytes();
        hex::encode(sig)
    }
}

#[async_trait]
impl BackupDestination for S3Destination {
    fn kind(&self) -> &'static str {
        "s3"
    }

    async fn upload(&self, name: &str, bytes: &[u8]) -> Result<(), DestinationError> {
        let url = self.url_for_key(name)?;
        let payload_hash = hex::encode(Sha256::digest(bytes));
        let now = chrono::Utc::now();
        let (auth, amz_date) = self.sign("PUT", &url, &payload_hash, now);

        let resp = self
            .client
            .put(url)
            .header("authorization", auth)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", amz_date)
            .body(bytes.to_vec())
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(DestinationError::Status { status, body });
        }
        Ok(())
    }

    async fn list(&self) -> Result<Vec<RemoteObject>, DestinationError> {
        let prefix_q = self.cfg.prefix.trim_matches('/');
        let base = self.cfg.endpoint.trim_end_matches('/');
        let mut url_str = format!("{base}/{}/?list-type=2", self.cfg.bucket);
        if !prefix_q.is_empty() {
            url_str.push_str(&format!("&prefix={}/", urlencoding(prefix_q)));
        }
        let url = reqwest::Url::parse(&url_str)?;
        let payload_hash = hex::encode(Sha256::digest(b""));
        let now = chrono::Utc::now();
        let (auth, amz_date) = self.sign("GET", &url, &payload_hash, now);

        let resp = self
            .client
            .get(url)
            .header("authorization", auth)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", amz_date)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(DestinationError::Status { status, body });
        }
        let xml = resp.text().await?;
        Ok(parse_list_v2(&xml, prefix_q))
    }

    async fn delete(&self, name: &str) -> Result<(), DestinationError> {
        let url = self.url_for_key(name)?;
        let payload_hash = hex::encode(Sha256::digest(b""));
        let now = chrono::Utc::now();
        let (auth, amz_date) = self.sign("DELETE", &url, &payload_hash, now);

        let resp = self
            .client
            .delete(url)
            .header("authorization", auth)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", amz_date)
            .send()
            .await?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(DestinationError::Status { status, body });
        }
        Ok(())
    }

    async fn download(&self, name: &str) -> Result<Vec<u8>, DestinationError> {
        let url = self.url_for_key(name)?;
        let payload_hash = hex::encode(Sha256::digest(b""));
        let now = chrono::Utc::now();
        let (auth, amz_date) = self.sign("GET", &url, &payload_hash, now);

        let resp = self
            .client
            .get(url)
            .header("authorization", auth)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", amz_date)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(DestinationError::Status { status, body });
        }
        let bytes = resp.bytes().await?.to_vec();
        Ok(bytes)
    }
}

/// Minimal URL-encoding for the prefix-list query.
///
/// Handles the chars likely to appear in operator-set prefixes
/// (slash, alnum, dash, underscore, dot, tilde). Everything else is
/// hex-encoded.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Hand-rolled parser for the bits of the S3 ListObjectsV2 response
/// the plugin needs.
///
/// Extracts `<Contents><Key>` and `<Contents><Size>` pairs;
/// pagination is not handled (retention prunes a small list).
fn parse_list_v2(xml: &str, prefix: &str) -> Vec<RemoteObject> {
    let mut out = Vec::new();
    let mut current_key: Option<String> = None;
    let mut current_size: Option<i64> = None;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_contents = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                let name_owned = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match name_owned.as_str() {
                    "Contents" => in_contents = true,
                    "Key" if in_contents => {
                        current_key = Some(read_text(&mut reader));
                    }
                    "Size" if in_contents => {
                        current_size = read_text(&mut reader).parse::<i64>().ok();
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::End(e)) => {
                let name_owned = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name_owned == "Contents" {
                    in_contents = false;
                    if let (Some(k), Some(sz)) = (current_key.take(), current_size.take()) {
                        let stripped = if prefix.is_empty() {
                            k.clone()
                        } else {
                            k.strip_prefix(&format!("{prefix}/"))
                                .unwrap_or(&k)
                                .to_string()
                        };
                        if !stripped.is_empty() {
                            out.push(RemoteObject {
                                name: stripped,
                                size: sz,
                            });
                        }
                    }
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Reads the text content of the current XML element.
fn read_text<R: std::io::BufRead>(reader: &mut quick_xml::Reader<R>) -> String {
    let mut buf = Vec::new();
    if let Ok(quick_xml::events::Event::Text(t)) = reader.read_event_into(&mut buf) {
        return t.unescape().map(|c| c.into_owned()).unwrap_or_default();
    }
    String::new()
}
