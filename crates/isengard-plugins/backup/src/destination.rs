//! Backup destination trait + implementations.
//!
//! Two backends ship in 11a:
//!
//! - `LocalDestination`: writes encrypted snapshots to a directory on the
//!   controller host. Useful for self-hosters with a NAS mount or external
//!   disk; also the integration-test substrate.
//! - `S3Destination`: PUTs / GETs / LISTs / DELETEs against any S3-compatible
//!   endpoint (Cloudflare R2 is the documented default; AWS S3, Wasabi, B2,
//!   MinIO all work via the same trait). SigV4 signing is hand-rolled with
//!   `hmac` + `sha2` so the dependency tree stays small (no aws-sdk-s3).

use std::path::PathBuf;

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tokio::fs;

/// A snapshot object we know about on the destination, used for retention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObject {
    pub name: String,
    pub size: i64,
}

/// Errors raised while talking to a backup destination.
#[derive(Debug, thiserror::Error)]
pub enum DestinationError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),

    #[error("xml parse: {0}")]
    Xml(String),

    #[error("destination returned status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("invalid object name: {0}")]
    InvalidName(String),
}

/// What an upload + retention runner uses to talk to remote storage.
#[async_trait]
pub trait BackupDestination: Send + Sync {
    /// Stable name shown in the UI (e.g. "local", "s3").
    fn kind(&self) -> &'static str;

    /// Upload bytes under `name`. The runner picks the name (timestamp-based).
    async fn upload(&self, name: &str, bytes: &[u8]) -> Result<(), DestinationError>;

    /// List every object the destination is responsible for, regardless of
    /// when it was uploaded. Used by retention pruning.
    async fn list(&self) -> Result<Vec<RemoteObject>, DestinationError>;

    /// Delete an object by name. No-op if missing (returns Ok).
    async fn delete(&self, name: &str) -> Result<(), DestinationError>;

    /// Download an object's bytes by name. Used by tests + the future restore
    /// flow.
    async fn download(&self, name: &str) -> Result<Vec<u8>, DestinationError>;
}

// =================== LocalDestination ===================

/// Filesystem-backed destination. Writes under `root/prefix/<name>`.
pub struct LocalDestination {
    pub root: PathBuf,
    pub prefix: String,
}

impl LocalDestination {
    pub fn new(root: impl Into<PathBuf>, prefix: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            prefix: prefix.into(),
        }
    }

    fn dir(&self) -> PathBuf {
        if self.prefix.is_empty() {
            self.root.clone()
        } else {
            self.root.join(&self.prefix)
        }
    }

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
/// `https://<accountid>.r2.cloudflarestorage.com`). For path-style endpoints
/// like MinIO, use the host URL and rely on `bucket` to be appended in the
/// request path.
#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub struct S3Destination {
    pub cfg: S3Config,
    client: reqwest::Client,
}

impl S3Destination {
    pub fn new(cfg: S3Config) -> Self {
        Self {
            cfg,
            client: reqwest::Client::new(),
        }
    }

    /// Path-style URL. R2, AWS S3, MinIO all accept `<endpoint>/<bucket>/<key>`.
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

    /// Build a SigV4 Authorization header for the given request.
    /// Pure SigV4-S3 signing: payload sha256 in header, no chunked encoding.
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

        // Canonical headers (sorted, lowercase). We always include host,
        // x-amz-content-sha256, x-amz-date.
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

/// Minimal URL-encoding for the prefix-list query. We only handle the chars
/// likely to appear in user-set prefixes (slash, alnum, dash, underscore,
/// dot, percent). Anything else is hex-encoded.
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

/// Hand-rolled parser for the bits of the ListObjectsV2 response we need.
/// We only extract `<Contents><Key>` and `<Size>` pairs; pagination is not
/// handled in 11a (retention prunes a small list).
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

fn read_text<R: std::io::BufRead>(reader: &mut quick_xml::Reader<R>) -> String {
    let mut buf = Vec::new();
    if let Ok(quick_xml::events::Event::Text(t)) = reader.read_event_into(&mut buf) {
        return t.unescape().map(|c| c.into_owned()).unwrap_or_default();
    }
    String::new()
}
